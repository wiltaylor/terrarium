//! The runner process: one [`Runner`] with a control socket for the daemon,
//! so that the harness lives through a daemon restart (PLAN §6, §18). The
//! `terrarium-runner` binary calls [`run`].

use std::fs::{File, OpenOptions};
use std::io::{Read, Seek, SeekFrom, Write};
use std::os::unix::fs::OpenOptionsExt;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use nix::fcntl::{Flock, FlockArg};
use terrarium_core::{Adapter, AgentId};

use serde::{Deserialize, Serialize};

use crate::config::{control_socket_path, create_private_dir, pid_path, private_dir, runners_dir};
use crate::error::{Result, RunnerError};
use crate::runner::{ExitStatus, Runner};
use crate::server::{ForwardHandler, Hub, ServerCtx, serve};
use crate::socket;
use crate::spec::RunnerSpec;

/// How long a runner process waits, after the harness exits, for the daemon
/// to read the last frames and disconnect.
pub const DEFAULT_LINGER: Duration = Duration::from_secs(2);

/// The settings of [`run`].
#[derive(Debug, Clone)]
pub struct ProcessOptions {
    pub agent: AgentId,
    pub state_dir: PathBuf,
    pub runtime_dir: PathBuf,
    pub spec: RunnerSpec,
    pub linger: Duration,
}

/// Who stopped a harness on purpose. The runner process writes it into
/// `runner-exit.json`, so that a daemon that starts later knows why the
/// harness ended (PLAN §18). On the wire and in the file it is one
/// string: `container`, `operator`, `policy`, or the label of the agent
/// that stopped it (`agent:<project>/<name>`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum StoppedBy {
    /// SIGTERM or SIGINT to the runner process: the container stops
    /// (`docker/entrypoint.sh`). The next daemon launches the agent again
    /// with resume, whatever the exit code.
    Container,
    /// The operator stopped the agent. The next daemon does not launch it.
    Operator,
    /// The daemon stopped the harness for a policy (a restart, a quota or
    /// budget hold). The next daemon applies the restart policy.
    Policy,
    /// An agent stopped it through the API (for example a project lead):
    /// the actor label, `agent:<project>/<name>`. The next daemon treats
    /// it as [`StoppedBy::Operator`]: it does not launch the agent.
    Actor(String),
}

impl StoppedBy {
    pub fn as_str(&self) -> &str {
        match self {
            Self::Container => "container",
            Self::Operator => "operator",
            Self::Policy => "policy",
            Self::Actor(a) => a,
        }
    }

    /// The value for a string: a known word, else an actor label.
    pub fn parse(s: &str) -> Self {
        match s {
            "container" => Self::Container,
            "operator" => Self::Operator,
            "policy" => Self::Policy,
            other => Self::Actor(other.to_owned()),
        }
    }

    /// A stop that the next daemon keeps: the operator, or an agent.
    pub fn is_requested(&self) -> bool {
        matches!(self, Self::Operator | Self::Actor(_))
    }
}

impl Serialize for StoppedBy {
    fn serialize<S: serde::Serializer>(&self, s: S) -> std::result::Result<S::Ok, S::Error> {
        s.serialize_str(self.as_str())
    }
}

impl<'de> Deserialize<'de> for StoppedBy {
    fn deserialize<D: serde::Deserializer<'de>>(d: D) -> std::result::Result<Self, D::Error> {
        let s = String::deserialize(d)?;
        Ok(Self::parse(&s))
    }
}

/// The content of `runner-exit.json`: the [`ExitStatus`] of the last
/// harness, and who stopped it, when someone did.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ExitRecord {
    #[serde(flatten)]
    pub status: ExitStatus,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub stopped_by: Option<StoppedBy>,
}

/// `<state>/agents/<ulid>/private/runner-exit.json`: the [`ExitRecord`] of
/// the last harness, written by the runner process before it exits.
pub fn exit_status_path(state_dir: &Path, agent: AgentId) -> PathBuf {
    private_dir(state_dir, agent).join("runner-exit.json")
}

/// The exit status that the last runner process of `agent` wrote, if any.
/// The daemon reads it when it finds no live runner for an agent.
pub fn read_exit_status(state_dir: &Path, agent: AgentId) -> Option<ExitStatus> {
    read_exit_record(state_dir, agent).map(|r| r.status)
}

