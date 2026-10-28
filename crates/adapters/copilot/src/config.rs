//! Per-agent files that the sandbox lays over the shared Copilot home
//! (PLAN §8 and §11, the overlay trick).
//!
//! All agents share one Copilot home (`/auth/copilot`, `COPILOT_HOME`) for
//! the login (`config.json`) and the extension. Some files in that home
//! are per agent, the session files among them. [`materialize`] writes
//! them under `<agent dir>/copilot/` and returns the overlays.
//!
//! | File in `<agent dir>/copilot/` | Bound over (in `COPILOT_HOME`) | Mode | Why |
//! |---|---|---|---|
//! | `SYSTEM.md` | `SYSTEM.md` | ro | The system-prompt extension reads it. With [`PromptMode::Replace`] it holds the prompt; else it is empty. An empty file counts as absent (the extension trims it). |
//! | `APPEND_SYSTEM.md` | `APPEND_SYSTEM.md` | ro | The same, for [`PromptMode::Append`] (the default). The operator's own prompt files stay out. |
//! | `copilot-instructions.md` (empty) | `copilot-instructions.md` | ro | The operator's global custom instructions stay out of agents (PLAN §8, the AGENTS.md rule). |
//! | `hooks/` (empty) | `hooks/` | ro | Every `hooks/*.json` in the home runs in every agent (on this host `orca.json`). An empty directory keeps them out. |
//! | `settings.json` | `settings.json` | rw | A copy of the shared file without `hooks` (that route also fires) and `disableAllHooks`, with `"experimental": true` (the extension needs experimental mode; see below). |
//! | `command-history-state.json` | the same | rw | The prompt history (the up-arrow list). Shared, each agent would see the turns (mail text) of the others and of the operator. |
//! | `open-sessions-state.json` | the same | rw | One map that every process rewrites. Per agent, there is no lost update. |
//! | `sidebar-sessions-state/` | the same | rw | Keyed by a hash; every agent has the same cwd, so the agents would share one file. |
//! | `session-state/` | the same | rw | The transcripts: `session-state/<id>/events.jsonl`, `workspace.yaml`, `checkpoints/`, `rewind-file-snapshots/`. One directory per session UUID does not collide, but a shared directory lets each agent read, and write into, the transcripts and file snapshots of every other agent in every project (a path for prompt injection). `--resume <id>` finds `session-state/<id>/` in the agent's own directory. The daemon's usage collector reads it from the agent's harness dir. |
//! | `session-store.db`, `session-store.db-wal`, `session-store.db-shm` | the same | rw | SQLite in WAL mode. The `turns` table holds the user message and the reply of every turn, and the model can query it (the `session_store_sql` tool, "Querying past sessions"). Shared, each agent could read the turns of every other agent. All three files are bound, so the database stays with its `-wal` and `-shm`. SQLite cannot delete the `-wal` of a file bind at the close; it ignores that error, and the next open reads the file (verified with `bwrap`, Python `sqlite3`, and `node:sqlite` on 2026-10-25). A start of Copilot on these binds is not tested yet. |
//! | `logs/` | `logs/` | rw | The same directory as `--log-dir`. The system-prompt extension subprocess ignores `--log-dir` and writes `logs/process-<ts>-<pid>.log` in the home (one file for each start; it holds the session ID). With the bind, both logs land in the agent's own `logs/`. |
//! | `cache/` | `$HOME/.cache/copilot` (when [`CopilotOptions::cache_dir`] is set) | rw | Copilot writes outside its home (`COPILOT_CACHE_HOME`). |
//!
//! The skills (PLAN §8.6): `settings.json` gets `skillDirectories` = the
//! agent's delivered skills dir (`<agent dir>/skills/`, read-only at its
//! own path), or none; `ignoredSkillsLocations` = the shared home's
//! `skills/`, `~/.agents/skills`, `~/.claude/skills`, and the
//! workspace's `.github/skills`, `.claude/skills`, `.agents/skills`; and
//! `disabledSkills` holds the built-in skills ([`skill_settings`]). In
//! replace mode the system message has no `<available_skills>` list, so
//! `SYSTEM.md` gets one ([`skills_block`]).
//!
//! Copilot writes `settings.json`, `command-history-state.json`,
//! `open-sessions-state.json`, and `config.json` by a rename over the
//! file (verified in the container on 2026-10-25: the inode changes). A
//! rename over a file bind fails with `EBUSY`. For the history and the
//! open-sessions map, Copilot ignores the error: the per-agent file keeps
//! its seed, and the write is lost. With `--experimental`, Copilot
//! writes `settings.json` at each start, and that `EBUSY` is fatal: exit
//! code 1 after about 0.5 s, with no output and no log. So the adapter
//! does not pass the flag. It writes the key into the per-agent copy, and
//! Copilot does not write the file at start. A later write (`/model`,
//! `/theme`, `/experimental`) is lost in the same way; whether Copilot
//! survives it is not tested.
//!
//! Also in `<agent dir>/copilot/`, at the same path inside the sandbox
//! (the harness directory is bound read-only): `plugin/` (the hooks,
//! `--plugin-dir`), `session-id` (the UUID of the last fresh start; the
//! daemon writes it), and `logs/` (`--log-dir`), which Copilot writes, so
//! it gets a read-write bind over itself.
//!
//! Shared on purpose: `config.json` (trusted folders, flags; Copilot also
//! writes `firstLaunchAt` there). The login token is per account
//! (`terrariumd::accounts`, PLAN §16.3): the daemon moves it out of
//! `authTokens` and gives each agent `COPILOT_GITHUB_TOKEN`, which wins over
//! the file. Also shared: `extensions/`
//! (the system-prompt extension), `pkg/`, `plugin-data/`,
//! `installed-plugins/`. Many processes on one `config.json` is the normal
//! multi-terminal case; it is not tested with many agents. The session
//! files that an older version left in the shared `session-state/` and
//! `session-store.db` stay there; no agent sees them.

