//! Per-agent files that the sandbox lays over the shared Grok home
//! (PLAN §8 and §11, the overlay trick).
//!
//! All grok agents share one Grok home (`/auth/grok` on the host,
//! `$HOME/.grok` in the sandbox, `GROK_HOME`). The login is per account:
//! the daemon binds the account's `auth.json`, and only that file, over
//! `auth.json` in the home (`terrariumd::accounts`, TER-1191). The files
//! below are per agent, under `<agent dir>/grok/`:
//!
//! | File in `<agent dir>/grok/` | Bound over (in the Grok home) | Mode | Why |
//! |---|---|---|---|
//! | `hooks/terrarium.json` (in `hooks/`) | `hooks/` | ro | Terrarium's status bridge as PERSONAL hooks ([`hooks_json`]). Personal hooks need no trust: no `--trust` and no `/hooks-trust`. The whole directory is bound, so the operator's own personal hooks stay out of the agents. |
//! | `sessions/` | `sessions/` | rw | The sessions (`~/.grok/sessions`, shared by the TUI, headless, and ACP). Shared, each agent could read and resume the sessions of every other agent. |
//!
//! Also in `<agent dir>/grok/`: `session-id`, the UUID of the last fresh
//! start (the daemon writes it).
//!
//! Shared on purpose: `config.toml` and `trusted_folders.toml`. Grok also
//! reads Claude Code hook files (`.claude/settings.json`) and Cursor ones
//! from the project; project hooks run only after a trust decision, which
//! the adapter never gives.
//!
//! In sandbox mode `none` the overlays are not bound, so the shared home's
//! own `hooks/` counts, and no Terrarium hook fires: the runner reads the
//! screen alone. Run grok agents in the bwrap sandbox.

use std::fs;
use std::path::{Path, PathBuf};

use serde_json::{Map, Value, json};
use terrarium_core::{Agent, FileOverlay};

use crate::GrokOptions;

/// The rendered prompt in the agent directory (PLAN §11).
pub const PROMPT_FILE: &str = "prompt.md";
/// The personal hook directory in the Grok home.
pub const HOOKS_DIR: &str = "hooks";
/// Terrarium's hook file in [`HOOKS_DIR`].
pub const HOOKS_FILE: &str = "terrarium.json";
/// The session directory in the Grok home.
pub const SESSIONS_DIR: &str = "sessions";
/// The name of the harness directory in the agent directory.
pub const HARNESS_DIR: &str = "harness";

fn io(path: &Path, e: std::io::Error) -> terrarium_core::Error {
    terrarium_core::Error::Io {
        path: path.to_path_buf(),
        source: e,
    }
}

/// `/state/agents/<ulid>/harness`: the per-agent harness files. The
/// sandbox binds it read-only at the same path.
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

/// `<agent dir>/grok`.
pub fn grok_dir(agent: &Agent) -> PathBuf {
    agent_dir(agent).join("grok")
}

/// `<agent dir>/grok/hooks`.
pub fn hooks_dir(agent: &Agent) -> PathBuf {
    grok_dir(agent).join(HOOKS_DIR)
}

/// `<agent dir>/grok/sessions`.
pub fn sessions_dir(agent: &Agent) -> PathBuf {
    grok_dir(agent).join(SESSIONS_DIR)
}

/// The file that holds the UUID of the last fresh start.
pub fn session_id_file(agent: &Agent) -> PathBuf {
    grok_dir(agent).join("session-id")
}

/// The rendered prompt file of the agent.
pub fn prompt_path(agent: &Agent) -> PathBuf {
    agent_dir(agent).join(PROMPT_FILE)
}

/// Read the rendered prompt. A missing file means "no prompt".
pub fn read_prompt(agent: &Agent) -> terrarium_core::Result<Option<String>> {
    let path = prompt_path(agent);
    match fs::read_to_string(&path) {
        Ok(s) => Ok(Some(s)),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(e) => Err(io(&path, e)),
    }
}

/// Write `bytes` to `path` by a rename.
pub fn write_atomic(path: &Path, bytes: &[u8]) -> terrarium_core::Result<()> {
    if let Some(dir) = path.parent() {
        fs::create_dir_all(dir).map_err(|e| io(dir, e))?;
    }
    let tmp = path.with_extension("tmp-terrarium");
    fs::write(&tmp, bytes).map_err(|e| io(&tmp, e))?;
    fs::rename(&tmp, path).map_err(|e| io(path, e))
}

