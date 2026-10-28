//! The session archive (PLAN §18): the session files of a deleted agent,
//! kept so that the card links to its sessions still open (PLAN §13).
//!
//! A delete moves the agent dir to the trash (`crate::trash`). Before the
//! move, [`archive_agent`] copies the session file of each session of the
//! agent to
//!
//! ```text
//! <state>/archive/agents/<agent ulid>/<harness>/<session ulid>/<file name>
//! ```
//!
//! and records the path, relative to the state dir, in `sessions.archive`
//! (migration 37). Only the session files go: no workspace, no other
//! harness state. The file keeps its name, so the blocks keep their
//! `source.file`. The transcript parsers read the copy as they read the
//! original.
//!
//! Retention: the archive of an agent stays [`archive_days`] days, the
//! same as the trash by default (`TERRARIUM_TRASH_DAYS`). Set
//! `TERRARIUM_SESSION_ARCHIVE_DAYS` to keep it longer; `0` keeps it
//! forever. The prune timer removes an older agent archive
//! ([`prune_archive`]). The session rows stay; a pruned session reads as
//! "no transcript".

use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime};

use terrarium_core::{AgentId, HarnessId};

use crate::api::Shared;
use crate::store::session_refs;

/// The archive dir, in the state dir.
pub const ARCHIVE_DIR: &str = "archive";

/// The env var with the days that the archive of an agent stays.
pub const ARCHIVE_DAYS_ENV: &str = "TERRARIUM_SESSION_ARCHIVE_DAYS";

/// The days from [`ARCHIVE_DAYS_ENV`], else the trash days
/// ([`crate::trash::trash_days`]). `0`: keep forever.
pub fn archive_days() -> u64 {
    std::env::var(ARCHIVE_DAYS_ENV)
        .ok()
        .and_then(|v| v.trim().parse().ok())
        .unwrap_or_else(crate::trash::trash_days)
}

/// The archive dir of one agent.
pub fn agent_dir(state_dir: &Path, agent: &str) -> PathBuf {
    state_dir.join(ARCHIVE_DIR).join("agents").join(agent)
}

/// The relative path of the copy of `file` for one session.
fn rel_path(agent: &str, harness: HarnessId, session: &str, file: &str) -> String {
    format!(
        "{ARCHIVE_DIR}/agents/{agent}/{}/{session}/{file}",
        harness.as_str()
    )
}

/// Copy the session files of the deleted `agent` into the archive. Call
/// it after the store delete and before the agent dir goes to the trash.
/// A session with no file (no harness session, or no file found) is
/// skipped. A failure is a warning only. Returns the number of files
/// copied.
pub async fn archive_agent(state: &Shared, agent: AgentId) -> usize {
    let id = agent.to_string();
    let rows = match state
        .db
        .read({
            let id = id.clone();
            move |c| session_refs::unarchived(c, &id)
        })
        .await
    {
        Ok(r) => r,
        Err(e) => {
            tracing::warn!(agent = %agent, "cannot list the sessions to archive: {e}");
            return 0;
        }
    };
    let mut copied = 0;
    for row in rows {
        let Ok(harness) = row.session.harness.parse::<HarnessId>() else {
            continue;
        };
        let loc = match crate::transcripts::locate_row(
            state,
            agent,
            harness,
            &row.session.id,
            row.harness_session_id.as_deref(),
        )
        .await
        {
            Ok(l) if l.path.is_file() => l,
            Ok(_) => continue,
            Err(e) => {
                tracing::debug!(session = %row.session.id, "no session file to archive: {}", e.message());
                continue;
            }
        };
        let Some(file) = loc
            .path
            .file_name()
            .map(|n| n.to_string_lossy().into_owned())
        else {
            continue;
        };
        let rel = rel_path(&id, loc.harness, &row.session.id, &file);
        let (from, to) = (loc.path.clone(), state.state_dir.join(&rel));
        let result = tokio::task::spawn_blocking(move || {
            if let Some(parent) = to.parent() {
                std::fs::create_dir_all(parent)?;
            }
            std::fs::copy(&from, &to).map(|_| ())
        })
        .await;
        match result {
            Ok(Ok(())) => {}
            Ok(Err(e)) => {
                tracing::warn!(session = %row.session.id, "cannot archive the session file: {e}");
                continue;
            }
            Err(e) => {
                tracing::warn!(session = %row.session.id, "the archive task failed: {e}");
                continue;
            }
        }
        let (sid, r) = (row.session.id.clone(), rel.clone());
        if let Err(e) = state
            .db
            .write("system", move |t| session_refs::set_archive(t, &sid, &r))
            .await
        {
            tracing::warn!(session = %row.session.id, "cannot record the archive: {e}");
            continue;
        }
        copied += 1;
    }
    if copied > 0 {
        tracing::info!(agent = %agent, files = copied, "archived the session files");
    }
    copied
}

