//! Explicit operator attestation for pre-ledger logs. Maintenance only prunes listings.
use rusqlite::{OptionalExtension, params};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};

use crate::api::Shared;
use crate::error::{ApiError, ApiResult};
use crate::executor::{RemoteCmd, sh_quote};
use crate::store::{jobs, targets};

const PAGE: usize = 256;
const PRUNE_SQL: &str = "DELETE FROM legacy_log_listings WHERE digest IN (SELECT digest FROM legacy_log_listings INDEXED BY legacy_log_listings_expiry WHERE expires_at < ? ORDER BY expires_at LIMIT 256)";

/// Keep listings for a day after expiry, including consumed listings. No log data is touched.
pub(crate) async fn prune(
    db: &crate::db::Db,
    now: chrono::DateTime<chrono::Utc>,
) -> ApiResult<usize> {
    let cutoff = (now - chrono::Duration::days(1)).to_rfc3339();
    db.write("system", move |tx| Ok(tx.execute(PRUNE_SQL, [cutoff])?))
        .await
}

const PAGE_SQL: &str = "SELECT id FROM jobs INDEXED BY jobs_legacy_log_page WHERE project=? AND id>? ORDER BY id LIMIT 256";

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
struct Entry {
    job: String,
    attempt: i64,
    ended_at: String,
    path: String,
    root: String,
    root_identity: String,
    file_identity: String,
    size: i64,
    mtime: String,
}

pub(crate) fn inspect_script(path: &str, root: &str) -> String {
    let Some(name) = std::path::Path::new(path)
        .file_name()
        .and_then(|p| p.to_str())
    else {
        return "exit 1".into();
    };
    if std::path::Path::new(path).parent().and_then(|p| p.to_str()) != Some(root)
        || name.starts_with('.')
    {
        return "exit 1".into();
    }
    format!(
        r#"root={root}
name={name}
{enter}
enter_root || exit 1
[ ! -L .retention.lock ] || exit 1
exec 7>>.retention.lock || exit 1
flock -n 7 || exit 1
[ -f "$name" ] && [ ! -L "$name" ] || exit 1
exec 9<"$name" || exit 1
flock -n 9 || exit 1
[ "$name" -ef /proc/self/fd/9 ] && [ ! -L "$name" ] || exit 1
printf '%s|' "$(stat -c '%d:%i' .)"
stat -Lc '%d:%i|%s|%y' /proc/self/fd/9
"#,
        root = sh_quote(root),
        name = sh_quote(name),
        enter = crate::job_logs::ENTER_ROOT
    )
}

async fn inspect(
    state: &Shared,
    target: &terrarium_core::config::TargetDef,
    mut entry: Entry,
) -> ApiResult<Option<Entry>> {
    let cmd = RemoteCmd {
        argv: vec![
            "sh".into(),
            "-c".into(),
            inspect_script(&entry.path, &entry.root),
        ],
        timeout_secs: Some(10),
        ..RemoteCmd::default()
    };
    let Ok(run) = crate::executor::start_cmd(state, target, &cmd) else {
        return Ok(None);
    };
    let result = run.collect(512).await;
    if !result.end.as_ref().is_some_and(|e| e.success()) {
        return Ok(None);
    }
    let out = String::from_utf8_lossy(&result.stdout);
    let parts: Vec<_> = out.trim().split('|').collect();
    if parts.len() != 4 {
        return Ok(None);
    }
    let Ok(size) = parts[2].parse::<i64>() else {
        return Ok(None);
    };
    if size < 0 {
        return Ok(None);
    }
    entry.root_identity = parts[0].to_owned();
    entry.file_identity = parts[1].to_owned();
    entry.size = size;
    entry.mtime = parts[3].to_owned();
    Ok(Some(entry))
}

