//! Find the session file and the session ID without a hook.
//!
//! The adapter passes `--session-dir <agent dir>/pi/sessions`. pi then puts
//! the files flat in that directory, named `<UTC start time>_<id>.jsonl`
//! (for example `2026-10-24T07-49-08-371Z_a6acb784-….jsonl`; verified).
//! Without `--session-dir`, the files go to
//! `$PI_CODING_AGENT_DIR/sessions/--<cwd without the leading "/", "/"
//! replaced by "-">--/` ([`default_session_dir`]). The file is created at
//! the first message, so a session with no message has no file.
//!
//! The `session_file` in every hook payload gives the exact path; use this
//! only when no hook came.

use std::fs;
use std::path::{Path, PathBuf};
use std::time::SystemTime;

use terrarium_core::HarnessSessionId;

/// pi's default session directory for a cwd (no `--session-dir`).
pub fn default_session_dir(agent_dir: &Path, cwd: &Path) -> PathBuf {
    let s = cwd.to_string_lossy();
    let s = s.strip_prefix('/').unwrap_or(&s);
    let safe: String = s
        .chars()
        .map(|c| {
            if matches!(c, '/' | '\\' | ':') {
                '-'
            } else {
                c
            }
        })
        .collect();
    agent_dir.join("sessions").join(format!("--{safe}--"))
}

fn looks_like_uuid(s: &str) -> bool {
    s.len() == 36
        && s.char_indices().all(|(i, c)| match i {
            8 | 13 | 18 | 23 => c == '-',
            _ => c.is_ascii_hexdigit(),
        })
}

/// The session ID in a session file name, `<time>_<id>.jsonl`.
pub fn id_from_file_name(path: &Path) -> Option<HarnessSessionId> {
    if path.extension()? != "jsonl" {
        return None;
    }
    let stem = path.file_stem()?.to_str()?;
    let id = stem.rsplit('_').next()?;
    looks_like_uuid(id).then(|| HarnessSessionId(id.to_owned()))
}

fn sessions(dir: &Path) -> Vec<(SystemTime, PathBuf, HarnessSessionId)> {
    let Ok(entries) = fs::read_dir(dir) else {
        return Vec::new();
    };
    entries
        .filter_map(|e| e.ok())
        .filter_map(|e| {
            let path = e.path();
            let id = id_from_file_name(&path)?;
            let at = e
                .metadata()
                .and_then(|m| m.modified())
                .unwrap_or(SystemTime::UNIX_EPOCH);
            Some((at, path, id))
        })
        .collect()
}

/// The newest session in `dir`: the file with the latest change time.
pub fn newest_session(dir: &Path) -> Option<HarnessSessionId> {
    sessions(dir)
        .into_iter()
        .max_by(|a, b| a.0.cmp(&b.0))
        .map(|(_, _, id)| id)
}

/// The session file of `id` in `dir`, if it exists.
pub fn find_session_file(dir: &Path, id: &HarnessSessionId) -> Option<PathBuf> {
    sessions(dir)
        .into_iter()
        .find(|(_, _, i)| i == id)
        .map(|(_, p, _)| p)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn default_dir_matches_pi() {
        assert_eq!(
            default_session_dir(Path::new("/a"), Path::new("/home/wil/dev/pi")),
            Path::new("/a/sessions/--home-wil-dev-pi--")
        );
        assert_eq!(
            default_session_dir(Path::new("/a"), Path::new("/tmp/claude-1000/x.y")),
            Path::new("/a/sessions/--tmp-claude-1000-x.y--")
        );
    }

    #[test]
    fn newest_session_and_file() {
        let tmp = tempfile::tempdir().expect("tmp");
        let dir = tmp.path();
        assert_eq!(newest_session(&dir.join("missing")), None);
        let old = "0aa4e42a-0404-423c-a41c-332c83e48e25";
        let new = "a6acb784-5d88-4161-8792-90143365c46a";
        fs::write(
            dir.join(format!("2026-10-24T07-29-07-955Z_{old}.jsonl")),
            "{}\n",
        )
        .expect("old");
        fs::write(dir.join("notes.jsonl"), "{}\n").expect("other");
        fs::write(dir.join(format!("x_{new}.txt")), "").expect("other");
        let f = fs::File::create(dir.join(format!("2026-10-24T07-49-08-371Z_{new}.jsonl")))
            .expect("new");
        f.set_modified(SystemTime::now() + std::time::Duration::from_secs(10))
            .expect("mtime");
        assert_eq!(newest_session(dir), Some(HarnessSessionId(new.into())));
        assert_eq!(
            find_session_file(dir, &HarnessSessionId(old.into())),
            Some(dir.join(format!("2026-10-24T07-29-07-955Z_{old}.jsonl")))
        );
        assert_eq!(
            find_session_file(dir, &HarnessSessionId("nope".into())),
            None
        );
    }
}
