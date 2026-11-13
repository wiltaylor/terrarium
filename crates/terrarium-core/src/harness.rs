use std::fmt;
use std::str::FromStr;

use serde::{Deserialize, Serialize};

/// A coding-agent CLI that Terrarium can wrap.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum HarnessId {
    Claude,
    Codex,
    Copilot,
    Pi,
    /// Grok Build (`grok`), the harness of an xAI plan (`crate::xai`).
    /// A value only: it is not in [`HarnessId::ALL`] (no pin, no installer,
    /// no adapter) until the grok adapter lands (TER-1192), and a launch of
    /// it is refused.
    Grok,
}

impl HarnessId {
    pub const ALL: [HarnessId; 4] = [Self::Claude, Self::Codex, Self::Copilot, Self::Pi];

    /// The name used in config files, paths, and the API.
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Claude => "claude",
            Self::Codex => "codex",
            Self::Copilot => "copilot",
            Self::Pi => "pi",
            Self::Grok => "grok",
        }
    }

    /// The program name of the harness binary.
    pub fn program(self) -> &'static str {
        self.as_str()
    }

    /// The shared harness home inside the container.
    pub fn auth_dir(self) -> &'static str {
        match self {
            Self::Claude => "/auth/claude",
            Self::Codex => "/auth/codex",
            Self::Copilot => "/auth/copilot",
            Self::Pi => "/auth/pi",
            Self::Grok => "/auth/grok",
        }
    }

    /// The TUI command that compacts the session context, when the daemon
    /// can run it and see its end (PLAN §18, "Auto compact"). The
    /// adapter maps the start and end signals of the harness to
    /// `Event::CompactStarted` and `Event::CompactFinished`.
    /// docs/compat.md, "Compaction", has the evidence for each harness.
    /// `None`: the daemon does not compact agents of this harness.
    pub fn compact_command(self) -> Option<&'static str> {
        match self {
            Self::Claude | Self::Codex | Self::Copilot | Self::Pi => Some("/compact"),
            // Not checked yet (TER-1192).
            Self::Grok => None,
        }
    }

    /// The env var the harness reads for its config home.
    pub fn home_env(self) -> &'static str {
        match self {
            Self::Claude => "CLAUDE_CONFIG_DIR",
            Self::Codex => "CODEX_HOME",
            Self::Copilot => "COPILOT_HOME",
            Self::Pi => "PI_CODING_AGENT_DIR",
            // Grok Build reads `~/.grok` (its login is `~/.grok/auth.json`):
            // its home is the dir that holds `.grok`.
            Self::Grok => "HOME",
        }
    }
}

impl fmt::Display for HarnessId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

impl FromStr for HarnessId {
    type Err = String;
    fn from_str(s: &str) -> Result<Self, Self::Err> {
        Self::ALL
            .into_iter()
            .chain([Self::Grok])
            .find(|h| h.as_str() == s)
            .ok_or_else(|| format!("unknown harness `{s}`"))
    }
}
