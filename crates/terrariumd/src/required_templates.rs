//! Templates used by the daemon itself, not by project configuration.
use std::path::Path;

use rusqlite::Connection;

use crate::db::{Tx, now};
use crate::error::ApiResult;
use crate::store::{inbox, templates};

/// Global template names required by this binary. Update pre-checks run
/// this binary from the destination image, so they use its requirements.
pub const REQUIRED: &[&str] = &["request"];

pub fn missing(conn: &Connection) -> ApiResult<Vec<String>> {
    let mut out = Vec::new();
    for name in REQUIRED {
        if templates::find_in(conn, None, name)?.is_none() {
            out.push(format!("global:{name}"));
        }
    }
    Ok(out)
}

pub fn warning(key: &str) -> String {
    format!("required daemon template {key} is missing; install it in the config repo before using operator request dispatch")
}

/// Persist one critical notice per missing template, including across restarts
/// and acknowledgement. The context is the persistent deduplication key.
pub fn check_start(t: &mut Tx<'_>) -> ApiResult<()> {
    for key in missing(t)? {
        let context = format!("required-template:{key}");
        let exists: bool = t.query_row(
            "SELECT EXISTS(SELECT 1 FROM questions WHERE agent = 'system' AND context = ?1)",
            [&context], |r| r.get(0),
        )?;
        if exists { continue; }
        let id = ulid::Ulid::new().to_string();
        let at = now();
        let inserted = t.execute(
            "INSERT OR IGNORE INTO questions
             (id, agent, project, text, kind, priority, thread_id, created_at, updated_at, context)
             VALUES (?1, 'system', '', ?2, 'notice', 'critical', ?1, ?3, ?3, ?4)",
            rusqlite::params![id, warning(&key), at, context],
        )?;
        if inserted != 0 {
            inbox::emit(t, &format!("notice:{id}"), "notice", "opened", None)?;
        }
    }
    Ok(())
}

/// Read only: no daemon, database migration, or template synchronization.
pub fn precheck(repo: &Path) -> Vec<String> {
    REQUIRED
        .iter()
        .filter_map(|name| {
            let path = format!("templates/{name}.wcl");
            match std::fs::read_to_string(repo.join(&path)) {
                Ok(text) if terrarium_core::template_file::parse(&text, &path).is_ok() => None,
                _ => Some(warning(&format!("global:{name}"))),
            }
        })
        .collect()
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn startup_warns_once_even_after_acknowledgement() {
        let mut conn = Connection::open_in_memory().unwrap();
        crate::db::migrate(&mut conn).unwrap();
        let db = crate::db::Db::new(conn, crate::events::EventHub::default());
        db.write("system", |t| {
            check_start(t)?;
            t.execute("UPDATE questions SET status = 'acknowledged'", [])?;
            check_start(t)?;
            let count: i64 = t.query_row(
                "SELECT count(*) FROM questions WHERE kind = 'notice' AND priority = 'critical'",
                [], |r| r.get(0),
            )?;
            assert_eq!(count, 1);
            Ok(())
        }).await.unwrap();
    }

    #[test]
    fn precheck_names_missing_and_invalid_templates() {
        let dir = tempfile::tempdir().unwrap();
        assert!(precheck(dir.path())[0].contains("global:request"));
        std::fs::create_dir(dir.path().join("templates")).unwrap();
        std::fs::write(dir.path().join("templates/request.wcl"), "invalid").unwrap();
        assert_eq!(precheck(dir.path()).len(), 1);
        std::fs::write(dir.path().join("templates/request.wcl"), "harness = \"codex\"\n").unwrap();
        assert!(precheck(dir.path()).is_empty());
    }

    #[tokio::test]
    async fn startup_is_quiet_when_present() {
        let mut conn = Connection::open_in_memory().unwrap();
        crate::db::migrate(&mut conn).unwrap();
        let db = crate::db::Db::new(conn, crate::events::EventHub::default());
        db.write("system", |t| {
            templates::create(t, &templates::NewTemplate {
                name: "request".into(), project: None, description: String::new(),
                spec: serde_json::from_value(serde_json::json!({"harness":"codex"})).unwrap(),
                created_by: "system".into(), source: "config".into(),
            }, None)?;
            check_start(t)?;
            assert!(missing(t)?.is_empty());
            let count: i64 = t.query_row("SELECT count(*) FROM questions", [], |r| r.get(0))?;
            assert_eq!(count, 0);
            Ok(())
        }).await.unwrap();
    }
}
