//! Only newly recorded daemon logs participate in retention. No directory scans.
use rusqlite::{Connection, params};
use serde::Serialize;
use terrarium_core::config::DeliveryDef;

use crate::api::Shared;
use crate::error::{ApiError, ApiResult};
use crate::executor::{RemoteCmd, sh_quote};
use crate::store::{jobs, targets};

const BATCH: i64 = 32;

#[derive(Clone, Debug, Serialize)]
pub struct Log {
    pub id: i64,
    pub job: String,
    pub attempt: i64,
    pub target: String,
    pub path: String,
    pub root: String,
    pub root_identity: String,
    pub file_identity: String,
    #[serde(skip_serializing)]
    pub target_identity: String,
    pub written_at: String,
    pub expires_at: String,
    pub bytes: i64,
    pub expired: bool,
    pub cleanup_state: String,
    pub quarantine: String,
}

fn row(r: &rusqlite::Row<'_>) -> rusqlite::Result<Log> {
    Ok(Log {
        id: r.get(0)?,
        job: r.get(1)?,
        attempt: r.get(2)?,
        target: r.get(3)?,
        path: r.get(4)?,
        written_at: r.get(5)?,
        expires_at: r.get(6)?,
        bytes: r.get(7)?,
        expired: r.get(8)?,
        root: r.get(9)?,
        root_identity: r.get(10)?,
        file_identity: r.get(11)?,
        target_identity: r.get(12)?,
        cleanup_state: r.get(13)?,
        quarantine: r.get(14)?,
    })
}

const FIELDS: &str = "id, job, attempt, target, path, written_at, expires_at, bytes, expired, root, root_identity, file_identity, target_identity, cleanup_state, quarantine";
const CANDIDATES: &str = "SELECT id, job, attempt, target, path, written_at, expires_at, bytes, expired, root, root_identity, file_identity, target_identity, cleanup_state, quarantine FROM job_logs INDEXED BY job_logs_cleanup WHERE project=? AND cleanup_state=? AND id>? ORDER BY id LIMIT ?";

pub fn target_identity(row: &targets::TargetRow) -> String {
    // Name is the target primary key. Creation time distinguishes remove/recreate,
    // definition binds address/container/VM, state_at binds lifecycle replacement.
    serde_json::json!([row.created_at, row.def, row.state_at, row.runtime.lab]).to_string()
}

pub async fn begin(
    state: &Shared,
    def: &terrarium_core::config::TargetDef,
    job: &jobs::Job,
) -> ApiResult<Option<String>> {
    let def = def.clone();
    let (job, attempt, project) = (job.id.clone(), job.attempt, job.project.clone());
    state.db.write("system", move |t| {
        use rusqlite::OptionalExtension;
        let pending=t.query_row("SELECT cleanup_state FROM job_logs WHERE job=? AND attempt=?",params![job,attempt],|r|r.get::<_,String>(0)).optional()?;
        if pending.as_deref().is_some_and(|s|s=="quarantining" || s=="conflict") { return Err(ApiError::conflict("this log has unresolved quarantine ownership")); }
        // A lost attempt may rewrite its log, including on a no-flock host.
        // Revoke its old ownership before any new writer can start.
        t.execute("UPDATE job_log_budgets SET bytes=MAX(0,bytes-COALESCE((SELECT bytes FROM job_logs WHERE job=? AND attempt=? AND expired=0),0)) WHERE project=?",params![job,attempt,project])?;
        t.execute("DELETE FROM job_logs WHERE job=? AND attempt=?",params![job,attempt])?;
        let row = targets::get(t, &def.name)?;
        Ok((row.def == def).then(|| target_identity(&row)))
    }).await
}

pub async fn record_output(
    state: &Shared,
    job: &jobs::Job,
    def: &DeliveryDef,
    path: &str,
    identity: &str,
    stdout: &[u8],
) -> ApiResult<()> {
    let stdout = String::from_utf8_lossy(stdout);
    if let Some(proof) = stdout
        .lines()
        .find_map(|l| l.strip_prefix("TERRARIUM_LOG_TRACKED "))
    {
        record(state, job, def, path, identity, proof).await?;
    }
    Ok(())
}

fn due(log: &Log, now: &str, total: i64, limit: i64) -> bool {
    log.expires_at.as_str() <= now || total > limit
}

