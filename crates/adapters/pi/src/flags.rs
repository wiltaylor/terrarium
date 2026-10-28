//! The command-line surface of the pinned pi version, as data.
//!
//! All version-specific names live in one table, [`PI_PINNED`]. The pin and
//! the range come from `harnesses.toml`, so the name has no version in it.
//! When a new pi release changes a flag, change the table (or add a second
//! one and pick it in [`crate::PiOptions`]). Every entry was checked by the
//! Phase 0 spikes on 0.85.1 (`docs/compat.md`, "pi 0.85.1"), by the
//! adapter's own live check, or by the 1.4.0 check (`docs/compat.md`,
//! "pi 1.4.0"), unless its doc comment says "read only" (from `pi --help`
//! and the installed source).
//!
//! [`default_model`] is the model that an agent with a subscription and no
//! `model` gets: pi 1.4 refuses `--provider` without `--model`.

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
    /// Since 1.4 it only narrows the search for `--model`: alone it is
    /// fatal ("Error: --provider requires --model"), and pi does not start.
    pub provider: &'static str,
    /// A model id or pattern. `provider/id` and a `:<thinking>` suffix work.
    pub model: &'static str,
    /// Load one extension file. A path that does not exist is fatal: pi
    /// prints "Extension path does not exist" and does not start.
    pub extension: &'static str,
    /// No extension discovery (the shared home's `extensions/`, packages,
    /// the project). Explicit `-e` paths still load. Verified: with a
    /// dummy extension in `$PI_CODING_AGENT_DIR/extensions/`, only
    /// `terrarium.ts` loaded. Since 1.4 it also turns off pi's built-in
    /// extensions (`-e builtin:<name>` loads one again).
    pub no_extensions: &'static str,
    /// No built-in MCP (new in 1.4): no servers connect and no MCP tools.
    /// Without it, pi reads `mcp.json` in the shared home, which is the
    /// operator's. Read only (`pi --help`); in the 1.4.0 start check.
    pub no_mcp: &'static str,
    /// The TUI mode flag and its value. 1.4 changed the default from
    /// `regular` to `fullscreen` (alternate screen and mouse tracking).
    /// `regular` keeps the screen that the detector's 0.85.1 captures show.
    pub tui_mode: (&'static str, &'static str),
    /// No skills. Without it, pi loads `~/.agents/skills` even with a
    /// temporary `PI_CODING_AGENT_DIR`.
    pub no_skills: &'static str,
    /// Load one skill dir (repeatable; PLAN §8.5). It loads also with
    /// [`Self::no_skills`], which stops only the discovered and settings
    /// paths (`dist/core/resource-loader.js:424`, `this.noSkills ?
    /// this.mergePaths(cliEnabledSkills, ...)`).
    pub skill: &'static str,
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

/// The pinned pi (`harnesses.toml`). Measured on 0.85.1 and checked again
/// on 1.4.0 (`docs/compat.md`).
pub const PI_PINNED: PiFlags = PiFlags {
    verified: terrarium_core::harnesses::pi::VERSION,
    tested: terrarium_core::harnesses::pi::TESTED,
    provider: "--provider",
    model: "--model",
    extension: "-e",
    no_extensions: "--no-extensions",
    no_mcp: "--no-mcp",
    tui_mode: ("--tui-mode", "regular"),
    no_skills: "--no-skills",
    skill: "--skill",
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

/// The default model of each pi provider, for an agent with a
/// subscription and no `model`. The agent definition's `model` overrides
/// it.
///
/// Why a table: without `--model`, pi takes `defaultProvider` and
/// `defaultModel` from `settings.json` (the agent's copy of the operator's
/// file, and later the last `/model` choice), and else the first provider
/// with a login, in its own order (`anthropic` comes before
/// `openai-codex`). Both can spend a different subscription. A model id of
/// the subscription's provider keeps that provider in force.
///
/// The ids are the cheap tier of each provider, from the 1.4.0 model
/// catalog (`pi-ai/dist/providers/*.models.js`). pi's own defaults
/// (`defaultModelPerProvider`) are the top tier. Read only: no turn ran
/// with them.
pub const DEFAULT_MODELS: &[(&str, &str)] = &[
    ("openai-codex", "gpt-6-luna"),
    ("openai", "gpt-6-luna"),
    // An Anthropic API key only: pi refuses a Claude plan login
    // ([`crate::provider::refusal`]).
    ("anthropic", "claude-haiku-5-5"),
    ("github-copilot", "gpt-6-luna"),
    ("google", "gemini-3.1-flash-lite"),
    ("openrouter", "openai/gpt-6-luna"),
    ("xai", "grok-4.3"),
    ("groq", "openai/gpt-oss-20b"),
    ("deepseek", "deepseek-flash"),
    ("mistral", "mistral-small-latest"),
    // Z.ai: the GLM Coding Plan (built-in `zai`) and pay as you go (the
    // custom `zai-api`). From pi 1.4.0's `zai` catalog and docs.z.ai.
    ("zai", terrarium_core::zai::MODEL_FAST),
    ("zai-api", terrarium_core::zai::MODEL_FAST),
];

/// The default model id for a pi provider id, or `None` when the table
/// has no row (for example a custom provider in the shared `models.json`).
pub fn default_model(provider: &str) -> Option<&'static str> {
    DEFAULT_MODELS
        .iter()
        .find(|(p, _)| *p == provider)
        .map(|(_, m)| *m)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_oauth_and_key_provider_has_a_default_model() {
        for p in [
            "openai-codex",
            "openai",
            "anthropic",
            "github-copilot",
            "google",
            "openrouter",
            "xai",
            "groq",
            "deepseek",
            "mistral",
            "zai",
            "zai-api",
        ] {
            assert!(default_model(p).is_some(), "{p}");
        }
        assert_eq!(default_model("openai-codex"), Some("gpt-6-luna"));
        assert_eq!(default_model("glm53-exl3"), None);
        assert_eq!(default_model("zai"), Some("glm-5.3-flash"));
    }
}