/// Terrarium's personal hook file: each event of
/// [`crate::GrokFlags::hook_events`] runs `<hook_program> hook <Name>`,
/// which forwards the stdin JSON to the runner socket
/// (`TERRARIUM_SOCKET`). No matcher: every tool. The hook prints nothing
/// on stdout and exits 0, so a `PreToolUse` never denies a tool.
pub fn hooks_json(opts: &GrokOptions) -> Value {
    let mut hooks = Map::new();
    for name in opts.flags.hook_events {
        hooks.insert(
            (*name).to_owned(),
            json!([{
                "hooks": [{
                    "type": "command",
                    "command": format!("{} hook {name}", opts.hook_program),
                    "timeout": opts.flags.hook_timeout_sec,
                }],
            }]),
        );
    }
    json!({"hooks": Value::Object(hooks)})
}

/// Write the per-agent files and return the overlays.
///
/// - `home` is the shared Grok home on the host (the daemon's view).
/// - `prompt`: when `Some`, write it to `prompt.md`. When `None`, keep the
///   file that is there (the daemon renders it).
///
/// Each overlay target gets a mount point in the shared home when it has
/// none: otherwise bwrap makes one and leaves it behind.
pub fn materialize(
    opts: &GrokOptions,
    agent: &Agent,
    home: &Path,
    prompt: Option<&str>,
) -> terrarium_core::Result<Vec<FileOverlay>> {
    if let Some(text) = prompt {
        write_atomic(&prompt_path(agent), text.as_bytes())?;
    }
    let hooks = hooks_dir(agent);
    let body = serde_json::to_vec_pretty(&hooks_json(opts))
        .map_err(|e| terrarium_core::Error::Other(format!("grok hooks: {e}")))?;
    write_atomic(&hooks.join(HOOKS_FILE), &body)?;
    let sessions = sessions_dir(agent);
    fs::create_dir_all(&sessions).map_err(|e| io(&sessions, e))?;
    for dir in [HOOKS_DIR, SESSIONS_DIR] {
        let mount = home.join(dir);
        fs::create_dir_all(&mount).map_err(|e| io(&mount, e))?;
    }
    Ok(vec![
        FileOverlay {
            source: hooks,
            target: opts.home_dir.join(HOOKS_DIR),
            writable: false,
        },
        FileOverlay {
            source: sessions,
            target: opts.home_dir.join(SESSIONS_DIR),
            writable: true,
        },
    ])
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_agent_in;

    #[test]
    fn the_hook_file_runs_the_bridge_for_each_event() {
        let opts = GrokOptions {
            hook_program: "/opt/terrarium/bin/terrarium".into(),
            ..GrokOptions::default()
        };
        let v = hooks_json(&opts);
        let hooks = v["hooks"].as_object().expect("hooks");
        assert_eq!(hooks.len(), opts.flags.hook_events.len());
        for name in opts.flags.hook_events {
            let entry = &hooks[*name][0];
            assert!(entry.get("matcher").is_none(), "{name}: every tool");
            let h = &entry["hooks"][0];
            assert_eq!(h["type"], "command");
            assert_eq!(
                h["command"],
                format!("/opt/terrarium/bin/terrarium hook {name}")
            );
            assert_eq!(h["timeout"], 10);
        }
        // The status events are there.
        for name in ["SessionStart", "UserPromptSubmit", "Stop", "StopFailure"] {
            assert!(hooks.contains_key(name), "{name}");
        }
    }

    #[test]
    fn materialize_writes_the_hooks_and_the_overlays() {
        let tmp = tempfile::tempdir().expect("tmp");
        let agent = test_agent_in(tmp.path(), None);
        let home = tmp.path().join("auth-grok");
        let opts = GrokOptions {
            home_dir: PathBuf::from("/home/agent/.grok"),
            host_home_dir: Some(home.clone()),
            ..GrokOptions::default()
        };
        let overlays = materialize(&opts, &agent, &home, Some("be brief")).expect("materialize");
        assert_eq!(
            fs::read_to_string(prompt_path(&agent)).expect("prompt"),
            "be brief"
        );
        let file = hooks_dir(&agent).join(HOOKS_FILE);
        let v: Value =
            serde_json::from_str(&fs::read_to_string(&file).expect("hooks")).expect("json");
        assert_eq!(v, hooks_json(&opts));
        assert_eq!(
            overlays,
            [
                FileOverlay {
                    source: hooks_dir(&agent),
                    target: PathBuf::from("/home/agent/.grok/hooks"),
                    writable: false,
                },
                FileOverlay {
                    source: sessions_dir(&agent),
                    target: PathBuf::from("/home/agent/.grok/sessions"),
                    writable: true,
                },
            ]
        );
        // The mount points, and no login file: the account binds that.
        assert!(home.join(HOOKS_DIR).is_dir());
        assert!(home.join(SESSIONS_DIR).is_dir());
        assert!(!home.join("auth.json").exists());
        // `None` keeps the prompt that is there.
        materialize(&opts, &agent, &home, None).expect("again");
        assert_eq!(
            fs::read_to_string(prompt_path(&agent)).expect("prompt"),
            "be brief"
        );
    }
}
