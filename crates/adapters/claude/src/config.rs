//! Per-agent files that the sandbox lays over the shared Claude home
//! (PLAN §8 and §11, the overlay trick).
//!
//! All agents share one Claude home (`/auth/claude`) for the login and the
//! session files. Claude Code also keeps its state file `.claude.json` in
//! that home (with `CLAUDE_CONFIG_DIR` set; without it, the file goes to
//! `$HOME/.claude.json`, outside the home). Each agent gets its own copy of
//! that file, for two reasons:
//!
//! - The quiet start needs per-workspace keys in it
//!   (`projects[<workspace>].hasTrustDialogAccepted`).
//! - Many Claude Code instances that write one `.claude.json` at the same
//!   time is not tested (`docs/compat.md`, "Not tested"). A per-agent copy
//!   removes the question.
//!
//! Layout under the agent directory (`/state/agents/<ulid>/`):
//!
//! | File | Use |
//! |---|---|
//! | `prompt.md` | the rendered system prompt; `launch` reads it |
//! | `claude/claude.json` | bound over `$CLAUDE_CONFIG_DIR/.claude.json` |
//! | `claude/settings.json` | the same JSON as `--settings`, for inspection |

use std::fs;
use std::path::{Path, PathBuf};

use serde_json::{Map, Value};
use terrarium_core::{Agent, FileOverlay};

use crate::error::{ClaudeError, Result};
use crate::{ClaudeOptions, launch};

/// The name of the state file in the Claude home.
pub const STATE_FILE: &str = ".claude.json";
/// The rendered prompt in the agent directory (PLAN §11).
pub const PROMPT_FILE: &str = "prompt.md";

/// `/state/agents/<ulid>`: the parent of the workspace.
pub fn agent_dir(agent: &Agent) -> PathBuf {
    agent
        .workspace
        .parent()
        .map(Path::to_path_buf)
        .unwrap_or_else(|| agent.workspace.clone())
}

/// `/state/agents/<ulid>/claude`.
pub fn claude_dir(agent: &Agent) -> PathBuf {
    agent_dir(agent).join("claude")
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
        Err(e) => Err(ClaudeError::io(path, e)),
    }
}

/// Add the quiet-start keys to a `.claude.json` value:
/// `hasCompletedOnboarding` (no theme picker) and
/// `projects[<cwd>].hasTrustDialogAccepted` (no trust dialog). The spikes
/// found that nothing else is needed. Other keys stay as they are.
pub fn seed_state(mut state: Value, cwd: &Path) -> Value {
    if !state.is_object() {
        state = Value::Object(Map::new());
    }
    let Value::Object(root) = &mut state else {
        return state;
    };
    root.insert("hasCompletedOnboarding".into(), Value::Bool(true));
    let projects = root
        .entry("projects")
        .or_insert_with(|| Value::Object(Map::new()));
    if !projects.is_object() {
        *projects = Value::Object(Map::new());
    }
    if let Value::Object(projects) = projects {
        let entry = projects
            .entry(cwd.to_string_lossy().into_owned())
            .or_insert_with(|| Value::Object(Map::new()));
        if !entry.is_object() {
            *entry = Value::Object(Map::new());
        }
        if let Value::Object(entry) = entry {
            entry.insert("hasTrustDialogAccepted".into(), Value::Bool(true));
        }
    }
    state
}

fn read_json(path: &Path) -> Result<Option<Value>> {
    match fs::read_to_string(path) {
        // A broken file is not fatal: the per-agent copy starts again.
        Ok(s) => Ok(serde_json::from_str(&s).ok()),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(e) => Err(ClaudeError::io(path, e)),
    }
}

/// Write `bytes` to `path` through a temporary file and a rename.
pub(crate) fn write_atomic(path: &Path, bytes: &[u8]) -> Result<()> {
    let tmp = path.with_extension("tmp-terrarium");
    fs::write(&tmp, bytes).map_err(|e| ClaudeError::io(&tmp, e))?;
    fs::rename(&tmp, path).map_err(|e| ClaudeError::io(path, e))
}

fn pretty(v: &Value) -> Result<Vec<u8>> {
    serde_json::to_vec_pretty(v).map_err(|e| ClaudeError::json("generated JSON", e))
}

