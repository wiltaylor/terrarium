use std::fmt;

use serde::{Deserialize, Serialize};

/// The status of an agent. See PLAN §5 for the rules.
///
/// - `working`: a turn is open (prompt-submit hook fired, stop hook has not).
/// - `idle`: no turn is open, the process is alive, and the agent has no
///   open wait ("done").
/// - `waiting`: no turn is open, the process is alive, and the agent has
///   one or more open waits (PLAN §12d). The daemon derives it from `idle`;
///   the runner never reports it.
/// - `blocked`: the harness asked a question, or a turn stalled.
/// - `error`: the process exited non-zero, or the restart budget is spent.
/// - `stopped`: a user or the API stopped the agent on purpose.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize, Default)]
#[serde(rename_all = "snake_case")]
pub enum AgentStatus {
    Starting,
    Idle,
    Waiting,
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
            Self::Waiting => "waiting",
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
            Self::Starting | Self::Idle | Self::Waiting | Self::Working | Self::Blocked
        )
    }

    /// True when no turn is open and the harness takes a turn: `idle` or
    /// `waiting`.
    pub fn is_resting(self) -> bool {
        matches!(self, Self::Idle | Self::Waiting)
    }

    /// The status that the daemon shows for a runner status `self` and
    /// `open_waits` open waits (PLAN §12d). Only `idle` changes: with an
    /// open wait it is `waiting`. Every other status wins over a wait.
    pub fn with_waits(self, open_waits: usize) -> Self {
        match self {
            Self::Idle | Self::Waiting if open_waits > 0 => Self::Waiting,
            Self::Waiting => Self::Idle,
            other => other,
        }
    }
}

impl fmt::Display for AgentStatus {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::*;

    /// The precedence table of PLAN §12d: only a resting harness shows the
    /// waits.
    #[test]
    fn waits_change_only_a_resting_status() {
        use AgentStatus::*;
        for s in [Starting, Working, Blocked, Error, Stopped, Restarting] {
            assert_eq!(s.with_waits(0), s);
            assert_eq!(s.with_waits(3), s);
        }
        assert_eq!(Idle.with_waits(0), Idle);
        assert_eq!(Idle.with_waits(1), Waiting);
        assert_eq!(Waiting.with_waits(0), Idle);
        assert_eq!(Waiting.with_waits(2), Waiting);
        assert!(Waiting.is_live() && Waiting.is_resting());
        assert_eq!(serde_json::to_value(Waiting).unwrap(), "waiting");
        assert_eq!(Waiting.to_string(), "waiting");
    }
}
