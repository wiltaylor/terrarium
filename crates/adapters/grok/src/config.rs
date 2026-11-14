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
//! | `hooks/terrarium.json` (in `hooks/`) | `hooks/` | ro | Terrarium's status bridge as PERSONAL hooks ([`hooks_json`]). In grok 1.0.77 an untrusted folder holds back personal hooks too, and with a personal hook installed grok asks the folder trust question in every folder (docs 10-hooks.md, TER-1312 F5): until the folder is trusted no Terrarium hook fires (TER-1327). The whole directory is bound, so the operator's own personal hooks stay out of the agents. |
//! | `sessions/` | `sessions/` | rw | The sessions (`~/.grok/sessions`, shared by the TUI, headless, and ACP). Shared, each agent could read and resume the sessions of every other agent. |
//! | `config.toml` | `config.toml` | rw | A copy of the shared `config.toml`, written again at each launch ([`materialize`]). Shared, one agent could change the config of every other agent (rev-1192 F1). A change that grok makes stays in the agent's copy until its next start. |
//! | `trusted_folders.toml` | `trusted_folders.toml` | rw | Empty at each launch. Grok stores its trust decisions there (`/hooks-trust`, `--trust`). Shared, one agent could trust a folder for every other agent (rev-1192 F1). The operator's own trust list does not reach the agents either. |
//!
//! Also in `<agent dir>/grok/`: `session-id`, the UUID of the last fresh
//! start (the daemon writes it).
//!
//! The two files are bound read-write, not read-only: whether grok exits
//! when it cannot write them is not captured (TER-1192 release gate). Each
//! is the agent's own file, so a write reaches nobody else.
//!
//! Grok also reads Claude Code hook files (`.claude/settings.json`) and
//! Cursor ones from the project; project hooks run only after a trust
//! decision, which the adapter never gives. A trust grant covers MCP,
//! LSP, hooks, project instructions and project skills of the folder
//! together (docs 10-hooks.md).
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
/// The config file in the Grok home (TOML).
pub const CONFIG_FILE: &str = "config.toml";
/// The trust decisions in the Grok home (TOML).
pub const TRUST_FILE: &str = "trusted_folders.toml";
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

/// The shared `config.toml` of `home`, when it is a plain file. A link is
/// not followed: the copy never reads another file.
fn shared_config(home: &Path) -> terrarium_core::Result<Vec<u8>> {
    let path = home.join(CONFIG_FILE);
    match fs::symlink_metadata(&path) {
        Ok(m) if m.is_file() => fs::read(&path).map_err(|e| io(&path, e)),
        Ok(_) => Ok(Vec::new()),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(Vec::new()),
        Err(e) => Err(io(&path, e)),
    }
}

