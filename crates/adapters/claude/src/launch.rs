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

/// The `--settings` JSON, in three layers (PLAN §8.2):
///
/// 1. The user settings file (`Agent::harness_settings`, from
///    `<config repo>/harness/claude/settings.json`). With
///    `--setting-sources ""`, Claude Code reads no other settings file,
///    so this is the only place for the operator's own keys.
/// 2. The per-agent options: none today (`--model` is a flag).
/// 3. The forced keys, which always win:
///    - `hooks`: the Terrarium hooks replace any `hooks` of the user
///      file. The runner learns each turn, tool call, and question from
///      them.
///    - [`crate::ClaudeFlags::quiet_settings`]: the keys that keep the
///      start free of dialogs.
///    - [`crate::ClaudeFlags::removed_settings`]: removed (they would stop
///      the hooks).
///    - `env`: the names that the launch sets (`CLAUDE_CONFIG_DIR`, the
///      quiet env, `IS_SANDBOX`) are removed from the user `env`, so the
///      home and the quiet start stay as the sandbox needs them.
///
/// Each hook runs `<hook_program> hook <Name>`. The CLI forwards the stdin
/// JSON to `TERRARIUM_SOCKET`. The payload also names the event in
/// `hook_event_name`, so the argument is only a convenience.
pub fn settings_json(opts: &ClaudeOptions, agent: &Agent) -> Value {
    let f = &opts.flags;
    let mut root = match &agent.harness_settings {
        Some(Value::Object(m)) => m.clone(),
        _ => Map::new(),
    };
    let mut hooks = Map::new();
    for name in f.hook_events {
        let command = format!("{} hook {name}", opts.hook_program);
        hooks.insert(
            (*name).to_owned(),
            json!([{"matcher": "", "hooks": [{"type": "command", "command": command}]}]),
        );
    }
    root.insert("hooks".into(), Value::Object(hooks));
    for (key, value) in f.quiet_settings {
        root.insert((*key).to_owned(), Value::Bool(*value));
    }
    for (key, value) in f.forced_string_settings {
        set_dotted(&mut root, key, Value::String((*value).to_owned()));
    }
    // The forced deny rules join the user's deny rules (D5). A user file
    // cannot take one out: the adapter adds each one again at each start.
    let permissions = root
        .entry("permissions")
        .or_insert_with(|| Value::Object(Map::new()));
    if !permissions.is_object() {
        *permissions = Value::Object(Map::new());
    }
    if let Value::Object(p) = permissions {
        let deny = p.entry("deny").or_insert_with(|| Value::Array(Vec::new()));
        if !deny.is_array() {
            *deny = Value::Array(Vec::new());
        }
        if let Value::Array(list) = deny {
            for tool in f.forced_deny {
                if !list.iter().any(|v| v.as_str() == Some(tool)) {
                    list.push(Value::String((*tool).to_owned()));
                }
            }
        }
        // An `allow` or `ask` rule for a forced tool would not win over the
        // deny, but it says the wrong thing: remove it.
        for kind in ["allow", "ask"] {
            if let Some(Value::Array(list)) = p.get_mut(kind) {
                list.retain(|v| !v.as_str().is_some_and(|r| f.forced_deny.contains(&r)));
            }
        }
    }
    for key in f.removed_settings {
        root.remove(*key);
    }
    if let Some(Value::Object(env)) = root.get_mut("env") {
        env.remove(f.config_env);
        env.remove(f.root_env.0);
        for (key, _) in f.quiet_env {
            env.remove(*key);
        }
    }
    Value::Object(root)
}