/// Write the per-agent files and return the overlays.
///
/// - `home` is the shared Claude home on the host (the daemon's view).
/// - `prompt`: when `Some`, write it to `prompt.md`. When `None`, keep the
///   file that is there (the daemon renders it).
///
/// The per-agent `claude.json` keeps what Claude Code wrote into it before
/// (on a restart). On the first start it is a copy of the shared
/// `.claude.json`, so `oauthAccount` comes along. Then the quiet-start keys
/// go on top.
///
/// If the shared home has no `.claude.json`, this writes `{}` there. bwrap
/// needs a mount point, and otherwise leaves an empty file behind
/// (`docs/compat.md`, change 9).
pub fn materialize(
    opts: &ClaudeOptions,
    agent: &Agent,
    home: &Path,
    prompt: Option<&str>,
) -> Result<Vec<FileOverlay>> {
    let dir = claude_dir(agent);
    fs::create_dir_all(&dir).map_err(|e| ClaudeError::io(&dir, e))?;

    if let Some(text) = prompt {
        write_atomic(&prompt_path(agent), text.as_bytes())?;
    }

    let own = dir.join("claude.json");
    let shared = home.join(STATE_FILE);
    let base = match read_json(&own)? {
        Some(v) => v,
        None => read_json(&shared)?.unwrap_or_else(|| Value::Object(Map::new())),
    };
    let state = seed_state(base, &opts.harness_cwd(agent));
    write_atomic(&own, &pretty(&state)?)?;

    write_atomic(
        &dir.join("settings.json"),
        &pretty(&launch::settings_json(opts))?,
    )?;

    if !shared.exists() {
        write_atomic(&shared, b"{}\n")?;
    }

    Ok(vec![FileOverlay {
        source: own,
        target: opts.config_dir.join(STATE_FILE),
        // Claude Code writes its state file (oauthAccount, project stats).
        writable: true,
    }])
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_agent_in;
    use serde_json::json;

    #[test]
    fn seed_keeps_other_keys() {
        let v = seed_state(
            json!({"oauthAccount": {"emailAddress": "x"}, "projects": {"/w": {"lastCost": 1}}}),
            Path::new("/w"),
        );
        assert_eq!(v["hasCompletedOnboarding"], true);
        assert_eq!(v["projects"]["/w"]["hasTrustDialogAccepted"], true);
        assert_eq!(v["projects"]["/w"]["lastCost"], 1);
        assert_eq!(v["oauthAccount"]["emailAddress"], "x");
        assert_eq!(
            seed_state(json!([1]), Path::new("/w"))["hasCompletedOnboarding"],
            true
        );
    }

    #[test]
    fn materialize_writes_files_and_overlay() {
        let tmp = tempfile::tempdir().expect("tmp");
        let home = tmp.path().join("auth-claude");
        fs::create_dir_all(&home).expect("home");
        fs::write(
            home.join(STATE_FILE),
            r#"{"oauthAccount":{"emailAddress":"<email>"},"projects":{"/other":{"hasTrustDialogAccepted":true}}}"#,
        )
        .expect("shared");
        let agent = test_agent_in(tmp.path(), None);
        let opts = ClaudeOptions::default();

        let overlays = materialize(&opts, &agent, &home, Some("hello")).expect("materialize");
        assert_eq!(overlays.len(), 1);
        assert_eq!(overlays[0].target, Path::new("/auth/claude/.claude.json"));
        assert!(overlays[0].writable);

        let own: Value =
            serde_json::from_slice(&fs::read(&overlays[0].source).expect("read")).expect("json");
        let ws = agent.workspace.to_string_lossy().into_owned();
        assert_eq!(own["hasCompletedOnboarding"], true);
        assert_eq!(own["projects"][&ws]["hasTrustDialogAccepted"], true);
        assert_eq!(own["projects"]["/other"]["hasTrustDialogAccepted"], true);
        assert_eq!(own["oauthAccount"]["emailAddress"], "<email>");
        assert_eq!(
            read_prompt(&agent).expect("prompt").as_deref(),
            Some("hello")
        );
        let settings: Value = serde_json::from_slice(
            &fs::read(claude_dir(&agent).join("settings.json")).expect("settings"),
        )
        .expect("json");
        assert_eq!(settings["skipDangerousModePermissionPrompt"], true);

        // The shared file does not change.
        let shared: Value =
            serde_json::from_slice(&fs::read(home.join(STATE_FILE)).expect("shared"))
                .expect("json");
        assert!(shared.get("hasCompletedOnboarding").is_none());

        // A restart keeps what Claude Code wrote into the per-agent copy.
        fs::write(&overlays[0].source, r#"{"numStartups": 7}"#).expect("write");
        materialize(&opts, &agent, &home, None).expect("again");
        let own: Value =
            serde_json::from_slice(&fs::read(&overlays[0].source).expect("read")).expect("json");
        assert_eq!(own["numStartups"], 7);
        assert_eq!(own["hasCompletedOnboarding"], true);
        assert_eq!(
            read_prompt(&agent).expect("prompt").as_deref(),
            Some("hello")
        );
    }

    #[test]
    fn materialize_creates_missing_shared_state_file() {
        let tmp = tempfile::tempdir().expect("tmp");
        let home = tmp.path().join("auth-claude");
        fs::create_dir_all(&home).expect("home");
        let agent = test_agent_in(tmp.path(), None);
        materialize(&ClaudeOptions::default(), &agent, &home, None).expect("materialize");
        assert_eq!(
            fs::read_to_string(home.join(STATE_FILE)).expect("shared"),
            "{}\n"
        );
        assert_eq!(read_prompt(&agent).expect("prompt"), None);
    }
}
