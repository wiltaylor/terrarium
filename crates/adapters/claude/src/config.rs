//! Per-agent files that the sandbox lays over the shared Claude home
//! (PLAN §8 and §11, the overlay trick).
//!
//! All agents share one Claude home (`/auth/claude`) for the login and the
//! caches. The session files are per agent (see the table below). Claude
//! Code also keeps its state file `.claude.json` in
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
//! Layout under the harness directory (`/state/agents/<ulid>/harness/`):
//!
//! | File | Use |
//! |---|---|
//! | `prompt.md` | the rendered system prompt; `launch` reads it |
//! | `claude/claude.json` | bound over `$CLAUDE_CONFIG_DIR/.claude.json` |
//! | `claude/settings.json` | the same JSON as `--settings`, for inspection |
//! | `claude/sessions/` | bound over `$CLAUDE_CONFIG_DIR/sessions/` (a directory) |
//! | `claude/history.jsonl` | bound over `$CLAUDE_CONFIG_DIR/history.jsonl` |
//! | `claude/backups/` | bound over `$CLAUDE_CONFIG_DIR/backups/` |
//! | `claude/projects/<cwd>/` | bound over `$CLAUDE_CONFIG_DIR/projects/<cwd>/` (the transcripts, and `memory/`) |
//! | `claude/file-history/` | bound over `$CLAUDE_CONFIG_DIR/file-history/` |
//! | `claude/todos/` | bound over `$CLAUDE_CONFIG_DIR/todos/` |
//! | `claude/session-env/` | bound over `$CLAUDE_CONFIG_DIR/session-env/` |
//! | `claude/plugin/` | the agent's skills as a plugin (`--plugin-dir`, PLAN §8.6): `.claude-plugin/plugin.json` and `skills/<name>/`, a copy of `skills/`; read-only. Absent when the agent has no skill. |
//!
//! The shared home's own skills (`$CLAUDE_CONFIG_DIR/skills`) and the
//! workspace's `.claude/skills` do not load: `--setting-sources ""` turns
//! off the user and project sources that read them (bundle markers
//! `Qt("userSettings")&&!w?SL(s,"userSettings","skills",n)` and
//! `Qt("projectSettings")`).
//!
//! # What Claude Code writes into its home, and where it goes
//!
//! Every agent has the same cwd (`/home/agent/work`) and, in its PID
//! namespace, the same pid (2). A path that Claude Code keys by cwd or by
//! pid is therefore one path for all agents. A path that it keys by the
//! session UUID, or by a time stamp and a random part, does not collide.
//!
//! | Path in the home | Decision | Why |
//! |---|---|---|
//! | `.credentials.json` | per account | the plan login. The daemon moves it into the account of its subscription (`terrariumd::accounts`, PLAN §16.3) and points `CLAUDE_SECURESTORAGE_CONFIG_DIR` at the account through the agent's `cred/account` symlink, so every agent on one account shares one file and one set of locks. Before the migration it is shared here. Never per-agent copies: they would break on refresh-token rotation. |
//! | `.claude.json` | per-agent | see above. Claude Code writes it on every start. |
//! | `sessions/` | per-agent | `sessions/<pid>.json` and `<pid>.<hash>.key`: pid 2 in every sandbox. |
//! | `history.jsonl` | per-agent | the prompt history (the up-arrow list), keyed by cwd. Shared, each agent would see the typed turns (mail text) of every other agent and of the operator. |
//! | `backups/` | per-agent | `.claude.json.backup.<ms>`: a copy of the agent's own `.claude.json` at each start. Claude Code restores from it after a broken state file, so a shared directory could give one agent another agent's state. |
//! | `projects/<cwd>/<uuid>.jsonl` (and `<uuid>/` for subagents) | per-agent | the transcripts. One file per session UUID does not collide, but a shared directory lets each agent read, and write into, the transcripts of every other agent in every project (a path for prompt injection). `--resume <uuid>` finds the file in the agent's own directory. The daemon's usage collector reads it from the agent's harness dir. |
//! | `projects/<cwd>/memory/` | per-agent (inside the directory above) | Claude Code's auto memory (`MEMORY.md`) for the cwd. One cwd for all agents means one memory for all agents: a note that one agent writes goes into the prompt of the next. Terrarium has its own memory (PLAN §14). An old `claude/memory/` moves into the project directory at the next start. |
//! | `file-history/<uuid>/` | per-agent | copies of the files that the agent edited (for the rewind). Shared, an agent could read the workspace files of other agents. |
//! | `todos/<uuid>-*.json`, `session-env/<uuid>/` | per-agent | keyed by the session UUID; the todo lists hold turn text. |
//! | `security_warnings_state_<uuid>.json` | shared | keyed by the session UUID; a file in the home root, so a directory bind cannot cover it. |
//! | `shell-snapshots/snapshot-<shell>-<ms>-<random>.sh` | shared | unique names. They hold the sandbox shell setup, which is the same for every agent. |
//! | `debug/` | shared | only with `--debug`. `debug/latest` is a symlink that the last writer wins; harmless. |
//! | `cache/`, `statsig/`, `plugins/`, `paste-cache/`, `.last-cleanup` | shared | caches of public data (model catalog, change log, feature flags) and content-addressed pastes. |
//!
//! The per-agent entries come from [`materialize`].

