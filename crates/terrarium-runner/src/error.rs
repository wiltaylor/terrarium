use std::path::PathBuf;

/// Errors from the runner.
#[derive(Debug, thiserror::Error)]
pub enum RunnerError {
    #[error("{action} {path}: {source}")]
    Io {
        action: &'static str,
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },
    #[error("pty: {0}")]
    Pty(String),
    #[error("the child process has exited")]
    Exited,
    #[error("{0}")]
    Other(String),
    /// The runner process answered a control call with an error.
    #[error("runner: {0}")]
    Control(crate::protocol::RpcError),
    /// The control connection to the runner process is closed.
    #[error("the connection to the runner process is closed")]
    Disconnected,
    /// Another live runner process holds this agent.
    #[error("a runner process (pid {pid}) already holds agent {agent}")]
    AlreadyRunning { agent: String, pid: String },
}

impl RunnerError {
    pub(crate) fn io(
        action: &'static str,
        path: impl Into<PathBuf>,
        source: std::io::Error,
    ) -> Self {
        Self::Io {
            action,
            path: path.into(),
            source,
        }
    }
}

pub type Result<T, E = RunnerError> = std::result::Result<T, E>;

/// Why a typed turn was not delivered.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum DeliveryError {
    /// Neither the prompt-submit hook nor a busy screen came after every
    /// attempt.
    #[error("no prompt-submit hook after {attempts} attempts")]
    NoSubmitHook { attempts: u32 },
    /// The child exited before the turn was delivered.
    #[error("the child exited before the turn was delivered")]
    Exited,
    /// The write to the PTY failed.
    #[error("cannot write to the pty: {0}")]
    Write(String),
    /// The runner dropped the turn without an answer.
    #[error("the runner dropped the turn")]
    Dropped,
}