/// The archived file at `rel`, when it is a regular file under the
/// archive dir. `None` when it is gone (pruned) or not safe to read.
pub fn archived_file(state_dir: &Path, rel: &str) -> Option<PathBuf> {
    let rel = Path::new(rel);
    if rel.is_absolute()
        || rel
            .components()
            .any(|c| !matches!(c, std::path::Component::Normal(_)))
        || !rel.starts_with(ARCHIVE_DIR)
    {
        return None;
    }
    let path = state_dir.join(rel);
    let meta = std::fs::symlink_metadata(&path).ok()?;
    meta.file_type().is_file().then_some(path)
}

/// Remove the agent archives older than `max_age` before `now` (by the
/// mtime of the agent dir). `max_age` zero keeps all. Returns the removed
/// dirs.
pub fn prune_archive(
    state_dir: &Path,
    now: SystemTime,
    max_age: Duration,
) -> std::io::Result<Vec<PathBuf>> {
    if max_age.is_zero() {
        return Ok(Vec::new());
    }
    let root = state_dir.join(ARCHIVE_DIR).join("agents");
    let entries = match std::fs::read_dir(&root) {
        Ok(e) => e,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(e) => return Err(e),
    };
    let Some(cutoff) = now.checked_sub(max_age) else {
        return Ok(Vec::new());
    };
    let mut removed = Vec::new();
    for e in entries.flatten() {
        let path = e.path();
        let Ok(meta) = std::fs::symlink_metadata(&path) else {
            continue;
        };
        if meta.modified().unwrap_or(now) > cutoff {
            continue;
        }
        let result = if meta.file_type().is_dir() {
            std::fs::remove_dir_all(&path)
        } else {
            std::fs::remove_file(&path)
        };
        match result {
            Ok(()) => removed.push(path),
            Err(err) => {
                tracing::warn!(path = %path.display(), "cannot prune the session archive: {err}")
            }
        }
    }
    Ok(removed)
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;

    #[test]
    fn the_path_rule() {
        let tmp = tempfile::tempdir().unwrap();
        let state = tmp.path();
        let rel = rel_path("01A", HarnessId::Claude, "01S", "h.jsonl");
        assert_eq!(rel, "archive/agents/01A/claude/01S/h.jsonl");
        assert!(archived_file(state, &rel).is_none(), "not there");
        let path = state.join(&rel);
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(&path, b"{}\n").unwrap();
        assert_eq!(archived_file(state, &rel), Some(path));
        for bad in [
            "/etc/passwd",
            "archive/../secret",
            "agents/01A/harness/x.jsonl",
            "archive/agents/01A/claude/01S",
        ] {
            assert!(archived_file(state, bad).is_none(), "{bad}");
        }
    }

    #[test]
    fn prune_keeps_young_archives_and_zero_keeps_all() {
        let tmp = tempfile::tempdir().unwrap();
        let state = tmp.path();
        std::fs::create_dir_all(agent_dir(state, "01A").join("claude")).unwrap();
        let day = Duration::from_secs(86_400);
        let now = SystemTime::now();
        assert!(prune_archive(state, now, 7 * day).unwrap().is_empty());
        assert!(
            prune_archive(state, now + 8 * day, Duration::ZERO)
                .unwrap()
                .is_empty()
        );
        assert_eq!(
            prune_archive(state, now + 8 * day, 7 * day).unwrap(),
            vec![agent_dir(state, "01A")]
        );
    }
}
