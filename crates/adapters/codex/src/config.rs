//! Per-agent files that the sandbox lays over the shared Codex home
//! (PLAN §8 and §11, the overlay trick).
//!
//! All agents share one Codex home (`/auth/codex`, `CODEX_HOME`) for the
//! login, the settings, and the caches. Codex is made for many terminals
//! on one home, so most of what it writes there is safe to share. The
//! session files, the session index, and the SQLite files hold turn text,
//! so they are per agent.
//!
//! Layout under the harness directory (`/state/agents/<ulid>/harness/`):
//!
//! | File | Use |
//! |---|---|
//! | `prompt.md` | the rendered system prompt; `-c model_instructions_file` names it (the sandbox binds the agent directory at the same path) |
//! | `codex/history.jsonl` | bound over `$CODEX_HOME/history.jsonl` |
//! | `codex/hooks.json` | bound over `$CODEX_HOME/hooks.json`; holds no hooks |
//! | `codex/sessions/` | bound over `$CODEX_HOME/sessions/` |
//! | `codex/session_index.jsonl` | bound over `$CODEX_HOME/session_index.jsonl` |
//! | `codex/shell_snapshots/` | bound over `$CODEX_HOME/shell_snapshots/` |
//! | `codex/sqlite/` | bound over itself, read-write; `-c sqlite_home` names it (see [`crate::launch`]) |
//!
//! # What Codex writes into its home, and where it goes
//!
//! Every agent has the same cwd (`/home/agent/work`) and, in its PID
//! namespace, the same pid. A path that Codex keys by cwd or by pid is one
//! path for all agents. A path that it keys by the session ID, or by a
//! random part, does not collide. The file list comes from the spikes
//! (`docs/compat.md`, "Quiet start") and from an operator's home of the
//! same version.
//!
//! | Path in the home | Decision | Why |
//! |---|---|---|
//! | `auth.json` | shared | the one login. A token refresh by one agent must reach all of them. Many instances on one home is not tested for refresh (`docs/compat.md`, "Codex: not tested"). |
//! | `history.jsonl` | per-agent | the prompt history (`session_id`, `ts`, `text`), the up-arrow list. Shared, each agent would see the typed turns (mail text) of every other agent and of the operator. The per-agent copy also tells which sessions are this agent's (see [`crate::session`]). |
//! | `hooks.json` | per-agent, empty | the operator's own hooks (`{"hooks": {...}}`). Shared, they would fire in every agent. The `-c` hooks replace the `hooks.<Event>` keys of `config.toml` already, because Terrarium sets all 11 events. |
//! | `sessions/YYYY/MM/DD/rollout-<time>-<id>.jsonl` | per-agent | the transcripts. One file per session ID does not collide, but a shared directory lets each agent read, and write into, the transcripts of every other agent in every project (a path for prompt injection). `codex resume <id>` finds the file in the agent's own directory (through the agent's own `threads` table, which a new SQLite home backfills from `sessions/`). The daemon's usage collector reads it from the agent's harness dir. |
//! | `session_index.jsonl` | per-agent | one line per session (`id`, `thread_name`, `updated_at`), appended; the newest line for an ID wins. The thread name is the start of the first turn. The `resume` picker and a resume by name read it. Codex appends to the file, so a file bind works; a failed write is a logged error ("failed to index thread name"), not tested. |
//! | `thread-writer-locks/<id>.lock` | shared | keyed by the session ID. Shared on purpose: it stops two instances on one session ("This conversation is open in another app"). |
//! | `shell_snapshots/<id>.<ns>.sh` | per-agent | keyed by the session ID, so no collision, but each file holds the exported env of the agent's shell (`declare -x`): `TERRARIUM_AGENT`, and an API key when the launch env has one. |
//! | `tmp/arg0*/codex-arg0<random>/` | shared | random names. |
//! | `state_5`, `logs_2`, `memories_1`, `goals_1`, `queue_1`, `thread_history_1` (`.sqlite`, `-wal`, `-shm`) | per-agent, in `codex/sqlite/` | SQLite in WAL mode. `threads` (in `state_5`) holds the first user message and the title of each session; `thread_items` (in `thread_history_1`) holds every item of each turn. Shared, each agent could read the turns of every other agent. `-c sqlite_home` moves all of them, so no file bind splits a database from its `-wal` and `-shm` files. The SQLite files that an older version left in the shared home stay there, unused by the agents. |
//! | `config.toml` | shared | the operator's settings (model, providers). Codex writes `[tui]` keys into it on the first start. Trust comes from `-c`, so no agent writes a trust decision. |
//! | `version.json`, `models_cache.json`, `installation_id`, `cache/`, `skills/`, `plugins/` | shared | caches of public data. The update check is off. |
//! | `packages/`, `app-server-daemon/` | not used | only the app-server daemon writes them; `--no-daemon` keeps it off. |
//!
//! Not handled: `notify` and other programs in the shared `config.toml`
//! run in every agent.