/// The full `runner-exit.json` of `agent`, with `stopped_by`.
pub fn read_exit_record(state_dir: &Path, agent: AgentId) -> Option<ExitRecord> {
    let text = crate::safefs::read_to_string(&exit_status_path(state_dir, agent)).ok()?;
    serde_json::from_str(&text).ok()
}

/// The pid of the live runner process of `agent`, if there is one. A runner
/// holds an exclusive lock on its pid file for its whole life.
pub fn live_runner_pid(runtime_dir: &Path, agent: AgentId) -> Option<u32> {
    let path = pid_path(runtime_dir, agent);
    let file = crate::safefs::open_read(&path).ok()?;
    match Flock::lock(file, FlockArg::LockSharedNonblock) {
        // We got the lock, so no runner holds it.
        Ok(_) => None,
        Err((mut f, _)) => {
            let mut s = String::new();
            f.read_to_string(&mut s).ok()?;
            s.trim().parse().ok()
        }
    }
}

/// Removes the control socket and empties the pid file when the process
/// ends, also on an error. It holds the pid lock, and lets go of it last:
/// a new runner for the agent cannot start before the socket is gone. The
/// pid file stays, because a runner that opened it must lock the same file.
struct Cleanup {
    control: PathBuf,
    lock: Flock<File>,
}

impl Drop for Cleanup {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.control);
        let _ = self.lock.set_len(0);
    }
}

fn lock_pid_file(path: &Path, agent: AgentId) -> Result<Flock<File>> {
    // No symlink follow (`docs/security-review.md`, F11).
    let file = crate::safefs::open(
        path,
        OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(false)
            .mode(0o600),
    )?;
    let mut lock = match Flock::lock(file, FlockArg::LockExclusiveNonblock) {
        Ok(l) => l,
        Err((mut f, _)) => {
            let mut pid = String::new();
            let _ = f.read_to_string(&mut pid);
            return Err(RunnerError::AlreadyRunning {
                agent: agent.to_string(),
                pid: pid.trim().to_owned(),
            });
        }
    };
    lock.set_len(0)
        .and_then(|()| lock.seek(SeekFrom::Start(0)).map(|_| ()))
        .and_then(|()| writeln!(lock, "{}", std::process::id()))
        .map_err(|e| RunnerError::io("write", path, e))?;
    Ok(lock)
}

/// Run one harness until it exits, and serve the control socket.
///
/// 1. Lock `<run>/runners/<ulid>.pid` and write the pid. A live runner for
///    the agent holds the lock: then this fails with `AlreadyRunning`.
/// 2. Bind `<run>/runners/<ulid>.sock` (the control socket).
/// 3. Spawn the harness with [`Runner::spawn`]. The hook socket is
///    `<run>/agents/<ulid>.sock`, as for an in-process runner. Agent RPC
///    requests go to the daemon over the control socket.
/// 4. Wait for the harness to exit (SIGTERM and SIGINT stop it first, with
///    [`Runner::stop`]).
/// 5. Write `runner-exit.json` (with `stopped_by`), give the daemon `linger` to read the last
///    frames, and remove the socket and the pid file.
pub async fn run(opts: ProcessOptions, adapter: Arc<dyn Adapter>) -> Result<ExitStatus> {
    let agent = opts.agent;
    let dir = runners_dir(&opts.runtime_dir);
    std::fs::create_dir_all(&dir).map_err(|e| RunnerError::io("create", &dir, e))?;
    let pid_file = pid_path(&opts.runtime_dir, agent);
    let lock = lock_pid_file(&pid_file, agent)?;

    let control = control_socket_path(&opts.runtime_dir, agent);
    let cleanup = Cleanup {
        control: control.clone(),
        lock,
    };
    create_private_dir(&private_dir(&opts.state_dir, agent))?;
    let exit_path = exit_status_path(&opts.state_dir, agent);
    let _ = std::fs::remove_file(&exit_path);

    let listener = socket::bind(&control)?;
    let hub = Arc::new(Hub::new(opts.spec.rpc_queue_timeout()));
    let config = opts
        .spec
        .to_config(agent, &opts.state_dir, &opts.runtime_dir)?;
    let handler = Arc::new(ForwardHandler { hub: hub.clone() });
    let harness = adapter.id();
    let runner = Runner::spawn(config, adapter, opts.spec.launch.clone(), handler).await?;
    tracing::info!(
        agent = %agent,
        harness = %harness,
        pid = ?runner.harness_pid(),
        control = %control.display(),
        "runner started"
    );
    let ctx = Arc::new(ServerCtx {
        runner: runner.clone(),
        hub: hub.clone(),
        harness,
        stopped_by: std::sync::Mutex::new(None),
    });
    let server = tokio::spawn(serve(listener, ctx.clone()));

    let (status, signalled) = wait_or_signal(&runner).await;
    // A signal to the runner process comes from the container stop; it
    // wins over what the daemon said before.
    let stopped_by = if signalled {
        Some(StoppedBy::Container)
    } else {
        ctx.stopped_by()
    };
    tracing::info!(
        agent = %agent,
        code = status.code,
        signal = ?status.signal,
        stopped_by = stopped_by.as_ref().map(StoppedBy::as_str),
        "harness exited"
    );
    write_exit_status(
        &exit_path,
        &ExitRecord {
            status: status.clone(),
            stopped_by,
        },
    );
    if hub.connected() {
        let _ = tokio::time::timeout(opts.linger, hub.disconnected()).await;
    }
    server.abort();
    drop(cleanup);
    Ok(status)
}

