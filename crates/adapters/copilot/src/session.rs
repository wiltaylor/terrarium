//! Find the session file and the session ID without a hook.
//!
//! Path: `$COPILOT_HOME/session-state/<id>/events.jsonl`, next to
//! `workspace.yaml`. Copilot makes the directory at start-up, before the
//! first prompt (the `SessionStart` hook fires only at the first prompt).
//!
//! All agents share one home and one cwd, so "the newest session" can be
//! another agent's. The adapter knows the UUID instead: it chooses it for
//! `--session-id` and records it in `<agent dir>/copilot/session-id`.

use std::fs;
use std::path::{Path, PathBuf};

use terrarium_core::{Agent, HarnessSessionId};

use crate::config::{self, SESSION_STATE_DIR};
use crate::error::{CopilotError, Result};

/// The directory of one session.
pub fn session_dir(home: &Path, id: &HarnessSessionId) -> PathBuf {
    home.join(SESSION_STATE_DIR).join(&id.0)
}

/// The session file of one session.
pub fn events_file(home: &Path, id: &HarnessSessionId) -> PathBuf {
    session_dir(home, id).join("events.jsonl")
}

fn looks_like_uuid(s: &str) -> bool {
    s.len() == 36
        && s.char_indices().all(|(i, c)| match i {
            8 | 13 | 18 | 23 => c == '-',
            _ => c.is_ascii_hexdigit(),
        })
}

/// Record the UUID of a fresh start.
pub fn record_session_id(agent: &Agent, id: &str) -> Result<()> {
    let path = config::session_id_file(agent);
    config::write_atomic(&path, format!("{id}\n").as_bytes())
}

/// The UUID of the last fresh start, if one was recorded.
pub fn recorded_session_id(agent: &Agent) -> Result<Option<HarnessSessionId>> {
    let path = config::session_id_file(agent);
    match fs::read_to_string(&path) {
        Ok(s) => Ok(Some(s.trim())
            .filter(|s| looks_like_uuid(s))
            .map(|s| HarnessSessionId(s.to_owned()))),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(e) => Err(CopilotError::io(path, e)),
    }
}

/// The session of the agent: the recorded UUID, when Copilot made its
/// session directory in `home` (the daemon's view of the home).
pub fn find_session_id(home: &Path, agent: &Agent) -> Option<HarnessSessionId> {
    let id = recorded_session_id(agent).ok().flatten()?;
    session_dir(home, &id).is_dir().then_some(id)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_agent_in;

    #[test]
    fn recorded_id_needs_the_session_dir() {
        let tmp = tempfile::tempdir().expect("tmp");
        let home = tmp.path().join("auth-copilot");
        let agent = test_agent_in(tmp.path(), None);
        assert_eq!(find_session_id(&home, &agent), None);
        let id = "62e05754-b66a-4ccc-ba6e-4846cc4fc1aa";
        record_session_id(&agent, id).expect("record");
        assert_eq!(find_session_id(&home, &agent), None);
        let sid = HarnessSessionId(id.into());
        fs::create_dir_all(session_dir(&home, &sid)).expect("dir");
        assert_eq!(find_session_id(&home, &agent), Some(sid.clone()));
        assert!(
            events_file(&home, &sid)
                .ends_with("session-state/62e05754-b66a-4ccc-ba6e-4846cc4fc1aa/events.jsonl")
        );
        // A broken file gives nothing.
        fs::write(config::session_id_file(&agent), "not-a-uuid").expect("w");
        assert_eq!(find_session_id(&home, &agent), None);
    }
}