use std::fs;
use std::path::{Path, PathBuf};

use serde_json::{Map, Value, json};
use terrarium_core::{Agent, FileOverlay};

use crate::error::{CopilotError, Result};
use crate::{CopilotOptions, PromptMode, launch};

/// The rendered prompt in the agent directory (PLAN §11).
pub const PROMPT_FILE: &str = "prompt.md";
/// The replacement prompt that the extension reads from `COPILOT_HOME`.
pub const SYSTEM_FILE: &str = "SYSTEM.md";
/// The appended prompt that the extension reads from `COPILOT_HOME`.
pub const APPEND_SYSTEM_FILE: &str = "APPEND_SYSTEM.md";
/// The operator's global custom instructions in the home. In append mode
/// Copilot adds them to the system message.
pub const INSTRUCTIONS_FILE: &str = "copilot-instructions.md";
/// The user hook directory in the home.
pub const HOOKS_DIR: &str = "hooks";
/// The user settings in the home. JSON.
pub const SETTINGS_FILE: &str = "settings.json";
/// The prompt history in the home.
pub const HISTORY_FILE: &str = "command-history-state.json";
/// The map of open sessions in the home.
pub const OPEN_SESSIONS_FILE: &str = "open-sessions-state.json";
/// The sidebar state in the home. A directory.
pub const SIDEBAR_DIR: &str = "sidebar-sessions-state";
/// The managed state file in the home (login, trusted folders). JSON with
/// `//` comment lines.
pub const CONFIG_FILE: &str = "config.json";
/// The session files in the home: `session-state/<id>/events.jsonl`.
pub const SESSION_STATE_DIR: &str = "session-state";
/// The log directory in the home. The extension subprocess writes there and
/// ignores `--log-dir`.
pub const LOGS_DIR: &str = "logs";
/// The session store in the home: one SQLite database in WAL mode, as
/// three files.
pub const SESSION_STORE_FILES: &[&str] = &[
    "session-store.db",
    "session-store.db-wal",
    "session-store.db-shm",
];

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

/// `/state/agents/<ulid>/copilot`.
pub fn copilot_dir(agent: &Agent) -> PathBuf {
    agent_dir(agent).join("copilot")
}

/// The Terrarium plugin (`--plugin-dir`): `plugin.json` and `hooks.json`.
pub fn plugin_dir(agent: &Agent) -> PathBuf {
    copilot_dir(agent).join("plugin")
}

/// The log directory (`--log-dir`).
pub fn logs_dir(agent: &Agent) -> PathBuf {
    copilot_dir(agent).join("logs")
}

/// The per-agent cache directory (`COPILOT_CACHE_HOME`), the daemon's view.
pub fn cache_dir(agent: &Agent) -> PathBuf {
    copilot_dir(agent).join("cache")
}

/// The file that holds the UUID of the last fresh start.
pub fn session_id_file(agent: &Agent) -> PathBuf {
    copilot_dir(agent).join("session-id")
}

/// The rendered prompt file of the agent.
pub fn prompt_path(agent: &Agent) -> PathBuf {
    agent_dir(agent).join(PROMPT_FILE)
}

/// Read the rendered prompt. A missing file means "no prompt".
pub fn read_prompt(agent: &Agent) -> Result<Option<String>> {
    let path = prompt_path(agent);
    match fs::read_to_string(&path) {
        Ok(s) => Ok(Some(s)),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(e) => Err(CopilotError::io(path, e)),
    }
}

/// Parse a Copilot JSON file. `config.json` starts with `//` comment
/// lines; those lines are dropped first. `None` when it does not parse.
pub fn parse_jsonc(text: &str) -> Option<Value> {
    let body: String = text
        .lines()
        .filter(|l| !l.trim_start().starts_with("//"))
        .collect::<Vec<_>>()
        .join("\n");
    serde_json::from_str(&body).ok()
}

fn read_jsonc(path: &Path) -> Result<Option<Value>> {
    match fs::read_to_string(path) {
        Ok(s) => Ok(parse_jsonc(&s)),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(e) => Err(CopilotError::io(path, e)),
    }
}