/// Write `runner-exit.json` through a temp file and a rename, so that a
/// reader never sees half a file. The temp file is made new, with no
/// symlink follow.
fn write_exit_status(path: &Path, status: &ExitRecord) {
    let bytes = match serde_json::to_vec(status) {
        Ok(b) => b,
        Err(e) => {
            tracing::warn!("serialise the exit status: {e}");
            return;
        }
    };
    let tmp = path.with_extension("json.tmp");
    if let Err(e) = crate::safefs::write_atomic(path, &tmp, &bytes) {
        tracing::warn!("write {}: {e}", path.display());
    }
}

/// Wait for the harness to exit. SIGTERM and SIGINT stop it with
/// [`Runner::stop`]: the exit keys for an idle harness, else SIGHUP, and
/// SIGKILL after the grace. True: a signal stopped it.
async fn wait_or_signal(runner: &Runner) -> (ExitStatus, bool) {
    use tokio::signal::unix::{SignalKind, signal};
    let (Ok(mut term), Ok(mut int), Ok(mut hup)) = (
        signal(SignalKind::terminate()),
        signal(SignalKind::interrupt()),
        signal(SignalKind::hangup()),
    ) else {
        tracing::warn!("cannot install the signal handlers");
        return (runner.wait().await, false);
    };
    loop {
        tokio::select! {
            s = runner.wait() => return (s, false),
            _ = term.recv() => {
                tracing::info!("SIGTERM: stopping the harness");
                break;
            }
            _ = int.recv() => {
                tracing::info!("SIGINT: stopping the harness");
                break;
            }
            // The runner has no terminal of its own; a hangup means nothing.
            _ = hup.recv() => tracing::debug!("SIGHUP ignored"),
        }
    }
    (runner.stop().await, true)
}

#[cfg(test)]
mod tests {
    use super::StoppedBy;

    /// One string on the wire: the known words, else an actor label. An
    /// agent stop is a requested stop, as an operator stop is.
    #[test]
    fn stopped_by_is_one_string() {
        for (v, s) in [
            (StoppedBy::Container, "container"),
            (StoppedBy::Operator, "operator"),
            (StoppedBy::Policy, "policy"),
            (StoppedBy::Actor("agent:p/lead".into()), "agent:p/lead"),
        ] {
            let j = serde_json::to_string(&v).unwrap();
            assert_eq!(j, format!("\"{s}\""));
            assert_eq!(serde_json::from_str::<StoppedBy>(&j).unwrap(), v);
        }
        assert!(StoppedBy::Operator.is_requested());
        assert!(StoppedBy::Actor("agent:p/lead".into()).is_requested());
        assert!(!StoppedBy::Container.is_requested());
        assert!(!StoppedBy::Policy.is_requested());
    }
}
