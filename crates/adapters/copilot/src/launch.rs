//! The command line for a fresh start or a resume (PLAN §4, §7, §8;
//! `docs/compat.md`, "Copilot CLI 1.0.103", "Quiet start").

use std::path::Path;

use serde_json::{Map, Value, json};
use terrarium_core::{Agent, HarnessSessionId, LaunchSpec};

use crate::CopilotOptions;
use crate::config;

/// How the session starts.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SessionStart<'a> {
    /// A new session with this UUID (`--session-id`). Use a new UUID for
    /// each fresh start: a UUID that Copilot saw before resumes that
    /// session, with no error.
    Fresh(&'a str),
    /// Continue this session (`--resume <id>`). Verified:
    /// `SessionStart.source` is `resume`, also from another cwd. A missing
    /// ID exits at once with "No session, task, or name matched".
    /// (`--session-id <used id>` resumes too, but its `source` was not
    /// checked, and on a missing ID it starts a new, empty session.)
    Resume(&'a HarnessSessionId),
}

/// The plugin's `plugin.json`. Copilot loads a plugin directory that has
/// it (verified in a probe: the footer said "Loading: 1 plugin").
pub fn plugin_manifest() -> Value {
    json!({
        "name": "terrarium",
        "version": terrarium_core::VERSION,
        "description": "Terrarium hooks: forward each Copilot hook to the runner socket.",
    })
}

/// The plugin's root `hooks.json`. Each hook runs
/// `<hook_program> hook <Name>`; the CLI forwards the stdin JSON to
/// `TERRARIUM_SOCKET`. The argument names the hook, because the
/// `PermissionRequest` payload has no `hook_event_name`.
pub fn hooks_json(opts: &CopilotOptions) -> Value {
    let mut hooks = Map::new();
    for name in opts.flags.hook_events {
        hooks.insert(
            (*name).to_owned(),
            json!([{
                "type": "command",
                "bash": format!("{} hook {name}", opts.hook_program),
                "timeoutSec": opts.flags.hook_timeout_sec,
            }]),
        );
    }
    json!({"version": 1, "hooks": Value::Object(hooks)})
}

/// Build the launch spec. This is pure: [`crate::CopilotAdapter::launch`]
/// picks the session UUID and records it.
///
/// The flags, in order:
///
/// - No `--experimental`: the per-agent `settings.json` turns experimental
///   mode on (see [`crate::flags::CopilotFlags::experimental`]).
/// - `--yolo`: Allow All mode. With `COPILOT_ALLOW_ALL=true` (env, below)
///   there is no trust dialog; each alone is not enough.
/// - `--no-ask-user --mode interactive --disable-builtin-mcps
///   --sync-session=off`: see [`crate::flags::CopilotFlags::forced_args`].
/// - `--no-auto-update`, `--disable-client-telemetry`: quiet start.
/// - `--secret-env-vars=COPILOT_GITHUB_TOKEN,GH_TOKEN,GITHUB_TOKEN`: keep
///   the token out of the agent's shell commands.
/// - `--plugin-dir <agent>/copilot/plugin`: the Terrarium hooks.
/// - `--plugin-dir <extension>`: the system-prompt extension package, when
///   [`CopilotOptions::prompt_extension_dir`] is set. Without it, the
///   extension must be installed in the shared home (the image entrypoint
///   does that).
/// - `--log-dir <agent>/copilot/logs`: logs stay with the agent.
/// - `-C <cwd>`.
/// - `--session-id <uuid>` or `--resume <id>`: see [`SessionStart`].
/// - `--model <m>` when the agent sets one.
/// - `--excluded-tools=skill` for an agent with no skill (PLAN §8.5). An
///   agent with skills keeps the tool; its `settings.json` names its
///   skills dir ([`config::skill_settings`]).
///
/// The env: `COPILOT_HOME`, `COPILOT_ALLOW_ALL=true`, the quiet vars, and
/// `COPILOT_CACHE_HOME` (see [`CopilotOptions::cache_dir`]). The token is
/// not here: the daemon adds `COPILOT_GITHUB_TOKEN` (see
/// [`crate::CopilotAdapter::required_env`]).
pub fn build(opts: &CopilotOptions, agent: &Agent, session: SessionStart<'_>) -> LaunchSpec {
    let f = &opts.flags;
    let cwd = opts.harness_cwd(agent);
    let mut spec = LaunchSpec::new(opts.program.as_str(), &cwd).arg(f.yolo);
    for a in f.forced_args {
        spec = spec.arg(*a);
    }
    spec = spec
        .arg(f.no_auto_update)
        .arg(f.disable_telemetry)
        .arg(format!("{}={}", f.secret_env_vars, f.token_env.join(",")))
        .arg(f.plugin_dir)
        .arg(path_str(&config::plugin_dir(agent)));
    if let Some(ext) = &opts.prompt_extension_dir {
        spec = spec.arg(f.plugin_dir).arg(path_str(ext));
    }
    spec = spec
        .arg(f.log_dir)
        .arg(path_str(&config::logs_dir(agent)))
        .arg(f.cwd)
        .arg(path_str(&cwd));
    spec = match session {
        SessionStart::Fresh(uuid) => spec.arg(f.session_id).arg(uuid),
        SessionStart::Resume(id) => spec.arg(f.resume).arg(id.0.as_str()),
    };
    if agent.delivered_skills.is_empty() {
        spec = spec.arg(format!("{}={}", f.excluded_tools, f.skill_tool));
    }
    if let Some(model) = agent.model.as_deref().filter(|m| !m.is_empty()) {
        spec = spec.arg(f.model).arg(model);
    }
    spec = spec
        .env(f.config_env, path_str(&opts.config_dir))
        .env(f.allow_all_env.0, f.allow_all_env.1)
        .env(f.cache_env, path_str(&opts.harness_cache_dir(agent)));
    for (key, value) in f.quiet_env {
        spec = spec.env(*key, *value);
    }
    if let Some(pkg) = &opts.pkg_cache_dir {
        spec = spec.env(f.pkg_cache_env, path_str(pkg));
    }
    spec
}

fn path_str(p: &Path) -> String {
    p.to_string_lossy().into_owned()
}

/// A new random UUID (version 4 layout) for `--session-id`.
pub fn new_session_uuid() -> String {
    // A ULID has 80 random bits. Set the version and variant bits on top.
    let mut n: u128 = terrarium_core::SessionId::new().0.0;
    n = (n & !(0xF << 76)) | (0x4 << 76);
    n = (n & !(0x3 << 62)) | (0x2 << 62);
    let h = format!("{n:032x}");
    format!(
        "{}-{}-{}-{}-{}",
        &h[0..8],
        &h[8..12],
        &h[12..16],
        &h[16..20],
        &h[20..32]
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_agent;
    use std::path::PathBuf;

    fn args(spec: &LaunchSpec) -> Vec<&str> {
        spec.args.iter().map(String::as_str).collect()
    }

    fn value_after<'a>(spec: &'a LaunchSpec, flag: &str) -> Option<&'a str> {
        let i = spec.args.iter().position(|a| a == flag)?;
        spec.args.get(i + 1).map(String::as_str)
    }

    #[test]
    fn fresh_start_has_every_flag() {
        let opts = CopilotOptions::default();
        let agent = test_agent(Some("claude-haiku-4.5"));
        let spec = build(
            &opts,
            &agent,
            SessionStart::Fresh("11111111-2222-4333-8444-555555555555"),
        );
        assert_eq!(spec.program, "copilot");
        assert!(spec.pty);
        assert_eq!(spec.cwd, agent.workspace);
        let a = args(&spec);
        assert_eq!(
            &a[..9],
            [
                "--yolo",
                "--no-ask-user",
                "--mode",
                "interactive",
                "--disable-builtin-mcps",
                "--sync-session=off",
                "--no-auto-update",
                "--disable-client-telemetry",
                "--secret-env-vars=COPILOT_GITHUB_TOKEN,GH_TOKEN,GITHUB_TOKEN",
            ]
        );
        let agent_dir = crate::config::agent_dir(&agent);
        assert_eq!(
            value_after(&spec, "--plugin-dir").map(PathBuf::from),
            Some(agent_dir.join("copilot/plugin"))
        );
        assert_eq!(
            value_after(&spec, "--log-dir").map(PathBuf::from),
            Some(agent_dir.join("copilot/logs"))
        );
        assert_eq!(
            value_after(&spec, "-C").map(PathBuf::from),
            Some(agent.workspace.clone())
        );
        assert_eq!(
            value_after(&spec, "--session-id"),
            Some("11111111-2222-4333-8444-555555555555")
        );
        assert!(!a.contains(&"--resume"));
        // The flag makes Copilot rename over the bound settings.json, which
        // fails with EBUSY: experimental mode comes from the settings copy.
        assert!(!a.contains(&"--experimental"));
        // PLAN §8.5: no skill, no skill tool; with skills, the tool.
        assert!(a.contains(&"--excluded-tools=skill"));
        let mut with = agent.clone();
        with.delivered_skills = vec!["fern-check".into()];
        let s = build(&opts, &with, SessionStart::Fresh("x"));
        assert!(!args(&s).iter().any(|x| x.starts_with("--excluded-tools")));
        assert_eq!(value_after(&spec, "--model"), Some("claude-haiku-4.5"));
        assert_eq!(a.iter().filter(|x| **x == "--plugin-dir").count(), 1);

        assert_eq!(spec.env["COPILOT_HOME"], "/auth/copilot");
        assert_eq!(spec.env["COPILOT_ALLOW_ALL"], "true");
        assert_eq!(spec.env["COPILOT_AUTO_UPDATE"], "false");
        assert_eq!(spec.env["COPILOT_DISABLE_TERMINAL_TITLE"], "1");
        assert_eq!(spec.env["COPILOT_DISABLE_DESKTOP_NOTIFICATIONS"], "1");
        assert_eq!(
            PathBuf::from(&spec.env["COPILOT_CACHE_HOME"]),
            agent_dir.join("copilot/cache")
        );
        assert!(!spec.env.contains_key("COPILOT_PKG_CACHE_HOME"));
        // No secret in the spec: the daemon adds the token.
        assert!(!spec.env.keys().any(|k| k.contains("TOKEN")));
    }

    #[test]
    fn resume_uses_resume_flag() {
        let opts = CopilotOptions {
            prompt_extension_dir: Some(PathBuf::from("/opt/ext")),
            cache_dir: Some(PathBuf::from("/home/agent/.cache/copilot")),
            pkg_cache_dir: Some(PathBuf::from("/var/cache/copilot-pkg")),
            harness_workspace: Some(PathBuf::from("/home/agent/work")),
            config_dir: PathBuf::from("/home/agent/.copilot"),
            ..CopilotOptions::default()
        };
        let id = HarnessSessionId("62e05754-b66a-4ccc-ba6e-4846cc4fc1aa".into());
        let spec = build(&opts, &test_agent(None), SessionStart::Resume(&id));
        assert_eq!(value_after(&spec, "--resume"), Some(id.0.as_str()));
        assert!(!args(&spec).contains(&"--session-id"));
        assert!(!args(&spec).contains(&"--model"));
        assert_eq!(spec.cwd, PathBuf::from("/home/agent/work"));
        assert_eq!(value_after(&spec, "-C"), Some("/home/agent/work"));
        let plugins: Vec<&str> = spec
            .args
            .windows(2)
            .filter(|w| w[0] == "--plugin-dir")
            .map(|w| w[1].as_str())
            .collect();
        assert_eq!(plugins.len(), 2);
        assert_eq!(plugins[1], "/opt/ext");
        assert_eq!(spec.env["COPILOT_HOME"], "/home/agent/.copilot");
        assert_eq!(spec.env["COPILOT_CACHE_HOME"], "/home/agent/.cache/copilot");
        assert_eq!(spec.env["COPILOT_PKG_CACHE_HOME"], "/var/cache/copilot-pkg");
    }

    #[test]
    fn hooks_json_wires_every_event() {
        let opts = CopilotOptions::default();
        let v = hooks_json(&opts);
        assert_eq!(v["version"], 1);
        for name in [
            "SessionStart",
            "UserPromptSubmit",
            "PreToolUse",
            "PermissionRequest",
            "PostToolUse",
            "Stop",
            "SessionEnd",
            "Notification",
            "ErrorOccurred",
        ] {
            let e = &v["hooks"][name][0];
            assert_eq!(e["type"], "command", "{name}");
            assert_eq!(e["bash"], format!("terrarium hook {name}"), "{name}");
            assert_eq!(e["timeoutSec"], 10, "{name}");
        }
        assert_eq!(plugin_manifest()["name"], "terrarium");
    }

    #[test]
    fn session_uuids_are_v4_and_differ() {
        let a = new_session_uuid();
        let b = new_session_uuid();
        assert_ne!(a, b);
        let parts: Vec<&str> = a.split('-').collect();
        assert_eq!(
            parts.iter().map(|p| p.len()).collect::<Vec<_>>(),
            [8, 4, 4, 4, 12]
        );
        assert!(parts[2].starts_with('4'));
        assert!(matches!(parts[3].as_bytes()[0], b'8' | b'9' | b'a' | b'b'));
    }
}
