//! Find the session file and the session ID without a hook.
//!
//! Layout (`docs/compat.md`, "Session file"):
//!
//! - `$CODEX_HOME/sessions/YYYY/MM/DD/rollout-<local time>-<id>.jsonl`, for
//!   example `rollout-2026-10-24T16-56-10-01a16cb2-0d95-7b40-894d-e19fc1b66c24.jsonl`.
//!   The first record is `session_meta` with `payload.id` and
//!   `payload.cwd`.
//! - `$CODEX_HOME/session_index.jsonl`: `id`, `thread_name`, `updated_at`.
//!
//! The `transcript_path` of every hook payload gives the exact path; use
//! this module only when no hook came. Codex has no session file before
//! the first prompt.
//!
//! All agents in the sandbox have the same cwd, so the cwd in
//! `session_meta` does not tell whose session a file is. The per-agent
//! `history.jsonl` does ([`crate::config`]): each line has the
//! `session_id` of a typed prompt. The fact that Codex writes one line for
//! each prompt comes from an operator's home of the same version, not from
//! a spike.

use std::fs;
use std::io::{BufRead, BufReader};
use std::path::{Path, PathBuf};
use std::time::SystemTime;

use serde_json::Value;
use terrarium_core::{Agent, HarnessSessionId};

use crate::{CodexOptions, config};

/// The directory of the session files.
pub const SESSIONS_DIR: &str = "sessions";

fn looks_like_uuid(s: &str) -> bool {
    s.len() == 36
        && s.char_indices().all(|(i, c)| match i {
            8 | 13 | 18 | 23 => c == '-',
            _ => c.is_ascii_hexdigit(),
        })
}

/// The session ID in a session file name: the last 36 characters of
/// `rollout-<time>-<id>.jsonl`.
pub fn id_from_file_name(name: &str) -> Option<HarnessSessionId> {
    let stem = name.strip_prefix("rollout-")?.strip_suffix(".jsonl")?;
    let id = stem.get(stem.len().checked_sub(36)?..)?;
    looks_like_uuid(id).then(|| HarnessSessionId(id.to_owned()))
}

/// Every session file under `home/sessions`, with its change time.
pub fn session_files(home: &Path) -> Vec<(SystemTime, PathBuf)> {
    let mut out = Vec::new();
    let mut dirs = vec![(home.join(SESSIONS_DIR), 0)];
    while let Some((dir, depth)) = dirs.pop() {
        let Ok(entries) = fs::read_dir(&dir) else {
            continue;
        };
        for e in entries.filter_map(|e| e.ok()) {
            let path = e.path();
            let Ok(meta) = e.metadata() else { continue };
            if meta.is_dir() {
                // YYYY/MM/DD: three levels.
                if depth < 3 {
                    dirs.push((path, depth + 1));
                }
            } else if path
                .file_name()
                .and_then(|n| n.to_str())
                .and_then(id_from_file_name)
                .is_some()
            {
                out.push((meta.modified().unwrap_or(SystemTime::UNIX_EPOCH), path));
            }
        }
    }
    out
}

/// The session file of `id` under `home` (the daemon's view of
/// `CODEX_HOME`).
pub fn find_session_file(home: &Path, id: &HarnessSessionId) -> Option<PathBuf> {
    session_files(home).into_iter().map(|(_, p)| p).find(|p| {
        p.file_name()
            .and_then(|n| n.to_str())
            .and_then(id_from_file_name)
            .is_some_and(|got| got == *id)
    })
}

/// The `session_id` of the last line of a `history.jsonl`.
pub fn last_history_session(history: &Path) -> Option<HarnessSessionId> {
    let file = fs::File::open(history).ok()?;
    BufReader::new(file)
        .lines()
        .map_while(Result::ok)
        .filter_map(|l| {
            let v: Value = serde_json::from_str(l.trim()).ok()?;
            v.get("session_id")?.as_str().map(str::to_owned)
        })
        .last()
        .map(HarnessSessionId)
}

/// The `payload.cwd` of the `session_meta` record in the first line.
pub fn session_cwd(file: &Path) -> Option<PathBuf> {
    let f = fs::File::open(file).ok()?;
    let mut first = String::new();
    BufReader::new(f).read_line(&mut first).ok()?;
    let v: Value = serde_json::from_str(first.trim()).ok()?;
    if v.get("type")?.as_str()? != "session_meta" {
        return None;
    }
    v.get("payload")?.get("cwd")?.as_str().map(PathBuf::from)
}

/// The newest session whose `session_meta.cwd` is `cwd`.
pub fn newest_session_for_cwd(home: &Path, cwd: &Path) -> Option<HarnessSessionId> {
    let mut files = session_files(home);
    files.sort_by_key(|f| std::cmp::Reverse(f.0));
    files
        .into_iter()
        .find(|(_, p)| session_cwd(p).as_deref() == Some(cwd))
        .and_then(|(_, p)| id_from_file_name(p.file_name()?.to_str()?))
}