use std::fs;
use std::path::{Path, PathBuf};

use serde_json::{Map, Value};
use terrarium_core::{Agent, FileOverlay};

use crate::error::{ClaudeError, Result};
use crate::{ClaudeOptions, launch, session};

/// The name of the state file in the Claude home.
pub const STATE_FILE: &str = ".claude.json";
/// The live-process registry in the Claude home. A directory.
pub const SESSIONS_DIR: &str = "sessions";
/// The prompt history in the Claude home. A JSON-lines file.
pub const HISTORY_FILE: &str = "history.jsonl";
/// The backups of `.claude.json` in the Claude home. A directory.
pub const BACKUPS_DIR: &str = "backups";
/// The auto-memory directory in a Claude project directory.
pub const MEMORY_DIR: &str = "memory";
/// The project directories (transcripts) in the Claude home.
pub const PROJECTS_DIR: &str = "projects";
/// The per-session directories in the Claude home that hold turn data,
/// each bound per agent (see the table at the top).
pub const SESSION_DIRS: &[&str] = &["file-history", "todos", "session-env"];
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

/// `/state/agents/<ulid>/claude`.
pub fn claude_dir(agent: &Agent) -> PathBuf {
    agent_dir(agent).join("claude")
}

/// The agent's plugin dir (PLAN §8.6): its skills, for `--plugin-dir`.
pub fn plugin_dir(agent: &Agent) -> PathBuf {
    claude_dir(agent).join("plugin")
}

/// Write the agent's plugin dir: a manifest and a copy of the delivered
/// skills (`<harness dir>/skills/`). No delivered skill: the dir goes.
pub fn write_plugin(opts: &ClaudeOptions, agent: &Agent) -> Result<()> {
    let dir = plugin_dir(agent);
    if agent.delivered_skills.is_empty() {
        if dir.exists() {
            fs::remove_dir_all(&dir).map_err(|e| ClaudeError::io(&dir, e))?;
        }
        return Ok(());
    }
    let skills = terrarium_core::skills::delivered_dir(&agent_dir(agent));
    terrarium_core::skills::copy_delivered(&skills, &agent.delivered_skills, &dir.join("skills"))
        .map_err(|e| ClaudeError::io(&dir, std::io::Error::other(e)))?;
    let manifest = serde_json::json!({
        "name": opts.flags.plugin_name,
        "description": "The Terrarium skills of this agent",
    });
    let meta = dir.join(".claude-plugin");
    fs::create_dir_all(&meta).map_err(|e| ClaudeError::io(&meta, e))?;
    write_atomic(&meta.join("plugin.json"), &pretty(&manifest)?)
}

