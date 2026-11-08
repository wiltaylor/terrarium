//! The command-line surface of one Claude Code version, as data.
//!
//! All version-specific names live in one table, [`CLAUDE_2_1`]. When a new
//! Claude Code release changes a flag, add a new table and pick it in
//! [`crate::ClaudeOptions`]. Every entry was checked by the Phase 0 spikes
//! (`docs/compat.md`) unless its doc comment says otherwise.

/// The flags, env names, and hooks of one Claude Code version range.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ClaudeFlags {
    /// The pinned version, which the image installs. It comes from
    /// `harnesses.toml` at compile time (`terrarium_core::harnesses`).
    pub verified: &'static str,
    /// Yolo. Claude Code refuses it as root unless [`Self::root_env`] is set.
    pub yolo: &'static str,
    /// Takes a JSON string or a file path. Adds settings, edits no file.
    pub settings: &'static str,
    /// Takes a comma list of `user`, `project`, `local`. Empty loads none.
    pub setting_sources: &'static str,
    /// A fresh session with a known UUID. Fails if the UUID was used before.
    pub session_id: &'static str,
    /// Continue a session by ID, from any cwd.
    pub resume: &'static str,
    pub model: &'static str,
    /// Adds text after the default system prompt.
    pub append_system_prompt: &'static str,
    /// Replaces the default system prompt, but keeps its first line.
    pub system_prompt: &'static str,
    /// Flag and value that stop Claude Code from freezing the first prompt.
    pub system_prompt_snapshot_off: [&'static str; 2],
    /// The env var for the config home.
    pub config_env: &'static str,
    /// Env vars that keep the start quiet and offline where possible.
    pub quiet_env: &'static [(&'static str, &'static str)],
    /// The env var that lets yolo run as root.
    pub root_env: (&'static str, &'static str),
    /// The hooks that Terrarium registers. Each runs `terrarium hook <name>`.
    pub hook_events: &'static [&'static str],
    /// The settings keys that Terrarium forces (beside the hooks): they
    /// win over the user settings file (PLAN §8.2). Only keys that an
    /// agent needs to start and run unattended go here; a key that only
    /// tunes the harness goes in the packaged default
    /// `defaults/harness/claude/settings.json`.
    pub quiet_settings: &'static [(&'static str, bool)],
    /// The settings keys with a string value that Terrarium forces, as
    /// [`Self::quiet_settings`]. A dotted key (`permissions.disableAutoMode`)
    /// names a key in an object.
    pub forced_string_settings: &'static [(&'static str, &'static str)],
    /// The tools that `permissions.deny` always holds (D5). A bare tool
    /// name removes the tool from the model's context, in every permission
    /// mode, and no other settings level can allow it again (Claude Code
    /// docs, "permissions"). The deny rules of the user settings file stay:
    /// the adapter adds these to them.
    pub forced_deny: &'static [&'static str],
    /// The settings keys that Terrarium removes from the user settings:
    /// each one would stop the Terrarium hooks, or load skills that
    /// Terrarium does not manage.
    pub removed_settings: &'static [&'static str],
    /// Flags that every launch gets, after the yolo flag.
    pub forced_args: &'static [&'static str],
    /// Loads a plugin from a directory (PLAN §8.6: the agent's skills).
    /// In `--help` of 2.1.310. Not gated on `--setting-sources`: an inline
    /// plugin is on unless `enabledPlugins` turns it off (bundle marker
    /// `lt.enabled=Aqt(lt.source,...)`).
    pub plugin_dir: &'static str,
    /// The `name` in the agent's plugin manifest. The model sees each
    /// skill as `<name>:<skill>` (bundle marker
    /// `ft=`${n}:${Ee.name.replace(...)}``).
    pub plugin_name: &'static str,
    /// The tool that runs a skill. An agent with no skill gets it in
    /// `permissions.deny`: a bare deny removes the tool and the skill
    /// list (bundle marker
    /// `if(!e.options.tools.some((we)=>Rt(we,Ao)))return[]`).
    pub skill_tool: &'static str,
}

