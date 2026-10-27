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
    /// The version range that the adapter accepts (a `semver::VersionReq`),
    /// from `harnesses.toml`. `just harness::update` widens it.
    pub tested: &'static str,
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
    /// The settings keys that Terrarium removes from the user settings:
    /// each one would stop the Terrarium hooks.
    pub removed_settings: &'static [&'static str],
}

/// Claude Code 2.1.x. Measured on 2.1.310 (`docs/compat.md`).
pub const CLAUDE_2_1: ClaudeFlags = ClaudeFlags {
    verified: terrarium_core::harnesses::claude::VERSION,
    tested: terrarium_core::harnesses::claude::TESTED,
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
    ],
    quiet_settings: &[
        // Forced: without it, yolo mode shows a dialog at each start, and
        // no one is there to answer it.
        ("skipDangerousModePermissionPrompt", true),
        // `autoMemoryEnabled = false` is not forced any more: it is in the
        // packaged default `defaults/harness/claude/settings.json`.
        // Terrarium has its own memory graph (PLAN §14), and the
        // per-agent `projects/<cwd>/memory/` overlay (`config.rs`) keeps
        // an agent's auto memory its own when the operator turns it on.
    ],
    // `disableAllHooks` would stop the Terrarium hooks, and so the status
    // machine, the turns, and the usage.
    removed_settings: &["disableAllHooks"],
};
