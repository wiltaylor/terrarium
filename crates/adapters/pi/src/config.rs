//! Per-agent files for pi (PLAN §8 and §11).
//!
//! All agents share one pi home (`/auth/pi`, seen as `PI_CODING_AGENT_DIR`)
//! for the logins (`auth.json`) and the custom providers (`models.json`).
//! Everything that is per agent lives in the harness directory
//! (`/state/agents/<ulid>/harness/`). The sandbox binds that directory
//! read-only at the same path, so these paths are the same for the daemon
//! and for pi. The two places that pi writes, `pi/sessions/` and the
//! settings overlay, get read-write binds of their own.
//!
//! | File | Use |
//! |---|---|
//! | `prompt.md` | the rendered system prompt (the daemon writes it); `launch` passes its path |
//! | `pi/terrarium.ts` | the event extension ([`EXTENSION`]); `launch` loads it with `-e` |
//! | `pi/sessions/` | the session files (`--session-dir`); bound read-write over itself |
//! | `pi/settings.json` | bound over `$PI_CODING_AGENT_DIR/settings.json` |
//!
//! # What pi keeps in its home, and where it goes
//!
//! | Path in the home | Decision | Why |
//! |---|---|---|
//! | `auth.json` | shared | the logins. pi writes it under a file lock (`proper-lockfile`) and refreshes OAuth tokens in it, so a refresh by one agent reaches all of them. |
//! | `models.json` | shared | custom providers (base URL, models, API key). Read only for pi. |
//! | `settings.json` | per agent | the operator's own defaults (provider, model, theme, packages) stay out, and a `/model` change in one agent does not change the default of the others. The first start copies the shared file; later starts keep the agent's copy. Every start sets `lastChangelogVersion` to the pinned version ([`LAST_CHANGELOG_KEY`]), so pi 1.4's "What's new" changelog does not show. |
//! | `mcp.json` | not loaded | `--no-mcp` (pi 1.4 has built-in MCP). |
//! | `sessions/` | not used | `--session-dir` puts the sessions in the agent directory. In the sandbox every agent has the cwd `$HOME/work`, so the default directory (`sessions/--home-…-work--/`) would be one directory for all agents, and `pi --resume` would list the sessions of every agent. Session IDs do not collide (each agent gets new UUIDs), but pi has **no lock on a session**: two processes with one `--session-id` both start and both append (`docs/compat.md`). The supervisor must run one pi per agent. |
//! | `extensions/`, `skills/`, `prompts/` | not loaded | `--no-extensions`, `--no-skills`, `--no-prompt-templates`. |
//! | `AGENTS.md`, `SYSTEM.md`, `APPEND_SYSTEM.md` | not loaded | `--no-context-files`; `--system-prompt`/`--append-system-prompt` replace the discovered files. The operator's personal instructions stay out of agents. |
//! | `trust.json` | not written | `--no-approve` makes no trust decision. |
//! | `models-store.json` | shared | written at start (seen in the live check). Not tested with many writers. |
//!
//! The extension file is also bound read-only over itself. The harness
//! directory is read-only already; the explicit bind keeps the extension
//! safe if the bind of the directory ever changes, because the agent must
//! not change the code that reports its state.

use std::fs;
use std::path::{Path, PathBuf};

use serde_json::Value;
use terrarium_core::{Agent, FileOverlay};

use crate::PiOptions;
use crate::error::{PiError, Result};

/// The Terrarium event extension (`ext/terrarium.ts`).
pub const EXTENSION: &str = include_str!("../ext/terrarium.ts");
/// The file name of the extension in the agent's pi directory.
pub const EXTENSION_FILE: &str = "terrarium.ts";
/// The settings file in the pi home.
pub const SETTINGS_FILE: &str = "settings.json";
/// The settings key of the last version whose changelog pi showed. pi
/// shows the "What's new" entries of every newer version at the start,
/// above the editor. The per-agent copy always holds the pinned version,
/// so the changelog never shows (the shared file can name an older one).
pub const LAST_CHANGELOG_KEY: &str = "lastChangelogVersion";
/// The logins in the pi home. Shared.
pub const AUTH_FILE: &str = "auth.json";
/// The session directory in the agent's pi directory.
pub const SESSIONS_DIR: &str = "sessions";
/// The rendered prompt in the agent directory (PLAN §11).
pub const PROMPT_FILE: &str = "prompt.md";

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

