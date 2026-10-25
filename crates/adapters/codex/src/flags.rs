//! The command-line surface of one Codex version, as data.
//!
//! All version-specific names live in one table, [`CODEX_0_160`]. When a new
//! Codex release changes a flag or a config key, add a new table and pick it
//! in [`crate::CodexOptions`]. Every entry was checked by the Phase 0 spikes
//! (`docs/compat.md`, "Codex 0.160.0") unless its doc comment says
//! otherwise.

/// The flags, config keys, and hooks of one Codex version range.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct CodexFlags {
    /// The version that the spikes measured.
    pub verified: &'static str,
    /// The version range that the adapter accepts (a `semver::VersionReq`).
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

/// Codex 0.160.x. Measured on 0.160.0 (`docs/compat.md`).
pub const CODEX_0_160: CodexFlags = CodexFlags {
    verified: "0.160.0",
    tested: ">=0.160.0, <0.161.0",
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
    ],
};

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_table_has_every_hook_that_the_binary_names() {
        assert_eq!(CODEX_0_160.hook_events.len(), 11);
        assert!(!CODEX_0_160.hook_events.contains(&"Notification"));
        let v = semver::Version::parse(CODEX_0_160.verified).expect("v");
        let req = semver::VersionReq::parse(CODEX_0_160.tested).expect("req");
        assert!(req.matches(&v));
    }
}
