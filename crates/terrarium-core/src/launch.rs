use std::collections::BTreeMap;
use std::path::PathBuf;

use serde::{Deserialize, Serialize};

/// The command line and environment to start a harness.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
pub struct LaunchSpec {
    pub program: String,
    #[serde(default)]
    pub args: Vec<String>,
    #[serde(default)]
    pub env: BTreeMap<String, String>,
    pub cwd: PathBuf,
    /// Run the program in a PTY. Always true for interactive harnesses.
    #[serde(default = "default_true")]
    pub pty: bool,
    /// Start from an empty environment: the child gets only `env` (and the
    /// runner's own `TERRARIUM_*` and `TERM` values). When false, the child
    /// also inherits the environment of the process that spawns it.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub clear_env: bool,
}

/// The exit code of `terrarium exec-env --require-credentials` when it
/// cannot get the harness credentials from the daemon (`EX_CONFIG`). It
/// does not start the harness then. The daemon names the cause in the
/// status reason of the exit (PLAN §16b.4).
pub const EXEC_ENV_NO_CREDENTIALS: i32 = 78;

fn default_true() -> bool {
    true
}

impl LaunchSpec {
    pub fn new(program: impl Into<String>, cwd: impl Into<PathBuf>) -> Self {
        Self {
            program: program.into(),
            args: Vec::new(),
            env: BTreeMap::new(),
            cwd: cwd.into(),
            pty: true,
            clear_env: false,
        }
    }

    pub fn arg(mut self, arg: impl Into<String>) -> Self {
        self.args.push(arg.into());
        self
    }

    pub fn env(mut self, key: impl Into<String>, value: impl Into<String>) -> Self {
        self.env.insert(key.into(), value.into());
        self
    }

    /// Start the child from an empty environment (see [`Self::clear_env`]).
    pub fn with_clear_env(mut self) -> Self {
        self.clear_env = true;
        self
    }
}

/// A file (or a directory) that the sandbox binds over a path inside a shared home, so each
/// agent sees its own copy (PLAN §8, §11). The sandbox makes the bind after
/// it mounts the shared home.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct FileOverlay {
    /// The per-agent file on the host (the daemon's view).
    pub source: PathBuf,
    /// The path that the harness sees inside the sandbox.
    pub target: PathBuf,
    /// Bind read-write. The harness writes to some of its state files.
    #[serde(default)]
    pub writable: bool,
}
