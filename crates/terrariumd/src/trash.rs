//! The trash of deleted agents. Each delete of an agent stops the agent and
//! moves the agent dir `<state>/agents/<ulid>/` to
//! `<state>/trash/<ulid>-<ts>/`, so that the operator can still read the
//! transcripts and the event log. The three deletes are
//! `DELETE /api/agents/{id}`, a reload after the removal of a definition
//! file, and a fleet prune. `DELETE /api/agents/{id}` also moves the
//! workspace `<work>/<project>/<agent>` to `<trash entry>/workspace`
//! (unless `?keep_workspace=true`). The prune timer removes a trash entry
//! older than `TERRARIUM_TRASH_DAYS` (default 7).

use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime};

use terrarium_core::AgentId;

use crate::api::Shared;
use crate::supervisor::SupervisorError;

/// The trash dir, in the state dir.
pub const TRASH_DIR: &str = "trash";

/// The env var with the days that a trash entry stays.
pub const TRASH_DAYS_ENV: &str = "TERRARIUM_TRASH_DAYS";

/// The days that a trash entry stays when [`TRASH_DAYS_ENV`] is not set.
pub const DEFAULT_TRASH_DAYS: u64 = 7;

/// The days from [`TRASH_DAYS_ENV`], or [`DEFAULT_TRASH_DAYS`].
pub fn trash_days() -> u64 {
    std::env::var(TRASH_DAYS_ENV)
        .ok()
        .and_then(|v| v.trim().parse().ok())
        .unwrap_or(DEFAULT_TRASH_DAYS)
}

/// How long [`stop_agent`] waits for a live agent to stop.
pub const STOP_WAIT: Duration = Duration::from_secs(10);

/// Stop `agent` if it runs, then wait (up to [`STOP_WAIT`]) for the harness
/// to end, so that the runner does not write into the agent dir while it
/// moves. An agent that does not run is not an error.
pub async fn stop_agent(state: &Shared, agent: AgentId) -> Result<(), SupervisorError> {
    match state.supervisor.stop(agent).await {
        Ok(()) | Err(SupervisorError::NotRunning(_)) => {}
        Err(e) => return Err(e),
    }
    let deadline = std::time::Instant::now() + STOP_WAIT;
    while std::time::Instant::now() < deadline
        && state
            .supervisor
            .status(agent)
            .await
            .is_some_and(|s| s.is_live())
    {
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    Ok(())
}

/// Move the agent dir of `agent` to the trash ([`trash_agent_dir`]) in a
/// blocking task, and log the result. With `workspace`, the workspace goes
/// into the same trash entry. A failure is a warning only: the agent is
/// gone from the store already.
pub async fn trash_agent(state: &Shared, agent: AgentId, workspace: Option<PathBuf>) {
    // --- session archive (PLAN §18): copy the session files out first,
    // so that the card links to the sessions still open.
    crate::session_archive::archive_agent(state, agent).await;
    // --- end of the session archive.
    let state_dir = state.state_dir.clone();
    let id = agent.to_string();
    match tokio::task::spawn_blocking(move || {
        trash_agent_dir(&state_dir, &id, workspace.as_deref())
    })
    .await
    {
        Ok(Ok(Some(to))) => {
            tracing::info!(agent = %agent, trash = %to.display(), "moved the agent dir to the trash");
        }
        Ok(Ok(None)) => {}
        Ok(Err(e)) => tracing::warn!(agent = %agent, "cannot move the agent dir to the trash: {e}"),
        Err(e) => tracing::warn!(agent = %agent, "the trash task failed: {e}"),
    }
}

/// The end of a delete of `agent` from the store: [`stop_agent`], then
/// [`trash_agent`]. A reload (a definition file is gone) and a fleet prune
/// call this after their transaction. `DELETE /api/agents/{id}` calls the
/// two steps itself, with its delete between them. A stop failure is a
/// warning: the dir goes to the trash all the same.
pub async fn retire_agent(state: &Shared, agent: AgentId) {
    if let Err(e) = stop_agent(state, agent).await {
        tracing::warn!(agent = %agent, "cannot stop a deleted agent: {e}");
    }
    trash_agent(state, agent, None).await;
}

/// The name of the workspace in a trash entry.
pub const TRASH_WORKSPACE: &str = "workspace";

/// Move the agent dir of `agent` to `<state>/trash/<agent>-<ts>/`, and
/// `workspace` (when given and there) to `<that entry>/workspace`. `None`:
/// neither is there. `rename` moves a symlink itself, never its target.
/// The work root is another file system in the container, so the
/// workspace move can be a copy and a delete ([`crate::workspace::migrate`]).
pub fn trash_agent_dir(
    state_dir: &Path,
    agent: &str,
    workspace: Option<&Path>,
) -> std::io::Result<Option<PathBuf>> {
    let dir = state_dir.join("agents").join(agent);
    let has_dir = std::fs::symlink_metadata(&dir).is_ok();
    let workspace = workspace.filter(|w| std::fs::symlink_metadata(w).is_ok());
    if !has_dir && workspace.is_none() {
        return Ok(None);
    }
    let trash = state_dir.join(TRASH_DIR);
    std::fs::create_dir_all(&trash)?;
    let ts = SystemTime::now()
        .duration_since(SystemTime::UNIX_EPOCH)
        .map_or(0, |d| d.as_secs());
    let mut to = trash.join(format!("{agent}-{ts}"));
    let mut n = 1;
    while std::fs::symlink_metadata(&to).is_ok() {
        to = trash.join(format!("{agent}-{ts}-{n}"));
        n += 1;
    }
    if has_dir {
        std::fs::rename(&dir, &to)?;
    } else {
        std::fs::create_dir_all(&to)?;
    }
    if let Some(ws) = workspace {
        // An old agent dir can hold a `workspace` dir of its own.
        let mut into = to.join(TRASH_WORKSPACE);
        if std::fs::symlink_metadata(&into).is_ok() {
            into = to.join(format!("{TRASH_WORKSPACE}.work"));
        }
        if let crate::workspace::Migration::Skipped(why) = crate::workspace::migrate(ws, &into) {
            return Err(std::io::Error::other(format!(
                "the agent dir is in {}, but the workspace stays: {why}",
                to.display()
            )));
        }
        // An empty workspace: `migrate` removes it and moves nothing.
        let _ = std::fs::remove_dir(ws);
    }
    Ok(Some(to))
}

/// Remove the trash entries that went to the trash more than `max_age`
/// before `now` (the time in the name; the mtime when the name has none).
/// Returns the removed paths. A symlink is removed, not followed.
pub fn prune_trash(
    state_dir: &Path,
    now: SystemTime,
    max_age: Duration,
) -> std::io::Result<Vec<PathBuf>> {
    let trash = state_dir.join(TRASH_DIR);
    let entries = match std::fs::read_dir(&trash) {
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
        let at = trashed_at(&e.file_name().to_string_lossy())
            .or_else(|| meta.modified().ok())
            .unwrap_or(now);
        if at > cutoff {
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
                tracing::warn!(path = %path.display(), "cannot prune the trash entry: {err}")
            }
        }
    }
    Ok(removed)
}