use std::fs;
use std::path::{Path, PathBuf};

use terrarium_core::{Agent, FileOverlay};

use crate::CodexOptions;
use crate::error::{CodexError, Result};

/// The prompt history in the Codex home. A JSON-lines file.
pub const HISTORY_FILE: &str = "history.jsonl";
/// The hook file in the Codex home.
pub const HOOKS_FILE: &str = "hooks.json";
/// The session files in the Codex home. A directory.
pub const SESSIONS_DIR: &str = crate::session::SESSIONS_DIR;
/// The session index in the Codex home. A JSON-lines file.
pub const SESSION_INDEX_FILE: &str = "session_index.jsonl";
/// The shell snapshots in the Codex home. A directory.
pub const SHELL_SNAPSHOTS_DIR: &str = "shell_snapshots";
/// The per-agent SQLite home in [`codex_dir`].
pub const SQLITE_DIR: &str = "sqlite";
/// The rendered prompt in the agent directory (PLAN §11).
pub const PROMPT_FILE: &str = "prompt.md";
/// A hook file with no hooks, in the format of the operator's file.
pub const EMPTY_HOOKS: &str = "{\n  \"hooks\": {}\n}\n";

/// `/state/agents/<ulid>/harness`: the directory of the per-agent harness
/// files, next to the workspace. The sandbox binds it read-only at the
/// same path (`docs/runner.md`, "Files"); only the overlays that the
/// harness writes are read-write. The runner's own records are in the
/// sibling `private/`, which the sandbox never binds.
pub fn agent_dir(agent: &Agent) -> PathBuf {
    agent
        .workspace
        .parent()
        .map(Path::to_path_buf)
        .unwrap_or_else(|| agent.workspace.clone())
        .join(HARNESS_DIR)
}

/// The name of the harness directory in the agent directory
/// (`terrarium_runner::HARNESS_DIR`).
pub const HARNESS_DIR: &str = "harness";

/// `/state/agents/<ulid>/codex`.
pub fn codex_dir(agent: &Agent) -> PathBuf {
    agent_dir(agent).join("codex")
}

/// The rendered prompt file of the agent.
pub fn prompt_path(agent: &Agent) -> PathBuf {
    agent_dir(agent).join(PROMPT_FILE)
}

/// The per-agent prompt history (the daemon's view).
pub fn history_path(agent: &Agent) -> PathBuf {
    codex_dir(agent).join(HISTORY_FILE)
}

/// The per-agent SQLite home (`-c sqlite_home`). The sandbox binds the
/// harness dir at the same path, so this path is the same inside.
pub fn sqlite_dir(agent: &Agent) -> PathBuf {
    codex_dir(agent).join(SQLITE_DIR)
}

/// Read the rendered prompt. A missing file means "no prompt".
pub fn read_prompt(agent: &Agent) -> Result<Option<String>> {
    let path = prompt_path(agent);
    match fs::read_to_string(&path) {
        Ok(s) => Ok(Some(s)),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(e) => Err(CodexError::io(path, e)),
    }
}

/// Write `bytes` to `path` through a temporary file and a rename.
fn write_atomic(path: &Path, bytes: &[u8]) -> Result<()> {
    let tmp = path.with_extension("tmp-terrarium");
    fs::write(&tmp, bytes).map_err(|e| CodexError::io(&tmp, e))?;
    fs::rename(&tmp, path).map_err(|e| CodexError::io(path, e))
}

