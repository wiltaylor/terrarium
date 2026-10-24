//! Per-agent files that the sandbox lays over the shared Codex home
//! (PLAN §8 and §11, the overlay trick).
//!
//! All agents share one Codex home (`/auth/codex`, `CODEX_HOME`) for the
//! login and the session files. Codex is made for many terminals on one
//! home, so most of what it writes there is safe to share.
//!
//! Layout under the agent directory (`/state/agents/<ulid>/`):
//!
//! | File | Use |
//! |---|---|
//! | `prompt.md` | the rendered system prompt; `-c model_instructions_file` names it (the sandbox binds the agent directory at the same path) |
//! | `codex/history.jsonl` | bound over `$CODEX_HOME/history.jsonl` |
//! | `codex/hooks.json` | bound over `$CODEX_HOME/hooks.json`; holds no hooks |
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
//! | `sessions/YYYY/MM/DD/rollout-<time>-<id>.jsonl` | shared | one file per session ID: no collision. `codex resume` reads them, and the daemon's usage collector reads them through the shared home. |
//! | `session_index.jsonl` | shared | one line per session (`id`, `thread_name`, `updated_at`), appended. The `resume` picker reads it. |
//! | `thread-writer-locks/<id>.lock` | shared | keyed by the session ID. Shared on purpose: it stops two instances on one session ("This conversation is open in another app"). |
//! | `shell_snapshots/<id>.<ns>.sh` | shared | keyed by the session ID. |
//! | `tmp/arg0*/codex-arg0<random>/` | shared | random names. |
//! | `state_5`, `logs_2`, `memories_1`, `goals_1`, `queue_1`, `thread_history_1` (`.sqlite`, `-wal`, `-shm`) | shared | SQLite in WAL mode, made for many processes. A per-file bind would split a database from its `-wal` and `-shm` files. The `memories` feature is off by default in 0.160.0 (`codex features list`), so no memory crosses agents. |
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
/// The rendered prompt in the agent directory (PLAN §11).
pub const PROMPT_FILE: &str = "prompt.md";
/// A hook file with no hooks, in the format of the operator's file.
pub const EMPTY_HOOKS: &str = "{\n  \"hooks\": {}\n}\n";

/// `/state/agents/<ulid>`: the parent of the workspace.
pub fn agent_dir(agent: &Agent) -> PathBuf {
    agent
        .workspace
        .parent()
        .map(Path::to_path_buf)
        .unwrap_or_else(|| agent.workspace.clone())
}

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
    Ok(vec![history, hooks])
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
    fn overlays_cover_history_and_hooks_only() {
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
            ]
        );
        for o in &overlays {
            let rel = o.target.strip_prefix("/auth/codex").expect("in home");
            assert!(o.source.is_file(), "{}", o.source.display());
            assert!(home.join(rel).is_file(), "{}", rel.display());
        }
        let hooks: serde_json::Value =
            serde_json::from_slice(&fs::read(own.join("hooks.json")).expect("hooks"))
                .expect("json");
        assert_eq!(hooks, serde_json::json!({"hooks": {}}));
        assert_eq!(read_prompt(&agent).expect("p").as_deref(), Some("hello"));

        // The login, the session files, and the locks stay shared.
        for shared in [
            "auth.json",
            "sessions/2026/10/24/rollout-x.jsonl",
            "session_index.jsonl",
            "thread-writer-locks/x.lock",
            "state_5.sqlite",
            "config.toml",
        ] {
            let p = Path::new("/auth/codex").join(shared);
            assert!(
                !overlays.iter().any(|o| p.starts_with(&o.target)),
                "{shared} must stay shared"
            );
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
