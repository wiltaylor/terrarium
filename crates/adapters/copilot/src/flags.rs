//! The command-line surface of one Copilot CLI version, as data.
//!
//! All version-specific names live in one table, [`COPILOT_1_0`]. The
//! version pin is [`CopilotFlags::verified`], which comes from
//! `harnesses.toml`; nothing else in the crate names a version. When a Copilot release
//! changes a flag, add a new table and pick it in [`crate::CopilotOptions`].
//! Every entry was checked by the Phase 0 spikes (`docs/compat.md`,
//! "Copilot CLI 1.0.103") unless its doc comment says otherwise.

/// The flags, env names, and hooks of one Copilot CLI version range.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct CopilotFlags {
    /// The pinned version, which the image installs. It comes from
    /// `harnesses.toml` at compile time (`terrarium_core::harnesses`).
    pub verified: &'static str,
    /// Experimental mode. The system-prompt extension loads only in it.
    /// The adapter does **not** pass this flag: with it, Copilot writes
    /// `experimental: true` to `settings.json` at each start by a rename.
    /// The per-agent `settings.json` is a file bind, so the rename fails
    /// with `EBUSY`, and Copilot exits with code 1 and no output (verified
    /// in the container on 2026-10-25). The adapter sets
    /// [`Self::experimental_key`] in that file instead. Kept here for the
    /// `--help` check.
    pub experimental: &'static str,
    /// The `settings.json` key that `--experimental` persists. With
    /// `true` there, the extension loads with no flag (verified: the log
    /// says "Installed 1 native extension(s)"; with no key and no flag it
    /// does not), and Copilot does not rewrite the file at start.
    pub experimental_key: &'static str,
    /// Allow All mode. It does not skip the folder-trust dialog; see
    /// [`Self::allow_all_env`].
    pub yolo: &'static str,
    /// Load a plugin from a directory. The plugin's root `hooks.json` fires;
    /// a `hooks/hooks.json` in the plugin does not.
    pub plugin_dir: &'static str,
    /// Set the UUID of a new session. A used UUID resumes that session.
    pub session_id: &'static str,
    /// Resume a session by ID. A missing ID exits with an error.
    pub resume: &'static str,
    pub model: &'static str,
    /// Change the working directory before anything else.
    pub cwd: &'static str,
    /// Quiet start (verified: the start screen has no update notice).
    pub no_auto_update: &'static str,
    pub disable_telemetry: &'static str,
    /// Where Copilot writes its process logs. Verified in a probe on
    /// 2026-10-24: the log file went to the given directory.
    pub log_dir: &'static str,
    /// Strip these env vars from the shell and MCP environments and redact
    /// their values. Takes a comma list after `=`. Verified only that
    /// Copilot starts with it; the stripping is read from `--help`.
    pub secret_env_vars: &'static str,
    /// The env var for the config home.
    pub config_env: &'static str,
    /// Must be exactly `true`: that value also trusts the folder (no
    /// trust dialog). `1` only auto-approves tools.
    pub allow_all_env: (&'static str, &'static str),
    /// Env vars that keep the start quiet.
    pub quiet_env: &'static [(&'static str, &'static str)],
    /// The cache home (default `$XDG_CACHE_HOME/copilot`). Copilot keeps
    /// feature flags, user cache, and the unpacked runtime (`pkg/`, about
    /// 236 MB for 1.0.103) there. Read from the binary; verified in a probe
    /// that the files go to the given directory.
    pub cache_env: &'static str,
    /// Only the unpacked runtime (`pkg/`). Read from the binary, not tested.
    pub pkg_cache_env: &'static str,
    /// The token env vars, in the order of precedence. The first one is the
    /// one Terrarium sets.
    pub token_env: &'static [&'static str],
    /// The hooks that Terrarium registers in the plugin's `hooks.json`.
    /// Each runs `terrarium hook <name>`.
    pub hook_events: &'static [&'static str],
    /// The `timeoutSec` of each hook entry.
    pub hook_timeout_sec: u32,
    /// Flags that every launch gets (`docs/research/harness-hardening.md`,
    /// Copilot CLI, 2a). Each is in `copilot --help` of 1.0.103; a start
    /// with them is not tested.
    pub forced_args: &'static [&'static str],
    /// The flag that hides tools from the model. It takes one quoted,
    /// comma-separated list (GitHub docs, "CLI command reference"); the
    /// 1.0.103 parser accepts `--excluded-tools=a,b` (checked with
    /// `--version` in a scratch home).
    pub excluded_tools_flag: &'static str,
    /// The tools that every launch hides: sub-agents, task lists, and the
    /// context tools. Terrarium does that work (workers, cards, `wait`).
    /// An unknown name is not an error (checked as above). The settings
    /// cannot add a hidden tool back: the catalog has no tool-list key.
    /// The adapter adds [`Self::skill_tool`] for an agent with no skill.
    pub excluded_tools: &'static [&'static str],
    /// The tool that loads a skill (PLAN §8.6). Its description lists no
    /// skill; the list is `<available_skills>` in the system message.
    pub skill_tool: &'static str,
    /// The built-in skills (`builtin-skills/` of the package). The adapter
    /// puts each in `disabledSkills`: only Terrarium-managed skills load.
    pub builtin_skills: &'static [&'static str],
}

