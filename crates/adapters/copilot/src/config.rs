//! Per-agent files that the sandbox lays over the shared Copilot home
//! (PLAN §8 and §11, the overlay trick).
//!
//! All agents share one Copilot home (`/auth/copilot`, `COPILOT_HOME`) for
//! the login (`config.json`) and the session files (`session-state/`).
//! Some files in that home are per agent. [`materialize`] writes them
//! under `<agent dir>/copilot/` and returns the overlays.
//!
//! | File in `<agent dir>/copilot/` | Bound over (in `COPILOT_HOME`) | Mode | Why |
//! |---|---|---|---|
//! | `SYSTEM.md` | `SYSTEM.md` | ro | The system-prompt extension reads it. With [`PromptMode::Replace`] it holds the prompt; else it is empty. An empty file counts as absent (the extension trims it). |
//! | `APPEND_SYSTEM.md` | `APPEND_SYSTEM.md` | ro | The same, for [`PromptMode::Append`] (the default). The operator's own prompt files stay out. |
//! | `hooks/` (empty) | `hooks/` | ro | Every `hooks/*.json` in the home runs in every agent (on this host `orca.json`). An empty directory keeps them out. |
//! | `settings.json` | `settings.json` | rw | A copy of the shared file without `hooks` (that route also fires) and `disableAllHooks`, with `"experimental": true` (the extension needs experimental mode; see below). |
//! | `command-history-state.json` | the same | rw | The prompt history (the up-arrow list). Shared, each agent would see the turns (mail text) of the others and of the operator. |
//! | `open-sessions-state.json` | the same | rw | One map that every process rewrites. Per agent, there is no lost update. |
//! | `sidebar-sessions-state/` | the same | rw | Keyed by a hash; every agent has the same cwd, so the agents would share one file. |
//! | `cache/` | `$HOME/.cache/copilot` (when [`CopilotOptions::cache_dir`] is set) | rw | Copilot writes outside its home (`COPILOT_CACHE_HOME`). |
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
//! Shared on purpose: `config.json` (the login and the plain-text token
//! fallback; Copilot also writes `firstLaunchAt` there), `session-state/`
//! (one directory per session UUID; `--resume` and the usage collector read
//! it), `session-store.db` (SQLite in WAL mode), `extensions/` (the
//! system-prompt extension), `pkg/`, `plugin-data/`, `installed-plugins/`.
//! Many processes on one `config.json` and one `session-store.db` is the
//! normal multi-terminal case; it is not tested with many agents.

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

/// The per-agent settings: `base` without the keys that would add or stop
/// hooks, and with `experimental_key` set to `true`. Other keys (model,
/// theme) stay.
pub fn agent_settings(base: Option<Value>, experimental_key: &str) -> Value {
    let mut root = match base {
        Some(Value::Object(m)) => m,
        _ => Map::new(),
    };
    root.remove("hooks");
    root.remove("disableAllHooks");
    root.insert(experimental_key.to_owned(), Value::Bool(true));
    Value::Object(root)
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
    let (system, append) = prompt_files(opts.prompt_mode, &text);

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

    for (name, body) in [(SYSTEM_FILE, system), (APPEND_SYSTEM_FILE, append)] {
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
    write_atomic(
        &own_settings,
        &pretty(&agent_settings(base, opts.flags.experimental_key))?,
    )?;
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

    // Copilot writes its logs (`--log-dir`); the rest of the harness
    // directory is read-only in the sandbox.
    let logs = logs_dir(agent);
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
            ("hooks", false),
            ("settings.json", true),
            ("command-history-state.json", true),
            ("open-sessions-state.json", true),
            ("sidebar-sessions-state", true),
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

        // Auth and sessions stay shared.
        for shared in [
            "config.json",
            "session-state/x/events.jsonl",
            "session-store.db",
        ] {
            let p = inside.join(shared);
            assert!(
                !overlays.iter().any(|o| p.starts_with(&o.target)),
                "{shared} must stay shared"
            );
        }
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
        let s: Value =
            serde_json::from_slice(&fs::read(own.join(SETTINGS_FILE)).expect("own")).expect("json");
        assert_eq!(
            s,
            json!({"model": "claude-haiku-4.5", "experimental": true})
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
        let s: Value =
            serde_json::from_slice(&fs::read(own.join(SETTINGS_FILE)).expect("own")).expect("json");
        assert_eq!(s, json!({"theme": "dark", "experimental": true}));
        assert!(
            fs::read_to_string(own.join(HISTORY_FILE))
                .expect("h")
                .contains("hi")
        );
        assert_eq!(fs::read_dir(own.join(HOOKS_DIR)).expect("hooks").count(), 0);
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
            agent_settings(Some(json!([1])), "experimental"),
            json!({"experimental": true})
        );
        // An operator `false` does not turn the prompt extension off.
        assert_eq!(
            agent_settings(Some(json!({"experimental": false})), "experimental"),
            json!({"experimental": true})
        );
    }
}