/// The time in a trash name `<ulid>-<ts>` or `<ulid>-<ts>-<n>`.
fn trashed_at(name: &str) -> Option<SystemTime> {
    let rest = name.split_once('-')?.1;
    let ts: u64 = rest.split('-').next()?.parse().ok()?;
    Some(SystemTime::UNIX_EPOCH + Duration::from_secs(ts))
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;

    #[test]
    fn trash_and_prune() {
        let tmp = tempfile::tempdir().unwrap();
        let state = tmp.path();
        assert!(trash_agent_dir(state, "01A", None).unwrap().is_none());
        let dir = state.join("agents/01A/private");
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("events.jsonl"), b"{}\n").unwrap();
        let to = trash_agent_dir(state, "01A", None).unwrap().unwrap();
        assert!(!state.join("agents/01A").exists());
        assert!(to.starts_with(state.join(TRASH_DIR)));
        assert_eq!(
            std::fs::read(to.join("private/events.jsonl")).unwrap(),
            b"{}\n"
        );
        // Twice in the same second: a second name.
        std::fs::create_dir_all(state.join("agents/01A")).unwrap();
        let again = trash_agent_dir(state, "01A", None).unwrap().unwrap();
        assert_ne!(again, to);

        let day = Duration::from_secs(86_400);
        let now = SystemTime::now();
        // Young entries stay.
        assert!(prune_trash(state, now, 7 * day).unwrap().is_empty());
        // An old one goes, by the time in its name.
        let old = state.join(TRASH_DIR).join("01B-1000");
        std::fs::create_dir_all(old.join("x")).unwrap();
        assert_eq!(prune_trash(state, now, 7 * day).unwrap(), vec![old.clone()]);
        assert!(!old.exists());
        assert!(to.exists());
        // Eight days on, all go.
        assert_eq!(prune_trash(state, now + 8 * day, 7 * day).unwrap().len(), 2);
    }

    /// The workspace goes into the trash entry of the agent dir, and the
    /// prune of the entry removes both.
    #[test]
    fn the_workspace_goes_into_the_same_entry() {
        let tmp = tempfile::tempdir().unwrap();
        let state = tmp.path().join("state");
        let ws = tmp.path().join("work/p/w1");
        std::fs::create_dir_all(state.join("agents/01A/private")).unwrap();
        std::fs::create_dir_all(ws.join("app")).unwrap();
        std::fs::write(ws.join("app/main.py"), b"print(1)\n").unwrap();
        let to = trash_agent_dir(&state, "01A", Some(&ws)).unwrap().unwrap();
        assert!(!ws.exists());
        assert!(to.join("private").is_dir());
        assert_eq!(
            std::fs::read(to.join("workspace/app/main.py")).unwrap(),
            b"print(1)\n"
        );
        // No agent dir, a workspace only: an entry all the same.
        std::fs::create_dir_all(ws.join("x")).unwrap();
        let only = trash_agent_dir(&state, "01B", Some(&ws)).unwrap().unwrap();
        assert!(only.join("workspace/x").is_dir());
        // No workspace there: the agent dir alone, no error.
        std::fs::create_dir_all(state.join("agents/01C")).unwrap();
        let c = trash_agent_dir(&state, "01C", Some(&ws)).unwrap().unwrap();
        assert!(!c.join("workspace").exists());
        let day = Duration::from_secs(86_400);
        let later = SystemTime::now() + 8 * day;
        assert_eq!(prune_trash(&state, later, 7 * day).unwrap().len(), 3);
    }

    #[test]
    fn the_name_carries_the_time() {
        let at =
            |s| trashed_at(s).map(|t| t.duration_since(SystemTime::UNIX_EPOCH).unwrap().as_secs());
        assert_eq!(at("01ABC-1700000000"), Some(1_700_000_000));
        assert_eq!(at("01ABC-1700000000-2"), Some(1_700_000_000));
        assert_eq!(at("junk"), None);
    }
}
