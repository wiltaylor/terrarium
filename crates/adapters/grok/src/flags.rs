//! The command-line surface of one Grok Build version, as data.
//!
//! All version-specific names live in one table, [`GROK_1_0`]. The pin is
//! [`GrokFlags::verified`], which comes from `harnesses.toml`. Sources:
//! `grok --help` of 1.0.77 and the public docs (TER-1190, res-1190 F12;
//! <https://docs.x.ai/build/cli/reference>,
//! <https://docs.x.ai/build/features/hooks>). Nothing here was checked in
//! a signed-in TUI yet: that capture is a release gate of TER-1192.

/// The flags, env names, and hooks of one Grok Build version range.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct GrokFlags {
    /// The pinned version, from `harnesses.toml` at compile time.
    pub verified: &'static str,
    /// Start a NEW conversation with this UUID (1.0.77 help: "Use a
    /// specific session UUID for a new conversation"). The headless guide
    /// says it can also resume; the help and the sessions page say new.
    pub session_id: &'static str,
    /// Resume a session by ID (`-r, --resume [<ID>]`).
    pub resume: &'static str,
    /// `-m, --model <MODEL>`.
    pub model: &'static str,
    /// `--cwd <PATH>`.
    pub cwd: &'static str,
    /// The Claude Code alias that appends to the default system prompt
    /// (reference page, "Claude Code flag aliases").
    pub append_system_prompt: &'static str,
    /// The Claude Code alias that replaces the default system prompt.
    pub system_prompt: &'static str,
    /// The env var of the Grok home (`$GROK_HOME`, else `~/.grok`).
    pub home_env: &'static str,
    /// The hooks that Terrarium registers in its personal hook file. Each
    /// runs `terrarium hook <name>`.
    pub hook_events: &'static [&'static str],
    /// The `timeout` (seconds) of each hook entry. The default is 5.
    pub hook_timeout_sec: u32,
    /// Flags that widen the permissions. The adapter never passes them
    /// (card TER-1192, lead triage: no `--trust`, no `--always-approve`,
    /// no broad allow rules). A test checks the launch for each.
    pub forbidden: &'static [&'static str],
}

/// Grok Build 1.0.x, read from 1.0.77.
pub const GROK_1_0: GrokFlags = GrokFlags {
    verified: terrarium_core::harnesses::grok::VERSION,
    session_id: "--session-id",
    resume: "--resume",
    model: "--model",
    cwd: "--cwd",
    append_system_prompt: "--append-system-prompt",
    system_prompt: "--system-prompt",
    home_env: "GROK_HOME",
    hook_events: &[
        "SessionStart",
        "SessionEnd",
        "UserPromptSubmit",
        "Stop",
        "StopFailure",
        "PreToolUse",
        "PostToolUse",
        "PostToolUseFailure",
        "PermissionDenied",
        "Notification",
        "PreCompact",
        "PostCompact",
    ],
    hook_timeout_sec: 10,
    forbidden: &[
        "--always-approve",
        "--yolo",
        "--trust",
        "--dangerously-skip-permissions",
        "--permission-mode",
        "--allow",
        "--allowedTools",
        "--sandbox",
    ],
};
