use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use terrarium_core::AgentId;

use crate::ready::ReadyDetector;

/// The default of [`RunnerConfig::kill_grace`].
pub const DEFAULT_KILL_GRACE: Duration = Duration::from_secs(3);
/// The default of [`RunnerConfig::stop_grace`].
pub const DEFAULT_STOP_GRACE: Duration = Duration::from_secs(5);
/// The default of [`RunnerConfig::output_capacity`].
pub const DEFAULT_OUTPUT_CAPACITY: usize = 1024;

/// The settings of one runner.
#[derive(Clone)]
pub struct RunnerConfig {
    /// The agent that this runner holds.
    pub agent: AgentId,
    /// The agent name, for `whoami`.
    pub name: String,
    /// The project name, for `whoami`.
    pub project: String,
    /// The roles above `member`, for `whoami`.
    pub roles: Vec<String>,
    /// The state root, for example `/state`. The runner writes its records
    /// to `<state_dir>/agents/<ulid>/private/` (see [`private_dir`]).
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
    /// How long to wait for the prompt-submit hook, or for a busy screen,
    /// after a turn is typed.
    pub submit_timeout: Duration,
    /// How many times to type a turn before the delivery fails. PLAN §7 asks
    /// for one retry, so the default is 2.
    pub submit_attempts: u32,
    /// `pty.log` rotates to `pty.log.1` when it grows past this size. `0`
    /// turns the PTY log off: the runner writes no `pty.log` at all (the
    /// login flow, which shows a device code, uses this).
    pub log_max_bytes: u64,
    /// How often the runner checks for prompt-ready, stalls, and timeouts.
    pub tick: Duration,
    /// Overrides the default prompt-ready detector.
    pub ready_detector: Option<Arc<dyn ReadyDetector>>,
    /// [`crate::Runner::kill`] sends SIGHUP first. When the harness has not
    /// exited after this time, the runner sends SIGKILL to its process
    /// group.
    pub kill_grace: Duration,
    /// [`crate::Runner::stop`] types the adapter's exit keys into an idle
    /// harness, and waits this long for the exit before it kills.
    pub stop_grace: Duration,
    /// The capacity of the PTY output channel. An attach client that falls
    /// this many chunks behind lags. The default is 1024. Tests set a small
    /// value to force a lag.
    pub output_capacity: usize,
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
            roles: Vec::new(),
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
            kill_grace: DEFAULT_KILL_GRACE,
            stop_grace: DEFAULT_STOP_GRACE,
            output_capacity: DEFAULT_OUTPUT_CAPACITY,
        }
    }

    /// `<state_dir>/agents/<ulid>`.
    pub fn agent_dir(&self) -> PathBuf {
        agent_dir(&self.state_dir, self.agent)
    }

    /// `<state_dir>/agents/<ulid>/private`.
    pub fn private_dir(&self) -> PathBuf {
        private_dir(&self.state_dir, self.agent)
    }

    /// `<runtime_dir>/agents/<ulid>.sock`.
    pub fn socket_path(&self) -> PathBuf {
        socket_path(&self.runtime_dir, self.agent)
    }
}

/// The state directory of one agent: `<state_dir>/agents/<ulid>`. It holds
/// three directories (`docs/runner.md`, "Files"):
///
/// - `private/` ([`private_dir`]): the records of the runner and the
///   daemon. The sandbox never binds it.
/// - `harness/` ([`harness_dir`]): the files that the adapter writes for
///   the harness. The sandbox binds it read-only, with read-write binds for
///   the few files that the harness writes itself.
/// - `workspace/` ([`workspace_dir`]): the agent's work tree, read-write at
///   `$HOME/work`.
pub fn agent_dir(state_dir: &Path, agent: AgentId) -> PathBuf {
    state_dir.join("agents").join(agent.to_string())
}

/// The name of the private directory in the agent directory.
pub const PRIVATE_DIR: &str = "private";
/// The name of the harness directory in the agent directory.
pub const HARNESS_DIR: &str = "harness";
/// The name of the workspace directory in the agent directory.
pub const WORKSPACE_DIR: &str = "workspace";

/// `<state_dir>/agents/<ulid>/private`: `events.jsonl`, `pty.log`,
/// `runner.json`, `runner-argv.json`, `runner-exit.json`, `runner.log`.
/// The harness never sees this directory.
pub fn private_dir(state_dir: &Path, agent: AgentId) -> PathBuf {
    agent_dir(state_dir, agent).join(PRIVATE_DIR)
}

/// `<state_dir>/agents/<ulid>/harness`: `prompt.md` and the per-agent
/// files of the adapter.
pub fn harness_dir(state_dir: &Path, agent: AgentId) -> PathBuf {
    agent_dir(state_dir, agent).join(HARNESS_DIR)
}

/// `<state_dir>/agents/<ulid>/workspace`.
pub fn workspace_dir(state_dir: &Path, agent: AgentId) -> PathBuf {
    agent_dir(state_dir, agent).join(WORKSPACE_DIR)
}

