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
        }
    }

    /// The env var the harness reads for its config home.
    pub fn home_env(self) -> &'static str {
        match self {
            Self::Claude => "CLAUDE_CONFIG_DIR",
            Self::Codex => "CODEX_HOME",
            Self::Copilot => "COPILOT_HOME",
            Self::Pi => "PI_CODING_AGENT_DIR",
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
            .find(|h| h.as_str() == s)
            .ok_or_else(|| format!("unknown harness `{s}`"))
    }
}