/// The agent's own project directory for `cwd` (the daemon's view):
/// `claude/projects/<encoded cwd>/`. The sandbox binds it over
/// `$CLAUDE_CONFIG_DIR/projects/<encoded cwd>/`, so the layout under
/// [`claude_dir`] is the layout of a Claude home.
pub fn own_project_dir(agent: &Agent, cwd: &Path) -> PathBuf {
    session::project_dir(&claude_dir(agent), cwd)
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

/// The global-config keys of `.claude.json` that each agent's copy gets
/// (`docs/research/harness-hardening.md`, Claude Code 2.3): no IDE
/// connect or extension install, no LSP plugin offer, no Claude in
/// Chrome, no Remote Control, no session upload. The names are in the
/// 2.1.310 global-config key list; their effect on a dialog is not tested.
pub const QUIET_STATE: &[(&str, bool)] = &[
    ("autoInstallIdeExtension", false),
    ("autoConnectIde", false),
    ("lspRecommendationDisabled", true),
    ("claudeInChromeDefaultEnabled", false),
    ("remoteControlAtStartup", false),
    ("autoUploadSessions", false),
];

/// Add the quiet-start keys to a `.claude.json` value:
/// `hasCompletedOnboarding` (no theme picker),
/// `projects[<cwd>].hasTrustDialogAccepted` (no trust dialog; the spikes
/// found that these two are needed), and [`QUIET_STATE`]. Other keys stay
/// as they are.
pub fn seed_state(mut state: Value, cwd: &Path) -> Value {
    if !state.is_object() {
        state = Value::Object(Map::new());
    }
    let Value::Object(root) = &mut state else {
        return state;
    };
    root.insert("hasCompletedOnboarding".into(), Value::Bool(true));
    for (key, value) in QUIET_STATE {
        root.insert((*key).to_owned(), Value::Bool(*value));
    }
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
        &pretty(&launch::settings_json(opts, agent))?,
    )?;
    write_plugin(opts, agent)?;

    if !shared.exists() {
        write_atomic(&shared, b"{}\n")?;
    }

    let mut overlays = vec![FileOverlay {
        source: own,
        target: opts.config_dir.join(STATE_FILE),
        // Claude Code writes its state file (oauthAccount, project stats).
        writable: true,
    }];

    // Claude Code registers each live process as `sessions/<pid>.json` in
    // the home. In a sandbox with its own PID namespace every agent has the
    // same pid (2), so the agents would overwrite one file, and each agent
    // would see the operator's own sessions. A per-agent directory keeps
    // them apart (`docs/compat.md`, "Claude inside bwrap").
    overlays.push(dir_overlay(
        &dir.join(SESSIONS_DIR),
        &home.join(SESSIONS_DIR),
        opts.config_dir.join(SESSIONS_DIR),
    )?);
    // The prompt history: per agent, so no agent sees what was typed into
    // another one (see the table at the top).
    overlays.push(file_overlay(
        &dir.join(HISTORY_FILE),
        &home.join(HISTORY_FILE),
        opts.config_dir.join(HISTORY_FILE),
    )?);
    // The backups of the per-agent `.claude.json` stay with the agent.
    overlays.push(dir_overlay(
        &dir.join(BACKUPS_DIR),
        &home.join(BACKUPS_DIR),
        opts.config_dir.join(BACKUPS_DIR),
    )?);
    // The project directory of the one cwd that all agents share: the
    // transcripts and the auto memory. Per agent, so no agent reads (or
    // writes into) the transcripts of another agent.
    let cwd = opts.harness_cwd(agent);
    let project = Path::new(PROJECTS_DIR).join(session::encode_cwd(&cwd));
    let own_project = own_project_dir(agent, &cwd);
    migrate_memory(&dir.join(MEMORY_DIR), &own_project.join(MEMORY_DIR))?;
    overlays.push(dir_overlay(
        &own_project,
        &home.join(&project),
        opts.config_dir.join(&project),
    )?);
    // The per-session data next to the transcripts.
    for name in SESSION_DIRS {
        overlays.push(dir_overlay(
            &dir.join(name),
            &home.join(name),
            opts.config_dir.join(name),
        )?);
    }
    Ok(overlays)
}