/// `/state/agents/<ulid>/pi`.
pub fn pi_dir(agent: &Agent) -> PathBuf {
    agent_dir(agent).join("pi")
}

/// The extension file of the agent.
pub fn extension_path(agent: &Agent) -> PathBuf {
    pi_dir(agent).join(EXTENSION_FILE)
}

/// The session directory of the agent (`--session-dir`).
pub fn sessions_dir(agent: &Agent) -> PathBuf {
    pi_dir(agent).join(SESSIONS_DIR)
}

/// The rendered prompt file of the agent.
pub fn prompt_path(agent: &Agent) -> PathBuf {
    agent_dir(agent).join(PROMPT_FILE)
}

/// The prompt file, when it holds text. `launch` passes the path, and pi
/// reads the file. A path that does not exist would go to pi as the
/// prompt text itself, so a missing or empty file gives `None`.
pub fn prompt_file(agent: &Agent) -> Option<PathBuf> {
    let path = prompt_path(agent);
    let text = fs::read_to_string(&path).ok()?;
    (!text.trim().is_empty()).then_some(path)
}

/// Write `bytes` to `path` through a temporary file and a rename.
pub(crate) fn write_atomic(path: &Path, bytes: &[u8]) -> Result<()> {
    let tmp = path.with_extension("tmp-terrarium");
    fs::write(&tmp, bytes).map_err(|e| PiError::io(&tmp, e))?;
    fs::rename(&tmp, path).map_err(|e| PiError::io(path, e))
}

fn read_json(path: &Path) -> Result<Option<Value>> {
    match fs::read_to_string(path) {
        // A broken file is not fatal: the per-agent copy starts again.
        Ok(s) => Ok(serde_json::from_str(&s).ok().filter(Value::is_object)),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(e) => Err(PiError::io(path, e)),
    }
}