pub async fn list(
    state: &Shared,
    project: String,
    target: String,
    after: String,
    older_than_secs: u64,
) -> ApiResult<Value> {
    let cutoff = (chrono::Utc::now()
        - chrono::Duration::seconds(older_than_secs.min(315_360_000) as i64))
    .to_rfc3339();
    let p = project.clone();
    let name = target.clone();
    let (def, identity, entries, next, mut skipped) = state
        .db
        .read(move |c| {
            let delivery = jobs::delivery(c, &p)?;
            if delivery.target.as_deref() != Some(&name) {
                return Err(ApiError::invalid(
                    "target is not the project's current delivery target",
                ));
            }
            let row = targets::get(c, &name)?;
            if row.def.project.as_ref().is_some_and(|owner| owner != &p) {
                return Err(ApiError::forbidden("target belongs to another project"));
            }
            let identity = crate::job_logs::target_identity(&row);
            let root = format!("{}/{}/logs", delivery.work_dir().trim_end_matches('/'), p);
            let ids = c
                .prepare(PAGE_SQL)?
                .query_map(params![p, after], |r| r.get::<_, String>(0))?
                .collect::<Result<Vec<_>, _>>()?;
            let next = if ids.len() == PAGE {
                ids.last().cloned().unwrap_or_default()
            } else {
                String::new()
            };
            let mut entries = Vec::new();
            let mut skipped = std::collections::BTreeMap::<String, usize>::new();
            for id in ids {
                let job = jobs::get(c, &id)?;
                let reason = if job.kind == jobs::JobKind::Release {
                    Some("release")
                } else if job.state.is_open()
                    || job.ended_at.as_ref().is_none_or(|end| end >= &cutoff)
                {
                    Some("not_terminal_before_cutoff")
                } else if !ulid::Ulid::from_string(&id).is_ok_and(|value| value.to_string() == id)
                    || job.attempt <= 0
                {
                    Some("non_matching_name")
                } else if c
                    .query_row(
                        "SELECT 1 FROM job_logs WHERE job=? AND attempt=?",
                        params![id, job.attempt],
                        |r| r.get::<_, i64>(0),
                    )
                    .optional()?
                    .is_some()
                {
                    Some("already_tracked")
                } else {
                    None
                };
                if let Some(reason) = reason {
                    *skipped.entry(reason.into()).or_default() += 1;
                    continue;
                }
                entries.push(Entry {
                    path: format!("{root}/{id}-{}.log", job.attempt),
                    root: root.clone(),
                    job: id,
                    attempt: job.attempt,
                    ended_at: job.ended_at.unwrap_or_default(),
                    root_identity: String::new(),
                    file_identity: String::new(),
                    size: 0,
                    mtime: String::new(),
                });
            }
            Ok((row.def, identity, entries, next, skipped))
        })
        .await?;
    let mut observed = Vec::new();
    for entry in entries {
        match inspect(state, &def, entry).await? {
            Some(entry) => observed.push(entry),
            None => {
                *skipped
                    .entry("missing_unsafe_locked_or_unreachable".into())
                    .or_default() += 1
            }
        }
    }
    let expires = (chrono::Utc::now() + chrono::Duration::minutes(15)).to_rfc3339();
    let encoded =
        serde_json::to_string(&observed).map_err(|e| ApiError::internal(e.to_string()))?;
    let nonce = ulid::Ulid::new().to_string();
    let digest = terrarium_host::protocol::hex(&Sha256::digest(
        serde_json::to_vec(&json!([project, target, identity, expires, encoded, nonce]))
            .map_err(|e| ApiError::internal(e.to_string()))?,
    ));
    let (p, t, d, e) = (project, target, digest.clone(), expires.clone());
    state.db.write("operator",move |tx| {
        tx.execute("INSERT INTO legacy_log_listings(digest,project,target,target_identity,expires_at,entries) VALUES (?,?,?,?,?,?)",params![d,p,t,identity,e,encoded])?;
        Ok(())
    }).await?;
    let total_bytes: i64 = observed.iter().map(|e| e.size).fold(0, i64::saturating_add);
    Ok(
        json!({"digest":digest,"expires_at":expires,"entries":observed,"total":observed.len(),"total_bytes":total_bytes,"skipped":skipped,"next":next,"note":"Only exact names derived from this indexed job page are probed. Unrelated directory entries are not scanned or counted. Confirmation is an explicit operator ownership attestation."}),
    )
}