/// The per-agent settings, in layers (PLAN §8.2):
///
/// 1. `base`: the agent's own copy (what Copilot wrote into it), or on the
///    first start the shared `settings.json` of the home.
/// 2. `user`: the user settings file (`Agent::harness_settings`, from
///    `<config repo>/harness/copilot/settings.json`), merged over it with
///    [`terrarium_core::harness_settings::deep_merge`]. A key of the file
///    wins over the copy at each start.
/// 3. The forced keys: `hooks` and `disableAllHooks` go (Copilot would run
///    the `hooks` of this file next to the Terrarium plugin, and
///    `disableAllHooks` stops the plugin hooks), and `experimental_key` is
///    `true` (the system-prompt extension loads only in experimental mode;
///    see `CopilotFlags::experimental`).
///
/// The per-agent options (`--model`) are flags.
pub fn agent_settings(base: Option<Value>, user: Option<&Value>, experimental_key: &str) -> Value {
    let mut merged = match base {
        Some(v @ Value::Object(_)) => v,
        _ => Value::Object(Map::new()),
    };
    if let Some(u @ Value::Object(_)) = user {
        terrarium_core::harness_settings::deep_merge(&mut merged, u);
    }
    let mut root = match merged {
        Value::Object(m) => m,
        _ => Map::new(),
    };
    root.remove("hooks");
    root.remove("disableAllHooks");
    root.insert(experimental_key.to_owned(), Value::Bool(true));
    let mut out = Value::Object(root);
    terrarium_core::harness_settings::deep_merge(&mut out, &forced_settings());
    out
}

/// The settings keys that Terrarium forces in each agent's copy, over the
/// user file (`docs/research/harness-hardening.md`, Copilot CLI, 2c). Each
/// key is in the 1.0.103 settings catalog.
///
/// - D5: `askUser: false` (with the flag `--no-ask-user`) removes the
///   `ask_user` tool; `defaultMode: "interactive"`, `stayInAutopilot: false`
///   keep the agent out of plan mode (its `exit_plan_mode` approval waits
///   for a human) and autopilot.
/// - No dialog: `defaultPermissionMode: "allow-all"` (with `--yolo`),
///   `permissions.disableAssistedPermissionsMode` (an LLM judge that can
///   ask), `shellShortcut: false` (a lone `$` opens a shell), `ide` (an IDE
///   diff approval), `autoUpdate: false`.
/// - No data out of the agent: `remoteExport` (the session export to
///   GitHub, on by default), `remoteSessions` (another client could steer
///   the agent), `memory` (Copilot Memory is shared by every agent of an
///   account), `connectors`.
/// - No sub-agents and no context tools: `builtInAgents` (the rubber-duck
///   agent can start by itself), `contextManagementTools`. The tools go by
///   `--excluded-tools` too ([`crate::flags::CopilotFlags::excluded_tools`]).
pub fn forced_settings() -> Value {
    json!({
        "askUser": false,
        "defaultMode": "interactive",
        "stayInAutopilot": false,
        "defaultPermissionMode": "allow-all",
        "permissions": {"disableAssistedPermissionsMode": true},
        "shellShortcut": false,
        "ide": {"autoConnect": false, "openDiffOnEdit": false},
        "autoUpdate": false,
        "remoteExport": false,
        "remoteSessions": false,
        "memory": false,
        "connectors": false,
        "builtInAgents": {"rubberDuck": false, "rubberDuckAutoInvoke": false},
        "contextManagementTools": false,
    })
}

/// Write `bytes` to `path` through a temporary file and a rename.
pub(crate) fn write_atomic(path: &Path, bytes: &[u8]) -> Result<()> {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent).map_err(|e| CopilotError::io(parent, e))?;
    }
    let tmp = path.with_extension("tmp-terrarium");
    fs::write(&tmp, bytes).map_err(|e| CopilotError::io(&tmp, e))?;
    fs::rename(&tmp, path).map_err(|e| CopilotError::io(path, e))
}

fn pretty(v: &Value) -> Result<Vec<u8>> {
    let mut out =
        serde_json::to_vec_pretty(v).map_err(|e| CopilotError::json("generated JSON", e))?;
    out.push(b'\n');
    Ok(out)
}

/// The skill keys of the agent's `settings.json` (PLAN §8.6), over the
/// merged settings:
///
/// - `skillDirectories`: the delivered skills dir, or none. Absolute;
///   Copilot reads each entry as a root of `<name>/SKILL.md`
///   (settings catalog `{"path":"skillDirectories",...}`).
/// - `ignoredSkillsLocations`: the user's entries, and each discovery
///   root that Terrarium does not manage: `$COPILOT_HOME/skills`
///   (`personal-copilot`), `~/.agents/skills` (`personal-agents`),
///   `~/.claude/skills`, and the workspace's `.github/skills`,
///   `.claude/skills`, `.agents/skills` (`project`). Each entry also
///   drops the dirs under it (`canonicalIgnoredSkillLocations`).
/// - `disabledSkills`: the user's names and the built-in skills.
pub fn skill_settings(opts: &CopilotOptions, agent: &Agent, settings: &mut Value) {
    let Value::Object(root) = settings else {
        return;
    };
    let dirs: Vec<Value> = if agent.delivered_skills.is_empty() {
        Vec::new()
    } else {
        vec![Value::String(path_string(
            &terrarium_core::skills::delivered_dir(&agent_dir(agent)),
        ))]
    };
    root.insert("skillDirectories".into(), Value::Array(dirs));
    let home = opts
        .config_dir
        .parent()
        .map(Path::to_path_buf)
        .unwrap_or_else(|| PathBuf::from("/"));
    let cwd = opts.harness_cwd(agent);
    let ignored = [
        opts.config_dir.join("skills"),
        home.join(".agents/skills"),
        home.join(".claude/skills"),
        cwd.join(".github/skills"),
        cwd.join(".claude/skills"),
        cwd.join(".agents/skills"),
    ];
    union_strings(
        root,
        "ignoredSkillsLocations",
        ignored.iter().map(|p| path_string(p)),
    );
    union_strings(
        root,
        "disabledSkills",
        opts.flags.builtin_skills.iter().map(|s| (*s).to_owned()),
    );
}

