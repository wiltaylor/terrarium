use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use terrarium_core::AgentId;

use crate::ready::ReadyDetector;

/// The settings of one runner.
#[derive(Clone)]
pub struct RunnerConfig {
    /// The agent that this runner holds.
    pub agent: AgentId,
    /// The agent name, for `whoami`.
    pub name: String,
    /// The project name, for `whoami`.
    pub project: String,
    /// The state root, for example `/state`. The runner writes to
    /// `<state_dir>/agents/<ulid>/`.
    pub state_dir: PathBuf,
    /// The runtime root, for example `/run/terrarium`. The runner socket is
    /// `<runtime_dir>/agents/<ulid>.sock`.
    pub runtime_dir: PathBuf,
    /// The PTY size.
    pub rows: u16,
    pub cols: u16,
    /// A turn that is open with no hook and no PTY output for this long makes
    /// the agent `blocked`.
    pub stall_timeout: Duration,
    /// How long to wait for the prompt-submit hook after a turn is typed.
    pub submit_timeout: Duration,
    /// How many times to type a turn before the delivery fails. PLAN §7 asks
    /// for one retry, so the default is 2.
    pub submit_attempts: u32,
    /// `pty.log` rotates to `pty.log.1` when it grows past this size.
    pub log_max_bytes: u64,
    /// How often the runner checks for prompt-ready, stalls, and timeouts.
    pub tick: Duration,
    /// Overrides the default prompt-ready detector.
    pub ready_detector: Option<Arc<dyn ReadyDetector>>,
}

impl RunnerConfig {
    pub fn new(
        agent: AgentId,
        state_dir: impl Into<PathBuf>,
        runtime_dir: impl Into<PathBuf>,
    ) -> Self {
        Self {
            agent,
            name: String::new(),
            project: String::new(),
            state_dir: state_dir.into(),
            runtime_dir: runtime_dir.into(),
            rows: 40,
            cols: 120,
            stall_timeout: Duration::from_secs(300),
            submit_timeout: Duration::from_secs(10),
            submit_attempts: 2,
            log_max_bytes: 8 * 1024 * 1024,
            tick: Duration::from_millis(50),
            ready_detector: None,
        }
    }

    /// `<state_dir>/agents/<ulid>`.
    pub fn agent_dir(&self) -> PathBuf {
        agent_dir(&self.state_dir, self.agent)
    }

    /// `<runtime_dir>/agents/<ulid>.sock`.
    pub fn socket_path(&self) -> PathBuf {
        socket_path(&self.runtime_dir, self.agent)
    }
}

/// The state directory of one agent: `<state_dir>/agents/<ulid>`.
pub fn agent_dir(state_dir: &Path, agent: AgentId) -> PathBuf {
    state_dir.join("agents").join(agent.to_string())
}

/// The runner socket of one agent: `<runtime_dir>/agents/<ulid>.sock`.
pub fn socket_path(runtime_dir: &Path, agent: AgentId) -> PathBuf {
    runtime_dir.join("agents").join(format!("{agent}.sock"))
}