pub async fn confirm(
    state: &Shared,
    project: String,
    target: String,
    digest: String,
) -> ApiResult<Value> {
    let (p, t, d) = (project.clone(), target.clone(), digest.clone());
    let (identity,entries,def) = state.db.read(move |c| {
        let row = c.query_row("SELECT target_identity,entries,expires_at,consumed FROM legacy_log_listings WHERE digest=? AND project=? AND target=?",params![d,p,t],|r| Ok((r.get::<_,String>(0)?,r.get::<_,String>(1)?,r.get::<_,String>(2)?,r.get::<_,bool>(3)?))).optional()?.ok_or_else(||ApiError::conflict("unknown or stale legacy log listing"))?;
        if row.3 || row.2 <= chrono::Utc::now().to_rfc3339() { return Err(ApiError::conflict("legacy log listing expired or already confirmed")); }
        let target = targets::get(c,&t)?;
        if crate::job_logs::target_identity(&target) != row.0 { return Err(ApiError::conflict("listing target changed")); }
        let entries: Vec<Entry> = serde_json::from_str(&row.1).map_err(|e|ApiError::internal(e.to_string()))?;
        Ok((row.0,entries,target.def))
    }).await?;
    let mut verified = Vec::new();
    let mut changed = 0;
    for entry in entries {
        if inspect(state, &def, entry.clone()).await?.as_ref() == Some(&entry) {
            verified.push(entry);
        } else {
            changed += 1;
        }
    }
    let p = project.clone();
    let ids = state.db.write("operator",move |tx| {
        if tx.execute("UPDATE legacy_log_listings SET consumed=1 WHERE digest=? AND consumed=0 AND expires_at>?",params![digest,chrono::Utc::now().to_rfc3339()])? != 1 { return Err(ApiError::conflict("legacy log listing expired or already confirmed")); }
        let current = targets::get(tx,&target)?;
        if crate::job_logs::target_identity(&current) != identity { return Err(ApiError::conflict("listing target changed")); }
        let mut ids = Vec::new();
        for e in verified {
            let job = jobs::get(tx,&e.job)?;
            if job.project != p || job.state.is_open() || job.ended_at.as_deref() != Some(e.ended_at.as_str()) || job.attempt != e.attempt || job.kind == jobs::JobKind::Release { changed += 1; continue; }
            let n = tx.execute("INSERT OR IGNORE INTO job_logs(project,job,attempt,target,path,root,root_identity,file_identity,target_identity,written_at,expires_at,bytes,legacy_size,legacy_mtime) VALUES (?,?,?,?,?,?,?,?,?,?,?,?,?,?)",params![p,e.job,e.attempt,target,e.path,e.root,e.root_identity,e.file_identity,identity,job.ended_at,"2000-01-01",e.size,e.size,e.mtime])?;
            if n == 0 { changed += 1; continue; }
            let id = tx.last_insert_rowid();
            let quarantine = format!("{}/.retention-delete-{id}",e.root);
            tx.execute("UPDATE job_logs SET cleanup_state='quarantining',quarantine=? WHERE id=?",params![quarantine,id])?;
            tx.execute("INSERT OR IGNORE INTO job_log_budgets(project) VALUES (?)",[&p])?;
            tx.execute("UPDATE job_log_budgets SET bytes=bytes+? WHERE project=?",params![e.size,p])?;
            tx.audit("job.legacy_log_confirmed",&e.path,Some(&p),json!({"digest":digest,"ledger_id":id,"inode":e.file_identity,"bytes":e.size}))?;
            ids.push(id);
        }
        Ok((ids,changed))
    }).await?;
    let mut result = crate::job_logs::cleanup_selected(state, project, 0, Some(ids.0)).await?;
    result["changed"] = json!(ids.1);
    Ok(result)
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]
    use super::*;

    #[test]
    fn listing_prune_is_bounded_indexed_and_preserves_live_rows() {
        let dir = tempfile::tempdir().unwrap();
        let db = crate::db::open(&dir.path().join("db")).unwrap();
        for n in 0..300 {
            db.execute("INSERT INTO legacy_log_listings VALUES (?,'p','t','identity','2026-01-01T00:00:00+00:00','[]',?)", params![format!("old-{n}"), n % 2]).unwrap();
        }
        for (id, expiry, consumed) in [
            ("live", "2026-01-04T00:00:00+00:00", 0),
            ("recent", "2026-01-02T12:00:00+00:00", 0),
            ("consumed", "2026-01-02T12:00:00+00:00", 1),
            ("boundary", "2026-01-02T00:00:00+00:00", 1),
        ] {
            db.execute(
                "INSERT INTO legacy_log_listings VALUES (?,'p','t','identity',?,'[]',?)",
                params![id, expiry, consumed],
            )
            .unwrap();
        }
        let cutoff = "2026-01-02T00:00:00+00:00";
        let plans = db
            .prepare(&format!("EXPLAIN QUERY PLAN {PRUNE_SQL}"))
            .unwrap()
            .query_map([cutoff], |r| r.get::<_, String>(3))
            .unwrap()
            .collect::<Result<Vec<_>, _>>()
            .unwrap();
        assert!(
            plans
                .iter()
                .any(|p| p.contains("SEARCH") && p.contains("legacy_log_listings_expiry")),
            "{plans:?}"
        );
        assert!(
            !plans
                .iter()
                .any(|p| p.contains("SCAN legacy_log_listings") || p.contains("TEMP B-TREE")),
            "{plans:?}"
        );
        assert_eq!(db.execute(PRUNE_SQL, [cutoff]).unwrap(), 256);
        assert_eq!(db.execute(PRUNE_SQL, [cutoff]).unwrap(), 44);
        assert_eq!(db.execute(PRUNE_SQL, [cutoff]).unwrap(), 0);
        assert_eq!(
            db.query_row("SELECT count(*) FROM legacy_log_listings", [], |r| r
                .get::<_, i64>(0))
                .unwrap(),
            4
        );
    }

    #[test]
    fn inspection_refuses_symlinked_roots_and_files() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().join("logs");
        std::fs::create_dir(&root).unwrap();
        let path = root.join("job-1.log");
        std::fs::write(&path, "owned").unwrap();
        let run = || {
            std::process::Command::new("sh")
                .args([
                    "-c",
                    &inspect_script(path.to_str().unwrap(), root.to_str().unwrap()),
                ])
                .output()
                .unwrap()
        };
        assert!(run().status.success());
        std::fs::rename(&path, root.join("other")).unwrap();
        std::os::unix::fs::symlink(root.join("other"), &path).unwrap();
        assert!(!run().status.success());
        let alias = dir.path().join("alias");
        std::os::unix::fs::symlink(&root, &alias).unwrap();
        assert!(
            !std::process::Command::new("sh")
                .args([
                    "-c",
                    &inspect_script(
                        alias.join("other").to_str().unwrap(),
                        alias.to_str().unwrap()
                    )
                ])
                .status()
                .unwrap()
                .success()
        );
    }

    #[test]
    fn legacy_page_uses_a_bounded_indexed_seek() {
        let dir = tempfile::tempdir().unwrap();
        let db = crate::db::open(&dir.path().join("db")).unwrap();
        db.execute_batch("BEGIN").unwrap();
        for n in 0..650 {
            db.execute("INSERT INTO jobs(id,project,card,kind,repo,branch,state,created_by,created_at) VALUES (?,?,'C','validate','r','b','running','system','x')",params![format!("{n:026}"),if n%2==0 {"p"} else {"other"}]).unwrap();
        }
        db.execute_batch("COMMIT").unwrap();
        let page = |after: &str| {
            db.prepare(PAGE_SQL)
                .unwrap()
                .query_map(params!["p", after], |r| r.get::<_, String>(0))
                .unwrap()
                .collect::<Result<Vec<_>, _>>()
                .unwrap()
        };
        let first = page("");
        assert_eq!(first.len(), PAGE);
        let second = page(first.last().unwrap());
        assert_eq!(second.len(), 325 - PAGE);
        assert!(second[0] > first[PAGE - 1]);
        let plans = db
            .prepare(&format!("EXPLAIN QUERY PLAN {PAGE_SQL}"))
            .unwrap()
            .query_map(params!["p", ""], |r| r.get::<_, String>(3))
            .unwrap()
            .collect::<Result<Vec<_>, _>>()
            .unwrap();
        assert!(
            plans
                .iter()
                .any(|p| p.contains("SEARCH") && p.contains("jobs_legacy_log_page")),
            "{plans:?}"
        );
        assert!(
            !plans
                .iter()
                .any(|p| p.contains("SCAN") || p.contains("TEMP B-TREE")),
            "{plans:?}"
        );
    }
}