/// The agent's latest session, in this order:
///
/// 1. The last `session_id` in the per-agent `history.jsonl` (the sandbox
///    binds it over the shared one).
/// 2. Without a sandbox cwd (each agent runs in its own workspace): the
///    newest session file with the agent's cwd. With a sandbox cwd this
///    step is skipped, because every agent has that cwd, and the newest
///    file can be another agent's.
pub fn find_session_id(opts: &CodexOptions, agent: &Agent) -> Option<HarnessSessionId> {
    if let Some(id) = last_history_session(&config::history_path(agent)) {
        return Some(id);
    }
    if opts.harness_workspace.is_some() {
        return None;
    }
    newest_session_for_cwd(opts.host_codex_home(), &opts.harness_cwd(agent))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_agent_in;

    const SID: &str = "01a16cb2-0d95-7b40-894d-e19fc1b66c24";

    fn fixture_line() -> String {
        let text = std::fs::read_to_string(format!(
            "{}/tests/fixtures/session.jsonl",
            env!("CARGO_MANIFEST_DIR")
        ))
        .expect("fixture");
        text.lines().next().expect("first").to_owned()
    }

    fn write_session(home: &Path, day: &str, time: &str, id: &str, cwd: &str) -> PathBuf {
        let dir = home.join(SESSIONS_DIR).join(day);
        fs::create_dir_all(&dir).expect("dir");
        let path = dir.join(format!("rollout-{time}-{id}.jsonl"));
        let line = fixture_line().replace("$CWD", cwd).replace(SID, id);
        fs::write(&path, format!("{line}\n")).expect("write");
        path
    }

    #[test]
    fn ids_from_file_names() {
        assert_eq!(
            id_from_file_name(&format!("rollout-2026-10-24T16-56-10-{SID}.jsonl")),
            Some(HarnessSessionId(SID.into()))
        );
        assert_eq!(id_from_file_name("rollout-short.jsonl"), None);
        assert_eq!(id_from_file_name(&format!("{SID}.jsonl")), None);
    }

    #[test]
    fn session_file_and_cwd_are_found() {
        let tmp = tempfile::tempdir().expect("tmp");
        let home = tmp.path();
        assert_eq!(find_session_file(home, &HarnessSessionId(SID.into())), None);
        let path = write_session(home, "2026/10/24", "2026-10-24T16-56-10", SID, "/w/a");
        assert_eq!(
            find_session_file(home, &HarnessSessionId(SID.into())),
            Some(path.clone())
        );
        assert_eq!(session_cwd(&path), Some(PathBuf::from("/w/a")));

        let other = "01a16cb0-f46c-7ea2-a000-55803cbcdad5";
        let newer = write_session(home, "2026/10/25", "2026-10-25T09-00-00", other, "/w/b");
        let f = fs::File::options().write(true).open(&newer).expect("open");
        f.set_modified(SystemTime::now() + std::time::Duration::from_secs(10))
            .expect("mtime");
        assert_eq!(
            newest_session_for_cwd(home, Path::new("/w/a")),
            Some(HarnessSessionId(SID.into()))
        );
        assert_eq!(
            newest_session_for_cwd(home, Path::new("/w/b")),
            Some(HarnessSessionId(other.into()))
        );
        assert_eq!(newest_session_for_cwd(home, Path::new("/w/c")), None);
    }

    #[test]
    fn history_wins_and_sandbox_skips_the_cwd_scan() {
        let tmp = tempfile::tempdir().expect("tmp");
        let home = tmp.path().join("auth-codex");
        let agent = test_agent_in(tmp.path(), None);
        let ws = agent.workspace.to_string_lossy().into_owned();
        write_session(&home, "2026/10/24", "2026-10-24T16-56-10", SID, &ws);
        let plain = CodexOptions {
            host_codex_home: Some(home.clone()),
            ..CodexOptions::default()
        };
        assert_eq!(
            find_session_id(&plain, &agent),
            Some(HarnessSessionId(SID.into()))
        );
        let sandbox = CodexOptions {
            harness_workspace: Some(PathBuf::from(&ws)),
            ..plain.clone()
        };
        assert_eq!(find_session_id(&sandbox, &agent), None);

        let history = config::history_path(&agent);
        fs::create_dir_all(history.parent().expect("parent")).expect("dir");
        fs::write(
            &history,
            "{\"session_id\":\"old\",\"ts\":1,\"text\":\"a\"}\n{\"session_id\":\"new\",\"ts\":2,\"text\":\"b\"}\nnot json\n",
        )
        .expect("history");
        assert_eq!(
            find_session_id(&sandbox, &agent),
            Some(HarnessSessionId("new".into()))
        );
        assert_eq!(
            find_session_id(&plain, &agent),
            Some(HarnessSessionId("new".into()))
        );
    }
}