/// Write the per-agent files and return the overlays.
///
/// - `home` is the shared Codex home on the host (the daemon's view).
/// - `prompt`: when `Some`, write it to `prompt.md`. When `None`, keep the
///   file that is there (the daemon renders it).
///
/// Each overlay target also gets a mount point in the shared home when it
/// has none: otherwise bwrap makes one and leaves it behind. A new
/// `hooks.json` mount point holds no hooks.
pub fn materialize(
    opts: &CodexOptions,
    agent: &Agent,
    home: &Path,
    prompt: Option<&str>,
) -> Result<Vec<FileOverlay>> {
    let dir = codex_dir(agent);
    fs::create_dir_all(&dir).map_err(|e| CodexError::io(&dir, e))?;
    if let Some(text) = prompt {
        write_atomic(&prompt_path(agent), text.as_bytes())?;
    }

    // The agent's own prompt history. A new one starts empty, not as a
    // copy of the shared one.
    let history = file_overlay(
        &dir.join(HISTORY_FILE),
        &home.join(HISTORY_FILE),
        "",
        opts.codex_home.join(HISTORY_FILE),
        true,
    )?;

    // No hooks but the `-c` ones. The file is the agent's, so a rewrite on
    // every start is safe; Codex only reads it.
    write_atomic(&dir.join(HOOKS_FILE), EMPTY_HOOKS.as_bytes())?;
    let hooks = file_overlay(
        &dir.join(HOOKS_FILE),
        &home.join(HOOKS_FILE),
        EMPTY_HOOKS,
        opts.codex_home.join(HOOKS_FILE),
        false,
    )?;

    // The agent's own transcripts and session index (see the table at the
    // top). A new agent starts with none.
    let sessions = dir_overlay(
        &dir.join(SESSIONS_DIR),
        &home.join(SESSIONS_DIR),
        opts.codex_home.join(SESSIONS_DIR),
    )?;
    let index = file_overlay(
        &dir.join(SESSION_INDEX_FILE),
        &home.join(SESSION_INDEX_FILE),
        "",
        opts.codex_home.join(SESSION_INDEX_FILE),
        true,
    )?;
    let snapshots = dir_overlay(
        &dir.join(SHELL_SNAPSHOTS_DIR),
        &home.join(SHELL_SNAPSHOTS_DIR),
        opts.codex_home.join(SHELL_SNAPSHOTS_DIR),
    )?;

    // The agent's own SQLite home. The rest of the harness dir is
    // read-only in the sandbox.
    let sqlite = sqlite_dir(agent);
    fs::create_dir_all(&sqlite).map_err(|e| CodexError::io(&sqlite, e))?;
    let sqlite = FileOverlay {
        source: sqlite.clone(),
        target: sqlite,
        writable: true,
    };
    Ok(vec![history, hooks, sessions, index, snapshots, sqlite])
}

/// A read-write per-agent directory over `target`. Both the agent's
/// directory and the mount point in the shared home (`shared`) must exist:
/// otherwise bwrap makes the mount point itself and leaves it behind.
fn dir_overlay(own: &Path, shared: &Path, target: PathBuf) -> Result<FileOverlay> {
    for d in [own, shared] {
        fs::create_dir_all(d).map_err(|e| CodexError::io(d, e))?;
    }
    Ok(FileOverlay {
        source: own.to_path_buf(),
        target,
        writable: true,
    })
}