/// Write the per-agent files and return the overlays.
///
/// - `home` is the shared pi home on the host (the daemon's view).
/// - `prompt`: when `Some`, write it to `prompt.md`. When `None`, keep the
///   file that is there (the daemon renders it).
///
/// The extension is written on every start, so a new adapter version
/// ships a new extension. It must exist before pi starts: a missing `-e`
/// path stops pi.
pub fn materialize(
    opts: &PiOptions,
    agent: &Agent,
    home: &Path,
    prompt: Option<&str>,
) -> Result<Vec<FileOverlay>> {
    let dir = pi_dir(agent);
    let sessions = sessions_dir(agent);
    for d in [&dir, &sessions] {
        fs::create_dir_all(d).map_err(|e| PiError::io(d, e))?;
    }
    if let Some(text) = prompt {
        write_atomic(&prompt_path(agent), text.as_bytes())?;
    }
    let ext = extension_path(agent);
    write_atomic(&ext, EXTENSION.as_bytes())?;

    // settings.json: a copy of the shared file on the first start. Every
    // start sets `lastChangelogVersion` to the pinned version.
    let own = dir.join(SETTINGS_FILE);
    let shared = home.join(SETTINGS_FILE);
    let current = read_json(&own)?;
    let mut settings = match &current {
        Some(v) => v.clone(),
        None => read_json(&shared)?.unwrap_or_else(|| Value::Object(Default::default())),
    };
    settings[LAST_CHANGELOG_KEY] = Value::String(opts.flags.verified.to_owned());
    if current.as_ref() != Some(&settings) {
        let bytes =
            serde_json::to_vec_pretty(&settings).map_err(|e| PiError::json("settings.json", e))?;
        write_atomic(&own, &bytes)?;
    }
    // bwrap needs a mount point. Otherwise it makes an empty file and
    // leaves it behind.
    if !shared.exists() {
        write_atomic(&shared, b"{}\n")?;
    }

    Ok(vec![
        FileOverlay {
            source: own,
            target: opts.agent_dir.join(SETTINGS_FILE),
            // pi writes settings (for example after `/model`).
            writable: true,
        },
        FileOverlay {
            source: ext.clone(),
            target: ext,
            writable: false,
        },
        // pi writes its session files here; the rest of the harness
        // directory is read-only.
        FileOverlay {
            source: sessions.clone(),
            target: sessions,
            writable: true,
        },
    ])
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_agent_in;

    #[test]
    fn materialize_writes_extension_settings_and_overlays() {
        let tmp = tempfile::tempdir().expect("tmp");
        let home = tmp.path().join("auth-pi");
        fs::create_dir_all(&home).expect("home");
        fs::write(
            home.join(SETTINGS_FILE),
            r#"{"defaultThinkingLevel":"off"}"#,
        )
        .expect("shared");
        fs::write(home.join(AUTH_FILE), "{}").expect("auth");
        let agent = test_agent_in(tmp.path(), None);
        let opts = PiOptions::default();

        let overlays = materialize(&opts, &agent, &home, Some("hello")).expect("materialize");
        let ext = extension_path(&agent);
        assert_eq!(fs::read_to_string(&ext).expect("ext"), EXTENSION);
        assert!(sessions_dir(&agent).is_dir());
        assert_eq!(prompt_file(&agent), Some(prompt_path(&agent)));
        assert_eq!(
            overlays,
            vec![
                FileOverlay {
                    source: pi_dir(&agent).join(SETTINGS_FILE),
                    target: PathBuf::from("/auth/pi/settings.json"),
                    writable: true,
                },
                FileOverlay {
                    source: ext.clone(),
                    target: ext.clone(),
                    writable: false,
                },
                FileOverlay {
                    source: sessions_dir(&agent),
                    target: sessions_dir(&agent),
                    writable: true,
                },
            ]
        );
        let own: Value =
            serde_json::from_slice(&fs::read(&overlays[0].source).expect("own")).expect("json");
        assert_eq!(own["defaultThinkingLevel"], "off");
        assert_eq!(own[LAST_CHANGELOG_KEY], opts.flags.verified);

        // A restart keeps the agent's settings, writes the extension again,
        // and keeps the prompt.
        fs::write(
            &overlays[0].source,
            r#"{"defaultModel":"x","lastChangelogVersion":"0.85.1"}"#,
        )
        .expect("write");
        fs::write(&ext, "tampered").expect("tamper");
        materialize(&opts, &agent, &home, None).expect("again");
        let own: Value =
            serde_json::from_slice(&fs::read(&overlays[0].source).expect("own")).expect("json");
        assert_eq!(own["defaultModel"], "x");
        assert_eq!(own[LAST_CHANGELOG_KEY], opts.flags.verified);
        assert_eq!(fs::read_to_string(&ext).expect("ext"), EXTENSION);
        assert_eq!(
            fs::read_to_string(prompt_path(&agent)).expect("prompt"),
            "hello"
        );

        // Auth and the shared settings stay as they were. No overlay covers
        // auth.json or models.json.
        assert_eq!(
            fs::read_to_string(home.join(AUTH_FILE)).expect("auth"),
            "{}"
        );
        assert_eq!(
            fs::read_to_string(home.join(SETTINGS_FILE)).expect("shared"),
            r#"{"defaultThinkingLevel":"off"}"#
        );
        for shared in ["auth.json", "models.json"] {
            let p = Path::new("/auth/pi").join(shared);
            assert!(
                !overlays.iter().any(|o| p.starts_with(&o.target)),
                "{shared}"
            );
        }
    }

    #[test]
    fn materialize_makes_missing_mount_point_and_no_prompt() {
        let tmp = tempfile::tempdir().expect("tmp");
        let home = tmp.path().join("auth-pi");
        fs::create_dir_all(&home).expect("home");
        let agent = test_agent_in(tmp.path(), None);
        materialize(&PiOptions::default(), &agent, &home, None).expect("materialize");
        assert_eq!(
            fs::read_to_string(home.join(SETTINGS_FILE)).expect("shared"),
            "{}\n"
        );
        assert_eq!(prompt_file(&agent), None);
        fs::write(prompt_path(&agent), " \n").expect("empty prompt");
        assert_eq!(prompt_file(&agent), None);
    }
}
