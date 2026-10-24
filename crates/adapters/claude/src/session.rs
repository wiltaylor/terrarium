//! Find the session file and the session ID without a hook.
//!
//! Path: `$CLAUDE_CONFIG_DIR/projects/<encoded cwd>/<session-id>.jsonl`.
//! The encoding replaces each character that is not an ASCII letter or
//! digit with `-`. The `transcript_path` in every hook payload gives the
//! exact path; use this only when no hook came.

use std::fs;
use std::path::{Path, PathBuf};
use std::time::SystemTime;

use terrarium_core::HarnessSessionId;

/// Claude Code's project directory name for a cwd.
pub fn encode_cwd(cwd: &Path) -> String {
    cwd.to_string_lossy()
        .chars()
        .map(|c| if c.is_ascii_alphanumeric() { c } else { '-' })
        .collect()
}

/// The directory that holds the session files for `cwd`.
pub fn project_dir(config_dir: &Path, cwd: &Path) -> PathBuf {
    config_dir.join("projects").join(encode_cwd(cwd))
}

/// The session file of one session.
pub fn session_file(config_dir: &Path, cwd: &Path, id: &HarnessSessionId) -> PathBuf {
    project_dir(config_dir, cwd).join(format!("{}.jsonl", id.0))
}

fn looks_like_uuid(s: &str) -> bool {
    s.len() == 36
        && s.char_indices().all(|(i, c)| match i {
            8 | 13 | 18 | 23 => c == '-',
            _ => c.is_ascii_hexdigit(),
        })
}

/// The newest session in the project directory of `cwd`: the file stem of
/// the `<uuid>.jsonl` with the latest change time.
pub fn newest_session(config_dir: &Path, cwd: &Path) -> Option<HarnessSessionId> {
    let entries = fs::read_dir(project_dir(config_dir, cwd)).ok()?;
    entries
        .filter_map(|e| e.ok())
        .filter_map(|e| {
            let path = e.path();
            if path.extension()? != "jsonl" {
                return None;
            }
            let stem = path.file_stem()?.to_str()?.to_owned();
            if !looks_like_uuid(&stem) {
                return None;
            }
            let at = e
                .metadata()
                .and_then(|m| m.modified())
                .unwrap_or(SystemTime::UNIX_EPOCH);
            Some((at, stem))
        })
        .max()
        .map(|(_, stem)| HarnessSessionId(stem))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn encoding_matches_compat() {
        assert_eq!(
            encode_cwd(Path::new("/tmp/terrarium-cwd-ab_c1.x")),
            "-tmp-terrarium-cwd-ab-c1-x"
        );
        assert_eq!(encode_cwd(Path::new("/home/a/work")), "-home-a-work");
        assert_eq!(encode_cwd(Path::new("/w/é")), "-w--");
    }

    #[test]
    fn newest_session_is_found() {
        let tmp = tempfile::tempdir().expect("tmp");
        let cwd = Path::new("/home/agent/work");
        let dir = project_dir(tmp.path(), cwd);
        fs::create_dir_all(&dir).expect("dir");
        assert_eq!(newest_session(tmp.path(), Path::new("/nowhere")), None);
        let old = "aee36d42-3246-4e16-be9c-16dd97c85c57";
        let new = "20997765-ed25-4336-99ed-c4be65c8e439";
        fs::write(dir.join(format!("{old}.jsonl")), "{}\n").expect("old");
        fs::write(dir.join("notes.jsonl"), "{}\n").expect("other");
        let f = fs::File::create(dir.join(format!("{new}.jsonl"))).expect("new");
        f.set_modified(SystemTime::now() + std::time::Duration::from_secs(10))
            .expect("mtime");
        assert_eq!(
            newest_session(tmp.path(), cwd),
            Some(HarnessSessionId(new.into()))
        );
        assert_eq!(
            session_file(tmp.path(), cwd, &HarnessSessionId(old.into())),
            dir.join(format!("{old}.jsonl"))
        );
    }
}
