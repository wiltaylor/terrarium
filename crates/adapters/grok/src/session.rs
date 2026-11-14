//! Find the session ID without a hook.
//!
//! Grok keeps its sessions under `~/.grok/sessions` (docs.x.ai,
//! "Sessions"); the agent's own copy is `<agent dir>/grok/sessions`
//! ([`crate::config`]). The file layout below it is not captured, so the
//! adapter does not guess a path: it chooses the UUID for `--session-id`,
//! records it in `<agent dir>/grok/session-id`, and counts it once a file
//! or directory whose name holds the UUID exists in the agent's sessions
//! directory (at most 3 levels deep). The `SessionStart` hook is the main
//! source of the ID.

use std::fs;
use std::path::Path;

use terrarium_core::{Agent, HarnessSessionId};

use crate::config;

fn looks_like_uuid(s: &str) -> bool {
    s.len() == 36
        && s.char_indices().all(|(i, c)| match i {
            8 | 13 | 18 | 23 => c == '-',
            _ => c.is_ascii_hexdigit(),
        })
}

/// Record the UUID of a fresh start.
pub fn record_session_id(agent: &Agent, id: &str) -> terrarium_core::Result<()> {
    config::write_atomic(
        &config::session_id_file(agent),
        format!("{id}\n").as_bytes(),
    )
}

/// The UUID of the last fresh start, if one was recorded.
pub fn recorded_session_id(agent: &Agent) -> Option<HarnessSessionId> {
    let text = fs::read_to_string(config::session_id_file(agent)).ok()?;
    Some(text.trim())
        .filter(|s| looks_like_uuid(s))
        .map(|s| HarnessSessionId(s.to_owned()))
}

/// True when a name under `dir` (at most `depth` levels) holds `id`.
fn mentions(dir: &Path, id: &str, depth: usize) -> bool {
    let Ok(entries) = fs::read_dir(dir) else {
        return false;
    };
    entries.flatten().any(|e| {
        e.file_name().to_string_lossy().contains(id)
            || (depth > 1
                && e.file_type().is_ok_and(|t| t.is_dir())
                && mentions(&e.path(), id, depth - 1))
    })
}

/// The session of the agent: the recorded UUID, once grok wrote it into
/// `sessions` (the daemon's view of the agent's sessions directory).
pub fn find_session_id(sessions: &Path, agent: &Agent) -> Option<HarnessSessionId> {
    let id = recorded_session_id(agent)?;
    mentions(sessions, &id.0, 3).then_some(id)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_agent_in;

    #[test]
    fn the_recorded_id_counts_once_grok_wrote_it() {
        let tmp = tempfile::tempdir().expect("tmp");
        let agent = test_agent_in(tmp.path(), None);
        let sessions = config::sessions_dir(&agent);
        assert_eq!(find_session_id(&sessions, &agent), None);
        let id = "62e05754-b66a-4ccc-ba6e-4846cc4fc1aa";
        record_session_id(&agent, id).expect("record");
        assert_eq!(
            recorded_session_id(&agent),
            Some(HarnessSessionId(id.into()))
        );
        fs::create_dir_all(&sessions).expect("dir");
        assert_eq!(find_session_id(&sessions, &agent), None);
        // Another session does not count.
        fs::create_dir_all(sessions.join("work/other.jsonl")).expect("other");
        assert_eq!(find_session_id(&sessions, &agent), None);
        fs::write(sessions.join("work").join(format!("{id}.jsonl")), "{}").expect("w");
        assert_eq!(
            find_session_id(&sessions, &agent),
            Some(HarnessSessionId(id.into()))
        );
        // A broken record gives nothing.
        fs::write(config::session_id_file(&agent), "not-a-uuid").expect("w");
        assert_eq!(find_session_id(&sessions, &agent), None);
    }
}