/// Make the private directory (and the agent directory), mode `0700`:
/// it holds the launch spec with the injected keys.
pub fn create_private_dir(dir: &Path) -> crate::error::Result<()> {
    use std::os::unix::fs::PermissionsExt;
    std::fs::create_dir_all(dir).map_err(|e| crate::error::RunnerError::io("create", dir, e))?;
    std::fs::set_permissions(dir, std::fs::Permissions::from_mode(0o700))
        .map_err(|e| crate::error::RunnerError::io("chmod", dir, e))
}

/// The files that a layout before the split kept at the top of the agent
/// directory and that now live in `private/`.
pub const LEGACY_PRIVATE: &[&str] = &[
    "events.jsonl",
    "pty.log",
    "pty.log.1",
    "runner.json",
    "runner-argv.json",
    "runner-exit.json",
    "runner.log",
    "session-prompt.sha256",
];

/// The entries that a layout before the split kept at the top of the agent
/// directory and that now live in `harness/`: the prompt and the per-agent
/// directory of each adapter.
pub const LEGACY_HARNESS: &[&str] = &["prompt.md", "claude", "codex", "copilot", "pi"];

/// Move the files of an agent directory from the layout before the split
/// (everything at the top) into `private/` and `harness/`. It is
/// idempotent: an entry that is not at the top, or that is already at its
/// new place, stays where it is. A rename keeps the inode, so a runner of
/// an older version that still has a file open keeps writing to it.
/// Returns the number of entries moved.
pub fn migrate_legacy_layout(dir: &Path) -> std::io::Result<usize> {
    if !dir.is_dir() {
        return Ok(0);
    }
    let mut moved = 0;
    for (sub, names) in [(PRIVATE_DIR, LEGACY_PRIVATE), (HARNESS_DIR, LEGACY_HARNESS)] {
        let to_dir = dir.join(sub);
        for name in names {
            let from = dir.join(name);
            if std::fs::symlink_metadata(&from).is_err() {
                continue;
            }
            std::fs::create_dir_all(&to_dir)?;
            let to = to_dir.join(name);
            if std::fs::symlink_metadata(&to).is_ok() {
                tracing::warn!(
                    "{} and {} both exist; the old one stays",
                    from.display(),
                    to.display()
                );
                continue;
            }
            std::fs::rename(&from, &to)?;
            moved += 1;
        }
    }
    Ok(moved)
}

/// The runner socket of one agent: `<runtime_dir>/agents/<ulid>.sock`.
pub fn socket_path(runtime_dir: &Path, agent: AgentId) -> PathBuf {
    runtime_dir.join("agents").join(format!("{agent}.sock"))
}

/// The directory of the runner process files: `<runtime_dir>/runners`.
pub fn runners_dir(runtime_dir: &Path) -> PathBuf {
    runtime_dir.join("runners")
}

/// The control socket of a runner process: `<runtime_dir>/runners/<ulid>.sock`.
/// The daemon connects to it (`docs/runner.md`, "The control socket").
pub fn control_socket_path(runtime_dir: &Path, agent: AgentId) -> PathBuf {
    runners_dir(runtime_dir).join(format!("{agent}.sock"))
}

/// The pid file of a runner process: `<runtime_dir>/runners/<ulid>.pid`.
/// The live runner holds an exclusive `flock` on it.
pub fn pid_path(runtime_dir: &Path, agent: AgentId) -> PathBuf {
    runners_dir(runtime_dir).join(format!("{agent}.pid"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn legacy_layout_moves_once() {
        let tmp = tempfile::tempdir().expect("tmp");
        let dir = tmp.path().join("a");
        std::fs::create_dir_all(dir.join("claude/sessions")).expect("mk");
        std::fs::create_dir_all(dir.join("workspace")).expect("mk");
        for f in ["events.jsonl", "pty.log", "runner.json", "prompt.md"] {
            std::fs::write(dir.join(f), f).expect("write");
        }
        assert_eq!(migrate_legacy_layout(&dir).expect("migrate"), 5);
        for f in ["events.jsonl", "pty.log", "runner.json"] {
            assert_eq!(
                std::fs::read_to_string(dir.join("private").join(f)).expect("moved"),
                f
            );
            assert!(!dir.join(f).exists());
        }
        assert!(dir.join("harness/prompt.md").is_file());
        assert!(dir.join("harness/claude/sessions").is_dir());
        assert!(dir.join("workspace").is_dir());
        // A second run changes nothing.
        assert_eq!(migrate_legacy_layout(&dir).expect("again"), 0);
        // An entry at both places: the new one wins, the old one stays.
        std::fs::write(dir.join("events.jsonl"), "old").expect("write");
        assert_eq!(migrate_legacy_layout(&dir).expect("both"), 0);
        assert_eq!(
            std::fs::read_to_string(dir.join("private/events.jsonl")).expect("new"),
            "events.jsonl"
        );
        assert_eq!(
            migrate_legacy_layout(&tmp.path().join("none")).expect("none"),
            0
        );
    }
}
