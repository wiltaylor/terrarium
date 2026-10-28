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
//! | `codex/AGENTS.md`, `codex/AGENTS.override.md` | empty; bound read-only over the same files in `$CODEX_HOME` |
//! | `<workspace>/generated_images/` | bound read-write over `$CODEX_HOME/generated_images/` (D4) |
//! | `skills/` | the agent's delivered skills (PLAN §8.6); bound read-only over `$CODEX_HOME/skills/`, also when empty |
//! | `codex/requirements.toml` | bound read-only over `/etc/codex/requirements.toml`: the managed requirements, see [`REQUIREMENTS`] |
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
//! | `version.json`, `models_cache.json`, `installation_id`, `cache/`, `plugins/` | shared | caches of public data. The update check is off. |
//! | `skills/` | per-agent, read-only | the user skill root of Codex (`ext/skills/src/host_roots.rs:95-101`). The agent's delivered skills go over it (PLAN §8.6), so the operator's skills in the shared home never load. An agent with no skill gets an empty dir. `.system` stays out: `skills.bundled.enabled = false`. `$HOME/.agents/skills`, the other user root, is on the sandbox's empty tmpfs `$HOME`. |
//! | `packages/`, `app-server-daemon/` | not used | only the app-server daemon writes them; `--no-daemon` keeps it off. |
//!
//! | `AGENTS.md`, `AGENTS.override.md` | per-agent, empty | the operator's global instructions. Codex loads them in every session, also with a Terrarium prompt in replace mode. They stay out of agents, as for the other harnesses (PLAN §8, the AGENTS.md rule). Codex still loads the `AGENTS.md` of the workspace root. |
//! | `generated_images/<session>/<call>.png` | per-agent, in the workspace | the images of the `image_gen.imagegen` tool (save root `$CODEX_HOME`). Shared, the images of all agents mix, and an agent can read the images of the others. The bind puts them in `<workspace>/generated_images/`, where the agent and its targets find them as files. The workspace root is not a repository, so no `.gitignore` is needed; an image is often a result to keep. |
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
/// The global instruction files in the Codex home. Each agent gets empty
/// ones.
pub const GLOBAL_INSTRUCTION_FILES: &[&str] = &["AGENTS.md", "AGENTS.override.md"];
/// The directory of the generated images, in the Codex home and in the
/// agent's workspace (D4).
pub const IMAGES_DIR: &str = "generated_images";
/// The per-agent copy of the managed requirements, in [`codex_dir`].
pub const REQUIREMENTS_FILE: &str = "requirements.toml";
/// Where Codex reads the system requirements on Unix
/// (`config/src/loader/mod.rs:113`, `tui/src/debug_config.rs:810`).
pub const SYSTEM_REQUIREMENTS: &str = "/etc/codex/requirements.toml";
/// The managed requirements of each agent: unified exec pinned off.
///
/// With unified exec on (the default), `exec_command` returns a session ID
/// when a command runs past its yield time, the process keeps running
/// after the turn ends, and `write_stdin` reads it later. Only `/stop` or
/// the end of the session stops it (`core/src/session/handlers.rs:64`,
/// `:316`). That is background work, which Terrarium does with
/// `terrarium run`. With unified exec off, Codex registers a one-shot
/// `exec_command`: it runs a command to completion (default timeout 10 s,
/// `timeout_ms` sets more), stops it on timeout or interrupt, and has no
/// `write_stdin` (`core/src/tools/spec_plan.rs:1257-1267`,
/// `core/src/tools/handlers/unified_exec/exec_command.rs:463-490`).
///
/// A user `config.toml` or `-c` cannot turn unified exec off: only a
/// managed requirement can (`core/src/config/managed_features.rs:178-183`).
/// The file is bound only in the sandbox; with no sandbox, Codex keeps
/// unified exec. Verified in the source and the binary strings; not run.
pub const REQUIREMENTS: &str = "# Written by Terrarium at each start: no background terminals.\n[features]\nunified_exec = false\n";

