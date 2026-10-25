//! The command-line surface of one Copilot CLI version, as data.
//!
//! All version-specific names live in one table, [`COPILOT_1_0`]. The
//! version pin is [`CopilotFlags::verified`] and [`CopilotFlags::tested`],
//! which come from `harnesses.toml`; nothing else in the crate names a
//! version. When a Copilot release
//! changes a flag, add a new table and pick it in [`crate::CopilotOptions`].
//! Every entry was checked by the Phase 0 spikes (`docs/compat.md`,
//! "Copilot CLI 1.0.103") unless its doc comment says otherwise.

/// The flags, env names, and hooks of one Copilot CLI version range.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct CopilotFlags {
    /// The pinned version, which the image installs. It comes from
    /// `harnesses.toml` at compile time (`terrarium_core::harnesses`).
    pub verified: &'static str,
    /// The version range that the adapter accepts (a `semver::VersionReq`),
    /// from `harnesses.toml`. `just harness::update` widens it.
    pub tested: &'static str,
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
}

/// Copilot CLI 1.0.x. Measured on 1.0.103 (`docs/compat.md`). Copilot
/// ships patch releases often; the range accepts later 1.0 patches, but
/// only 1.0.103 was measured.
pub const COPILOT_1_0: CopilotFlags = CopilotFlags {
    verified: terrarium_core::harnesses::copilot::VERSION,
    tested: terrarium_core::harnesses::copilot::TESTED,
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
};