/// Claude Code 2.1.x. Measured on 2.1.310 (`docs/compat.md`).
pub const CLAUDE_2_1: ClaudeFlags = ClaudeFlags {
    verified: terrarium_core::harnesses::claude::VERSION,
    yolo: "--dangerously-skip-permissions",
    settings: "--settings",
    setting_sources: "--setting-sources",
    session_id: "--session-id",
    resume: "--resume",
    model: "--model",
    append_system_prompt: "--append-system-prompt",
    system_prompt: "--system-prompt",
    system_prompt_snapshot_off: ["--system-prompt-snapshot", "off"],
    config_env: "CLAUDE_CONFIG_DIR",
    quiet_env: &[
        // Verified: no changelog fetch, no promotion banner.
        ("CLAUDE_CODE_DISABLE_NONESSENTIAL_TRAFFIC", "1"),
        // Verified (indirect): `auth status` shows `analyticsDisabled`.
        ("DISABLE_TELEMETRY", "1"),
        // Read only: the names are in the binary.
        ("DISABLE_AUTOUPDATER", "1"),
        ("DISABLE_ERROR_REPORTING", "1"),
        // Read only. An agent must not log out the shared account.
        ("DISABLE_LOGOUT_COMMAND", "1"),
        // The rest come from docs/research/harness-hardening.md (Claude
        // Code, 2.2): read in the docs (env-vars page) and the 2.1.310
        // bundle, not run. Each one keeps an unattended agent from a
        // dialog, a login or update flow, a turn that nobody typed, or an
        // outside service. The env wins over a settings file.
        // No `claude update` from inside the agent.
        ("DISABLE_UPDATES", "1"),
        // No login flow from inside the agent.
        ("DISABLE_LOGIN_COMMAND", "1"),
        // No feedback upload, and no survey that eats typed digits.
        ("DISABLE_FEEDBACK_COMMAND", "1"),
        ("CLAUDE_CODE_SEND_FEEDBACK", "0"),
        ("CLAUDE_CODE_DISABLE_FEEDBACK_SURVEY", "1"),
        // The operator's claude.ai connectors (mail, drives) stay out of
        // agents; the settings key `disableClaudeAiConnectors` is forced too.
        ("ENABLE_CLAUDEAI_MCP_SERVERS", "false"),
        // No self-scheduled turns (`CronCreate`, `/loop`): Terrarium has its
        // own schedules (PLAN §12b).
        ("CLAUDE_CODE_DISABLE_CRON", "1"),
        // PLAN §8.6: no managed-policy skills (`<managed>/.claude/skills`
        // loads with `--setting-sources ""` too). Bundle marker
        // `i.CLAUDE_CODE_DISABLE_POLICY_SKILLS||...`; not run.
        ("CLAUDE_CODE_DISABLE_POLICY_SKILLS", "1"),
        // No marketplace install at start.
        ("CLAUDE_CODE_DISABLE_OFFICIAL_MARKETPLACE_AUTOINSTALL", "1"),
        // Commands that start a flow with a human.
        ("DISABLE_INSTALL_GITHUB_APP_COMMAND", "1"),
        ("DISABLE_UPGRADE_COMMAND", "1"),
        ("DISABLE_EXTRA_USAGE_COMMAND", "1"),
        // No side request for a terminal title.
        ("CLAUDE_CODE_DISABLE_TERMINAL_TITLE", "1"),
        // No predicted prompt in the input box: the screen detector reads
        // text in the box as "not ready", and a turn would not go in.
        ("CLAUDE_CODE_ENABLE_PROMPT_SUGGESTION", "false"),
        // No background work in the harness: Terrarium has `terrarium run`
        // and the waits for that. Read in the 2.1.310 bundle: with it set,
        // the Bash schema drops `run_in_background`
        // (`ap()?rn().omit({run_in_background:!0})`), and no command or
        // MCP call moves to the background by itself. Not run.
        ("CLAUDE_CODE_DISABLE_BACKGROUND_TASKS", "1"),
        // Never set `CLAUDE_CODE_DISABLE_DANGEROUS_RM_TIMEOUT`: it removes
        // the 2-minute auto-deny of the critical-path `rm` prompt, so the
        // prompt would wait forever.
    ],
    root_env: ("IS_SANDBOX", "1"),
    hook_events: &[
        "SessionStart",
        "UserPromptSubmit",
        "PreToolUse",
        "PostToolUse",
        // Not captured. Registered so a failed tool call still closes.
        "PostToolUseFailure",
        "Stop",
        // Not captured. Registered so a turn that ends in an API error
        // still closes.
        "StopFailure",
        "PermissionRequest",
        "Notification",
        "SessionEnd",
        // The start and the end of a compaction (docs/compat.md,
        // "Compaction"). PostCompact fires only after a compaction that
        // did not fail.
        "PreCompact",
        "PostCompact",
    ],
    quiet_settings: &[
        // Forced: without it, yolo mode shows a dialog at each start, and
        // no one is there to answer it.
        ("skipDangerousModePermissionPrompt", true),
        // The usage-limit menu ("Stop and wait for limit to reset",
        // "Upgrade your plan") waits for a key; with this key on, Claude
        // Code waits for the reset by itself.
        ("autoContinueAtUsageLimit", true),
        // Outside services that an agent must not reach: the operator's
        // claude.ai connectors, Remote Control (another client could type
        // into the agent), and the upload of the sessions to claude.ai.
        ("disableClaudeAiConnectors", true),
        ("disableRemoteControl", true),
        ("remoteControlAtStartup", false),
        ("autoUploadSessions", false),
        // PLAN §8.6: only Terrarium-managed skills, the same on every
        // harness. The bundled skills (`schedule`, `claude-api`, ...) stay
        // off; the agent's own skills come from its plugin dir. Bundle
        // marker `e.source==="builtin"&&...`: the key hides only the
        // bundled ones.
        ("disableBundledSkills", true),
        // `autoMemoryEnabled = false` is not forced any more: it is in the
        // packaged default `defaults/harness/claude/settings.json`.
        // Terrarium has its own memory graph (PLAN §14), and the
        // per-agent `projects/<cwd>/memory/` overlay (`config.rs`) keeps
        // an agent's auto memory its own when the operator turns it on.
    ],
    forced_string_settings: &[
        // D5: no goal proposal with a one-key approval.
        ("modelProposedGoals", "disabled"),
        // No auto mode: its classifier can ask before an action. The
        // 2.1.310 schema has the top-level key; the docs name the one in
        // `permissions`. Both until one is tested.
        ("disableAutoMode", "disable"),
        ("permissions.disableAutoMode", "disable"),
        // A safety net only: an `AskUserQuestion` that still shows goes on
        // by itself after 60 s. The deny below removes the tool.
        ("askUserQuestionTimeout", "60s"),
        // A message from another local session is held for approval.
        ("crossSessionInbound", "refuse"),
        // Operator rule: no `Co-Authored-By` or other co-author line in a
        // commit, and no "Generated with" line in a PR (TER-127). An empty
        // string hides each text (schema: "Empty string hides
        // attribution"). Read in the 2.1.319 bundle: `CJo` takes
        // `attribution.commit` and `attribution.pr` before the deprecated
        // `includeCoAuthoredBy`, and `fg` adds no attribution reminder to a
        // fresh session when both are empty. A session that already got
        // the reminder gets "From here on, do not add attribution lines"
        // one time. A user `attribution: true` becomes this object.
        ("attribution.commit", ""),
        ("attribution.pr", ""),
    ],
    forced_deny: &[
        // D5: the tools that ask a human and wait. Plan mode ends with a
        // plan approval that never resolves on its own.
        "AskUserQuestion",
        "EnterPlanMode",
        "ExitPlanMode",
        "ProposeGoal",
        // A worktree changes the cwd under Terrarium: the workspace, the
        // trust key, and the session dir are all keyed by the cwd.
        "EnterWorktree",
        "ExitWorktree",
        // Sub-agents, task lists, and background work: Terrarium does
        // these (leads make workers, cards on the board, `terrarium run`,
        // `wait`, `watch`). Each name is a tool constant in the 2.1.310
        // bundle; `Task` is an alias of `Agent`, and `KillShell` of
        // `TaskStop`, so the deny covers them. `TaskOutput`, `BashOutput`,
        // and the team tools are not tools in 2.1.310.
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
        // Turns that nobody typed (`CLAUDE_CODE_DISABLE_CRON` too).
        "CronCreate",
        "CronDelete",
        "CronList",
        "ScheduleWakeup",
    ],
    // `disableAllHooks` would stop the Terrarium hooks, and so the status
    // machine, the turns, and the usage.
    // PLAN §8.6: `enabledPlugins` could turn on a plugin of the shared
    // home (and its skills), or turn off the agent's own plugin.
    removed_settings: &["disableAllHooks", "enabledPlugins"],
    forced_args: &[
        // No Claude in Chrome (its setup is a dialog). In `--help`.
        "--no-chrome",
        // Only `--mcp-config` servers, and Terrarium passes none: no
        // "New MCP server found in this project" dialog for a `.mcp.json`
        // in a repo. In `--help`; the dialog part is not tested.
        "--strict-mcp-config",
    ],
    plugin_dir: "--plugin-dir",
    plugin_name: "terrarium",
    skill_tool: "Skill",
};