fn path_string(p: &Path) -> String {
    p.to_string_lossy().into_owned()
}

/// `root[key]` as a list of strings with each of `add` in it: the user's
/// entries first, then the ones that were missing.
fn union_strings(root: &mut Map<String, Value>, key: &str, add: impl Iterator<Item = String>) {
    let mut list: Vec<Value> = match root.remove(key) {
        Some(Value::Array(a)) => a.into_iter().filter(Value::is_string).collect(),
        _ => Vec::new(),
    };
    for s in add {
        if !list.iter().any(|v| v.as_str() == Some(s.as_str())) {
            list.push(Value::String(s));
        }
    }
    root.insert(key.to_owned(), Value::Array(list));
}

/// The skill list for `SYSTEM.md` in replace mode (PLAN §8.6): Copilot
/// puts `<available_skills>` in the system message, which `SYSTEM.md`
/// replaces. Empty when the agent has no skill.
pub fn skills_block(agent: &Agent) -> String {
    if agent.delivered_skills.is_empty() {
        return String::new();
    }
    let dir = terrarium_core::skills::delivered_dir(&agent_dir(agent));
    let mut out = String::from(
        "<available_skills>\nLoad a skill with the `skill` tool (its name only) before you \
         act on a task that it fits.\n",
    );
    for m in terrarium_core::skills::describe(&dir, &agent.delivered_skills) {
        out.push_str(&format!(
            "<skill>\n<name>{}</name>\n<description>{}</description>\n</skill>\n",
            m.name,
            m.description.replace('<', "&lt;").replace('>', "&gt;")
        ));
    }
    out.push_str("</available_skills>\n");
    out
}

/// The text of the two prompt files for `prompt`: (`SYSTEM.md`,
/// `APPEND_SYSTEM.md`).
pub fn prompt_files(mode: PromptMode, prompt: &str) -> (String, String) {
    match mode {
        PromptMode::Append => (String::new(), prompt.to_owned()),
        PromptMode::Replace => (prompt.to_owned(), String::new()),
    }
}

/// Write the per-agent files and return the overlays (see the table at the
/// top).
///
/// - `home` is the shared Copilot home on the host (the daemon's view).
/// - `prompt`: when `Some`, write it to `prompt.md`. When `None`, use the
///   `prompt.md` that is there (the daemon renders it). No file: both
///   prompt files are empty, and the extension does not load (it throws
///   "Create ~/.copilot/SYSTEM.md ..."). What Copilot does then is not
///   tested.
///
/// A mount point that is missing in the shared home is made first (an
/// empty file, `{}`, or a directory): otherwise bwrap makes it and leaves
/// it behind. Files that the agent changed (settings, history) stay on a
/// restart; the prompt files are written again each time.
pub fn materialize(
    opts: &CopilotOptions,
    agent: &Agent,
    home: &Path,
    prompt: Option<&str>,
) -> Result<Vec<FileOverlay>> {
    let dir = copilot_dir(agent);
    fs::create_dir_all(&dir).map_err(|e| CopilotError::io(&dir, e))?;
    if let Some(text) = prompt {
        write_atomic(&prompt_path(agent), text.as_bytes())?;
    }
    let text = read_prompt(agent)?.unwrap_or_default();
    let mode = opts.prompt_mode_for(agent);
    let (mut system, append) = prompt_files(mode, &text);
    let block = skills_block(agent);
    if mode == PromptMode::Replace && !block.is_empty() {
        system = format!("{}\n\n{block}", system.trim_end());
    }

    // The Terrarium plugin and the agent's own directories.
    let plugin = plugin_dir(agent);
    write_atomic(
        &plugin.join("plugin.json"),
        &pretty(&launch::plugin_manifest())?,
    )?;
    write_atomic(
        &plugin.join("hooks.json"),
        &pretty(&launch::hooks_json(opts))?,
    )?;
    for d in [logs_dir(agent), cache_dir(agent)] {
        fs::create_dir_all(&d).map_err(|e| CopilotError::io(&d, e))?;
    }

    let target = |rel: &str| opts.config_dir.join(rel);
    let mut overlays = Vec::new();

    // The operator's global custom instructions stay out (PLAN §8, the
    // AGENTS.md rule): an empty file over the shared one.
    for (name, body) in [
        (SYSTEM_FILE, system),
        (APPEND_SYSTEM_FILE, append),
        (INSTRUCTIONS_FILE, String::new()),
    ] {
        let own = dir.join(name);
        write_atomic(&own, body.as_bytes())?;
        ensure_file(&home.join(name), b"")?;
        overlays.push(FileOverlay {
            source: own,
            target: target(name),
            writable: false,
        });
    }

    // An empty hook directory. Clear it, in case an old version wrote to it.
    let own_hooks = dir.join(HOOKS_DIR);
    if own_hooks.is_dir() {
        fs::remove_dir_all(&own_hooks).map_err(|e| CopilotError::io(&own_hooks, e))?;
    }
    overlays.push(dir_overlay(
        &own_hooks,
        &home.join(HOOKS_DIR),
        target(HOOKS_DIR),
        false,
    )?);

    let own_settings = dir.join(SETTINGS_FILE);
    let base = match read_jsonc(&own_settings)? {
        Some(v) => Some(v),
        None => read_jsonc(&home.join(SETTINGS_FILE))?,
    };
    let mut settings = agent_settings(
        base,
        agent.harness_settings.as_ref(),
        opts.flags.experimental_key,
    );
    skill_settings(opts, agent, &mut settings);
    write_atomic(&own_settings, &pretty(&settings)?)?;
    ensure_file(&home.join(SETTINGS_FILE), b"{}\n")?;
    overlays.push(FileOverlay {
        source: own_settings,
        target: target(SETTINGS_FILE),
        writable: true,
    });

    let empty_history = pretty(&json!({"commandHistory": []}))?;
    for (name, seed) in [
        (HISTORY_FILE, empty_history.as_slice()),
        (OPEN_SESSIONS_FILE, b"{}\n".as_slice()),
    ] {
        let own = dir.join(name);
        ensure_file(&own, seed)?;
        ensure_file(&home.join(name), seed)?;
        overlays.push(FileOverlay {
            source: own,
            target: target(name),
            writable: true,
        });
    }

    overlays.push(dir_overlay(
        &dir.join(SIDEBAR_DIR),
        &home.join(SIDEBAR_DIR),
        target(SIDEBAR_DIR),
        true,
    )?);

    // The agent's own transcripts and session store (see the table at the
    // top). A new agent starts with none: an empty file is a new SQLite
    // database.
    overlays.push(dir_overlay(
        &dir.join(SESSION_STATE_DIR),
        &home.join(SESSION_STATE_DIR),
        target(SESSION_STATE_DIR),
        true,
    )?);
    for name in SESSION_STORE_FILES {
        let own = dir.join(name);
        ensure_file(&own, b"")?;
        ensure_file(&home.join(name), b"")?;
        overlays.push(FileOverlay {
            source: own,
            target: target(name),
            writable: true,
        });
    }

    // The extension subprocess writes its log to `$COPILOT_HOME/logs/` and
    // ignores `--log-dir`. Bind the agent's own log directory there too.
    let logs = logs_dir(agent);
    overlays.push(dir_overlay(
        &logs,
        &home.join(LOGS_DIR),
        target(LOGS_DIR),
        true,
    )?);

    // Copilot writes its logs (`--log-dir`); the rest of the harness
    // directory is read-only in the sandbox.
    overlays.push(FileOverlay {
        source: logs.clone(),
        target: logs,
        writable: true,
    });

    if let Some(inside) = &opts.cache_dir {
        overlays.push(FileOverlay {
            source: cache_dir(agent),
            target: inside.clone(),
            writable: true,
        });
    }
    Ok(overlays)
}