/// Set `key` in `root`. A dotted key names a key in an object; a value
/// on the way that is not an object becomes one.
fn set_dotted(root: &mut Map<String, Value>, key: &str, value: Value) {
    match key.split_once('.') {
        None => {
            root.insert(key.to_owned(), value);
        }
        Some((head, rest)) => {
            let child = root
                .entry(head)
                .or_insert_with(|| Value::Object(Map::new()));
            if !child.is_object() {
                *child = Value::Object(Map::new());
            }
            if let Value::Object(m) = child {
                set_dotted(m, rest, value);
            }
        }
    }
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
/// - `--settings '<json>'`: the user settings with the forced keys on top;
///   see [`settings_json`].
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
    let settings = settings_json(opts, agent).to_string();
    let mut spec = LaunchSpec::new(opts.program.as_str(), cwd).arg(f.yolo);
    for a in f.forced_args {
        spec = spec.arg(*a);
    }
    spec = spec
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
        let flag = match opts.prompt_mode_for(agent) {
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
        // Not forced: it comes from the user settings file.
        assert!(s.get("autoMemoryEnabled").is_none());
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

    /// The user settings come first; the forced keys win over them.
    #[test]
    fn user_settings_layer_under_the_forced_keys() {
        let opts = ClaudeOptions::default();
        let mut agent = test_agent(None);
        agent.harness_settings = Some(json!({
            "autoMemoryEnabled": false,
            "theme": "dark",
            "permissions": {"deny": ["WebFetch"]},
            "skipDangerousModePermissionPrompt": false,
            "disableAllHooks": true,
            "hooks": {"Stop": [{"hooks": [{"type": "command", "command": "evil"}]}],
                      "WorktreeCreate": []},
            "env": {"CLAUDE_CONFIG_DIR": "/elsewhere", "DISABLE_TELEMETRY": "0", "MY_VAR": "1"},
        }));
        let spec = build(&opts, &agent, SessionStart::Fresh("x"), None, false);
        let s: Value = serde_json::from_str(value_after(&spec, "--settings").expect("settings"))
            .expect("json");
        // Tweakable keys pass through.
        assert_eq!(s["autoMemoryEnabled"], false);
        assert_eq!(s["theme"], "dark");
        assert_eq!(s["permissions"]["deny"][0], "WebFetch");
        assert_eq!(s["env"]["MY_VAR"], "1");
        // Forced keys win.
        assert_eq!(s["skipDangerousModePermissionPrompt"], true);
        assert!(s.get("disableAllHooks").is_none());
        assert_eq!(
            s["hooks"]["Stop"][0]["hooks"][0]["command"],
            "terrarium hook Stop"
        );
        assert!(s["hooks"].get("WorktreeCreate").is_none());
        assert!(s["env"].get("CLAUDE_CONFIG_DIR").is_none());
        assert!(s["env"].get("DISABLE_TELEMETRY").is_none());
        // The launch env still sets them.
        assert_eq!(spec.env["CLAUDE_CONFIG_DIR"], "/auth/claude");
        assert_eq!(spec.env["DISABLE_TELEMETRY"], "1");
    }

    /// The packaged default settings file parses, and holds only keys
    /// that Terrarium does not force.
    #[test]
    fn the_packaged_default_has_no_forced_key() {
        let path = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("../../../defaults/harness/claude/settings.json");
        let v = terrarium_core::harness_settings::load(&path, terrarium_core::HarnessId::Claude)
            .expect("parses")
            .expect("exists");
        let f = ClaudeOptions::default().flags;
        for key in v.as_object().expect("object").keys() {
            assert!(key != "hooks", "hooks are forced");
            assert!(
                !f.quiet_settings.iter().any(|(k, _)| k == key),
                "{key} is forced"
            );
            assert!(!f.removed_settings.contains(&key.as_str()), "{key}");
            assert!(
                !f.forced_string_settings.iter().any(|(k, _)| k == key),
                "{key} is forced"
            );
        }
        // The packaged deny list holds the tweakable denies only.
        let deny = v["permissions"]["deny"].as_array().expect("deny list");
        for t in deny {
            let t = t.as_str().expect("name");
            assert!(!f.forced_deny.contains(&t), "{t} is forced");
        }
        assert!(deny.iter().any(|t| t == "Artifact"));
        assert_eq!(v["autoMemoryEnabled"], false);
    }

    /// D5: the asking tools and plan mode stay off whatever the user file
    /// says; the user's own deny rules stay.
    #[test]
    fn a_user_file_cannot_turn_the_asking_tools_on() {
        let opts = ClaudeOptions::default();
        let mut agent = test_agent(None);
        agent.harness_settings = Some(json!({
            "permissions": {
                "deny": ["WebFetch", "AskUserQuestion"],
                "allow": ["EnterPlanMode", "Bash(ls:*)"],
                "disableAutoMode": "enable",
            },
            "modelProposedGoals": "auto",
            "disableAutoMode": false,
            "askUserQuestionTimeout": "never",
            "autoContinueAtUsageLimit": false,
            "disableRemoteControl": false,
            "switchModelsOnFlag": false,
        }));
        let spec = build(&opts, &agent, SessionStart::Fresh("x"), None, false);
        let s: Value = serde_json::from_str(value_after(&spec, "--settings").expect("settings"))
            .expect("json");
        let deny: Vec<&str> = s["permissions"]["deny"]
            .as_array()
            .expect("deny")
            .iter()
            .filter_map(Value::as_str)
            .collect();
        assert_eq!(deny[0], "WebFetch");
        for tool in [
            "AskUserQuestion",
            "EnterPlanMode",
            "ExitPlanMode",
            "ProposeGoal",
            "EnterWorktree",
            "ExitWorktree",
        ] {
            assert_eq!(deny.iter().filter(|t| **t == tool).count(), 1, "{tool}");
        }
        assert_eq!(s["permissions"]["allow"], json!(["Bash(ls:*)"]));
        assert_eq!(s["permissions"]["disableAutoMode"], "disable");
        assert_eq!(s["disableAutoMode"], "disable");
        assert_eq!(s["modelProposedGoals"], "disabled");
        assert_eq!(s["askUserQuestionTimeout"], "60s");
        assert_eq!(s["crossSessionInbound"], "refuse");
        assert_eq!(s["autoContinueAtUsageLimit"], true);
        assert_eq!(s["disableRemoteControl"], true);
        assert_eq!(s["disableClaudeAiConnectors"], true);
        assert_eq!(s["autoUploadSessions"], false);
        // A tweakable key passes through.
        assert_eq!(s["switchModelsOnFlag"], false);
        // The forced flags and env.
        let a = args(&spec);
        assert!(a.contains(&"--no-chrome") && a.contains(&"--strict-mcp-config"));
        assert_eq!(spec.env["DISABLE_UPDATES"], "1");
        assert_eq!(spec.env["CLAUDE_CODE_ENABLE_PROMPT_SUGGESTION"], "false");
        assert_eq!(spec.env["ENABLE_CLAUDEAI_MCP_SERVERS"], "false");
        assert!(
            !spec
                .env
                .contains_key("CLAUDE_CODE_DISABLE_DANGEROUS_RM_TIMEOUT")
        );
    }

    /// Sub-agents, task lists, and background work stay off whatever the
    /// user file says: Terrarium does them (workers, cards, `terrarium run`).
    #[test]
    fn a_user_file_cannot_turn_sub_agents_tasks_or_background_on() {
        let opts = ClaudeOptions::default();
        let mut agent = test_agent(None);
        agent.harness_settings = Some(json!({
            "permissions": {
                "allow": ["Agent", "TodoWrite", "Monitor", "Bash(ls:*)"],
                "ask": ["TaskStop"],
            },
            "env": {"CLAUDE_CODE_DISABLE_BACKGROUND_TASKS": "0"},
        }));
        let spec = build(&opts, &agent, SessionStart::Fresh("x"), None, false);
        let s: Value = serde_json::from_str(value_after(&spec, "--settings").expect("settings"))
            .expect("json");
        let deny: Vec<&str> = s["permissions"]["deny"]
            .as_array()
            .expect("deny")
            .iter()
            .filter_map(Value::as_str)
            .collect();
        for tool in [
            "Agent",
            "SendMessage",
            "ListAgents",
            "Workflow",
            "TodoWrite",
            "TaskCreate",
            "TaskGet",
            "TaskUpdate",
            "TaskList",
            "TaskStop",
            "Monitor",
            "CronCreate",
            "CronDelete",
            "CronList",
            "ScheduleWakeup",
        ] {
            assert_eq!(deny.iter().filter(|t| **t == tool).count(), 1, "{tool}");
        }
        assert_eq!(s["permissions"]["allow"], json!(["Bash(ls:*)"]));
        assert_eq!(s["permissions"]["ask"], json!([]));
        // The env of the launch wins; the settings `env` loses the name.
        assert_eq!(spec.env["CLAUDE_CODE_DISABLE_BACKGROUND_TASKS"], "1");
        assert!(
            s["env"]
                .get("CLAUDE_CODE_DISABLE_BACKGROUND_TASKS")
                .is_none()
        );
        // The core tools stay.
        for keep in ["Bash", "Read", "Edit", "Write", "Skill", "ToolSearch"] {
            assert!(!deny.contains(&keep), "{keep}");
        }
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

    /// No base system prompt: `--append-system-prompt` with the Terrarium
    /// prompt only. A base system prompt: `--system-prompt` with the
    /// composed text (base, blank line, Terrarium prompt), and the snapshot
    /// stays off so a change applies on resume (PLAN §8).
    #[test]
    fn a_base_system_prompt_selects_replace() {
        let mut agent = test_agent(None);
        let spec = build(
            &ClaudeOptions::default(),
            &agent,
            SessionStart::Fresh("x"),
            Some("# Terrarium agent a"),
            false,
        );
        assert_eq!(
            value_after(&spec, "--append-system-prompt"),
            Some("# Terrarium agent a")
        );
        assert!(!args(&spec).contains(&"--system-prompt"));
        agent.system_prompt = Some("You review code.".into());
        let text = "You review code.\n\n# Terrarium agent a";
        let spec = build(
            &ClaudeOptions::default(),
            &agent,
            SessionStart::Resume(&HarnessSessionId("s".into())),
            Some(text),
            false,
        );
        assert_eq!(value_after(&spec, "--system-prompt"), Some(text));
        assert!(!args(&spec).contains(&"--append-system-prompt"));
        assert_eq!(value_after(&spec, "--system-prompt-snapshot"), Some("off"));
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