/// An empty file at `path` when nothing is there: a bwrap mount point.
fn mount_point_file(path: &Path) -> terrarium_core::Result<()> {
    match fs::symlink_metadata(path) {
        Ok(_) => Ok(()),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            fs::write(path, b"").map_err(|e| io(path, e))
        }
        Err(e) => Err(io(path, e)),
    }
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
    // The agent's own config and trust files, new at each launch.
    let own = grok_dir(agent);
    write_atomic(&own.join(CONFIG_FILE), &shared_config(home)?)?;
    write_atomic(&own.join(TRUST_FILE), b"")?;
    for file in [CONFIG_FILE, TRUST_FILE] {
        mount_point_file(&home.join(file))?;
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
        FileOverlay {
            source: own.join(CONFIG_FILE),
            target: opts.home_dir.join(CONFIG_FILE),
            writable: true,
        },
        FileOverlay {
            source: own.join(TRUST_FILE),
            target: opts.home_dir.join(TRUST_FILE),
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
                FileOverlay {
                    source: grok_dir(&agent).join(CONFIG_FILE),
                    target: PathBuf::from("/home/agent/.grok/config.toml"),
                    writable: true,
                },
                FileOverlay {
                    source: grok_dir(&agent).join(TRUST_FILE),
                    target: PathBuf::from("/home/agent/.grok/trusted_folders.toml"),
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

    /// rev-1192 F1: each agent has its own config and trust files, new at
    /// each launch. A write by one agent (in its sandbox, the bind source)
    /// reaches neither the other agent nor the shared home, and is gone at
    /// the next launch.
    #[test]
    fn config_and_trust_are_per_agent_and_new_at_each_launch() {
        let tmp = tempfile::tempdir().expect("tmp");
        let home = tmp.path().join("auth-grok");
        fs::create_dir_all(&home).expect("home");
        fs::write(home.join(CONFIG_FILE), "[cli]\ninstaller = \"npm\"\n").expect("cfg");
        fs::write(home.join(TRUST_FILE), "trusted = [\"/operator/repo\"]\n").expect("trust");
        let opts = GrokOptions {
            home_dir: PathBuf::from("/home/agent/.grok"),
            ..GrokOptions::default()
        };
        let mk = |name: &str| {
            let def: terrarium_core::config::AgentDef = toml::from_str(&format!(
                "name = \"{name}\"\nproject = \"p\"\nharness = \"grok\"\n"
            ))
            .expect("def");
            let mut a = Agent::from_def(&def, tmp.path());
            a.harness_dir = Some(tmp.path().join(name).join("harness"));
            a
        };
        let (a, b) = (mk("a"), mk("b"));
        let source = |overlays: &[FileOverlay], file: &str| {
            overlays
                .iter()
                .find(|o| o.target == opts.home_dir.join(file))
                .map(|o| o.source.clone())
                .expect("overlay")
        };
        let oa = materialize(&opts, &a, &home, None).expect("a");
        let ob = materialize(&opts, &b, &home, None).expect("b");
        for file in [CONFIG_FILE, TRUST_FILE] {
            let (sa, sb) = (source(&oa, file), source(&ob, file));
            assert_ne!(sa, sb, "{file}");
            assert!(sa.starts_with(grok_dir(&a)) && sb.starts_with(grok_dir(&b)));
            assert_ne!(sa, home.join(file));
        }
        // The config is the shared one; the operator's trust list stays out.
        assert_eq!(
            fs::read_to_string(source(&oa, CONFIG_FILE)).expect("cfg"),
            "[cli]\ninstaller = \"npm\"\n"
        );
        assert_eq!(fs::read(source(&oa, TRUST_FILE)).expect("trust"), b"");
        // Agent a trusts a folder and changes its config.
        fs::write(source(&oa, TRUST_FILE), "trusted = [\"/x\"]\n").expect("w");
        fs::write(source(&oa, CONFIG_FILE), "[cli]\nevil = 1\n").expect("w");
        assert_eq!(fs::read(source(&ob, TRUST_FILE)).expect("b"), b"");
        assert_eq!(
            fs::read_to_string(source(&ob, CONFIG_FILE)).expect("b"),
            "[cli]\ninstaller = \"npm\"\n"
        );
        assert_eq!(
            fs::read_to_string(home.join(TRUST_FILE)).expect("shared"),
            "trusted = [\"/operator/repo\"]\n"
        );
        assert_eq!(
            fs::read_to_string(home.join(CONFIG_FILE)).expect("shared"),
            "[cli]\ninstaller = \"npm\"\n"
        );
        // The next launch of a starts from the shared config again.
        let oa = materialize(&opts, &a, &home, None).expect("again");
        assert_eq!(fs::read(source(&oa, TRUST_FILE)).expect("trust"), b"");
        assert_eq!(
            fs::read_to_string(source(&oa, CONFIG_FILE)).expect("cfg"),
            "[cli]\ninstaller = \"npm\"\n"
        );
        // A link in the shared home is not followed.
        let secret = tmp.path().join("secret");
        fs::write(&secret, "token").expect("secret");
        fs::remove_file(home.join(CONFIG_FILE)).expect("rm");
        std::os::unix::fs::symlink(&secret, home.join(CONFIG_FILE)).expect("ln");
        let ob = materialize(&opts, &b, &home, None).expect("b");
        assert_eq!(fs::read(source(&ob, CONFIG_FILE)).expect("cfg"), b"");
    }
}