/// Make `path` with `seed` when it does not exist. A file that is there
/// keeps its content.
fn ensure_file(path: &Path, seed: &[u8]) -> Result<()> {
    if path.exists() {
        return Ok(());
    }
    write_atomic(path, seed)
}

/// A per-agent directory over `target`. Both the agent's directory and the
/// mount point in the shared home (`shared`) must exist.
fn dir_overlay(own: &Path, shared: &Path, target: PathBuf, writable: bool) -> Result<FileOverlay> {
    for d in [own, shared] {
        fs::create_dir_all(d).map_err(|e| CopilotError::io(d, e))?;
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

    /// `v` with the forced keys on top.
    fn with_forced(mut v: Value) -> Value {
        terrarium_core::harness_settings::deep_merge(&mut v, &forced_settings());
        v
    }

    /// The written settings without the skill keys ([`skill_settings`],
    /// tested on its own).
    fn read_settings(path: &Path) -> Value {
        let mut v: Value = serde_json::from_slice(&fs::read(path).expect("read")).expect("json");
        if let Value::Object(m) = &mut v {
            for k in [
                "skillDirectories",
                "ignoredSkillsLocations",
                "disabledSkills",
            ] {
                assert!(m.remove(k).is_some(), "{k} is set");
            }
        }
        v
    }

    /// PLAN §8.6: the skill keys of the agent's settings, and the skill
    /// list in `SYSTEM.md` in replace mode.
    #[test]
    fn skills_go_to_the_settings_and_the_replace_prompt() {
        let tmp = tempfile::tempdir().expect("tmp");
        let home = tmp.path().join("auth-copilot");
        fs::create_dir_all(&home).expect("home");
        let mut agent = test_agent_in(tmp.path(), None);
        agent.harness_settings = Some(json!({
            "disabledSkills": ["mine"],
            "ignoredSkillsLocations": ["/x"],
            "skillDirectories": ["/operator/skills"],
        }));
        let opts = CopilotOptions {
            config_dir: PathBuf::from("/home/agent/.copilot"),
            harness_workspace: Some(PathBuf::from("/home/agent/work")),
            ..CopilotOptions::default()
        };
        materialize(&opts, &agent, &home, Some("P")).expect("m");
        let own = copilot_dir(&agent);
        let v: Value =
            serde_json::from_slice(&fs::read(own.join(SETTINGS_FILE)).expect("r")).expect("json");
        // No skill: no skill dir, every other root ignored, built-ins off.
        assert_eq!(v["skillDirectories"], json!([]));
        assert_eq!(
            v["ignoredSkillsLocations"],
            json!([
                "/x",
                "/home/agent/.copilot/skills",
                "/home/agent/.agents/skills",
                "/home/agent/.claude/skills",
                "/home/agent/work/.github/skills",
                "/home/agent/work/.claude/skills",
                "/home/agent/work/.agents/skills",
            ])
        );
        assert_eq!(
            v["disabledSkills"],
            json!([
                "mine",
                "customize-cloud-agent",
                "github-pr-media",
                "discover-resources"
            ])
        );
        // With a skill, in replace mode.
        let skills = terrarium_core::skills::delivered_dir(&agent_dir(&agent));
        fs::create_dir_all(skills.join("fern-check")).expect("mkdir");
        fs::write(
            skills.join("fern-check/SKILL.md"),
            "---\nname: fern-check\ndescription: Reply <FERN-42>\n---\nbody\n",
        )
        .expect("w");
        agent.delivered_skills = vec!["fern-check".into()];
        agent.system_prompt = Some("Base.".into());
        materialize(&opts, &agent, &home, Some("Base.\n\nP")).expect("m");
        let v: Value =
            serde_json::from_slice(&fs::read(own.join(SETTINGS_FILE)).expect("r")).expect("json");
        assert_eq!(v["skillDirectories"], json!([skills.to_string_lossy()]));
        let system = fs::read_to_string(own.join(SYSTEM_FILE)).expect("s");
        assert!(
            system.starts_with("Base.\n\nP\n\n<available_skills>"),
            "{system}"
        );
        assert!(
            system.contains(
                "<name>fern-check</name>\n<description>Reply &lt;FERN-42&gt;</description>"
            ),
            "{system}"
        );
        // Append mode keeps Copilot's own list: no block.
        agent.system_prompt = None;
        materialize(&opts, &agent, &home, Some("P")).expect("m");
        assert_eq!(fs::read_to_string(own.join(SYSTEM_FILE)).expect("s"), "");
        assert_eq!(
            fs::read_to_string(own.join(APPEND_SYSTEM_FILE)).expect("a"),
            "P"
        );
    }

    /// D5: a user file cannot turn `ask_user`, plan mode, or autopilot on
    /// again; a tweakable key passes.
    #[test]
    fn a_user_file_cannot_turn_the_asking_tool_on() {
        let user = json!({
            "askUser": true,
            "defaultMode": "plan",
            "stayInAutopilot": true,
            "remoteExport": true,
            "permissions": {"disableAssistedPermissionsMode": false, "other": 1},
            "banner": "always",
        });
        let v = agent_settings(None, Some(&user), "experimental");
        assert_eq!(v["askUser"], false);
        assert_eq!(v["defaultMode"], "interactive");
        assert_eq!(v["stayInAutopilot"], false);
        assert_eq!(v["remoteExport"], false);
        assert_eq!(
            v["permissions"],
            json!({"disableAssistedPermissionsMode": true, "other": 1})
        );
        assert_eq!(v["banner"], "always");
    }

    /// A user file cannot turn the sub-agents or the context tools on.
    #[test]
    fn a_user_file_cannot_turn_sub_agents_or_context_tools_on() {
        let user = json!({
            "builtInAgents": {"rubberDuck": true, "rubberDuckAutoInvoke": true},
            "contextManagementTools": true,
            "subagents": {"disabledSubagents": []},
        });
        let v = agent_settings(None, Some(&user), "experimental");
        assert_eq!(
            v["builtInAgents"],
            json!({"rubberDuck": false, "rubberDuckAutoInvoke": false})
        );
        assert_eq!(v["contextManagementTools"], false);
        // A tweakable key passes; `task` is hidden by the flag anyway.
        assert_eq!(v["subagents"]["disabledSubagents"], json!([]));
    }

    /// The packaged default holds no forced key.
    #[test]
    fn the_packaged_default_has_no_forced_key() {
        let path = Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("../../../defaults/harness/copilot/settings.json");
        let v = terrarium_core::harness_settings::load(&path, terrarium_core::HarnessId::Copilot)
            .expect("parses")
            .expect("exists");
        let forced = forced_settings();
        for key in v.as_object().expect("object").keys() {
            assert!(forced.get(key).is_none(), "{key} is forced");
            assert!(!["hooks", "disableAllHooks", "experimental"].contains(&key.as_str()));
        }
        assert_eq!(v["imageGeneration"], false);
    }
    use crate::test_agent_in;

    fn setup() -> (tempfile::TempDir, PathBuf, Agent) {
        let tmp = tempfile::tempdir().expect("tmp");
        let home = tmp.path().join("auth-copilot");
        fs::create_dir_all(home.join("hooks")).expect("home");
        fs::write(home.join("hooks/orca.json"), "{}").expect("user hook");
        fs::write(
            home.join(SETTINGS_FILE),
            r#"{"model": "claude-haiku-4.5", "hooks": {"Stop": []}, "disableAllHooks": true}"#,
        )
        .expect("settings");
        fs::write(home.join(APPEND_SYSTEM_FILE), "operator prompt").expect("append");
        let agent = test_agent_in(tmp.path(), None);
        (tmp, home, agent)
    }

    #[test]
    fn overlay_list() {
        let (_tmp, home, agent) = setup();
        let opts = CopilotOptions {
            config_dir: PathBuf::from("/home/agent/.copilot"),
            cache_dir: Some(PathBuf::from("/home/agent/.cache/copilot")),
            ..CopilotOptions::default()
        };
        let overlays = materialize(&opts, &agent, &home, Some("You are agent a.")).expect("ok");
        let own = copilot_dir(&agent);
        let got: Vec<(PathBuf, PathBuf, bool)> = overlays
            .iter()
            .map(|o| (o.source.clone(), o.target.clone(), o.writable))
            .collect();
        let inside = Path::new("/home/agent/.copilot");
        let mut want: Vec<(PathBuf, PathBuf, bool)> = [
            ("SYSTEM.md", false),
            ("APPEND_SYSTEM.md", false),
            ("copilot-instructions.md", false),
            ("hooks", false),
            ("settings.json", true),
            ("command-history-state.json", true),
            ("open-sessions-state.json", true),
            ("sidebar-sessions-state", true),
            ("session-state", true),
            ("session-store.db", true),
            ("session-store.db-wal", true),
            ("session-store.db-shm", true),
            ("logs", true),
        ]
        .iter()
        .map(|(n, w)| (own.join(n), inside.join(n), *w))
        .collect();
        want.push((own.join("logs"), own.join("logs"), true));
        want.push((
            own.join("cache"),
            PathBuf::from("/home/agent/.cache/copilot"),
            true,
        ));
        assert_eq!(got, want);

        // Each source and each mount point in the shared home exists, with
        // the same kind.
        for o in overlays.iter().filter(|o| o.target.starts_with(inside)) {
            let rel = o.target.strip_prefix(inside).expect("in home");
            let shared = home.join(rel);
            assert!(o.source.exists(), "{}", o.source.display());
            assert!(shared.exists(), "{}", shared.display());
            assert_eq!(o.source.is_dir(), shared.is_dir(), "{}", rel.display());
        }

        // The login and the extension stay shared.
        for shared in ["config.json", "extensions/x/extension.mjs", "pkg/x"] {
            let p = inside.join(shared);
            assert!(
                !overlays.iter().any(|o| p.starts_with(&o.target)),
                "{shared} must stay shared"
            );
        }
        // The transcripts and the session store are the agent's own.
        for own_path in [
            "session-state/0b5c1d2e-0000-4000-8000-000000000001/events.jsonl",
            "session-state/0b5c1d2e-0000-4000-8000-000000000001/rewind-file-snapshots/x",
            "session-store.db",
            "session-store.db-wal",
            "session-store.db-shm",
        ] {
            let p = inside.join(own_path);
            let o = overlays
                .iter()
                .find(|o| p.starts_with(&o.target))
                .unwrap_or_else(|| panic!("{own_path} must be per agent"));
            assert!(o.source.starts_with(&own), "{own_path}");
        }
        // A new session store starts empty, not as a copy of the shared one.
        assert_eq!(fs::read(own.join("session-store.db")).expect("db"), b"");
    }

    #[test]
    fn prompt_hooks_and_settings() {
        let (_tmp, home, agent) = setup();
        let opts = CopilotOptions::default();
        materialize(&opts, &agent, &home, Some("You are agent a.")).expect("ok");
        let own = copilot_dir(&agent);
        assert_eq!(fs::read_to_string(own.join(SYSTEM_FILE)).expect("s"), "");
        assert_eq!(
            fs::read_to_string(own.join(APPEND_SYSTEM_FILE)).expect("a"),
            "You are agent a."
        );
        // The operator's prompt and hooks stay out, and stay as they were.
        assert_eq!(
            fs::read_to_string(home.join(APPEND_SYSTEM_FILE)).expect("shared"),
            "operator prompt"
        );
        assert_eq!(fs::read_dir(own.join(HOOKS_DIR)).expect("hooks").count(), 0);
        assert!(home.join("hooks/orca.json").is_file());
        let s = read_settings(&own.join(SETTINGS_FILE));
        assert_eq!(
            s,
            with_forced(json!({"model": "claude-haiku-4.5", "experimental": true}))
        );
        // The plugin.
        let hooks: Value =
            serde_json::from_slice(&fs::read(plugin_dir(&agent).join("hooks.json")).expect("h"))
                .expect("json");
        assert_eq!(hooks["hooks"]["Stop"][0]["bash"], "terrarium hook Stop");
        assert!(plugin_dir(&agent).join("plugin.json").is_file());
        assert!(logs_dir(&agent).is_dir() && cache_dir(&agent).is_dir());
        // The default has no cache overlay: COPILOT_CACHE_HOME points at the
        // agent directory, which the sandbox binds at the same path.
        let overlays = materialize(&opts, &agent, &home, None).expect("again");
        assert!(overlays.iter().all(|o| o.source != cache_dir(&agent)));
    }

    #[test]
    fn replace_mode_and_restart() {
        let (_tmp, home, agent) = setup();
        let opts = CopilotOptions {
            prompt_mode: PromptMode::Replace,
            ..CopilotOptions::default()
        };
        materialize(&opts, &agent, &home, Some("P1")).expect("ok");
        let own = copilot_dir(&agent);
        assert_eq!(fs::read_to_string(own.join(SYSTEM_FILE)).expect("s"), "P1");
        assert_eq!(
            fs::read_to_string(own.join(APPEND_SYSTEM_FILE)).expect("a"),
            ""
        );

        // The agent changed its settings and history; a stray hook appeared.
        fs::write(own.join(SETTINGS_FILE), r#"{"theme": "dark", "hooks": {}}"#).expect("w");
        fs::write(own.join(HISTORY_FILE), r#"{"commandHistory": ["hi"]}"#).expect("w");
        fs::write(own.join("hooks/x.json"), "{}").expect("w");
        // The daemon rendered a new prompt.
        fs::write(prompt_path(&agent), "P2").expect("w");
        materialize(&opts, &agent, &home, None).expect("again");
        assert_eq!(fs::read_to_string(own.join(SYSTEM_FILE)).expect("s"), "P2");
        let s = read_settings(&own.join(SETTINGS_FILE));
        assert_eq!(
            s,
            with_forced(json!({"theme": "dark", "experimental": true}))
        );
        assert!(
            fs::read_to_string(own.join(HISTORY_FILE))
                .expect("h")
                .contains("hi")
        );
        assert_eq!(fs::read_dir(own.join(HOOKS_DIR)).expect("hooks").count(), 0);
    }

    /// The CopilotCustomSystemPrompt extension: with no base system
    /// prompt, `APPEND_SYSTEM.md` holds the Terrarium prompt and
    /// `SYSTEM.md` is empty (the extension treats an empty file as absent).
    /// With one, `SYSTEM.md` holds the composed text and `APPEND_SYSTEM.md`
    /// is empty (PLAN §8).
    #[test]
    fn a_base_system_prompt_writes_system_md() {
        let (_tmp, home, mut agent) = setup();
        let own = copilot_dir(&agent);
        let opts = CopilotOptions::default();
        materialize(&opts, &agent, &home, Some("# Terrarium agent a")).expect("ok");
        assert_eq!(fs::read_to_string(own.join(SYSTEM_FILE)).expect("s"), "");
        assert_eq!(
            fs::read_to_string(own.join(APPEND_SYSTEM_FILE)).expect("a"),
            "# Terrarium agent a"
        );
        agent.system_prompt = Some("You review code.".into());
        let text = "You review code.\n\n# Terrarium agent a";
        materialize(&opts, &agent, &home, Some(text)).expect("ok");
        assert_eq!(fs::read_to_string(own.join(SYSTEM_FILE)).expect("s"), text);
        assert_eq!(
            fs::read_to_string(own.join(APPEND_SYSTEM_FILE)).expect("a"),
            ""
        );
    }

    #[test]
    fn empty_home_gets_mount_points() {
        let tmp = tempfile::tempdir().expect("tmp");
        let home = tmp.path().join("auth-copilot");
        fs::create_dir_all(&home).expect("home");
        let agent = test_agent_in(tmp.path(), None);
        materialize(&CopilotOptions::default(), &agent, &home, None).expect("ok");
        assert_eq!(
            fs::read_to_string(home.join(SETTINGS_FILE)).expect("s"),
            "{}\n"
        );
        assert_eq!(fs::read_to_string(home.join(SYSTEM_FILE)).expect("s"), "");
        assert!(home.join(HOOKS_DIR).is_dir() && home.join(SIDEBAR_DIR).is_dir());
        let h: Value =
            serde_json::from_slice(&fs::read(home.join(HISTORY_FILE)).expect("h")).expect("json");
        assert_eq!(h, json!({"commandHistory": []}));
        assert_eq!(read_prompt(&agent).expect("prompt"), None);
    }

    #[test]
    fn jsonc_comments_are_dropped() {
        let v = parse_jsonc(
            "// User settings belong in settings.json.\n// This file is managed automatically.\n{\n  \"a\": 1\n}\n",
        );
        assert_eq!(v, Some(json!({"a": 1})));
        assert_eq!(parse_jsonc("not json"), None);
        assert_eq!(
            agent_settings(Some(json!([1])), None, "experimental"),
            with_forced(json!({"experimental": true}))
        );
        // An operator `false` does not turn the prompt extension off.
        assert_eq!(
            agent_settings(Some(json!({"experimental": false})), None, "experimental"),
            with_forced(json!({"experimental": true}))
        );
    }

    /// The user file merges over the agent's copy; the forced keys win.
    #[test]
    fn user_settings_merge_under_the_forced_keys() {
        let base = json!({"model": "gpt-x", "theme": "light", "banner": {"a": 1}});
        let user = json!({
            "theme": "dark",
            "banner": {"b": 2},
            "experimental": false,
            "disableAllHooks": true,
            "hooks": {"Stop": []},
        });
        assert_eq!(
            agent_settings(Some(base), Some(&user), "experimental"),
            with_forced(
                json!({"model": "gpt-x", "theme": "dark", "banner": {"a": 1, "b": 2},
                   "experimental": true})
            )
        );
    }

    #[test]
    fn materialize_writes_the_user_settings_into_the_copy() {
        let tmp = tempfile::tempdir().expect("tmp");
        let home = tmp.path().join("auth-copilot");
        fs::create_dir_all(&home).expect("home");
        let mut agent = test_agent_in(tmp.path(), None);
        agent.harness_settings = Some(json!({"theme": "dark", "hooks": {}}));
        materialize(&CopilotOptions::default(), &agent, &home, Some("p")).expect("m");
        let v = read_settings(&copilot_dir(&agent).join(SETTINGS_FILE));
        assert_eq!(
            v,
            with_forced(json!({"theme": "dark", "experimental": true}))
        );
    }
}
