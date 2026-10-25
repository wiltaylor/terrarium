//! The command-line surface of the pinned Codex version, as data.
//!
//! All version-specific names live in one table, [`CODEX_PINNED`]. The pin
//! and the range come from `harnesses.toml`, so the name has no version in
//! it. When a new Codex release changes a flag or a config key, change the
//! table (or add a second one and pick it in [`crate::CodexOptions`]).
//! Every entry was checked by the Phase 0 spikes (`docs/compat.md`,
//! "Codex 0.160.0") and again on 0.164.0 ("Codex 0.164.0") unless its doc
//! comment says otherwise.

/// The flags, config keys, and hooks of one Codex version range.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct CodexFlags {
    /// The pinned version, which the image installs. It comes from
    /// `harnesses.toml` at compile time (`terrarium_core::harnesses`).
    pub verified: &'static str,
    /// The version range that the adapter accepts (a `semver::VersionReq`),
    /// from `harnesses.toml`. `just harness::update` widens it.
    pub tested: &'static str,
    /// The subcommand that continues a session: `codex resume <id>`. The
    /// session ID stays the same. A missing ID exits at once with
    /// `ERROR: No saved session found with ID <id>.`
    pub resume: &'static str,
    /// Run the TUI without the shared app-server daemon. Without it (and
    /// with no `-c` override), Codex starts a detached
    /// `codex app-server --managed-daemon` and an update loop. Both outlive
    /// the TUI and copy 736 MB into `$CODEX_HOME/packages/`. Always pass it.
    pub no_daemon: &'static str,
    /// Yolo: no approvals, no Codex sandbox. The header shows
    /// "permissions: YOLO mode". No "are you sure" dialog.
    pub yolo: &'static str,
    /// Hooks from `-c` show a "Hooks need review" dialog without this flag.
    /// With it, the start shows a "⚠ 1 warning" line, and no dialog.
    pub hook_trust: &'static str,
    /// `-c key=value`. The value is parsed as TOML; when that fails, the
    /// raw string is used.
    pub config: &'static str,
    /// `-C <dir>`: the working directory of the session.
    pub cd: &'static str,
    pub model: &'static str,
    /// The env var for the config home.
    pub home_env: &'static str,
    /// `-c` overrides that keep the start quiet and offline where possible.
    /// Each is `(key, TOML value)`.
    pub quiet_config: &'static [(&'static str, &'static str)],
    /// `-c` overrides for `codex resume` only. `tui.resume_cwd="session"`
    /// removes the "Working directory · resume" dialog.
    pub resume_config: &'static [(&'static str, &'static str)],
    /// The config key that replaces the whole base prompt with a file.
    /// `session_meta.base_instructions` then holds the file text. A missing
    /// file stops the start (`failed to read model instructions file`).
    pub model_instructions_file: &'static str,
    /// The config key that adds a `developer` message. The default prompt
    /// stays.
    pub developer_instructions: &'static str,
    /// The config key for the directory of the SQLite files (`state_5`,
    /// `logs_2`, `memories_1`, `goals_1`, `queue_1`, `thread_history_1`).
    /// Default: `CODEX_HOME`. Verified on 0.160.0 with `--strict-config`:
    /// all the SQLite files go to the named directory, and none to
    /// `CODEX_HOME`. A new directory backfills its `threads` table from
    /// `$CODEX_HOME/sessions/` at the start.
    pub sqlite_home: &'static str,
    /// The hooks that Terrarium registers. Each runs `terrarium hook
    /// <name>`. The list comes from the binary: Codex counts 11 hooks when
    /// all 11 are passed. An unknown name is ignored without a message.
    pub hook_events: &'static [&'static str],
}

/// The pinned Codex (`harnesses.toml`). Measured on 0.160.0 and checked
/// again on 0.164.0 (`docs/compat.md`).
pub const CODEX_PINNED: CodexFlags = CodexFlags {
    verified: terrarium_core::harnesses::codex::VERSION,
    tested: terrarium_core::harnesses::codex::TESTED,
    resume: "resume",
    no_daemon: "--no-daemon",
    yolo: "--dangerously-bypass-approvals-and-sandbox",
    hook_trust: "--dangerously-bypass-hook-trust",
    config: "-c",
    cd: "-C",
    model: "-m",
    home_env: "CODEX_HOME",
    quiet_config: &[
        // Verified: removes the "Update available!" box.
        ("check_for_update_on_startup", "false"),
        // Known keys (`--strict-config` accepts them). Their effect on the
        // network traffic is not measured.
        ("analytics.enabled", "false"),
        ("feedback.enabled", "false"),
    ],
    resume_config: &[("tui.resume_cwd", "\"session\"")],
    model_instructions_file: "model_instructions_file",
    developer_instructions: "developer_instructions",
    sqlite_home: "sqlite_home",
    hook_events: &[
        // Fires at the first prompt, not at start-up (0.45 s after the
        // Enter byte). No session file exists before it.
        "SessionStart",
        "UserPromptSubmit",
        "PreToolUse",
        "PostToolUse",
        "Stop",
        "SessionEnd",
        // Not captured: it did not fire in yolo mode. Registered so a
        // question still reaches the runner if one comes.
        "PermissionRequest",
        // Not captured. Registered so that the operator's hooks for the
        // same names in a shared `config.toml` are replaced, and so the
        // events show in the log.
        "SubagentStart",
        "SubagentStop",
        "PreCompact",
        "PostCompact",
        // Not registered: `PreModelRequest` (new in 0.164.0) and
        // `Interrupt`. `PreModelRequest` runs before every model request,
        // not once per turn, so a hook process would delay each request,
        // and its payload and the effect of its output are not captured.
        // Register it for logging only after a spike (`docs/compat.md`,
        // "Codex 0.164.0").
    ],
};

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_table_has_every_hook_that_the_binary_names() {
        assert_eq!(CODEX_PINNED.hook_events.len(), 11);
        assert!(!CODEX_PINNED.hook_events.contains(&"Notification"));
        assert!(!CODEX_PINNED.hook_events.contains(&"PreModelRequest"));
        let v = semver::Version::parse(CODEX_PINNED.verified).expect("v");
        let req = semver::VersionReq::parse(CODEX_PINNED.tested).expect("req");
        assert!(req.matches(&v));
    }
}