/// Move the auto memory of an older layout (`claude/memory/`, bound over
/// `projects/<cwd>/memory/`) into the per-agent project directory. A
/// memory that is there already wins; the old directory then stays.
fn migrate_memory(old: &Path, new: &Path) -> Result<()> {
    if !old.is_dir() || new.exists() {
        return Ok(());
    }
    if let Some(parent) = new.parent() {
        fs::create_dir_all(parent).map_err(|e| ClaudeError::io(parent, e))?;
    }
    fs::rename(old, new).map_err(|e| ClaudeError::io(old, e))
}

/// A read-write per-agent directory over `target`. Both the agent's
/// directory and the mount point in the shared home (`shared`) must exist:
/// otherwise bwrap makes the mount point itself and leaves it behind.
fn dir_overlay(own: &Path, shared: &Path, target: PathBuf) -> Result<FileOverlay> {
    for d in [own, shared] {
        fs::create_dir_all(d).map_err(|e| ClaudeError::io(d, e))?;
    }
    Ok(FileOverlay {
        source: own.to_path_buf(),
        target,
        writable: true,
    })
}

/// A read-write per-agent file over `target`. A missing file (the agent's
/// or the mount point in the shared home) is made empty; a file that is
/// there keeps its content.
fn file_overlay(own: &Path, shared: &Path, target: PathBuf) -> Result<FileOverlay> {
    for f in [own, shared] {
        if let Some(parent) = f.parent() {
            fs::create_dir_all(parent).map_err(|e| ClaudeError::io(parent, e))?;
        }
        fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(f)
            .map_err(|e| ClaudeError::io(f, e))?;
    }
    Ok(FileOverlay {
        source: own.to_path_buf(),
        target,
        writable: true,
    })
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
        assert_eq!(v["autoInstallIdeExtension"], false);
        assert_eq!(v["lspRecommendationDisabled"], true);
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
    fn overlay_list() {
        let tmp = tempfile::tempdir().expect("tmp");
        let home = tmp.path().join("auth-claude");
        fs::create_dir_all(&home).expect("home");
        let agent = test_agent_in(tmp.path(), None);
        let opts = ClaudeOptions {
            harness_workspace: Some(PathBuf::from("/home/agent/work")),
            ..ClaudeOptions::default()
        };
        let overlays = materialize(&opts, &agent, &home, None).expect("materialize");
        let own = claude_dir(&agent);
        let got: Vec<(PathBuf, PathBuf)> = overlays
            .iter()
            .map(|o| (o.source.clone(), o.target.clone()))
            .collect();
        let want: Vec<(PathBuf, PathBuf)> = [
            ("claude.json", ".claude.json"),
            ("sessions", "sessions"),
            ("history.jsonl", "history.jsonl"),
            ("backups", "backups"),
            ("projects/-home-agent-work", "projects/-home-agent-work"),
            ("file-history", "file-history"),
            ("todos", "todos"),
            ("session-env", "session-env"),
        ]
        .iter()
        .map(|(s, t)| (own.join(s), Path::new("/auth/claude").join(t)))
        .collect();
        assert_eq!(got, want);
        assert!(overlays.iter().all(|o| o.writable));

        // Each source and each mount point in the shared home exists, with
        // the same kind (file or directory).
        for o in &overlays {
            let rel = o.target.strip_prefix("/auth/claude").expect("in home");
            let shared = home.join(rel);
            assert!(o.source.exists(), "{}", o.source.display());
            assert!(shared.exists(), "{}", shared.display());
            assert_eq!(o.source.is_dir(), shared.is_dir(), "{}", rel.display());
        }
        assert!(own.join("history.jsonl").is_file());
        assert_eq!(fs::read(own.join("history.jsonl")).expect("history"), b"");

        // The credentials and the caches stay shared: no overlay covers them.
        for shared in [
            ".credentials.json",
            "shell-snapshots/snapshot-bash-1-x.sh",
            "plugins/x",
            "projects/-other-cwd/x.jsonl",
        ] {
            let p = Path::new("/auth/claude").join(shared);
            assert!(
                !overlays.iter().any(|o| p.starts_with(&o.target)),
                "{shared} must stay shared"
            );
        }
        // The transcripts, the auto memory, and the per-session data are
        // the agent's own: an overlay from the agent's directory covers
        // each one.
        for own_path in [
            "projects/-home-agent-work/0b5c1d2e-0000-4000-8000-000000000001.jsonl",
            "projects/-home-agent-work/0b5c1d2e-0000-4000-8000-000000000001/subagents/a.jsonl",
            "projects/-home-agent-work/memory/MEMORY.md",
            "file-history/0b5c1d2e-0000-4000-8000-000000000001/x@v1",
            "todos/0b5c1d2e-0000-4000-8000-000000000001-agent-x.json",
            "session-env/0b5c1d2e-0000-4000-8000-000000000001/x",
        ] {
            let p = Path::new("/auth/claude").join(own_path);
            let o = overlays
                .iter()
                .filter(|o| p.starts_with(&o.target))
                .max_by_key(|o| o.target.components().count())
                .unwrap_or_else(|| panic!("{own_path} must be per agent"));
            assert!(o.source.starts_with(&own), "{own_path}");
        }
        assert_eq!(
            own_project_dir(&agent, Path::new("/home/agent/work")),
            own.join("projects/-home-agent-work")
        );
    }

    #[test]
    fn old_memory_moves_into_the_project_dir() {
        let tmp = tempfile::tempdir().expect("tmp");
        let home = tmp.path().join("auth-claude");
        fs::create_dir_all(&home).expect("home");
        let agent = test_agent_in(tmp.path(), None);
        let opts = ClaudeOptions {
            harness_workspace: Some(PathBuf::from("/home/agent/work")),
            ..ClaudeOptions::default()
        };
        // The layout before the per-agent project dir.
        let old = claude_dir(&agent).join(MEMORY_DIR);
        fs::create_dir_all(&old).expect("old");
        fs::write(old.join("MEMORY.md"), "note").expect("memory");
        materialize(&opts, &agent, &home, None).expect("materialize");
        let new = own_project_dir(&agent, Path::new("/home/agent/work")).join(MEMORY_DIR);
        assert_eq!(
            fs::read_to_string(new.join("MEMORY.md")).expect("moved"),
            "note"
        );
        assert!(!old.exists());
        // No overlay names the old directory any more.
        let overlays = materialize(&opts, &agent, &home, None).expect("again");
        assert!(overlays.iter().all(|o| o.source != old));
        assert!(new.join("MEMORY.md").is_file());
    }

    #[test]
    fn overlays_keep_existing_content() {
        let tmp = tempfile::tempdir().expect("tmp");
        let home = tmp.path().join("auth-claude");
        fs::create_dir_all(&home).expect("home");
        // The operator's own history in the shared home.
        fs::write(home.join(HISTORY_FILE), "{\"display\":\"operator\"}\n").expect("shared");
        let agent = test_agent_in(tmp.path(), None);
        let opts = ClaudeOptions::default();
        materialize(&opts, &agent, &home, None).expect("materialize");
        let own = claude_dir(&agent).join(HISTORY_FILE);
        // The agent starts with an empty history, not the operator's.
        assert_eq!(fs::read(&own).expect("own"), b"");
        fs::write(&own, "{\"display\":\"agent\"}\n").expect("write");
        fs::write(claude_dir(&agent).join(BACKUPS_DIR).join("b"), "x").expect("backup");

        // A restart keeps the agent's files and does not touch the shared one.
        materialize(&opts, &agent, &home, None).expect("again");
        assert_eq!(
            fs::read_to_string(&own).expect("own"),
            "{\"display\":\"agent\"}\n"
        );
        assert!(claude_dir(&agent).join(BACKUPS_DIR).join("b").is_file());
        assert_eq!(
            fs::read_to_string(home.join(HISTORY_FILE)).expect("shared"),
            "{\"display\":\"operator\"}\n"
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