/// `/state/agents/<ulid>/harness`: the directory of the per-agent harness
/// files, next to the workspace. The sandbox binds it read-only at the
/// same path (`docs/runner.md`, "Files"); only the overlays that the
/// harness writes are read-write. The runner's own records are in the
/// sibling `private/`, which the sandbox never binds.
pub fn agent_dir(agent: &Agent) -> PathBuf {
    if let Some(d) = &agent.harness_dir {
        return d.clone();
    }
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

/// The agent's own image directory, in its workspace (the daemon's view).
pub fn images_dir(agent: &Agent) -> PathBuf {
    agent.workspace.join(IMAGES_DIR)
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
    let mut overlays = vec![history, hooks, sessions, index, snapshots, sqlite];

    // No global instructions of the operator: empty files, written again
    // at each start.
    for name in GLOBAL_INSTRUCTION_FILES {
        write_atomic(&dir.join(name), b"")?;
        overlays.push(file_overlay(
            &dir.join(name),
            &home.join(name),
            "",
            opts.codex_home.join(name),
            false,
        )?);
    }

    // D4: the generated images go to the agent's workspace.
    overlays.push(dir_overlay(
        &images_dir(agent),
        &home.join(IMAGES_DIR),
        opts.codex_home.join(IMAGES_DIR),
    )?);

    // PLAN §8.6: the agent's skills (maybe none) over the user skill dir
    // of the shared home, read-only.
    let skills = terrarium_core::skills::delivered_dir(&agent_dir(agent));
    let mut over_skills = dir_overlay(
        &skills,
        &home.join(opts.flags.skills_dir),
        opts.codex_home.join(opts.flags.skills_dir),
    )?;
    over_skills.writable = false;
    overlays.push(over_skills);

    // No background terminals: the managed requirements, written again at
    // each start. bwrap makes `/etc/codex/` in its own root, so the host
    // needs no mount point.
    let requirements = dir.join(REQUIREMENTS_FILE);
    write_atomic(&requirements, REQUIREMENTS.as_bytes())?;
    overlays.push(FileOverlay {
        source: requirements,
        target: PathBuf::from(SYSTEM_REQUIREMENTS),
        writable: false,
    });
    Ok(overlays)
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
                (
                    own.join("AGENTS.md"),
                    PathBuf::from("/auth/codex/AGENTS.md"),
                    false
                ),
                (
                    own.join("AGENTS.override.md"),
                    PathBuf::from("/auth/codex/AGENTS.override.md"),
                    false
                ),
                (
                    agent.workspace.join("generated_images"),
                    PathBuf::from("/auth/codex/generated_images"),
                    true
                ),
                // PLAN §8.6: the agent's skills, read-only, also with none.
                (
                    agent_dir(&agent).join("skills"),
                    PathBuf::from("/auth/codex/skills"),
                    false
                ),
                (
                    own.join("requirements.toml"),
                    PathBuf::from("/etc/codex/requirements.toml"),
                    false
                ),
            ]
        );
        assert!(home.join("skills").is_dir(), "the mount point");
        // D4: the images dir is in the workspace, made at the start.
        assert!(images_dir(&agent).is_dir());
        assert_eq!(fs::read(own.join("AGENTS.md")).expect("agents"), b"");
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

    /// The managed requirements pin unified exec off, so `exec_command` is
    /// one-shot and no process runs on after a turn. The file is TOML in
    /// the requirements shape (`[features]`, a bool per feature key), and a
    /// start writes it again over an edit.
    #[test]
    fn the_requirements_pin_unified_exec_off() {
        let (_tmp, home, agent) = setup();
        let opts = CodexOptions::default();
        materialize(&opts, &agent, &home, None).expect("mat");
        let path = codex_dir(&agent).join(REQUIREMENTS_FILE);
        fs::write(&path, "[features]\nunified_exec = true\n").expect("edit");
        let overlays = materialize(&opts, &agent, &home, None).expect("again");
        let o = overlays
            .iter()
            .find(|o| o.target == Path::new(SYSTEM_REQUIREMENTS))
            .expect("overlay");
        assert!(!o.writable);
        let t: toml::Table =
            toml::from_str(&fs::read_to_string(&o.source).expect("read")).expect("toml");
        assert_eq!(t.len(), 1);
        assert_eq!(t["features"]["unified_exec"].as_bool(), Some(false));
        assert_eq!(t["features"].as_table().expect("table").len(), 1);
        // Nothing goes into the shared home for it.
        assert!(!home.join(REQUIREMENTS_FILE).exists());
    }

    /// The operator's global `AGENTS.md` stays in the shared home, and the
    /// agent sees an empty file over it.
    #[test]
    fn the_global_agents_md_is_empty_for_the_agent() {
        let (_tmp, home, agent) = setup();
        fs::write(home.join("AGENTS.md"), "operator rules").expect("w");
        let overlays = materialize(&CodexOptions::default(), &agent, &home, None).expect("mat");
        let o = overlays
            .iter()
            .find(|o| o.target == Path::new("/auth/codex/AGENTS.md"))
            .expect("overlay");
        assert_eq!(fs::read(&o.source).expect("own"), b"");
        assert!(!o.writable);
        assert_eq!(
            fs::read_to_string(home.join("AGENTS.md")).expect("shared"),
            "operator rules"
        );
        assert!(home.join("AGENTS.override.md").is_file());
        assert!(home.join("generated_images").is_dir());
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
