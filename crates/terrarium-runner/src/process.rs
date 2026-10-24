//! The runner process: one [`Runner`] with a control socket for the daemon,
//! so that the harness lives through a daemon restart (PLAN §6, §18). The
//! `terrarium-runner` binary calls [`run`].

use std::fs::{File, OpenOptions};
use std::io::{Read, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use nix::fcntl::{Flock, FlockArg};
use terrarium_core::{Adapter, AgentId};

use crate::config::{agent_dir, control_socket_path, pid_path, runners_dir};
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

/// `<state>/agents/<ulid>/runner-exit.json`: the [`ExitStatus`] of the last
/// harness, written by the runner process before it exits.
pub fn exit_status_path(state_dir: &Path, agent: AgentId) -> PathBuf {
    agent_dir(state_dir, agent).join("runner-exit.json")
}

/// The exit status that the last runner process of `agent` wrote, if any.
/// The daemon reads it when it finds no live runner for an agent.
pub fn read_exit_status(state_dir: &Path, agent: AgentId) -> Option<ExitStatus> {
    let text = std::fs::read_to_string(exit_status_path(state_dir, agent)).ok()?;
    serde_json::from_str(&text).ok()
}

/// The pid of the live runner process of `agent`, if there is one. A runner
/// holds an exclusive lock on its pid file for its whole life.
pub fn live_runner_pid(runtime_dir: &Path, agent: AgentId) -> Option<u32> {
    let path = pid_path(runtime_dir, agent);
    let file = File::open(&path).ok()?;
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
    let file = OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .open(path)
        .map_err(|e| RunnerError::io("open", path, e))?;
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
/// 4. Wait for the harness to exit (SIGTERM and SIGINT kill it first).
/// 5. Write `runner-exit.json`, give the daemon `linger` to read the last
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
    let agent_dir = agent_dir(&opts.state_dir, agent);
    std::fs::create_dir_all(&agent_dir).map_err(|e| RunnerError::io("create", &agent_dir, e))?;
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
    });
    let server = tokio::spawn(serve(listener, ctx));

    let status = wait_or_signal(&runner).await;
    tracing::info!(agent = %agent, code = status.code, signal = ?status.signal, "harness exited");
    match serde_json::to_vec(&status) {
        Ok(bytes) => {
            if let Err(e) = std::fs::write(&exit_path, bytes) {
                tracing::warn!("write {}: {e}", exit_path.display());
            }
        }
        Err(e) => tracing::warn!("serialise the exit status: {e}"),
    }
    if hub.connected() {
        let _ = tokio::time::timeout(opts.linger, hub.disconnected()).await;
    }
    server.abort();
    drop(cleanup);
    Ok(status)
}

async fn wait_or_signal(runner: &Runner) -> ExitStatus {
    use tokio::signal::unix::{SignalKind, signal};
    let (Ok(mut term), Ok(mut int), Ok(mut hup)) = (
        signal(SignalKind::terminate()),
        signal(SignalKind::interrupt()),
        signal(SignalKind::hangup()),
    ) else {
        tracing::warn!("cannot install the signal handlers");
        return runner.wait().await;
    };
    loop {
        tokio::select! {
            s = runner.wait() => return s,
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
    if let Err(e) = runner.kill() {
        tracing::warn!("kill the harness: {e}");
    }
    runner.wait().await
}
