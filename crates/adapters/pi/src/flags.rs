//! The command-line surface of one pi version, as data.
//!
//! All version-specific names live in one table, [`PI_0_85`]. When a new pi
//! release changes a flag, add a new table and pick it in
//! [`crate::PiOptions`]. Every entry was checked by the Phase 0 spikes
//! (`docs/compat.md`, "pi 0.85.1") or by the adapter's own live check on
//! 0.85.1, unless its doc comment says "read only" (from `pi --help` and
//! the installed source).

/// The flags and env names of one pi version range.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PiFlags {
    /// The pinned version, which the image installs. It comes from
    /// `harnesses.toml` at compile time (`terrarium_core::harnesses`).
    pub verified: &'static str,
    /// The version range that the adapter accepts (a `semver::VersionReq`),
    /// from `harnesses.toml`. `just harness::update` widens it.
    pub tested: &'static str,
    /// The provider id, for example `openai-codex` (see [`crate::provider`]).
    pub provider: &'static str,
    /// A model id or pattern. `provider/id` and a `:<thinking>` suffix work.
    pub model: &'static str,
    /// Load one extension file. A path that does not exist is fatal: pi
    /// prints "Extension path does not exist" and does not start.
    pub extension: &'static str,
    /// No extension discovery (the shared home's `extensions/`, packages,
    /// the project). Explicit `-e` paths still load. Verified: with a
    /// dummy extension in `$PI_CODING_AGENT_DIR/extensions/`, only
    /// `terrarium.ts` loaded.
    pub no_extensions: &'static str,
    /// No skills. Without it, pi loads `~/.agents/skills` even with a
    /// temporary `PI_CODING_AGENT_DIR`.
    pub no_skills: &'static str,
    /// No prompt templates (`/name` expansions). Read only.
    pub no_prompt_templates: &'static str,
    /// No `AGENTS.md` / `CLAUDE.md`, also not the one in the shared home.
    /// Verified: an `AGENTS.md` in the cwd did not load.
    pub no_context_files: &'static str,
    /// Ignore project-local `.pi/` files for this run: no "Trust project
    /// folder?" dialog. pi shows one line, "This project is not trusted".
    pub no_approve: &'static str,
    /// No start-up network calls: no update notice.
    pub offline: &'static str,
    /// Create or resume the session with this exact ID, in the session
    /// directory, for this cwd. It never fails.
    pub session_id: &'static str,
    /// Where the session files go. The files lie flat in the directory
    /// (`<UTC time>_<id>.jsonl`); `--session-id` looks up IDs there.
    pub session_dir: &'static str,
    /// Replaces the whole default prompt (also pi's tool guidance). Takes
    /// text or a file path (read in the installed source).
    pub system_prompt: &'static str,
    /// Appends to the default prompt. Takes text or a file path.
    pub append_system_prompt: &'static str,
    /// The env var for the config home.
    pub config_env: &'static str,
    /// Env vars that keep the start quiet.
    pub quiet_env: &'static [(&'static str, &'static str)],
}

/// pi 0.85.x. Measured on 0.85.1 (`docs/compat.md`).
pub const PI_0_85: PiFlags = PiFlags {
    verified: terrarium_core::harnesses::pi::VERSION,
    tested: terrarium_core::harnesses::pi::TESTED,
    provider: "--provider",
    model: "--model",
    extension: "-e",
    no_extensions: "--no-extensions",
    no_skills: "--no-skills",
    no_prompt_templates: "--no-prompt-templates",
    no_context_files: "--no-context-files",
    no_approve: "--no-approve",
    offline: "--offline",
    session_id: "--session-id",
    session_dir: "--session-dir",
    system_prompt: "--system-prompt",
    append_system_prompt: "--append-system-prompt",
    config_env: "PI_CODING_AGENT_DIR",
    quiet_env: &[
        // Read only: the name is in `pi --help`. Set in every spike.
        ("PI_TELEMETRY", "0"),
        // Verified: the same as `--offline` (no update notice).
        ("PI_OFFLINE", "1"),
    ],
};