pub fn for_job(c: &Connection, job: &str) -> ApiResult<Vec<Log>> {
    Ok(c.prepare(&format!(
        "SELECT {FIELDS} FROM job_logs WHERE job=? ORDER BY attempt LIMIT 32"
    ))?
    .query_map([job], row)?
    .collect::<Result<_, _>>()?)
}

pub async fn record(
    state: &Shared,
    job: &jobs::Job,
    def: &DeliveryDef,
    path: &str,
    identity: &str,
    proof: &str,
) -> ApiResult<()> {
    if job.kind == jobs::JobKind::Release {
        return Ok(());
    }
    let (project, job, attempt, target, path) = (
        job.project.clone(),
        job.id.clone(),
        job.attempt,
        def.target
            .clone()
            .ok_or_else(|| ApiError::invalid("no job log target"))?,
        path.to_owned(),
    );
    let secs = def.log_retention_secs.unwrap_or(7 * 86400).min(315_360_000);
    let root = std::path::Path::new(&path)
        .parent()
        .and_then(|p| p.to_str())
        .ok_or_else(|| ApiError::invalid("invalid log root"))?
        .to_owned();
    let Some((root_identity, file_identity)) = proof.split_once(' ') else {
        return Err(ApiError::invalid("invalid log ownership proof"));
    };
    let (root_identity, file_identity, identity) = (
        root_identity.to_owned(),
        file_identity.to_owned(),
        identity.to_owned(),
    );
    let written = chrono::Utc::now();
    let expires = (written + chrono::Duration::seconds(secs as i64)).to_rfc3339();
    let written = written.to_rfc3339();
    state.db.write("system", move |t| {
        t.execute("INSERT OR IGNORE INTO job_log_budgets(project) VALUES (?)", [&project])?;
        // A lost attempt can run again. Its original ownership and expiry stay fixed.
        t.execute("INSERT OR IGNORE INTO job_logs(project,job,attempt,target,path,written_at,expires_at,root,root_identity,file_identity,target_identity) VALUES (?,?,?,?,?,?,?,?,?,?,?)",
            params![project, job, attempt, target, path, written, expires,root,root_identity,file_identity,identity])?;
        let owned: bool = t.query_row("SELECT target=? AND path=? AND expired=0 AND root_identity=? AND file_identity=? AND target_identity=? FROM job_logs WHERE job=? AND attempt=?", params![target,path,root_identity,file_identity,identity,job,attempt], |r| r.get(0))?;
        if !owned { return Err(ApiError::conflict("the retained log of this attempt changed target/path or expired")); }
        Ok(())
    }).await
}

pub async fn completed(state: &Shared, job: &jobs::Job, bytes: i64) -> ApiResult<()> {
    if job.kind == jobs::JobKind::Release {
        return Ok(());
    }
    let (id, attempt) = (job.id.clone(), job.attempt);
    state.db.write("system", move |t| {
        t.execute("UPDATE job_log_budgets SET bytes=bytes + ? - COALESCE((SELECT bytes FROM job_logs WHERE job=? AND attempt=?),0) WHERE project=(SELECT project FROM job_logs WHERE job=? AND attempt=? AND expired=0)",
            params![bytes, id, attempt, id, attempt])?;
        t.execute("UPDATE job_logs SET bytes=? WHERE job=? AND attempt=? AND expired=0", params![bytes,id,attempt])?;
        Ok(())
    }).await
}

