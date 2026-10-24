use std::fmt;

use serde::{Deserialize, Serialize};

/// The status of an agent. See PLAN §5 for the rules.
///
/// - `working`: a turn is open (prompt-submit hook fired, stop hook has not).
/// - `idle`: no turn is open and the process is alive.
/// - `blocked`: the harness asked a question, or a turn stalled.
/// - `error`: the process exited non-zero, or the restart budget is spent.
/// - `stopped`: a user or the API stopped the agent on purpose.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize, Default)]
#[serde(rename_all = "snake_case")]
pub enum AgentStatus {
    Starting,
    Idle,
    Working,
    Blocked,
    Error,
    #[default]
    Stopped,
    Restarting,
}

impl AgentStatus {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Starting => "starting",
            Self::Idle => "idle",
            Self::Working => "working",
            Self::Blocked => "blocked",
            Self::Error => "error",
            Self::Stopped => "stopped",
            Self::Restarting => "restarting",
        }
    }

    /// True when the harness process is expected to be alive.
    pub fn is_live(self) -> bool {
        matches!(
            self,
            Self::Starting | Self::Idle | Self::Working | Self::Blocked
        )
    }
}

impl fmt::Display for AgentStatus {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}