/// A per-agent file over `target`. A missing file (the agent's, or the
/// mount point in the shared home) is made with `initial`; a file that is
/// there keeps its content.
fn file_overlay(
    own: &Path,
    shared: &Path,
    initial: &str,
    target: PathBuf,
    writable: bool,
) -> Result<FileOverlay> {
    for f in [own, shared] {
        if let Some(parent) = f.parent() {
            fs::create_dir_all(parent).map_err(|e| CodexError::io(parent, e))?;
        }
        match fs::OpenOptions::new().write(true).create_new(true).open(f) {
            Ok(_) => fs::write(f, initial).map_err(|e| CodexError::io(f, e))?,
            Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => {}
            Err(e) => return Err(CodexError::io(f, e)),
        }
    }
    Ok(FileOverlay {
        source: own.to_path_buf(),
        target,
        writable,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_agent_in;

    fn setup() -> (tempfile::TempDir, PathBuf, Agent) {
        let tmp = tempfile::tempdir().expect("tmp");
        let home = tmp.path().join("auth-codex");
        fs::create_dir_all(&home).expect("home");
        let agent = test_agent_in(tmp.path(), None);
        (tmp, home, agent)
    }

    #[test]
    fn overlay_list() {
        let (_tmp, home, agent) = setup();
        let overlays =
            materialize(&CodexOptions::default(), &agent, &home, Some("hello")).expect("mat");
        let own = codex_dir(&agent);
        let got: Vec<(PathBuf, PathBuf, bool)> = overlays
            .iter()
            .map(|o| (o.source.clone(), o.target.clone(), o.writable))
            .collect();
        assert_eq!(
            got,
            [
                (
                    own.join("history.jsonl"),
                    PathBuf::from("/auth/codex/history.jsonl"),
                    true
                ),
                (
                    own.join("hooks.json"),
                    PathBuf::from("/auth/codex/hooks.json"),
                    false
                ),
                (
                    own.join("sessions"),
                    PathBuf::from("/auth/codex/sessions"),
                    true
                ),
                (
                    own.join("session_index.jsonl"),
                    PathBuf::from("/auth/codex/session_index.jsonl"),
                    true
                ),
                (
                    own.join("shell_snapshots"),
                    PathBuf::from("/auth/codex/shell_snapshots"),
                    true
                ),
                (own.join("sqlite"), own.join("sqlite"), true),
            ]
        );
        // Each source and each mount point in the shared home exists, with
        // the same kind (file or directory).
        for o in overlays
            .iter()
            .filter(|o| o.target.starts_with("/auth/codex"))
        {
            let rel = o.target.strip_prefix("/auth/codex").expect("in home");
            let shared = home.join(rel);
            assert!(o.source.exists(), "{}", o.source.display());
            assert!(shared.exists(), "{}", rel.display());
            assert_eq!(o.source.is_dir(), shared.is_dir(), "{}", rel.display());
        }
        assert!(sqlite_dir(&agent).is_dir());
        let hooks: serde_json::Value =
            serde_json::from_slice(&fs::read(own.join("hooks.json")).expect("hooks"))
                .expect("json");
        assert_eq!(hooks, serde_json::json!({"hooks": {}}));
        assert_eq!(read_prompt(&agent).expect("p").as_deref(), Some("hello"));

        // The login, the settings, and the locks stay shared.
        for shared in [
            "auth.json",
            "thread-writer-locks/x.lock",
            "config.toml",
            "models_cache.json",
        ] {
            let p = Path::new("/auth/codex").join(shared);
            assert!(
                !overlays.iter().any(|o| p.starts_with(&o.target)),
                "{shared} must stay shared"
            );
        }
        // The transcripts and the session index are the agent's own.
        for own_path in [
            "sessions/2026/10/24/rollout-x.jsonl",
            "session_index.jsonl",
            "shell_snapshots/x.1.sh",
        ] {
            let p = Path::new("/auth/codex").join(own_path);
            let o = overlays
                .iter()
                .find(|o| p.starts_with(&o.target))
                .unwrap_or_else(|| panic!("{own_path} must be per agent"));
            assert!(o.source.starts_with(&own), "{own_path}");
        }
    }

    #[test]
    fn existing_files_keep_their_content() {
        let (_tmp, home, agent) = setup();
        // The operator's own history and hooks in the shared home.
        fs::write(home.join(HISTORY_FILE), "{\"text\":\"operator\"}\n").expect("h");
        fs::write(home.join(HOOKS_FILE), "{\"hooks\":{\"Stop\":[]}}").expect("k");
        let opts = CodexOptions::default();
        materialize(&opts, &agent, &home, None).expect("mat");
        let own = history_path(&agent);
        // The agent starts with an empty history, not the operator's.
        assert_eq!(fs::read(&own).expect("own"), b"");
        fs::write(&own, "{\"text\":\"agent\"}\n").expect("write");

        materialize(&opts, &agent, &home, None).expect("again");
        assert_eq!(
            fs::read_to_string(&own).expect("own"),
            "{\"text\":\"agent\"}\n"
        );
        assert_eq!(
            fs::read_to_string(home.join(HISTORY_FILE)).expect("shared"),
            "{\"text\":\"operator\"}\n"
        );
        assert_eq!(
            fs::read_to_string(home.join(HOOKS_FILE)).expect("shared"),
            "{\"hooks\":{\"Stop\":[]}}"
        );
        assert_eq!(read_prompt(&agent).expect("prompt"), None);
    }
}