/// Never open a symlink, never create a missing log, and never unlink a writer's
/// locked inode. Failure leaves the ledger unchanged so a later batch retries.
pub(crate) const ENTER_ROOT: &str = r#"enter_root() {
case "$root" in /*) ;; *) return 1;; esac
part=
saved_ifs=$IFS
IFS=/
set -f
for component in $root; do
  case "$component" in '') continue;; .|..) IFS=$saved_ifs; return 1;; esac
  part=$part/$component
  [ ! -L "$part" ] || { IFS=$saved_ifs; return 1; }
done
IFS=$saved_ifs
set +f
cd -P -- "$root" || return 1
[ "$(pwd -P)" = "$root" ] || return 1
}"#;

pub(crate) fn remove_script(
    path: &str,
    root: &str,
    root_identity: &str,
    file_identity: &str,
    quarantine: &str,
) -> String {
    let Some(name) = std::path::Path::new(path)
        .file_name()
        .and_then(|n| n.to_str())
    else {
        return "exit 1".into();
    };
    if std::path::Path::new(path).parent().and_then(|p| p.to_str()) != Some(root)
        || name.starts_with('.')
    {
        return "exit 1".into();
    }
    let Some(trash) = std::path::Path::new(quarantine)
        .file_name()
        .and_then(|p| p.to_str())
    else {
        return "exit 1".into();
    };
    if std::path::Path::new(quarantine)
        .parent()
        .and_then(|p| p.to_str())
        != Some(root)
        || !trash.starts_with(".retention-delete-")
    {
        return "exit 1".into();
    }
    format!(
        r#"root={root}
name={name}
{enter}
enter_root || exit 1
[ "$(stat -c '%d:%i' .)" = {root_identity} ] || exit 1
[ ! -L .retention.lock ] || exit 1
exec 7>>.retention.lock || exit 1
flock -n 7 || exit 1
trash={trash}
[ ! -L "$trash" ] || exit 1
reconcile() {{
  [ -e "$trash/log" ] || [ -L "$trash/log" ] || return 2
  if [ ! -L "$trash/log" ] && [ "$(stat -c '%d:%i' "$trash/log")" = {file_identity} ]; then
    exec 9<"$trash/log" || return 1
    flock -n 9 || return 1
    [ "$trash/log" -ef /proc/self/fd/9 ] && [ ! -L "$trash/log" ] || return 1
    [ "$(stat -Lc '%d:%i' /proc/self/fd/9)" = {file_identity} ] || return 1
    rm -- "$trash/log" || return 1
    rmdir -- "$trash" 2>/dev/null || :
    printf 'expired\n'
    return 0
  fi
  # A replacement was captured. Restore without overwriting anything. An
  # occupied restore is a durable conflict, not a successful expiry.
  if ln -P -- "$trash/log" "$name" 2>/dev/null; then
    rm -- "$trash/log" || return 1
    rmdir -- "$trash" 2>/dev/null || :
    printf 'expired\n'
  else
    printf 'conflict\n'
  fi
  return 0
}}
reconcile
rc=$?
[ "$rc" = 2 ] || exit "$rc"
if [ ! -e "$name" ] && [ ! -L "$name" ]; then rmdir -- "$trash" 2>/dev/null || :; printf 'expired\n'; exit 0; fi
[ -f "$name" ] && [ ! -L "$name" ] || exit 1
exec 9<"$name" || exit 1
flock -n 9 || exit 1
[ "$name" -ef /proc/self/fd/9 ] && [ ! -L "$name" ] || exit 1
if [ "$(stat -Lc '%d:%i' /proc/self/fd/9)" != {file_identity} ]; then printf 'expired\n'; exit 0; fi
[ -d "$trash" ] || (umask 077 && mkdir -- "$trash") || exit 1
mv -- "$name" "$trash/log" || exit 1
# AFTER_MOVE: persisted quarantining state makes this boundary recoverable.
exec 9<&-
reconcile
"#,
        root = sh_quote(root),
        name = sh_quote(name),
        enter = ENTER_ROOT,
        root_identity = sh_quote(root_identity),
        file_identity = sh_quote(file_identity),
        trash = sh_quote(trash),
    )
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
pub(crate) fn test_remove_script(path: &std::path::Path) -> String {
    use std::os::unix::fs::MetadataExt;
    let root = path.parent().unwrap();
    let identity = |p| {
        std::fs::metadata(p)
            .map(|m| format!("{}:{}", m.dev(), m.ino()))
            .unwrap_or_default()
    };
    remove_script(
        path.to_str().unwrap(),
        root.to_str().unwrap(),
        &identity(root),
        &identity(path),
        root.join(".retention-delete-1").to_str().unwrap(),
    )
}

/// A page is bounded before checking job states. The caller carries `after`
/// across pages, so active or unreachable logs cannot starve later records.
pub async fn cleanup(state: &Shared, project: String, after: i64) -> ApiResult<serde_json::Value> {
    let _guard = state
        .job_queue
        .log_cleanup
        .try_lock()
        .map_err(|_| ApiError::conflict("job log cleanup is already running"))?;
    let p = project.clone();
    let (logs, def, mut total, next) = state
        .db
        .write("system", move |c| {
            use rusqlite::OptionalExtension;
            let (total, cursor) = c
                .query_row(
                    "SELECT bytes,reconcile_cursor FROM job_log_budgets WHERE project=?",
                    [&p],
                    |r| Ok((r.get::<_, i64>(0)?, r.get::<_, i64>(1)?)),
                )
                .optional()?
                .unwrap_or((0, 0));
            // Reconcile first, independently paginated so inaccessible attempts
            // cannot starve later quarantines. Both seeks are indexed and bounded.
            let mut recovery = c
                .prepare(CANDIDATES)?
                .query_map(params![p, "quarantining", cursor, BATCH], row)?
                .collect::<Result<Vec<_>, _>>()?;
            let cursor = if recovery.len() == BATCH as usize {
                recovery.last().map(|r| r.id).unwrap_or(0)
            } else {
                0
            };
            c.execute(
                "UPDATE job_log_budgets SET reconcile_cursor=? WHERE project=?",
                params![cursor, p],
            )?;
            let logs = c
                .prepare(CANDIDATES)?
                .query_map(params![p, "retained", after, BATCH], row)?
                .collect::<Result<Vec<_>, _>>()?;
            let next = if logs.len() == BATCH as usize {
                logs.last().map(|r| r.id).unwrap_or(0)
            } else {
                0
            };
            recovery.extend(logs);
            Ok((recovery, jobs::delivery(c, &p)?, total, next))
        })
        .await?;
    let limit = def
        .log_retention_bytes
        .unwrap_or(1 << 30)
        .min(i64::MAX as u64) as i64;
    let mut removed = 0;
    let mut skipped = 0;
    for log in &logs {
        if log.cleanup_state != "quarantining"
            && !due(log, &chrono::Utc::now().to_rfc3339(), total, limit)
        {
            continue;
        }
        let jid = log.job.clone();
        let target = log.target.clone();
        let expected_project = project.clone();
        let expected_target = log.target_identity.clone();
        let found = state
            .db
            .read(move |c| {
                let job = jobs::get(c, &jid)?;
                if matches!(job.state, jobs::JobState::Queued | jobs::JobState::Running) {
                    return Ok(None);
                }
                let row = targets::get(c, &target)?;
                if target_identity(&row) != expected_target {
                    return Ok(None);
                }
                let target = row.def;
                if target
                    .project
                    .as_ref()
                    .is_some_and(|p| p != &expected_project)
                {
                    return Ok(None);
                }
                Ok(Some(target))
            })
            .await;
        let Ok(Some(target)) = found else {
            skipped += 1;
            continue;
        };
        let id = log.id;
        let quarantine = if log.cleanup_state == "quarantining" {
            log.quarantine.clone()
        } else {
            format!("{}/.retention-delete-{}", log.root, log.id)
        };
        let q = quarantine.clone();
        state.db.write("system",move |t| {
            t.execute("UPDATE job_logs SET cleanup_state='quarantining',quarantine=? WHERE id=? AND cleanup_state='retained'",params![q,id])?;
            Ok(())
        }).await?;
        let cmd = RemoteCmd {
            argv: vec![
                "sh".into(),
                "-c".into(),
                remove_script(
                    &log.path,
                    &log.root,
                    &log.root_identity,
                    &log.file_identity,
                    &quarantine,
                ),
            ],
            timeout_secs: Some(10),
            ..RemoteCmd::default()
        };
        let Ok(run) = crate::executor::start_cmd(state, &target, &cmd) else {
            skipped += 1;
            continue;
        };
        let result = run.collect(128).await;
        let outcome = String::from_utf8_lossy(&result.stdout);
        if !result.end.as_ref().is_some_and(|e| e.success()) {
            skipped += 1;
            continue;
        }
        if outcome.trim() == "conflict" {
            state.db.write("system",move |t| {
                t.execute("UPDATE job_logs SET cleanup_state='conflict' WHERE id=? AND cleanup_state='quarantining'",[id])?;
                Ok(())
            }).await?;
            continue;
        }
        if outcome.trim() != "expired" {
            skipped += 1;
            continue;
        }
        let id = log.id;
        let p = project.clone();
        let bytes = log.bytes;
        state
            .db
            .write("system", move |t| {
                if t.execute(
                    "UPDATE job_logs SET expired=1,cleanup_state='expired' WHERE id=? AND cleanup_state='quarantining'",
                    [id],
                )? != 0
                {
                    t.execute(
                        "UPDATE job_log_budgets SET bytes=MAX(0,bytes-?) WHERE project=?",
                        params![bytes, p],
                    )?;
                }
                Ok(())
            })
            .await?;
        total = total.saturating_sub(bytes);
        removed += 1;
    }
    let p = project.clone();
    let conflicts = state
        .db
        .read(move |c| {
            Ok(c.prepare(CANDIDATES)?
                .query_map(params![p, "conflict", 0, BATCH], row)?
                .collect::<Result<Vec<_>, _>>()?)
        })
        .await?;
    Ok(
        serde_json::json!({"removed":removed,"skipped":skipped,"scanned":logs.len(),"next":next,"conflicts":conflicts}),
    )
}

/// One indexed project seek and one bounded page per minute.
pub async fn maintain(state: &Shared, project: &mut String, after: &mut i64) -> ApiResult<()> {
    if *after == 0 {
        let p = project.clone();
        let found = state
            .db
            .read(move |c| {
                use rusqlite::OptionalExtension;
                Ok(c.query_row(
                    "SELECT project FROM job_log_budgets WHERE project>? ORDER BY project LIMIT 1",
                    [p],
                    |r| r.get::<_, String>(0),
                )
                .optional()?)
            })
            .await?;
        let Some(found) = found else {
            project.clear();
            return Ok(());
        };
        *project = found;
    }
    let result = cleanup(state, project.clone(), *after).await?;
    *after = result["next"].as_i64().unwrap_or(0);
    Ok(())
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]
    use super::*;

    #[test]
    fn age_and_size_boundaries() {
        let log = Log {
            id: 1,
            job: "J".into(),
            attempt: 1,
            target: "t".into(),
            path: "p".into(),
            root: "".into(),
            root_identity: "".into(),
            file_identity: "".into(),
            target_identity: "".into(),
            written_at: "a".into(),
            expires_at: "b".into(),
            bytes: 10,
            expired: false,
            cleanup_state: "retained".into(),
            quarantine: String::new(),
        };
        assert!(!due(&log, "a", 10, 10));
        assert!(due(&log, "b", 10, 10));
        assert!(due(&log, "c", 0, 10));
        assert!(due(&log, "a", 11, 10));
        assert!(due(&log, "a", 1, 0));
    }

    #[test]
    fn ledger_pages_and_job_attempts_use_indexed_seeks() {
        let dir = tempfile::tempdir().unwrap();
        let c = crate::db::open(&dir.path().join("db")).unwrap();
        for n in 0..2048 {
            c.execute("INSERT INTO job_logs(project,job,attempt,target,path,written_at,expires_at,expired) VALUES (?,?,?,?,?,'a','b',?)", params![if n%2==0 {"p"} else {"other"},format!("J{}",n/2),n%2+1,"t",format!("/logs/{n}"),n%3==0]).unwrap();
        }
        let page: Vec<Log> = c
            .prepare(CANDIDATES)
            .unwrap()
            .query_map(params!["p", "retained", 0, BATCH], row)
            .unwrap()
            .collect::<Result<_, _>>()
            .unwrap();
        assert_eq!(page.len(), 32);
        let next: Vec<Log> = c
            .prepare(CANDIDATES)
            .unwrap()
            .query_map(params!["p", "retained", page[31].id, BATCH], row)
            .unwrap()
            .collect::<Result<_, _>>()
            .unwrap();
        assert!(next[0].id > page[31].id);
        let attempts = for_job(&c, "J1").unwrap();
        assert_eq!(attempts.len(), 2);
        assert_eq!(attempts[0].attempt, 1);
        assert_eq!(attempts[1].attempt, 2);
        let plans: Vec<String> = c
            .prepare(&format!("EXPLAIN QUERY PLAN {CANDIDATES}"))
            .unwrap()
            .query_map(params!["p", "retained", 0, BATCH], |r| r.get(3))
            .unwrap()
            .collect::<Result<_, _>>()
            .unwrap();
        assert!(
            plans
                .iter()
                .any(|p| p.contains("SEARCH") && p.contains("job_logs_cleanup")),
            "{plans:?}"
        );
        assert!(
            !plans
                .iter()
                .any(|p| p.contains("SCAN") || p.contains("TEMP B-TREE")),
            "{plans:?}"
        );
    }

    #[test]
    fn cleanup_refuses_symlinks_and_accepts_missing_logs() {
        use std::os::unix::fs::symlink;
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("log");
        let other = dir.path().join("other");
        std::fs::write(&other, "keep").unwrap();
        symlink(&other, &path).unwrap();
        let run = || {
            std::process::Command::new("sh")
                .args(["-c", &test_remove_script(&path)])
                .output()
                .unwrap()
        };
        assert!(!run().status.success());
        assert_eq!(std::fs::read_to_string(&other).unwrap(), "keep");
        std::fs::remove_file(&path).unwrap();
        assert!(run().status.success());
        assert!(!path.exists());
    }

    #[test]
    fn cleanup_refuses_symlinked_parent() {
        let dir = tempfile::tempdir().unwrap();
        let outside = tempfile::tempdir().unwrap();
        std::fs::write(outside.path().join("attempt.log"), "outside").unwrap();
        let root = dir.path().join("logs");
        std::os::unix::fs::symlink(outside.path(), &root).unwrap();
        let script = test_remove_script(&root.join("attempt.log"));
        let result = std::process::Command::new("sh")
            .args(["-c", &script])
            .output()
            .unwrap();
        assert!(!result.status.success());
        assert_eq!(
            std::fs::read_to_string(outside.path().join("attempt.log")).unwrap(),
            "outside"
        );
    }

    #[test]
    fn replacement_between_inode_check_and_unlink_survives() {
        let dir = tempfile::tempdir().unwrap();
        let log = dir.path().join("attempt.log");
        std::fs::write(&log, "owned").unwrap();
        let script = test_remove_script(&log).replace("mv -- \"$name\" \"$trash/log\"", "mv -- \"$name\" old-log; printf replacement > \"$name\"; mv -- \"$name\" \"$trash/log\"");
        let result = std::process::Command::new("sh")
            .args(["-c", &script])
            .output()
            .unwrap();
        assert!(result.status.success());
        assert_eq!(std::fs::read_to_string(&log).unwrap(), "replacement");
        assert_eq!(
            std::fs::read_to_string(dir.path().join("old-log")).unwrap(),
            "owned"
        );
    }

    #[test]
    fn interruption_after_move_is_reconciled_on_retry() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("attempt.log");
        std::fs::write(&path, "owned bytes").unwrap();
        let script = test_remove_script(&path);
        let interrupted = script.replace("# AFTER_MOVE:", "exit 99\n# AFTER_MOVE:");
        let run = |script: &str| {
            std::process::Command::new("sh")
                .args(["-c", script])
                .output()
                .unwrap()
        };
        assert_eq!(run(&interrupted).status.code(), Some(99));
        let captured = dir.path().join(".retention-delete-1/log");
        assert_eq!(std::fs::read_to_string(&captured).unwrap(), "owned bytes");
        assert!(!path.exists());
        let retry = run(&script);
        assert!(retry.status.success(), "{retry:?}");
        assert_eq!(retry.stdout, b"expired\n");
        assert!(!captured.exists());
    }

    #[test]
    fn occupied_restore_preserves_both_files_as_conflict() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("attempt.log");
        std::fs::write(&path, "owned").unwrap();
        let script=test_remove_script(&path).replace("mv -- \"$name\" \"$trash/log\"", "mv -- \"$name\" old-log; printf captured > \"$name\"; mv -- \"$name\" \"$trash/log\"; printf occupied > \"$name\"");
        let result = std::process::Command::new("sh")
            .args(["-c", &script])
            .output()
            .unwrap();
        assert!(result.status.success(), "{result:?}");
        assert_eq!(result.stdout, b"conflict\n");
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "occupied");
        assert_eq!(
            std::fs::read_to_string(dir.path().join(".retention-delete-1/log")).unwrap(),
            "captured"
        );
    }
}