/// Copilot CLI 1.0.x. Measured on 1.0.103 (`docs/compat.md`). Copilot
/// ships patch releases often; the range accepts later 1.0 patches, but
/// only 1.0.103 was measured.
pub const COPILOT_1_0: CopilotFlags = CopilotFlags {
    verified: terrarium_core::harnesses::copilot::VERSION,
    experimental: "--experimental",
    experimental_key: "experimental",
    yolo: "--yolo",
    plugin_dir: "--plugin-dir",
    session_id: "--session-id",
    resume: "--resume",
    model: "--model",
    cwd: "-C",
    no_auto_update: "--no-auto-update",
    disable_telemetry: "--disable-client-telemetry",
    log_dir: "--log-dir",
    secret_env_vars: "--secret-env-vars",
    config_env: "COPILOT_HOME",
    allow_all_env: ("COPILOT_ALLOW_ALL", "true"),
    quiet_env: &[
        // Verified: `--no-auto-update` and this env together.
        ("COPILOT_AUTO_UPDATE", "false"),
        // Used in every spike run. Not tested alone.
        ("COPILOT_DISABLE_TERMINAL_TITLE", "1"),
        // No desktop notification (there is no display in the sandbox).
        // Read in the 1.0.103 bundle.
        ("COPILOT_DISABLE_DESKTOP_NOTIFICATIONS", "1"),
    ],
    cache_env: "COPILOT_CACHE_HOME",
    pkg_cache_env: "COPILOT_PKG_CACHE_HOME",
    token_env: &["COPILOT_GITHUB_TOKEN", "GH_TOKEN", "GITHUB_TOKEN"],
    hook_events: &[
        // Captured on the plugin route (spike 1).
        "SessionStart",
        "UserPromptSubmit",
        "PreToolUse",
        "PermissionRequest",
        "PostToolUse",
        "Stop",
        "SessionEnd",
        // Captured on the home route only (spike 5): `elicitation_dialog`
        // and `shell_completed`.
        "Notification",
        // Registered by the spikes, never seen to fire.
        "PostToolUseFailure",
        "SubagentStart",
        "SubagentStop",
        "PreCompact",
        "ErrorOccurred",
    ],
    hook_timeout_sec: 10,
    forced_args: &[
        // D5: no `ask_user` tool (with `"askUser": false` in the settings).
        "--no-ask-user",
        // D5: the flag wins over `defaultMode` and, very likely, over the
        // stored mode of a resumed session: no plan mode, no autopilot.
        "--mode",
        "interactive",
        // No built-in MCP servers (`github-mcp-server`, `githubiq`): no
        // GitHub API tools with the login token.
        "--disable-builtin-mcps",
        // No session export to GitHub for this run.
        "--sync-session=off",
    ],
    excluded_tools_flag: "--excluded-tools",
    excluded_tools: &[
        // Sub-agents: `task` and the tools that manage a background agent
        // (verified: in the tool list of a 1.0.103 session,
        // `spikes/copilot/session-id-and-resume/captured/events.jsonl`).
        "task",
        "read_agent",
        "list_agents",
        "write_agent",
        "search_code_subagent",
        // Sub-agents, workflows, schedules, and the sidekick inbox: tool
        // modules in the 1.0.103 runtime (`src/runtime/src/tools/`), not
        // sent in the captured session.
        "execution_subagent",
        "run_dynamic_workflow",
        "dynamic_workflows_manage",
        "manage_schedule",
        "context_board",
        "read_inbox",
        "send_inbox",
        // Task lists: the `sql` tool holds the `todos` and `todo_deps`
        // tables; `update_todo` is the old Markdown list. The whole `sql`
        // tool goes: no setting removes only the tables.
        "sql",
        "update_todo",
        // The context tools (also off by `contextManagementTools`). With
        // the setting off, and with no extension loaded for the two
        // extension tools below, Copilot does not register them and shows
        // "Unknown tool name in the tool excludedlist" for each (1.0.103
        // and 1.0.105; the names are the same in both `runtime.node`).
        // The names stay, in case a later setting registers the tools.
        "session_artifacts",
        "get_context_remaining",
        "session_history",
        "new_context",
        // Seen in a live 1.0.103 tool list on 2026-10-28: the canvas UI,
        // the extension manager (it could turn off the system-prompt
        // extension that carries the agent's prompt), and the session
        // store query tool.
        "list_canvas_capabilities",
        "open_canvas",
        "invoke_canvas_action",
        "extensions_reload",
        "extensions_manage",
        "session_store_sql",
        // Removed by the operator's choice (2026-10-28): Copilot's own
        // documentation lookup, and the GitHub code index search.
        "fetch_copilot_cli_documentation",
        "semantic_code_search",
        // Not hidden: `bash`, `read_bash`, `write_bash`, `list_bash`,
        // `stop_bash`. A `sync` command that runs past `initial_wait`
        // moves to the background by itself (the bash tool text in the
        // runtime), and only `read_bash` and `stop_bash` read or stop it.
        // No flag or setting turns off `mode: "async"` or `detach`; the
        // harness prompt says to use `terrarium run` instead.
    ],
    skill_tool: "skill",
    builtin_skills: &[
        "customize-cloud-agent",
        "github-pr-media",
        "discover-resources",
    ],
};
