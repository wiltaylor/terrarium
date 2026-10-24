//! The command line for a fresh start or a resume (PLAN §7, §8).

use std::path::Path;

use serde_json::{Map, Value, json};
use terrarium_core::{Agent, HarnessSessionId, LaunchSpec};

use crate::{ClaudeOptions, PromptMode};

/// How the session starts.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SessionStart<'a> {
    /// A new session with this UUID (`--session-id`). Use a new UUID each
    /// time: Claude Code exits with "Session ID ... is already in use" on a
    /// UUID that it saw before.
    Fresh(&'a str),
    /// Continue this session (`--resume`). The session ID stays the same.
    Resume(&'a HarnessSessionId),
}

/// The `--settings` JSON: the Terrarium hooks and the keys that stop the
/// start-up dialogs.
///
/// Each hook runs `<hook_program> hook <Name>`. The CLI forwards the stdin
/// JSON to `TERRARIUM_SOCKET`. The payload also names the event in
/// `hook_event_name`, so the argument is only a convenience.
pub fn settings_json(opts: &ClaudeOptions) -> Value {
    let mut hooks = Map::new();
    for name in opts.flags.hook_events {
        let command = format!("{} hook {name}", opts.hook_program);
        hooks.insert(
            (*name).to_owned(),
            json!([{"matcher": "", "hooks": [{"type": "command", "command": command}]}]),
        );
    }
    let mut root = Map::new();
    root.insert("hooks".into(), Value::Object(hooks));
    for (key, value) in opts.flags.quiet_settings {
        root.insert((*key).to_owned(), Value::Bool(*value));
    }
    Value::Object(root)
}

/// Build the launch spec. `prompt` is the rendered system prompt text, if
/// the agent has one. This is pure: [`crate::ClaudeAdapter`] reads the
/// prompt file and the uid, and then calls this.
///
/// The flags, in order:
///
/// - `--dangerously-skip-permissions`: yolo; the sandbox holds the agent.
/// - `--setting-sources ""`: load no user, project, or local settings. Only
///   the `--settings` hooks fire. A consequence: Claude Code does not load
///   `$CLAUDE_CONFIG_DIR/CLAUDE.md` either, so the operator's personal
///   instructions stay out of agents. Whether a `CLAUDE.md` in the
///   workspace still loads is not tested.
/// - `--settings '<json>'`: see [`settings_json`].
/// - `--system-prompt-snapshot off`: without it, Claude Code freezes the
///   system prompt at the first request, and a `--resume` with a changed
///   template still sends the old prompt.
/// - `--session-id <uuid>` or `--resume <id>`: see [`SessionStart`].
/// - `--model <m>` when the agent sets one.
/// - `--append-system-prompt <text>` (default) or `--system-prompt <text>`
///   (see [`PromptMode`]).
pub fn build(
    opts: &ClaudeOptions,
    agent: &Agent,
    session: SessionStart<'_>,
    prompt: Option<&str>,
    as_root: bool,
) -> LaunchSpec {
    let f = &opts.flags;
    let cwd = opts.harness_cwd(agent);
    let settings = settings_json(opts).to_string();
    let mut spec = LaunchSpec::new(opts.program.as_str(), cwd)
        .arg(f.yolo)
        .arg(f.setting_sources)
        .arg("")
        .arg(f.settings)
        .arg(settings)
        .arg(f.system_prompt_snapshot_off[0])
        .arg(f.system_prompt_snapshot_off[1]);
    spec = match session {
        SessionStart::Fresh(uuid) => spec.arg(f.session_id).arg(uuid),
        SessionStart::Resume(id) => spec.arg(f.resume).arg(id.0.as_str()),
    };
    if let Some(model) = agent.model.as_deref().filter(|m| !m.is_empty()) {
        spec = spec.arg(f.model).arg(model);
    }
    if let Some(text) = prompt.map(str::trim).filter(|t| !t.is_empty()) {
        let flag = match opts.prompt_mode {
            PromptMode::Append => f.append_system_prompt,
            PromptMode::Replace => f.system_prompt,
        };
        spec = spec.arg(flag).arg(text);
    }
    spec = spec.env(f.config_env, path_str(&opts.config_dir));
    for (key, value) in f.quiet_env {
        spec = spec.env(*key, *value);
    }
    if as_root {
        spec = spec.env(f.root_env.0, f.root_env.1);
    }
    spec
}

fn path_str(p: &Path) -> String {
    p.to_string_lossy().into_owned()
}

/// A new random UUID (version 4 layout) for `--session-id`. Claude Code
/// asks for "a valid UUID".
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

    fn args(spec: &LaunchSpec) -> Vec<&str> {
        spec.args.iter().map(String::as_str).collect()
    }

    fn value_after<'a>(spec: &'a LaunchSpec, flag: &str) -> Option<&'a str> {
        let i = spec.args.iter().position(|a| a == flag)?;
        spec.args.get(i + 1).map(String::as_str)
    }

    #[test]
    fn fresh_start_has_every_flag() {
        let opts = ClaudeOptions::default();
        let agent = test_agent(Some("haiku"));
        let spec = build(
            &opts,
            &agent,
            SessionStart::Fresh("11111111-2222-4333-8444-555555555555"),
            Some("Be brief."),
            false,
        );
        assert_eq!(spec.program, "claude");
        assert!(spec.pty);
        assert_eq!(spec.cwd, agent.workspace);
        let a = args(&spec);
        assert_eq!(a[0], "--dangerously-skip-permissions");
        assert_eq!(value_after(&spec, "--setting-sources"), Some(""));
        assert_eq!(
            value_after(&spec, "--session-id"),
            Some("11111111-2222-4333-8444-555555555555")
        );
        assert!(!a.contains(&"--resume"));
        assert_eq!(value_after(&spec, "--system-prompt-snapshot"), Some("off"));
        assert_eq!(value_after(&spec, "--model"), Some("haiku"));
        assert_eq!(
            value_after(&spec, "--append-system-prompt"),
            Some("Be brief.")
        );
        assert!(!a.contains(&"--system-prompt"));
        assert_eq!(spec.env["CLAUDE_CONFIG_DIR"], "/auth/claude");
        assert_eq!(spec.env["DISABLE_AUTOUPDATER"], "1");
        assert_eq!(spec.env["DISABLE_TELEMETRY"], "1");
        assert_eq!(spec.env["CLAUDE_CODE_DISABLE_NONESSENTIAL_TRAFFIC"], "1");
        assert!(!spec.env.contains_key("IS_SANDBOX"));
    }

    #[test]
    fn settings_carry_hooks_and_skip_prompt() {
        let opts = ClaudeOptions::default();
        let spec = build(
            &opts,
            &test_agent(None),
            SessionStart::Fresh("x"),
            None,
            false,
        );
        let s: Value = serde_json::from_str(value_after(&spec, "--settings").expect("settings"))
            .expect("json");
        assert_eq!(s["skipDangerousModePermissionPrompt"], true);
        for name in [
            "SessionStart",
            "UserPromptSubmit",
            "PreToolUse",
            "PostToolUse",
            "Stop",
            "PermissionRequest",
            "Notification",
            "SessionEnd",
        ] {
            assert_eq!(
                s["hooks"][name][0]["hooks"][0]["command"],
                format!("terrarium hook {name}"),
                "{name}"
            );
            assert_eq!(s["hooks"][name][0]["hooks"][0]["type"], "command");
        }
        assert!(s["hooks"].get("WorktreeCreate").is_none());
    }

    #[test]
    fn resume_never_reuses_session_id() {
        let opts = ClaudeOptions::default();
        let id = HarnessSessionId("aee36d42-3246-4e16-be9c-16dd97c85c57".into());
        let spec = build(
            &opts,
            &test_agent(None),
            SessionStart::Resume(&id),
            None,
            false,
        );
        assert_eq!(value_after(&spec, "--resume"), Some(id.0.as_str()));
        assert!(!args(&spec).contains(&"--session-id"));
        assert!(!args(&spec).contains(&"--model"));
        assert!(!args(&spec).contains(&"--append-system-prompt"));
        assert_eq!(value_after(&spec, "--system-prompt-snapshot"), Some("off"));
    }

    #[test]
    fn replace_mode_and_root() {
        let opts = ClaudeOptions {
            prompt_mode: PromptMode::Replace,
            ..ClaudeOptions::default()
        };
        let spec = build(
            &opts,
            &test_agent(None),
            SessionStart::Fresh("x"),
            Some("  You are Terrarium agent a.  \n"),
            true,
        );
        assert_eq!(
            value_after(&spec, "--system-prompt"),
            Some("You are Terrarium agent a.")
        );
        assert_eq!(spec.env["IS_SANDBOX"], "1");
    }

    #[test]
    fn empty_prompt_adds_no_flag() {
        let spec = build(
            &ClaudeOptions::default(),
            &test_agent(None),
            SessionStart::Fresh("x"),
            Some(" \n"),
            false,
        );
        assert!(!args(&spec).contains(&"--append-system-prompt"));
    }

    #[test]
    fn session_uuids_are_v4_and_differ() {
        let a = new_session_uuid();
        let b = new_session_uuid();
        assert_ne!(a, b);
        assert_eq!(a.len(), 36);
        let parts: Vec<&str> = a.split('-').collect();
        assert_eq!(
            parts.iter().map(|p| p.len()).collect::<Vec<_>>(),
            [8, 4, 4, 4, 12]
        );
        assert!(parts[2].starts_with('4'));
        assert!(matches!(parts[3].as_bytes()[0], b'8' | b'9' | b'a' | b'b'));
    }
}
