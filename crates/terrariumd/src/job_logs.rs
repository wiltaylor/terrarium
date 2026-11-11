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
    pub written_at: String,
    pub expires_at: String,
    pub bytes: i64,
    pub expired: bool,
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
    })
}

const FIELDS: &str = "id, job, attempt, target, path, written_at, expires_at, bytes, expired";
const CANDIDATES: &str = "SELECT id, job, attempt, target, path, written_at, expires_at, bytes, expired FROM job_logs INDEXED BY job_logs_cleanup WHERE project=? AND expired=0 AND id>? ORDER BY id LIMIT ?";

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
    let written = chrono::Utc::now();
    let expires = (written + chrono::Duration::seconds(secs as i64)).to_rfc3339();
    let written = written.to_rfc3339();
    state.db.write("system", move |t| {
        t.execute("INSERT OR IGNORE INTO job_log_budgets(project) VALUES (?)", [&project])?;
        // A lost attempt can run again. Its original ownership and expiry stay fixed.
        t.execute("INSERT OR IGNORE INTO job_logs(project,job,attempt,target,path,written_at,expires_at) VALUES (?,?,?,?,?,?,?)",
            params![project, job, attempt, target, path, written, expires])?;
        let owned: bool = t.query_row("SELECT target=? AND path=? AND expired=0 FROM job_logs WHERE job=? AND attempt=?", params![target,path,job,attempt], |r| r.get(0))?;
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
pub(crate) fn remove_script(path: &str) -> String {
    format!(
        r#"log={}
if [ ! -e "$log" ] && [ ! -L "$log" ]; then printf 'expired\n'; exit 0; fi
[ -f "$log" ] && [ ! -L "$log" ] || exit 1
exec 9<"$log" || exit 1
flock -n 9 || exit 1
[ "$log" -ef /proc/self/fd/9 ] && [ ! -L "$log" ] || exit 1
rm -- "$log" && printf 'expired\n'
"#,
        sh_quote(path)
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
    let (logs, def, mut total) = state
        .db
        .read(move |c| {
            let logs = c
                .prepare(CANDIDATES)?
                .query_map(params![p, after, BATCH], row)?
                .collect::<Result<Vec<_>, _>>()?;
            let total = c
                .query_row(
                    "SELECT bytes FROM job_log_budgets WHERE project=?",
                    [&p],
                    |r| r.get::<_, i64>(0),
                )
                .unwrap_or(0);
            Ok((logs, jobs::delivery(c, &p)?, total))
        })
        .await?;
    let limit = def
        .log_retention_bytes
        .unwrap_or(1 << 30)
        .min(i64::MAX as u64) as i64;
    let mut removed = 0;
    let mut skipped = 0;
    let mut next = 0;
    for log in &logs {
        next = log.id;
        if !due(log, &chrono::Utc::now().to_rfc3339(), total, limit) {
            continue;
        }
        let jid = log.job.clone();
        let target = log.target.clone();
        let expected_project = project.clone();
        let found = state
            .db
            .read(move |c| {
                let job = jobs::get(c, &jid)?;
                if matches!(job.state, jobs::JobState::Queued | jobs::JobState::Running) {
                    return Ok(None);
                }
                let target = targets::get(c, &target)?.def;
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
        let cmd = RemoteCmd {
            argv: vec!["sh".into(), "-c".into(), remove_script(&log.path)],
            timeout_secs: Some(10),
            ..RemoteCmd::default()
        };
        let Ok(run) = crate::executor::start_cmd(state, &target, &cmd) else {
            skipped += 1;
            continue;
        };
        let result = run.collect(128).await;
        if !result.end.as_ref().is_some_and(|e| e.success())
            || String::from_utf8_lossy(&result.stdout).trim() != "expired"
        {
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
                    "UPDATE job_logs SET expired=1 WHERE id=? AND expired=0",
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
    Ok(
        serde_json::json!({"removed":removed,"skipped":skipped,"scanned":logs.len(),"next":if logs.len()==BATCH as usize {next} else {0}}),
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
            written_at: "a".into(),
            expires_at: "b".into(),
            bytes: 10,
            expired: false,
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
            .query_map(params!["p", 0, BATCH], row)
            .unwrap()
            .collect::<Result<_, _>>()
            .unwrap();
        assert_eq!(page.len(), 32);
        let next: Vec<Log> = c
            .prepare(CANDIDATES)
            .unwrap()
            .query_map(params!["p", page[31].id, BATCH], row)
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
            .query_map(params!["p", 0, BATCH], |r| r.get(3))
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
                .args(["-c", &remove_script(path.to_str().unwrap())])
                .output()
                .unwrap()
        };
        assert!(!run().status.success());
        assert_eq!(std::fs::read_to_string(&other).unwrap(), "keep");
        std::fs::remove_file(&path).unwrap();
        assert!(run().status.success());
        assert!(!path.exists());
    }
}
