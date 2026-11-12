//! The executor (PLAN §19): run commands, copy files, and drive the
//! lifecycle of targets.
//!
//! Each call resolves a target to a backend:
//!
//! | Kind | Exec | Copy | Lifecycle |
//! |---|---|---|---|
//! | `host` | `exec` on the host agent | `cp_push` / `cp_pull` | no-op |
//! | `docker` | `docker.exec` | `docker.exec` of `cat` with stdin / stdout | `docker.inspect`, `docker.run`, `docker.start`, `docker.stop`, `docker.rm` |
//! | `vmlab` | `vmlab.exec`, `vmlab.shell` | `cp_*` to a staging file in the lab dir, then `vmlab.cp` | `vmlab.status`, `up`, `down`, `destroy`, `snapshot_*` |
//! | `ssh` | the system `ssh` | `ssh` of `cat` with stdin / stdout | no-op |
//!
//! A backend run is a [`Running`]: a bounded channel of events and an
//! unbounded channel of controls. A host session and a local process both
//! become one, so the code above them does not care where a command runs.
//! The bounded event channel is backpressure: the daemon acks the host's
//! output only when the consumer has taken it.
//!
//! [`ExecSession`] is the HTTP side: a buffer of output that the client
//! reads with `GET .../output?since=` (long-poll), and a control channel
//! that `POST .../input` writes to. The `since` of a read acknowledges the
//! bytes before it. When the unacknowledged output reaches the buffer
//! limit, the session stops taking events, and the backpressure goes back
//! to the process.

use std::collections::{BTreeMap, HashMap, VecDeque};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, MutexGuard};
use std::time::{Duration, Instant};

use base64::Engine;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use terrarium_core::config::{TargetDef, TargetKind};
use terrarium_core::protocol::LifecycleVerb;
use terrarium_core::protocol::{CpDirection, HostCapabilities};
use terrarium_host::protocol::{
    CpPull, CpPush, DaemonFrame, DockerExec, DockerInspect, DockerRm, DockerRun, DockerStart,
    DockerStop, ExecLimits, ExecRequest, LabSource, Limits, Probe, PtySize, StreamTag, VmlabCp,
    VmlabExec, VmlabLifecycle, VmlabPrepare, VmlabRelease, VmlabShell, VmlabSnapshot,
    VmlabSnapshotDelete, VmlabSnapshotList, VmlabStatus, WorkspaceMount, hex,
};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::sync::{Notify, mpsc};
use tracing::{debug, warn};

use crate::actor::Actor;
use crate::api::Shared;
use crate::error::{ApiError, ApiResult, Code};
use crate::hosts::{HostRegistry, HostSession, SessionEvent};
use crate::policy;
use crate::secrets::{RunDirs, Secret};
use crate::store::leases;
use crate::store::targets::{self, TargetRow};

pub(crate) mod durable;
mod workspace_sync;

/// Live exec sessions an agent may hold at once.
pub const MAX_SESSIONS_PER_AGENT: usize = 8;
/// Unacknowledged output an exec session buffers before backpressure.
pub const SESSION_BUFFER: usize = 8 * 1024 * 1024;
/// The most output bytes one read returns.
pub const MAX_READ: usize = 1024 * 1024;
/// The longest a read waits for output.
pub const MAX_WAIT_SECS: u64 = 60;
/// The most bytes one input request carries (before base64).
pub const MAX_INPUT: usize = 1024 * 1024;
/// The command text in an audit row is cut to this many characters.
const AUDIT_CMD_CHARS: usize = 200;
/// A finished session is forgotten after this idle time.
const DONE_IDLE: Duration = Duration::from_secs(300);
/// A session nobody touches for this long is killed.
const LIVE_IDLE: Duration = Duration::from_secs(1800);
/// The default limit of a lifecycle verb or a checkout.
const VERB_TIMEOUT: u64 = 600;
/// The events a backend may queue before it waits for the consumer.
const EVENT_QUEUE: usize = 16;

// ------------------------------------------------------------------ types

/// An output stream of an exec session.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Stream {
    Stdout,
    Stderr,
    File,
}

impl From<StreamTag> for Stream {
    fn from(t: StreamTag) -> Self {
        match t {
            StreamTag::Stderr => Self::Stderr,
            StreamTag::File => Self::File,
            StreamTag::Stdout | StreamTag::Stdin => Self::Stdout,
        }
    }
}

/// How a run ended.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum End {
    Exit {
        #[serde(skip_serializing_if = "Option::is_none")]
        code: Option<i32>,
        #[serde(skip_serializing_if = "Option::is_none")]
        signal: Option<i32>,
        reason: String,
    },
    Error {
        code: String,
        message: String,
    },
    Cp {
        bytes: u64,
        sha256: String,
    },
}

impl End {
    fn error(code: &str, message: impl Into<String>) -> Self {
        Self::Error {
            code: code.to_owned(),
            message: message.into(),
        }
    }

    /// Exit code 0, or a finished copy.
    pub fn success(&self) -> bool {
        matches!(self, Self::Exit { code: Some(0), .. } | Self::Cp { .. })
    }
}

/// An event of a backend run.
#[derive(Debug)]
pub enum Ev {
    Started,
    Data(Stream, Vec<u8>),
    Result(Value),
    End(End),
}

/// A control for a backend run.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Control {
    Stdin(Vec<u8>),
    Eof,
    Resize {
        rows: u16,
        cols: u16,
    },
    Signal(String),
    /// File bytes of a host `cp_push`.
    File(Vec<u8>),
    /// The end of a push, with the client's count and digest.
    Finish {
        bytes: u64,
        sha256: String,
    },
    Kill,
}

/// A backend run: events out, controls in. Dropping it ends the run.
pub struct Running {
    pub events: mpsc::Receiver<Ev>,
    pub ctl: mpsc::UnboundedSender<Control>,
    pub host_session: Option<(String, u32)>,
}

/// Everything a run made, for short verbs.
#[derive(Debug, Default)]
pub struct Collected {
    pub stdout: Vec<u8>,
    pub stderr: Vec<u8>,
    pub result: Option<Value>,
    pub end: Option<End>,
}

impl Collected {
    pub(crate) fn end(&self) -> End {
        self.end
            .clone()
            .unwrap_or_else(|| End::error("lost", "the run ended without an end"))
    }

    pub(crate) fn stderr_tail(&self) -> String {
        let s = String::from_utf8_lossy(&self.stderr);
        let start = s.len().saturating_sub(2000);
        let mut i = start;
        while !s.is_char_boundary(i) {
            i += 1;
        }
        s[i..].trim().to_owned()
    }
}

impl Running {
    /// Read every event to the end. Output past `max` bytes per stream is
    /// dropped.
    pub async fn collect(mut self, max: usize) -> Collected {
        let mut c = Collected::default();
        while let Some(ev) = self.events.recv().await {
            match ev {
                Ev::Started => {}
                Ev::Data(s, b) => {
                    let buf = if s == Stream::Stderr {
                        &mut c.stderr
                    } else {
                        &mut c.stdout
                    };
                    let room = max.saturating_sub(buf.len());
                    buf.extend_from_slice(&b[..b.len().min(room)]);
                }
                Ev::Result(v) => c.result = Some(v),
                Ev::End(e) => {
                    c.end = Some(e);
                    break;
                }
            }
        }
        c
    }
}

/// Turn a host session into a [`Running`].
pub fn pump_host(mut hs: HostSession) -> Running {
    let host_session = Some((hs.host().to_owned(), hs.id()));
    let (etx, erx) = mpsc::channel(EVENT_QUEUE);
    let (ctx, mut crx) = mpsc::unbounded_channel::<Control>();
    tokio::spawn(async move {
        loop {
            tokio::select! {
                ev = hs.recv() => {
                    let Some(ev) = ev else { break };
                    let (mapped, last) = match ev {
                        SessionEvent::Started { .. } => (Ev::Started, false),
                        SessionEvent::Data { stream, bytes } => (Ev::Data(stream.into(), bytes), false),
                        SessionEvent::Result(v) => (Ev::Result(v), false),
                        SessionEvent::Exit { code, signal, reason } => (
                            Ev::End(End::Exit {
                                code,
                                signal,
                                reason: serde_json::to_value(reason)
                                    .ok()
                                    .and_then(|v| v.as_str().map(str::to_owned))
                                    .unwrap_or_else(|| "exited".into()),
                            }),
                            true,
                        ),
                        SessionEvent::Error { code, message } => (Ev::End(End::Error { code, message }), true),
                        SessionEvent::CpDone { bytes, sha256 } => (Ev::End(End::Cp { bytes, sha256 }), true),
                        SessionEvent::Lost { message } => (Ev::End(End::error("lost", message)), true),
                    };
                    if etx.send(mapped).await.is_err() || last {
                        break;
                    }
                }
                c = crx.recv() => match c {
                    None | Some(Control::Kill) => { hs.abandon(); break; },
                    Some(Control::Stdin(b)) => { hs.stdin(b); }
                    Some(Control::File(b)) => { hs.file(b); }
                    Some(Control::Eof) => { hs.eof(); }
                    Some(Control::Resize { rows, cols }) => { hs.resize(rows, cols); }
                    Some(Control::Signal(s)) => { hs.signal(&s); }
                    Some(Control::Finish { bytes, sha256 }) => { hs.cp_end(bytes, sha256); }
                },
            }
        }
        // Dropping `hs` before its end abandons the session on the host.
    });
    Running {
        events: erx,
        ctl: ctx,
        host_session,
    }
}

/// Run a local process (the `ssh` client) as a [`Running`]. `prefix` goes
/// to stdin first. `guard` lives until the process ends (a key file dir).
pub fn pump_process(
    mut cmd: tokio::process::Command,
    prefix: Option<Vec<u8>>,
    timeout: Option<Duration>,
    guard: Option<Box<dyn Send + 'static>>,
) -> Running {
    let (etx, erx) = mpsc::channel(EVENT_QUEUE);
    let (ctx, mut crx) = mpsc::unbounded_channel::<Control>();
    tokio::spawn(async move {
        let _guard = guard;
        cmd.stdin(std::process::Stdio::piped())
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::piped())
            .kill_on_drop(true)
            .process_group(0);
        let mut child = match cmd.spawn() {
            Ok(c) => c,
            Err(e) => {
                let _ = etx
                    .send(Ev::End(End::error("spawn_failed", e.to_string())))
                    .await;
                return;
            }
        };
        let mut stdin = child.stdin.take();
        if let (Some(p), Some(s)) = (prefix, stdin.as_mut())
            && s.write_all(&p).await.is_err()
        {
            debug!("ssh closed stdin before the env preamble");
        }
        let _ = etx.send(Ev::Started).await;
        let mut readers = Vec::new();
        for (pipe, stream) in [
            (
                child
                    .stdout
                    .take()
                    .map(|p| Box::new(p) as Box<dyn tokio::io::AsyncRead + Send + Unpin>),
                Stream::Stdout,
            ),
            (
                child
                    .stderr
                    .take()
                    .map(|p| Box::new(p) as Box<dyn tokio::io::AsyncRead + Send + Unpin>),
                Stream::Stderr,
            ),
        ] {
            let Some(mut pipe) = pipe else { continue };
            let etx = etx.clone();
            readers.push(tokio::spawn(async move {
                let mut buf = vec![0u8; 32 * 1024];
                loop {
                    match pipe.read(&mut buf).await {
                        Ok(0) | Err(_) => break,
                        Ok(n) => {
                            if etx.send(Ev::Data(stream, buf[..n].to_vec())).await.is_err() {
                                break;
                            }
                        }
                    }
                }
            }));
        }
        let deadline = timeout.map(|t| tokio::time::Instant::now() + t);
        let mut timed_out = false;
        let mut ctl_open = true;
        let status = loop {
            tokio::select! {
                status = child.wait() => break status,
                c = crx.recv(), if ctl_open => match c {
                    None | Some(Control::Kill) => {
                        ctl_open = false;
                        let _ = child.start_kill();
                    }
                    Some(Control::Stdin(b)) => {
                        if let Some(s) = stdin.as_mut()
                            && s.write_all(&b).await.is_err()
                        {
                            stdin = None;
                        }
                    }
                    Some(Control::Eof) => stdin = None,
                    Some(Control::Signal(_)) => { let _ = child.start_kill(); }
                    Some(_) => {}
                },
                () = async {
                    match deadline {
                        Some(d) => tokio::time::sleep_until(d).await,
                        None => std::future::pending().await,
                    }
                }, if !timed_out => {
                    timed_out = true;
                    let _ = child.start_kill();
                }
            }
        };
        drop(stdin);
        for r in readers {
            let _ = tokio::time::timeout(Duration::from_secs(2), r).await;
        }
        let end = match status {
            Ok(st) => {
                use std::os::unix::process::ExitStatusExt;
                let reason = if timed_out {
                    "timeout"
                } else if st.signal().is_some() {
                    "signaled"
                } else {
                    "exited"
                };
                End::Exit {
                    code: st.code(),
                    signal: st.signal(),
                    reason: reason.into(),
                }
            }
            Err(e) => End::error("io", e.to_string()),
        };
        let _ = etx.send(Ev::End(end)).await;
    });
    Running {
        events: erx,
        ctl: ctx,
        host_session: None,
    }
}

// ------------------------------------------------------------ the commands

/// A command for a target. `Debug` shows the env names, not the values
/// (a value can be a secret, TER-142).
#[derive(Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct RemoteCmd {
    /// Explicit opt-in: no stdin, PTY, or interactive shell.
    #[serde(default)]
    pub durable: bool,
    /// Only daemon-owned root scripts may request helper provisioning.
    #[serde(default)]
    pub prepare_root: Option<terrarium_host::native::OwnedRoot>,
    /// Durable queue-step identity, bound atomically to a host session.
    pub resume_key: Option<String>,
    /// The program and its arguments. Empty with `shell`.
    pub argv: Vec<String>,
    pub cwd: Option<String>,
    /// Goes into the process environment, never onto a command line.
    pub env: BTreeMap<String, String>,
    pub pty: Option<PtySize>,
    pub timeout_secs: Option<u64>,
    /// A login shell in place of `argv`.
    pub shell: bool,
    /// A `lab_from_workspace` target: the holder whose instance runs the
    /// command (an agent key). An agent caller uses its own.
    pub agent: Option<String>,
    /// The owner of the resource budget on a `host` target with `limits`:
    /// `<project>/<agent>` of the caller. `None` is the operator.
    pub holder: Option<String>,
    /// The operator runs the command (set from the actor). On a host with
    /// native GPU enforcement a command with no holder runs as the
    /// operator only with this set; otherwise it is refused
    /// ([`native_gpu_limits`]), so a daemon path that forgets the holder
    /// never gets the operator's whole workspace root.
    pub operator: bool,
}

impl std::fmt::Debug for RemoteCmd {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("RemoteCmd")
            .field("argv", &self.argv)
            .field("cwd", &self.cwd)
            .field("env", &self.env.keys().collect::<Vec<_>>())
            .field("pty", &self.pty)
            .field("timeout_secs", &self.timeout_secs)
            .field("shell", &self.shell)
            .field("agent", &self.agent)
            .field("holder", &self.holder)
            .field("operator", &self.operator)
            .finish()
    }
}

/// The [`RemoteCmd::holder`] of an actor.
pub fn holder_of(actor: &Actor) -> Option<String> {
    actor.agent().map(|a| format!("{}/{}", a.project, a.name))
}

/// The limits of a command on a `host` target with `limits`
/// (docs/api.md §17.2, "Resource limits"). Each holder gets its own
/// budget on the target. A host agent that cannot apply them is a `409
/// conflict`: the command never runs without its limits.
pub fn exec_limits(
    def: &TargetDef,
    caps: Option<&HostCapabilities>,
    holder: Option<&str>,
) -> ApiResult<Option<ExecLimits>> {
    let (TargetKind::Host, Some(tl)) = (def.kind, &def.limits) else {
        return Ok(None);
    };
    let limits = tl.resolve().map_err(ApiError::invalid)?;
    if limits.is_empty() {
        return Ok(None);
    }
    let Some(caps) = caps else {
        return Err(crate::hosts::HostError::Offline(host_of(def)?.to_owned()).into());
    };
    limits
        .host_support(caps.limits.as_ref())
        .map_err(|r| ApiError::conflict(terrarium_core::limits::refusal(&r)))?;
    Ok(Some(ExecLimits {
        reservation: None,
        target: def.name.clone(),
        holder: holder.unwrap_or("operator").to_owned(),
        limits,
        native_gpu: None,
    }))
}

/// A `host` target whose host agent reported enforced native GPU support
/// (TER-345): its commands run through the root helper.
pub fn native_enforced(def: &TargetDef, caps: Option<&HostCapabilities>) -> bool {
    def.kind == TargetKind::Host
        && caps
            .and_then(|c| c.native_gpu.as_ref())
            .is_some_and(|n| n.enforced)
}

/// On a host agent with enforced native GPU support (TER-345), every
/// command on a `host` target runs through the root helper: with no
/// reservation it gets no GPU. Elsewhere the grant is dropped, so a host
/// agent never sees a request it cannot enforce. On such a host a command
/// needs a holder: the helper gives each holder its workspace scope. No
/// holder is the operator only when `operator` says so (the actor was the
/// operator); any other command with no holder is refused, never given the
/// operator's whole root by default.
pub fn native_gpu_limits(
    def: &TargetDef,
    caps: Option<&HostCapabilities>,
    holder: Option<&str>,
    operator: bool,
    limits: Option<ExecLimits>,
) -> ApiResult<Option<ExecLimits>> {
    if !native_enforced(def, caps) {
        return Ok(limits.map(|l| ExecLimits {
            native_gpu: None,
            ..l
        }));
    }
    let holder = match (holder, operator) {
        (Some(h), _) => h,
        (None, true) => "operator",
        (None, false) => {
            return Err(ApiError::invalid(format!(
                "a command on target {} has no holder; with native GPU enforcement each command \
                 needs one (an agent, a project service, or the operator)",
                def.name
            )));
        }
    };
    let mut l = limits.unwrap_or_else(|| ExecLimits {
        target: def.name.clone(),
        holder: holder.to_owned(),
        limits: Limits::default(),
        reservation: None,
        native_gpu: None,
    });
    l.native_gpu.get_or_insert_with(Default::default);
    Ok(Some(l))
}

fn valid_env_key(k: &str) -> bool {
    let mut c = k.chars();
    matches!(c.next(), Some(x) if x.is_ascii_alphabetic() || x == '_')
        && c.all(|x| x.is_ascii_alphanumeric() || x == '_')
}

pub(crate) fn path_kind(state: &Shared, def: &TargetDef) -> terrarium_host::protocol::PathKind {
    if def.kind == TargetKind::Host {
        def.host
            .as_deref()
            .map(|h| state.executor.hosts.path_kind(h))
            .unwrap_or_default()
    } else {
        terrarium_host::protocol::PathKind::Posix
    }
}

impl RemoteCmd {
    fn check(&self) -> ApiResult<()> {
        self.check_paths(terrarium_host::protocol::PathKind::Posix)
    }

    fn check_paths(&self, kind: terrarium_host::protocol::PathKind) -> ApiResult<()> {
        if self.argv.is_empty() && !self.shell {
            return Err(ApiError::invalid("`cmd` is empty; name a program"));
        }
        if let Some(k) = self.env.keys().find(|k| !valid_env_key(k)) {
            return Err(ApiError::invalid(format!("bad env name `{k}`")));
        }
        if let Some(c) = &self.cwd
            && kind.key(c).is_none()
        {
            return Err(ApiError::invalid(format!(
                "`cwd` must be an absolute path, not `{c}`"
            )));
        }
        Ok(())
    }
}

/// Quote for a POSIX shell.
pub fn sh_quote(s: &str) -> String {
    format!("'{}'", s.replace('\'', "'\\''"))
}

/// The `ssh` invocation for a command on an `ssh` target.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SshPlan {
    pub program: String,
    pub args: Vec<String>,
    /// Bytes for stdin before the command's own stdin: the env, as one
    /// base64 line that the remote shell reads and evaluates. So no value
    /// is on a command line, local or remote.
    pub stdin_prefix: Option<Vec<u8>>,
}

/// Build the `ssh` command line. `ssh` joins the remote words with spaces
/// and gives them to the login shell, so the daemon sends one quoted
/// string. Needs `base64` on the remote side when `env` is set.
pub fn ssh_plan(
    ssh_bin: &str,
    def: &TargetDef,
    key: Option<&Path>,
    known_hosts: &Path,
    cmd: &RemoteCmd,
) -> ApiResult<SshPlan> {
    cmd.check()?;
    let address = def
        .address
        .as_deref()
        .ok_or_else(|| ApiError::invalid("an ssh target needs `address`"))?;
    if address.starts_with('-') || address.chars().any(char::is_whitespace) {
        return Err(ApiError::invalid(format!("bad ssh address `{address}`")));
    }
    let mut args: Vec<String> = [
        "-o",
        "BatchMode=yes",
        "-o",
        "StrictHostKeyChecking=accept-new",
        "-o",
    ]
    .iter()
    .map(|s| (*s).to_owned())
    .collect();
    args.push(format!("UserKnownHostsFile={}", known_hosts.display()));
    args.extend(["-o".to_owned(), "ConnectTimeout=15".to_owned()]);
    if let Some(k) = key {
        args.extend([
            "-i".to_owned(),
            k.display().to_string(),
            "-o".to_owned(),
            "IdentitiesOnly=yes".to_owned(),
        ]);
    }
    if let Some(p) = def.port {
        args.extend(["-p".to_owned(), p.to_string()]);
    }
    args.push(if cmd.pty.is_some() { "-tt" } else { "-T" }.to_owned());
    args.push("--".to_owned());
    args.push(address.to_owned());

    let mut parts = Vec::new();
    let mut prefix = None;
    if !cmd.env.is_empty() {
        let script: String = cmd
            .env
            .iter()
            .map(|(k, v)| format!("export {k}={}\n", sh_quote(v)))
            .collect();
        let mut line = base64::engine::general_purpose::STANDARD
            .encode(script)
            .into_bytes();
        line.push(b'\n');
        prefix = Some(line);
        parts.push(
            "IFS= read -r __terrarium_env && eval \"$(printf '%s' \"$__terrarium_env\" | base64 -d)\" && unset __terrarium_env"
                .to_owned(),
        );
    }
    if let Some(c) = &cmd.cwd {
        parts.push(format!("cd {}", sh_quote(c)));
    }
    if cmd.shell && cmd.argv.is_empty() {
        parts.push("exec \"${SHELL:-/bin/sh}\" -l".to_owned());
    } else {
        let words: Vec<String> = cmd.argv.iter().map(|a| sh_quote(a)).collect();
        parts.push(format!("exec {}", words.join(" ")));
    }
    args.push(parts.join(" && "));
    Ok(SshPlan {
        program: ssh_bin.to_owned(),
        args,
        stdin_prefix: prefix,
    })
}

/// The container that a docker target runs in.
pub fn container_name(def: &TargetDef) -> String {
    def.container
        .clone()
        .unwrap_or_else(|| format!("terrarium-{}", def.name))
}

pub(crate) fn host_of(def: &TargetDef) -> ApiResult<&str> {
    def.host
        .as_deref()
        .ok_or_else(|| ApiError::invalid(format!("target `{}` names no host agent", def.name)))
}

fn lab_of(def: &TargetDef) -> ApiResult<PathBuf> {
    def.lab
        .clone()
        .ok_or_else(|| ApiError::invalid(format!("vmlab target `{}` has no `lab`", def.name)))
}

fn lab_dir(def: &TargetDef) -> ApiResult<PathBuf> {
    let lab = lab_of(def)?;
    Ok(if lab.file_name().is_some_and(|n| n == "vmlab.wcl") {
        lab.parent().map(Path::to_path_buf).unwrap_or(lab)
    } else {
        lab
    })
}

fn machine_of(def: &TargetDef) -> ApiResult<String> {
    def.machine.clone().ok_or_else(|| {
        ApiError::invalid(format!(
            "vmlab target `{}` has no `machine`; exec and copy need one",
            def.name
        ))
    })
}

/// Refuse exec, shell, copy, and checkout on a target that has only a
/// display (`vnc`, `rdp`, `desktop`; docs/api.md §17.10).
pub fn no_exec(def: &TargetDef) -> ApiResult<()> {
    if def.kind.display_only() {
        return Err(ApiError::invalid(format!(
            "a `{}` target has only a display: use `terrarium screen {}` (POST /api/targets/{}/display/...)",
            targets::kind_str(def.kind),
            def.name,
            def.name
        )));
    }
    Ok(())
}

/// The host-agent frame for a command on a host, docker, or vmlab target.
pub fn host_frame(
    def: &TargetDef,
    shell: Option<&str>,
    cmd: &RemoteCmd,
    limits: Option<ExecLimits>,
) -> ApiResult<impl FnOnce(u32) -> DaemonFrame + use<>> {
    host_frame_paths(
        def,
        shell,
        cmd,
        limits,
        terrarium_host::protocol::PathKind::Posix,
    )
}

fn host_frame_paths(
    def: &TargetDef,
    shell: Option<&str>,
    cmd: &RemoteCmd,
    limits: Option<ExecLimits>,
    kind: terrarium_host::protocol::PathKind,
) -> ApiResult<impl FnOnce(u32) -> DaemonFrame + use<>> {
    no_exec(def)?;
    cmd.check_paths(kind)?;
    let def = def.clone();
    let cmd = cmd.clone();
    let windows = kind == terrarium_host::protocol::PathKind::Windows;
    let shell = shell
        .unwrap_or(if windows { "cmd.exe" } else { "/bin/sh" })
        .to_owned();
    let (program, args) = if cmd.shell && cmd.argv.is_empty() {
        (
            shell,
            if windows {
                Vec::new()
            } else {
                vec!["-l".to_owned()]
            },
        )
    } else {
        (cmd.argv[0].clone(), cmd.argv[1..].to_vec())
    };
    let pty = if cmd.shell {
        Some(cmd.pty.unwrap_or_default())
    } else {
        cmd.pty
    };
    if def.kind == TargetKind::Vmlab {
        if !cmd.env.is_empty() || cmd.cwd.is_some() {
            return Err(ApiError::invalid(
                "a vmlab target takes no `cwd` or `env`; vmlab exec has no such options",
            ));
        }
        if pty.is_some() && !(cmd.shell && cmd.argv.is_empty()) {
            return Err(ApiError::invalid(
                "a vmlab target runs a PTY only for `shell`",
            ));
        }
        machine_of(&def)?;
        lab_of(&def)?;
    }
    Ok(move |session| match def.kind {
        TargetKind::Docker => DaemonFrame::DockerExec(DockerExec {
            session,
            container: container_name(&def),
            cmd: if cmd.shell && cmd.argv.is_empty() {
                "/bin/sh".into()
            } else {
                program
            },
            args,
            cwd: cmd.cwd,
            env: cmd.env,
            user: None,
            pty,
            timeout_secs: cmd.timeout_secs,
        }),
        TargetKind::Vmlab if cmd.shell && cmd.argv.is_empty() => {
            DaemonFrame::VmlabShell(VmlabShell {
                session,
                lab: def.lab.clone().unwrap_or_default(),
                machine: def.machine.clone().unwrap_or_default(),
                user: None,
                pty: pty.unwrap_or_default(),
            })
        }
        TargetKind::Vmlab => DaemonFrame::VmlabExec(VmlabExec {
            session,
            lab: def.lab.clone().unwrap_or_default(),
            machine: def.machine.clone().unwrap_or_default(),
            cmd: program,
            args,
            user: None,
            timeout_secs: cmd.timeout_secs,
        }),
        // `no_exec` refused the display-only kinds above.
        // An exedev target never reaches a host agent: it runs over ssh.
        TargetKind::Host
        | TargetKind::Ssh
        | TargetKind::Exedev
        | TargetKind::Vnc
        | TargetKind::Rdp
        | TargetKind::Desktop => DaemonFrame::Exec(ExecRequest {
            windows_run_as: None,
            session,
            cmd: program,
            args,
            cwd: cmd.cwd.map(PathBuf::from),
            env: cmd.env,
            pty,
            timeout_secs: cmd.timeout_secs,
            limits,
        }),
    })
}

// ------------------------------------------------------------ exec sessions

#[derive(Default)]
struct Buf {
    /// The output offset of `chunks[0]`.
    base: u64,
    chunks: VecDeque<(Stream, Vec<u8>)>,
    len: usize,
    end: Option<End>,
    result: Option<Value>,
}

/// Reporting outcomes are not termination receipts.
pub enum CleanupAttempt {
    DeliveryUnknown,
    MissingReport,
    Deadline,
    LegacyReport(Value),
}

/// One exec, shell, or copy that a client drives over HTTP.
pub struct ExecSession {
    pub id: String,
    pub target: String,
    /// The agent ULID of the caller; `None` for the operator.
    pub owner: Option<String>,
    /// `exec`, `shell`, `push`, or `pull`.
    pub kind: &'static str,
    buf: Mutex<Buf>,
    changed: Notify,
    room: Notify,
    control: mpsc::UnboundedSender<Control>,
    touched: Mutex<Instant>,
    limit: usize,
    /// A `tool` session: does the tool take stdin? `true` for every other
    /// kind. Kept on the session, so the answer does not change when the
    /// run ends.
    pub takes_stdin: std::sync::atomic::AtomicBool,
    /// The report of a kill of a wrapped run ([`crate::run_kill`]).
    kill_report: Mutex<Option<crate::run_kill::Report>>,
    sync_end: Mutex<Option<mpsc::UnboundedSender<End>>>,
}

#[derive(Serialize, Deserialize)]
struct SavedExec {
    #[serde(default)]
    durable: bool,
    id: String,
    target: String,
    owner: Option<String>,
    shell: bool,
    project: Option<String>,
    host_session: Option<(String, u32)>,
    secrets: Vec<(String, String)>,
    #[serde(default)]
    remote_key: Option<String>,
}

/// Rebuild output consumers before the wait engine reconciles process waits.
pub fn restore_sessions(state: &Shared) -> ApiResult<()> {
    let journal = crate::hosts::resume::Journal::with_db(&state.state_dir, state.db.clone());
    for (_, saved) in journal.scan::<SavedExec>("exec/")? {
        let kind = if saved.shell { "shell" } else { "exec" };
        let (mut session, ctl) = ExecSession::new(
            &saved.target,
            saved.owner,
            kind,
            state.executor.buffer_limit,
        );
        Arc::get_mut(&mut session)
            .ok_or_else(|| ApiError::internal("restored session is unexpectedly shared"))?
            .id = saved.id;
        session.takes_stdin.store(!saved.durable, Ordering::SeqCst);
        if let Some((host, id)) = saved.host_session {
            let hs = state
                .executor
                .hosts
                .reattach(&host, id)
                .ok_or_else(|| ApiError::internal("saved exec has no host replay channel"))?;
            let secrets: Vec<_> = saved
                .secrets
                .into_iter()
                .map(|(name, value)| (name, zeroize::Zeroizing::new(value)))
                .collect();
            let view = saved.project.map_or(
                terrarium_core::redact::View::All,
                terrarium_core::redact::View::Scope,
            );
            let run = redact_run(
                pump_host(hs),
                own_redactor(&secrets),
                state.db.redactor().get(),
                view,
            );
            tokio::spawn(attach(run, session.clone(), ctl, false, false));
        } else if let Some(key) = saved.remote_key {
            let run = match durable::resume(state, &key) {
                Ok(run) => run,
                Err(error) => {
                    session.finish(End::error("interrupted", error.message));
                    state.executor.insert(session);
                    continue;
                }
            };
            let secrets: Vec<_> = saved
                .secrets
                .into_iter()
                .map(|(name, value)| (name, zeroize::Zeroizing::new(value)))
                .collect();
            let view = saved.project.map_or(
                terrarium_core::redact::View::All,
                terrarium_core::redact::View::Scope,
            );
            let run = redact_run(run, own_redactor(&secrets), state.db.redactor().get(), view);
            tokio::spawn(attach(run, session.clone(), ctl, false, false));
        } else {
            session.finish(End::error("interrupted", "interrupted (ssh transport)"));
        }
        state.executor.insert(session);
    }
    Ok(())
}

/// One answer of `GET .../output`.
#[derive(Debug, Clone, Serialize)]
pub struct Output {
    pub session_id: String,
    /// The offset of the first byte in `chunks`.
    pub since: u64,
    /// The offset to send as the next `since`.
    pub next: u64,
    pub chunks: Vec<OutChunk>,
    /// True when the session ended and `chunks` holds its last byte.
    pub done: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub end: Option<End>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub result: Option<Value>,
}

#[derive(Debug, Clone, Serialize)]
pub struct OutChunk {
    pub stream: Stream,
    /// The bytes, base64.
    pub data: String,
}

async fn wait_on(n: &Notify, cond: impl Fn() -> bool, limit: Option<Duration>) -> bool {
    let notified = n.notified();
    tokio::pin!(notified);
    notified.as_mut().enable();
    if cond() {
        return true;
    }
    match limit {
        Some(d) => tokio::time::timeout(d, notified).await.is_ok(),
        None => {
            notified.await;
            true
        }
    }
}

impl ExecSession {
    fn new(
        target: &str,
        owner: Option<String>,
        kind: &'static str,
        limit: usize,
    ) -> (Arc<Self>, mpsc::UnboundedReceiver<Control>) {
        let (tx, rx) = mpsc::unbounded_channel();
        (
            Arc::new(Self {
                id: ulid::Ulid::new().to_string(),
                target: target.to_owned(),
                owner,
                kind,
                buf: Mutex::default(),
                changed: Notify::new(),
                room: Notify::new(),
                control: tx,
                touched: Mutex::new(Instant::now()),
                limit: limit.max(1),
                takes_stdin: std::sync::atomic::AtomicBool::new(true),
                kill_report: Mutex::new(None),
                sync_end: Mutex::new(None),
            }),
            rx,
        )
    }

    fn lock(&self) -> MutexGuard<'_, Buf> {
        self.buf.lock().unwrap_or_else(|e| e.into_inner())
    }

    fn touch(&self) {
        *self.touched.lock().unwrap_or_else(|e| e.into_inner()) = Instant::now();
    }

    fn idle(&self) -> Duration {
        self.touched
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .elapsed()
    }

    pub fn is_done(&self) -> bool {
        self.lock().end.is_some()
    }

    /// How the run ended, once it has ended.
    pub fn end(&self) -> Option<End> {
        self.lock().end.clone()
    }

    /// Store output; wait while the unacknowledged output is at the limit.
    async fn push(&self, stream: Stream, bytes: Vec<u8>) {
        if bytes.is_empty() {
            return;
        }
        loop {
            {
                let mut b = self.lock();
                if b.end.is_some() {
                    return;
                }
                if b.len < self.limit {
                    b.len += bytes.len();
                    b.chunks.push_back((stream, bytes));
                    drop(b);
                    self.changed.notify_waiters();
                    return;
                }
            }
            wait_on(
                &self.room,
                || {
                    let b = self.lock();
                    b.len < self.limit || b.end.is_some()
                },
                None,
            )
            .await;
        }
    }

    fn set_result(&self, v: Value) {
        self.lock().result = Some(v);
    }

    fn finish(&self, end: End) {
        if let Some(tx) = self
            .sync_end
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .as_ref()
        {
            let _ = tx.send(end);
            return;
        }
        self.finish_synced(end);
    }

    fn finish_synced(&self, end: End) {
        {
            let mut b = self.lock();
            if b.end.is_none() {
                b.end = Some(end);
            }
        }
        self.changed.notify_waiters();
        self.room.notify_waiters();
    }

    /// Send a control to the run. False when the run has ended.
    pub fn control(&self, c: Control) -> bool {
        self.touch();
        !self.is_done() && self.control.send(c).is_ok()
    }

    /// End the run: kill the process, release the host session.
    pub fn kill(&self) {
        let _ = self.control.send(Control::Kill);
    }

    /// [`Self::kill`], then wait for the report of the kill on the target
    /// ([`crate::run_kill::report`]): the PIDs that got `TERM` and `KILL`,
    /// and the ones left. `None` for a run without one, or one that ended
    /// before.
    pub async fn kill_and_report(&self) -> Option<Value> {
        let rx = self
            .kill_report
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .clone();
        let done = self.is_done();
        self.kill();
        match rx {
            Some(rx) if !done => crate::run_kill::report(rx).await,
            _ => None,
        }
    }

    /// Claim only a live wrapped route. Restored bare sessions are unavailable.
    /// Taking the channel prevents a second reporting attempt from claiming it.
    pub fn claim_cleanup(&self) -> Option<crate::run_kill::Report> {
        if self.is_done() { return None; }
        self.kill_report.lock().unwrap_or_else(|e| e.into_inner()).take()
    }

    /// Called only after a durable dispatch marker. Exactly one control send,
    /// followed by bounded observation of the original channel, never replay.
    pub async fn attempt_cleanup(&self, report: crate::run_kill::Report) -> CleanupAttempt {
        if !self.control(Control::Kill) {
            return CleanupAttempt::DeliveryUnknown;
        }
        match tokio::time::timeout(Duration::from_secs(12), crate::run_kill::report(report)).await {
            Ok(Some(report)) => CleanupAttempt::LegacyReport(report),
            Ok(None) => CleanupAttempt::MissingReport,
            Err(_) => CleanupAttempt::Deadline,
        }
    }

    fn set_kill_report(&self, r: Option<crate::run_kill::Report>) {
        *self.kill_report.lock().unwrap_or_else(|e| e.into_inner()) = r;
    }

    /// Read output from `since`, waiting up to `wait` for some. `since`
    /// acknowledges the bytes before it.
    pub async fn read(&self, since: u64, wait: Duration, max: usize) -> ApiResult<Output> {
        self.touch();
        let max = max.clamp(1, MAX_READ);
        {
            let mut b = self.lock();
            if since < b.base {
                return Err(ApiError::conflict(format!(
                    "the bytes before {} are acknowledged and gone; read from there",
                    b.base
                )));
            }
            if since > b.base + b.len as u64 {
                return Err(ApiError::invalid(format!(
                    "`since` {since} is past the end of the output ({})",
                    b.base + b.len as u64
                )));
            }
            let mut drop_n = (since - b.base) as usize;
            while drop_n > 0 {
                let Some((_, front)) = b.chunks.front_mut() else {
                    break;
                };
                if front.len() <= drop_n {
                    let n = front.len();
                    b.chunks.pop_front();
                    b.len -= n;
                    b.base += n as u64;
                    drop_n -= n;
                } else {
                    front.drain(..drop_n);
                    b.len -= drop_n;
                    b.base += drop_n as u64;
                    drop_n = 0;
                }
            }
        }
        self.room.notify_waiters();
        wait_on(
            &self.changed,
            || {
                let b = self.lock();
                b.len > 0 || b.end.is_some()
            },
            Some(wait),
        )
        .await;
        let b = self.lock();
        let mut chunks = Vec::new();
        let mut taken = 0usize;
        for (s, bytes) in &b.chunks {
            if taken >= max {
                break;
            }
            let n = bytes.len().min(max - taken);
            chunks.push(OutChunk {
                stream: *s,
                data: base64::engine::general_purpose::STANDARD.encode(&bytes[..n]),
            });
            taken += n;
        }
        let done = b.end.is_some() && taken == b.len;
        Ok(Output {
            session_id: self.id.clone(),
            since,
            next: since + taken as u64,
            chunks,
            done,
            end: if done { b.end.clone() } else { None },
            result: if done { b.result.clone() } else { None },
        })
    }

    /// A snapshot for `GET .../sessions/{id}`.
    pub fn view(&self) -> Value {
        let b = self.lock();
        json!({"session_id": self.id, "target": self.target, "kind": self.kind,
               "acknowledged": b.base, "buffered": b.len, "done": b.end.is_some(),
               "end": b.end})
    }
}

/// Forward a run into a session. `push` maps stdin to file bytes and
/// `Finish` to `cp_end`; `verify` checks the digest of a pull.
async fn attach(
    mut run: Running,
    sess: Arc<ExecSession>,
    mut ctl: mpsc::UnboundedReceiver<Control>,
    push: bool,
    verify: bool,
) {
    let mut hasher = Sha256::new();
    let mut count = 0u64;
    let mut ctl_open = true;
    loop {
        tokio::select! {
            ev = run.events.recv() => match ev {
                None => {
                    sess.finish(End::error("lost", "the run ended without an end"));
                    break;
                }
                Some(Ev::Started) => {}
                Some(Ev::Data(s, b)) => {
                    if verify {
                        hasher.update(&b);
                        count += b.len() as u64;
                    }
                    sess.push(s, b).await;
                }
                Some(Ev::Result(v)) => sess.set_result(v),
                Some(Ev::End(e)) => {
                    let e = match e {
                        End::Cp { bytes, sha256 } if verify => {
                            let mine = hex(&hasher.clone().finalize());
                            if bytes != count || sha256 != mine {
                                End::error("checksum_mismatch", format!(
                                    "the host sent {count} bytes with SHA-256 {mine}; it reported {bytes} and {sha256}"))
                            } else {
                                End::Cp { bytes, sha256 }
                            }
                        }
                        other => other,
                    };
                    sess.finish(e);
                    break;
                }
            },
            c = ctl.recv(), if ctl_open => match c {
                None => ctl_open = false,
                Some(Control::Kill) => {
                    let _ = run.ctl.send(Control::Kill);
                    sess.finish(End::error("killed", "the session was closed"));
                    break;
                }
                Some(Control::Stdin(b)) if push => { let _ = run.ctl.send(Control::File(b)); }
                Some(Control::Eof) if push => {}
                Some(c) => { let _ = run.ctl.send(c); }
            },
        }
    }
}

/// A two-step push through a shell on the target: `cat` into a partial
/// file, then a check of the size and a rename. `start` starts a command.
async fn push_via_shell<F>(
    start: F,
    sess: Arc<ExecSession>,
    mut ctl: mpsc::UnboundedReceiver<Control>,
    dst: String,
    mode: Option<u32>,
) where
    F: Fn(RemoteCmd) -> ApiResult<Running> + Send + Sync,
{
    let part = format!("{dst}.terrarium-{}.part", ulid::Ulid::new());
    let sh = |script: &str, args: &[&str]| RemoteCmd {
        argv: ["sh", "-c", script, "sh"]
            .iter()
            .chain(args)
            .map(|s| (*s).to_owned())
            .collect(),
        timeout_secs: Some(VERB_TIMEOUT),
        ..RemoteCmd::default()
    };
    let mut run = match start(sh(
        "mkdir -p -- \"$(dirname -- \"$1\")\" && cat > \"$1\"",
        &[&part],
    )) {
        Ok(r) => r,
        Err(e) => return sess.finish(End::error("spawn_failed", e.message)),
    };
    let mut hasher = Sha256::new();
    let mut count = 0u64;
    let mut stderr = Vec::new();
    let claimed = loop {
        tokio::select! {
            ev = run.events.recv() => match ev {
                Some(Ev::Data(Stream::Stderr, b)) => stderr.extend_from_slice(&b),
                Some(Ev::End(e)) => {
                    let msg = String::from_utf8_lossy(&stderr).trim().to_owned();
                    return sess.finish(End::error("io", format!("the copy ended early ({e:?}): {msg}")));
                }
                None => return sess.finish(End::error("lost", "the copy ended early")),
                Some(_) => {}
            },
            c = ctl.recv() => match c {
                Some(Control::Stdin(b)) => {
                    hasher.update(&b);
                    count += b.len() as u64;
                    let _ = run.ctl.send(Control::Stdin(b));
                }
                Some(Control::Finish { bytes, sha256 }) => break Some((bytes, sha256)),
                Some(Control::Kill) | None => break None,
                Some(_) => {}
            },
        }
    };
    let mine = hex(&hasher.finalize());
    let ok = matches!(&claimed, Some((b, s)) if *b == count && *s == mine);
    if !ok {
        let _ = run.ctl.send(Control::Kill);
        drop(run);
        if let Ok(r) = start(sh("rm -f -- \"$1\"", &[&part])) {
            r.collect(4096).await;
        }
        return sess.finish(match claimed {
            None => End::error("killed", "the session was closed"),
            Some((b, s)) => End::error(
                "checksum_mismatch",
                format!(
                    "the daemon got {count} bytes with SHA-256 {mine}; the client sent {b} and {s}"
                ),
            ),
        });
    }
    let _ = run.ctl.send(Control::Eof);
    let first = run.collect(64 * 1024).await;
    if !first.end().success() {
        return sess.finish(End::error(
            "io",
            format!("writing {part} failed: {}", first.stderr_tail()),
        ));
    }
    let chmod = mode.map(|m| format!("{m:o}")).unwrap_or_default();
    let finish = start(sh(
        "test \"$(wc -c < \"$1\" | tr -d ' ')\" = \"$2\" || { rm -f -- \"$1\"; echo \"size mismatch\" >&2; exit 1; }; \
         if [ -n \"$4\" ]; then chmod \"$4\" \"$1\"; fi; mv -f -- \"$1\" \"$3\"",
        &[&part, &count.to_string(), &dst, &chmod],
    ));
    match finish {
        Ok(r) => {
            let c = r.collect(64 * 1024).await;
            if c.end().success() {
                sess.finish(End::Cp {
                    bytes: count,
                    sha256: mine,
                });
            } else {
                sess.finish(End::error(
                    "io",
                    format!("moving the file into place failed: {}", c.stderr_tail()),
                ));
            }
        }
        Err(e) => sess.finish(End::error("spawn_failed", e.message)),
    }
}

/// A pull through `cat` on the target: stdout becomes file bytes.
async fn pull_via_cat(
    mut run: Running,
    sess: Arc<ExecSession>,
    mut ctl: mpsc::UnboundedReceiver<Control>,
) {
    let mut hasher = Sha256::new();
    let mut count = 0u64;
    let mut stderr = Vec::new();
    loop {
        tokio::select! {
            ev = run.events.recv() => match ev {
                Some(Ev::Data(Stream::Stderr, b)) => stderr.extend_from_slice(&b),
                Some(Ev::Data(_, b)) => {
                    hasher.update(&b);
                    count += b.len() as u64;
                    sess.push(Stream::File, b).await;
                }
                Some(Ev::End(e)) => {
                    if e.success() {
                        sess.finish(End::Cp { bytes: count, sha256: hex(&hasher.finalize()) });
                    } else {
                        let msg = String::from_utf8_lossy(&stderr).trim().to_owned();
                        sess.finish(End::error("io", format!("reading the file failed: {msg}")));
                    }
                    return;
                }
                None => return sess.finish(End::error("lost", "the copy ended early")),
                Some(_) => {}
            },
            c = ctl.recv() => if matches!(c, None | Some(Control::Kill)) {
                let _ = run.ctl.send(Control::Kill);
                return sess.finish(End::error("killed", "the session was closed"));
            },
        }
    }
}

// --------------------------------------------------------------- executor

/// The executor state: the host registry and the exec sessions.
pub struct Executor {
    /// Serialize reservation enforcement and cleanup across API calls/timers.
    resource_locks: Mutex<HashMap<String, Arc<tokio::sync::Mutex<()>>>>,
    resource_verbs: Mutex<HashMap<String, usize>>,
    resource_pending: Mutex<HashMap<String, String>>,
    pub hosts: Arc<HostRegistry>,
    sessions: Mutex<HashMap<String, Arc<ExecSession>>>,
    pub max_per_agent: usize,
    pub ssh_bin: String,
    /// The exe.dev HTTPS API of `exedev` targets ([`crate::exedev`]).
    /// Tests point it at a fake.
    pub exedev_api: String,
    /// The locks and the sweep timer of `exedev` targets.
    pub exedev: crate::exedev::Locks,
    pub buffer_limit: usize,
    reaper: AtomicBool,
    /// The input rate limit of target displays (docs/api.md §17.10).
    pub display_limiter: crate::display::Limiter,
}

impl Default for Executor {
    fn default() -> Self {
        Self {
            hosts: Arc::default(),
            resource_locks: Mutex::default(),
            resource_verbs: Mutex::default(),
            resource_pending: Mutex::default(),
            sessions: Mutex::default(),
            max_per_agent: MAX_SESSIONS_PER_AGENT,
            ssh_bin: std::env::var("TERRARIUM_SSH").unwrap_or_else(|_| "ssh".into()),
            exedev_api: crate::exedev::DEFAULT_API.into(),
            exedev: crate::exedev::Locks::default(),
            buffer_limit: SESSION_BUFFER,
            reaper: AtomicBool::new(false),
            display_limiter: crate::display::Limiter::default(),
        }
    }
}

struct ResourceVerb {
    executor: Arc<Executor>,
    host: String,
}

impl Drop for ResourceVerb {
    fn drop(&mut self) {
        let mut verbs = self
            .executor
            .resource_verbs
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        if let Some(count) = verbs.get_mut(&self.host) {
            *count -= 1;
            if *count == 0 {
                verbs.remove(&self.host);
            }
        }
    }
}

impl Executor {
    pub fn resource_lock(&self, host: &str) -> Arc<tokio::sync::Mutex<()>> {
        self.resource_lock_key(format!("host:{host}"))
    }

    fn resource_lock_key(&self, key: String) -> Arc<tokio::sync::Mutex<()>> {
        self.resource_locks
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .entry(key)
            .or_default()
            .clone()
    }

    pub fn target_resource_lock(&self, def: &TargetDef) -> Arc<tokio::sync::Mutex<()>> {
        match def.host.as_deref() {
            Some(host) => self.resource_lock(host),
            None => self.resource_lock_key(format!("target:{}", def.name)),
        }
    }

    /// Wait for reconnect enforcement without holding the lock it needs.
    /// Return the admission guard so callers retain the existing atomic
    /// budget check and session registration boundary.
    pub(crate) async fn target_admission(
        &self,
        def: &TargetDef,
    ) -> ApiResult<tokio::sync::OwnedMutexGuard<()>> {
        let lock = self.target_resource_lock(def);
        tokio::time::timeout(Duration::from_secs(60), async {
            loop {
                let guard = lock.clone().lock_owned().await;
                if !def
                    .host
                    .as_deref()
                    .is_some_and(|h| self.resource_pending(h))
                {
                    return guard;
                }
                drop(guard);
                tokio::time::sleep(Duration::from_millis(50)).await;
            }
        })
        .await
        .map_err(|_| {
            ApiError::conflict(
                "host reservation reconciliation is pending; retry after enforcement is restored",
            )
        })
    }

    pub fn resource_verb_idle(&self, host: &str) -> ApiResult<()> {
        if let Some(count) = self
            .resource_verbs
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .get(host)
        {
            return Err(ApiError::resource_busy(format!(
                "{count} lifecycle verbs in progress on host `{host}`; retry the resource change"
            )));
        }
        Ok(())
    }

    fn begin_resource_verb(self: &Arc<Self>, def: &TargetDef) -> ApiResult<ResourceVerb> {
        let host = host_of(def)?.to_owned();
        *self
            .resource_verbs
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .entry(host.clone())
            .or_default() += 1;
        Ok(ResourceVerb {
            executor: self.clone(),
            host,
        })
    }

    pub fn set_resource_pending(&self, host: &str, pending: bool) {
        let mut hosts = self
            .resource_pending
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        if pending {
            hosts.insert(host.to_owned(), ulid::Ulid::new().to_string());
        } else {
            hosts.remove(host);
        }
    }

    pub fn resource_pending(&self, host: &str) -> bool {
        self.resource_pending
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .contains_key(host)
    }

    pub fn resource_epoch(&self, host: &str) -> Option<String> {
        self.resource_pending
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .get(host)
            .cloned()
    }

    pub fn finish_resource_reconcile(&self, host: &str, epoch: Option<&str>) -> bool {
        let mut pending = self
            .resource_pending
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        if pending.get(host).map(String::as_str) != epoch {
            return false;
        }
        pending.remove(host);
        true
    }

    fn table(&self) -> MutexGuard<'_, HashMap<String, Arc<ExecSession>>> {
        self.sessions.lock().unwrap_or_else(|e| e.into_inner())
    }

    /// The exec sessions of agent `agent` (a ULID) that have not ended:
    /// its shells and its `terrarium run` jobs. They keep the agent from
    /// an idle stop (`crate::lifecycle`).
    pub fn live_of(&self, agent: &str) -> usize {
        self.table()
            .values()
            .filter(|s| s.owner.as_deref() == Some(agent) && !s.is_done())
            .count()
    }

    fn insert(self: &Arc<Self>, s: Arc<ExecSession>) {
        self.table().insert(s.id.clone(), s);
        if !self.reaper.swap(true, Ordering::SeqCst) {
            let me = Arc::downgrade(self);
            tokio::spawn(async move {
                loop {
                    tokio::time::sleep(Duration::from_secs(30)).await;
                    let Some(me) = me.upgrade() else { return };
                    me.reap();
                }
            });
        }
    }

    fn reap(&self) {
        let mut t = self.table();
        let old: Vec<String> = t
            .values()
            .filter(|s| {
                let idle = s.idle();
                (s.is_done() && idle > DONE_IDLE) || (s.kind != "internal" && idle > LIVE_IDLE)
            })
            .map(|s| s.id.clone())
            .collect();
        for id in old {
            if let Err(error) = self.hosts.forget_exec(&id) {
                warn!(%error, session = id, "cannot expire durable exec session");
                continue;
            }
            if let Some(s) = t.remove(&id) {
                s.kill();
            }
        }
    }

    /// A session the actor may drive.
    pub fn session(&self, actor: &Actor, target: &str, id: &str) -> ApiResult<Arc<ExecSession>> {
        let s = self
            .table()
            .get(id)
            .cloned()
            .ok_or_else(|| ApiError::not_found(format!("no exec session `{id}`")))?;
        let mine = match actor {
            Actor::Operator => true,
            Actor::Agent(a) => s.owner.as_deref() == Some(a.id.as_str()),
        };
        // A tool run has its own routes (`/api/tools/sessions/...`).
        if !mine || s.target != target || s.kind == "tool" {
            return Err(ApiError::not_found(format!(
                "no exec session `{id}` on target `{target}`"
            )));
        }
        Ok(s)
    }

    /// A session by its ID alone, for the daemon's own readers (a process
    /// wait, docs/api.md §6.7). No rights check.
    pub fn find(&self, id: &str) -> Option<Arc<ExecSession>> {
        self.table().get(id).cloned()
    }

    /// Close a session: kill its run and forget it.
    /// The tool run `id` for `actor`: the agent that started it, or the
    /// operator. Anyone else gets `404`, as for an unknown session.
    pub fn tool_session(&self, actor: &Actor, id: &str) -> ApiResult<Arc<ExecSession>> {
        let missing = || ApiError::not_found(format!("no tool session `{id}`"));
        let s = self.table().get(id).cloned().ok_or_else(missing)?;
        let mine = match actor {
            Actor::Operator => true,
            Actor::Agent(a) => s.owner.as_deref() == Some(a.id.as_str()),
        };
        if !mine || s.kind != "tool" {
            return Err(missing());
        }
        Ok(s)
    }

    /// Kill tool run `id` and forget it ([`Self::tool_session`]).
    pub fn close_tool(&self, actor: &Actor, id: &str) -> ApiResult<()> {
        let s = self.tool_session(actor, id)?;
        s.kill();
        self.table().remove(&s.id);
        Ok(())
    }

    pub fn close(&self, actor: &Actor, target: &str, id: &str) -> ApiResult<()> {
        let s = self.session(actor, target, id)?;
        self.hosts.forget_exec(&s.id)?;
        s.kill();
        self.table().remove(&s.id);
        Ok(())
    }

    /// Live sessions, for tests and the dashboard.
    pub fn session_count(&self) -> usize {
        self.table().len()
    }

    pub fn resource_sessions(&self, targets: &[String], holder: Option<&str>) -> Vec<String> {
        self.table()
            .values()
            .filter(|s| {
                targets.contains(&s.target)
                    && holder.is_none_or(|a| s.owner.as_deref() == Some(a))
                    && !s.is_done()
            })
            .map(|s| s.id.clone())
            .collect()
    }

    pub async fn end_resource_sessions(
        &self,
        targets: &[String],
        holder: Option<&str>,
    ) -> Vec<Value> {
        let sessions: Vec<_> = self
            .table()
            .values()
            .filter(|s| {
                targets.contains(&s.target)
                    && holder.is_none_or(|a| s.owner.as_deref() == Some(a))
                    && !s.is_done()
            })
            .cloned()
            .collect();
        let mut checks = tokio::task::JoinSet::new();
        for s in sessions {
            let report = s
                .kill_report
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .clone();
            s.finish(End::error(
                "reservation_ended",
                "your resource reservation on this host ended",
            ));
            s.kill();
            checks.spawn(async move {
                let result = match report {
                    Some(report) => crate::run_kill::report(report).await,
                    None => None,
                };
                let confirmed = result.as_ref().is_some_and(|r| {
                    r.get("error").is_none()
                        && r["pid"].is_u64()
                        && r["left"].as_array().is_some_and(Vec::is_empty)
                });
                (!confirmed).then(|| json!({"session":s.id,"target":s.target,"report":result}))
            });
        }
        let mut unconfirmed = Vec::new();
        while let Some(result) = checks.join_next().await {
            match result {
                Ok(Some(detail)) => unconfirmed.push(detail),
                Err(e) => unconfirmed.push(json!({"error":e.to_string()})),
                _ => {}
            }
        }
        unconfirmed
    }

    /// End the live sessions of `agent` on `target`: each ends with the
    /// error `code` and `message`, and its run is killed. For a lease that
    /// ended (`lease_revoked`). Returns the number of sessions.
    pub fn end_sessions(&self, target: &str, agent: &str, code: &str, message: &str) -> usize {
        let live: Vec<Arc<ExecSession>> = self
            .table()
            .values()
            .filter(|s| s.target == target && s.owner.as_deref() == Some(agent) && !s.is_done())
            .cloned()
            .collect();
        for s in &live {
            s.finish(End::error(code, message));
            s.kill();
        }
        live.len()
    }
}

/// The repo host keys, for the git steps on a target
/// (`TERRARIUM_GIT_KNOWN_HOSTS`): ssh there reads no `~/.ssh/known_hosts`,
/// which a recipe or an agent could seed. These are the keys that the
/// daemon's own git trusts (`crate::git::known_hosts_dir`: GitHub's
/// published keys and the recorded ones), not the host keys of the
/// targets. `None` when there are none.
pub fn git_known_hosts(state: &Shared) -> Option<String> {
    repo_known_hosts(&state.state_dir)
}

fn repo_known_hosts(state_dir: &Path) -> Option<String> {
    let dir = match crate::git::ensure_known_hosts_dir(state_dir) {
        Ok(d) => d,
        Err(e) => {
            warn!("cannot read the repo host keys: {e}");
            return None;
        }
    };
    let text = std::fs::read_to_string(dir.join(crate::git::KNOWN_HOSTS_FILE)).ok()?;
    let text = text.trim();
    (!text.is_empty()).then(|| text.to_owned())
}

pub(crate) fn known_hosts(state_dir: &Path) -> ApiResult<PathBuf> {
    use std::os::unix::fs::PermissionsExt;
    let dir = state_dir.join("ssh");
    std::fs::create_dir_all(&dir)
        .and_then(|()| std::fs::set_permissions(&dir, std::fs::Permissions::from_mode(0o700)))
        .map_err(|e| ApiError::internal(format!("cannot create {}: {e}", dir.display())))?;
    Ok(dir.join("known_hosts"))
}

/// Start a command on a target.
pub fn start_cmd(state: &Shared, def: &TargetDef, cmd: &RemoteCmd) -> ApiResult<Running> {
    let lock = state.executor.target_resource_lock(def);
    if let Ok(_resources) = lock.clone().try_lock_owned() {
        let run = start_cmd_locked(state, def, cmd)?;
        return track_resource_run(state, def, cmd, run, None);
    }
    // This entry point is synchronous (also used by process-tree wrappers).
    // Wait asynchronously rather than blocking the runtime or rejecting a
    // command merely because a short admission write is in flight.
    let (tx, events) = mpsc::channel(16);
    let (ctl, mut controls) = mpsc::unbounded_channel();
    let state = state.clone();
    let def = def.clone();
    let cmd = cmd.clone();
    tokio::spawn(async move {
        let guard = tokio::select! {
            _ = tx.closed() => return,
            result = tokio::time::timeout(Duration::from_millis(500), lock.lock_owned()) => {
                match result {
                    Ok(guard) => guard,
                    Err(_) => {
                        let _ = tx.send(Ev::End(End::error("resource_busy", "host admission is busy; retry the command"))).await;
                        return;
                    }
                }
            }
        };
        let run = start_cmd_locked(&state, &def, &cmd)
            .and_then(|run| track_resource_run(&state, &def, &cmd, run, None));
        drop(guard);
        let mut run = match run {
            Ok(run) => run,
            Err(error) => {
                let code = serde_json::to_value(error.code).unwrap_or_default();
                let _ = tx
                    .send(Ev::End(End::error(
                        code.as_str().unwrap_or("internal"),
                        error.message,
                    )))
                    .await;
                return;
            }
        };
        loop {
            tokio::select! {
                _ = tx.closed() => { let _ = run.ctl.send(Control::Kill); break; }
                control = controls.recv() => match control {
                    Some(control) => { let _ = run.ctl.send(control); }
                    None => { let _ = run.ctl.send(Control::Kill); break; }
                },
                event = run.events.recv() => match event {
                    Some(event) => {
                        let ended = matches!(event, Ev::End(_));
                        if tx.send(event).await.is_err() || ended { break; }
                    }
                    None => break,
                },
            }
        }
    });
    Ok(Running {
        events,
        ctl,
        host_session: None,
    })
}

/// Internal jobs and probes must participate in reservation admission and
/// cleanup too. Forward their output without retaining unredacted bytes in
/// the public session buffer.
pub(crate) fn track_resource_run(
    state: &Shared,
    def: &TargetDef,
    cmd: &RemoteCmd,
    mut run: Running,
    report: Option<crate::run_kill::Report>,
) -> ApiResult<Running> {
    let host_session = run.host_session.clone();
    let owner = state.db.read_now(|conn| {
        Ok(cmd
            .holder
            .as_deref()
            .and_then(|holder| crate::store::agents::find(conn, holder, None).ok())
            .map(|a| a.agent.id.to_string()))
    })?;
    let (session, mut controls) = ExecSession::new(&def.name, owner, "internal", 1);
    session.set_kill_report(report);
    state.executor.insert(session.clone());
    let weak = Arc::downgrade(&state.executor);
    let ctl = run.ctl.clone();
    let forward_ctl = ctl.clone();
    let control_task = tokio::spawn(async move {
        while let Some(control) = controls.recv().await {
            if forward_ctl.send(control).is_err() {
                break;
            }
        }
    });
    let (tx, events) = mpsc::channel(16);
    tokio::spawn(async move {
        let mut sent_end = false;
        while let Some(mut event) = run.events.recv().await {
            let ended = matches!(event, Ev::End(_));
            if let Ev::End(end) = &mut event {
                session.finish(end.clone());
                if let Some(recorded) = session.end() {
                    *end = recorded;
                }
            }
            if tx.send(event).await.is_err() {
                let _ = run.ctl.send(Control::Kill);
                break;
            }
            if ended {
                sent_end = true;
                break;
            }
        }
        session.finish(End::error(
            "lost",
            "the internal command ended without an end",
        ));
        if !sent_end && let Some(end) = session.end() {
            let _ = tx.send(Ev::End(end)).await;
        }
        control_task.abort();
        if let Some(executor) = weak.upgrade() {
            executor.table().remove(&session.id);
        }
    });
    Ok(Running {
        events,
        ctl,
        host_session,
    })
}

/// The caller holds resource_lock through session registration.
pub(crate) fn start_cmd_locked(
    state: &Shared,
    def: &TargetDef,
    cmd: &RemoteCmd,
) -> ApiResult<Running> {
    if cmd.durable && (cmd.pty.is_some() || cmd.shell) {
        return Err(ApiError::invalid(
            "durable execution cannot use a PTY or shell",
        ));
    }
    if state.executor.hosts.is_stopped() {
        return Err(ApiError::conflict("executor is stopping"));
    }
    no_exec(def)?;
    // An exedev target runs its commands over ssh on its VM.
    if def.kind == TargetKind::Exedev {
        let d = crate::exedev::exec_def(state, def)?;
        return start_cmd_locked(state, &d, cmd);
    }
    // SSH takes a separate execution path, but must not bypass a host's
    // exclusive reservation policy.
    if def.kind == TargetKind::Ssh {
        crate::reservations::exec_budget(state, def, cmd.holder.as_deref())?;
    }
    let ex = &state.executor;
    match def.kind {
        TargetKind::Ssh => {
            // The secret of a project target: the project's first, then
            // the global one (PLAN §16b.2).
            let (key, guard): (Option<PathBuf>, Option<Box<dyn Send>>) = match &def.credential {
                None => (None, None),
                Some(name) => {
                    match crate::secrets::load_now(state, crate::store::targets::owner(def), name)?
                    {
                        Secret::SshKey(k) => {
                            let run = RunDirs::new(&state.state_dir).run_dir()?;
                            let p = run.write_private("id", k.as_bytes())?;
                            (Some(p), Some(Box::new(run)))
                        }
                        other => {
                            return Err(ApiError::invalid(format!(
                                "secret `{name}` is a {}; an ssh target needs an ssh_key",
                                other.kind_label()
                            )));
                        }
                    }
                }
            };
            let plan = ssh_plan(
                &ex.ssh_bin,
                def,
                key.as_deref(),
                &known_hosts(&state.state_dir)?,
                cmd,
            )?;
            if cmd.durable {
                return durable::start(state, def, cmd, plan, guard);
            }
            let mut c = tokio::process::Command::new(&plan.program);
            c.args(&plan.args);
            Ok(pump_process(
                c,
                plan.stdin_prefix,
                cmd.timeout_secs.filter(|t| *t > 0).map(Duration::from_secs),
                guard,
            ))
        }
        _ => {
            let host = host_of(def)?;
            let caps = ex.hosts.capabilities(host);
            let run_as = windows_run_as(state, def)?;
            if cmd.prepare_root.is_some()
                && caps
                    .as_ref()
                    .and_then(|c| c.native_gpu.as_ref())
                    .is_none_or(|n| {
                        !n.enforced || n.version < terrarium_host::native::PROTOCOL_VERSION
                    })
            {
                return Err(ApiError::invalid(
                    "owned native roots require matching host agent and helper protocol 2; install both before retrying",
                ));
            }
            let limits = match crate::reservations::exec_budget(state, def, cmd.holder.as_deref())?
            {
                Some(limits) => Some(limits),
                None => exec_limits(def, caps.as_ref(), cmd.holder.as_deref())?,
            };
            let mut limits = native_gpu_limits(
                def,
                caps.as_ref(),
                cmd.holder.as_deref(),
                cmd.operator,
                limits,
            )?;
            if let Some(native) = limits.as_mut().and_then(|l| l.native_gpu.as_mut()) {
                native.prepare_root = cmd.prepare_root.clone();
            }
            let shell = caps.and_then(|c| c.shell);
            let make = host_frame_paths(def, shell.as_deref(), cmd, limits, path_kind(state, def))?;
            Ok(pump_host(ex.hosts.open_bound(
                host,
                cmd.resume_key.as_deref(),
                move |id| {
                    let mut frame = make(id);
                    if let DaemonFrame::Exec(req) = &mut frame {
                        req.windows_run_as = run_as;
                    }
                    frame
                },
            )?))
        }
    }
}

fn windows_run_as(
    state: &Shared,
    def: &TargetDef,
) -> ApiResult<Option<terrarium_host::protocol::WindowsRunAs>> {
    let Some(config) = &def.windows_run_as else {
        return Ok(None);
    };
    crate::links::refuse_for_link("use a Windows run-as target")?;
    crate::store::targets::validate(def)?;
    if !state.executor.hosts.supports_windows_run_as(host_of(def)?) {
        return Err(ApiError::invalid(
            "windows_run_as requires a Windows host with the windows_run_as capability",
        ));
    }
    match crate::secrets::load_now(
        state,
        crate::store::targets::owner(def),
        &config.password_secret,
    )? {
        Secret::Login { password, .. } => Ok(Some(terrarium_host::protocol::WindowsRunAs {
            account: config.account.clone(),
            password: password.to_string(),
        })),
        _ => Err(ApiError::invalid(
            "windows_run_as.password_secret must name a login secret",
        )),
    }
}

/// Run a host-agent verb to its end.
/// Only the run-kill guard calls this for its generated cleanup commands.
/// Cleanup must work while reservations block new user commands, and must
/// not enter the very slice being stopped.
pub(crate) fn start_resource_cleanup(
    state: &Shared,
    def: &TargetDef,
    cmd: &RemoteCmd,
) -> ApiResult<Running> {
    if !matches!(def.kind, TargetKind::Host | TargetKind::Docker) {
        return start_cmd(state, def, cmd);
    }
    let host = host_of(def)?;
    let shell = state
        .executor
        .hosts
        .capabilities(host)
        .and_then(|c| c.shell);
    let make = host_frame_paths(def, shell.as_deref(), cmd, None, path_kind(state, def))?;
    Ok(pump_host(state.executor.hosts.open(host, make)?))
}

async fn verb(
    state: &Shared,
    def: &TargetDef,
    make: impl FnOnce(u32) -> DaemonFrame,
) -> ApiResult<Collected> {
    let host = host_of(def)?;
    let run = pump_host(state.executor.hosts.open(host, make)?);
    Ok(run.collect(1024 * 1024).await)
}

fn verb_failed(what: &str, c: &Collected) -> ApiError {
    let end = c.end();
    let code = match &end {
        End::Error { code, .. } => code.clone(),
        _ => "failed".into(),
    };
    let msg = match &end {
        End::Error { message, .. } => message.clone(),
        End::Exit { code, signal, .. } => format!(
            "exit {}{}",
            code.map_or("-".into(), |c| c.to_string()),
            signal.map(|s| format!(", signal {s}")).unwrap_or_default()
        ),
        End::Cp { .. } => String::new(),
    };
    ApiError::invalid(format!("{what} failed: {msg}")).with_details(json!({
        "verb": what, "code": code, "end": end, "stderr": c.stderr_tail(),
        "stdout": String::from_utf8_lossy(&c.stdout).chars().take(2000).collect::<String>(),
    }))
}

pub(crate) async fn verb_ok(
    state: &Shared,
    def: &TargetDef,
    what: &str,
    make: impl FnOnce(u32) -> DaemonFrame,
) -> ApiResult<Collected> {
    let c = verb(state, def, make).await?;
    if c.end().success() {
        Ok(c)
    } else {
        Err(verb_failed(what, &c))
    }
}

// ----------------------------------------------------------------- policy

/// Load a target and check that the actor may use it: its policy lists
/// the target, or it holds an active lease on it (docs/api.md §17.7).
pub async fn usable(state: &Shared, actor: &Actor, name: &str) -> ApiResult<TargetRow> {
    let n = name.to_owned();
    let agent = actor.agent().map(|a| a.id.clone());
    let (row, lease) = state
        .db
        .write_full(actor.label(), move |c| {
            let row = targets::get(c, &n)?;
            // Until typed dispatch-to-exec serialization is wired, never
            // route a live registration through the legacy backend. The
            // disabled production gate also applies to operator exec.
            let live: bool = c.query_row(
                "SELECT EXISTS(SELECT 1 FROM live_workspace_targets WHERE target_id = ?1 AND state != 'destroyed')",
                [&n], |r| r.get(0),
            )?;
            if live {
                return Err(ApiError::new(Code::Conflict, "live workspace exec activation is disabled"));
            }
            crate::store::live_workspaces::admit_target_exec(c, agent.as_deref(), &n)?;
            let lease = match &agent {
                Some(a) => {
                    // TER-623: a release target is for the release job
                    // and the operator only, whatever the policy or lease.
                    crate::store::jobs::check_not_release_target(c, &n, "using the target")?;
                    leases::standing(c, &n, a)?
                }
                None => leases::Standing::None,
            };
            Ok((row, lease))
        })
        .await?;
    policy::use_target(actor, &row.def.name, row.def.project.as_deref(), lease)?;
    Ok(row)
}

fn cmd_text(cmd: &RemoteCmd) -> String {
    let s = if cmd.shell && cmd.argv.is_empty() {
        "<login shell>".to_owned()
    } else {
        cmd.argv.join(" ")
    };
    if s.chars().count() > AUDIT_CMD_CHARS {
        let mut t: String = s.chars().take(AUDIT_CMD_CHARS).collect();
        t.push('…');
        t
    } else {
        s
    }
}

fn check_cap(state: &Shared, actor: &Actor) -> ApiResult<()> {
    if let Actor::Agent(a) = actor {
        let n = state.executor.live_of(&a.id);
        if n >= state.executor.max_per_agent {
            return Err(ApiError::new(
                Code::RateLimited,
                format!(
                    "{n} exec sessions are open; an agent may hold {}. Wait for one to end or close one",
                    state.executor.max_per_agent
                ),
            ));
        }
    }
    Ok(())
}

async fn audit(
    state: &Shared,
    actor: &Actor,
    row: &TargetRow,
    action: &str,
    detail: Value,
) -> ApiResult<()> {
    let target = row.def.name.clone();
    let project = row.def.project.clone();
    let action = action.to_owned();
    state
        .db
        .write(actor.label(), move |t| {
            t.audit(&action, &target, project.as_deref(), detail)
        })
        .await
}

/// `POST /api/targets/{name}/exec`: start a command in a new session.
pub async fn start_exec(
    state: &Shared,
    actor: &Actor,
    name: &str,
    cmd: RemoteCmd,
) -> ApiResult<Arc<ExecSession>> {
    start_exec_as(state, actor, name, cmd, None).await
}

/// Who a command runs for, when it is not the actor itself: a schedule
/// run. `label` is the actor of the `target.exec` audit row
/// (`schedule:<id>`), and `holder` the resource budget of the command.
#[derive(Debug, Clone, Default)]
pub struct OnBehalf {
    pub label: Option<String>,
    pub holder: Option<String>,
    /// More fields of the audit row (`run_as`, `owner`).
    pub detail: serde_json::Map<String, Value>,
}

/// [`start_exec`] for a schedule run: the rights are `actor`'s, the
/// audit row and the budget are `on`'s.
pub async fn start_exec_as(
    state: &Shared,
    actor: &Actor,
    name: &str,
    cmd: RemoteCmd,
    on: Option<OnBehalf>,
) -> ApiResult<Arc<ExecSession>> {
    start_exec_secrets(state, actor, name, cmd, on, Vec::new()).await
}

/// [`start_exec`] for `terrarium run` with secret values in `cmd.env`
/// (TER-142, `crate::secrets::run_env`): `secrets` is `(label, value)` of
/// each one. The output is redacted with these values first, also a value
/// that the daemon matcher leaves out as weak (at least
/// [`TOOL_SECRET_MIN`] bytes), then with the daemon matcher.
pub async fn start_run_exec(
    state: &Shared,
    actor: &Actor,
    name: &str,
    cmd: RemoteCmd,
    secrets: Vec<(String, zeroize::Zeroizing<String>)>,
) -> ApiResult<Arc<ExecSession>> {
    start_exec_secrets(state, actor, name, cmd, None, secrets).await
}

async fn start_exec_secrets(
    state: &Shared,
    actor: &Actor,
    name: &str,
    cmd: RemoteCmd,
    on: Option<OnBehalf>,
    secrets: Vec<(String, zeroize::Zeroizing<String>)>,
) -> ApiResult<Arc<ExecSession>> {
    let on = on.unwrap_or_default();
    let _root_guard = crate::target_roots::lock_card(state, actor, name).await?;
    let row = usable(state, actor, name).await?;
    check_cap(state, actor)?;
    let def = resolved_def(state, &row, Some(actor), cmd.agent.as_deref()).await?;
    let mut cmd = cmd;
    if cmd.durable && cmd.resume_key.is_none() {
        cmd.resume_key = Some(format!("remote/{}", ulid::Ulid::new()));
    }
    cmd.holder = on.holder.clone().or_else(|| holder_of(actor));
    cmd.operator = actor.is_operator();
    crate::target_roots::prepare_exec(state, actor, &def, &mut cmd).await?;
    // VMLab's exec protocol cannot carry an environment.
    if def.kind != TargetKind::Vmlab
        && let Some(a) = actor.agent()
    {
        let p = crate::git_identity::provenance(state, &a.id).await?;
        for key in [
            "GIT_AUTHOR_NAME",
            "GIT_AUTHOR_EMAIL",
            "GIT_COMMITTER_NAME",
            "GIT_COMMITTER_EMAIL",
        ] {
            cmd.env.remove(key);
        }
        cmd.env.insert("TERRARIUM_GIT_TRAILERS".into(), p.trailers);
        cmd.env.remove("TERRARIUM_GIT_TRAILERS_FILE");
    }
    cmd.check_paths(path_kind(state, &def))?;
    let sync = workspace_sync::prepare(
        state,
        actor,
        &def,
        cmd.agent.as_deref().or(cmd.holder.as_deref()),
        row.def.kind == TargetKind::Exedev,
    )
    .await?;
    if let Some(sync) = &sync {
        sync.translate_cmd(&mut cmd);
    }
    let resources = state.executor.target_admission(&def).await?;
    let (run, report) = crate::run_kill::start_registered(state, &def, &cmd, &resources)?;
    // The output is redacted for the caller (PLAN §16b.6, TER-33).
    let view = match actor {
        Actor::Agent(a) => terrarium_core::redact::View::Scope(a.project.clone()),
        Actor::Operator => terrarium_core::redact::View::All,
    };
    let own = own_redactor(&secrets);
    let run = redact_run(run, own, state.db.redactor().get(), view);
    let kind = if cmd.shell { "shell" } else { "exec" };
    let (sess, ctl) = ExecSession::new(
        name,
        actor.agent().map(|a| a.id.clone()),
        kind,
        state.executor.buffer_limit,
    );
    sess.set_kill_report(report);
    sess.takes_stdin.store(!cmd.durable, Ordering::SeqCst);
    let saved = SavedExec {
        durable: cmd.durable,
        remote_key: if cmd.durable && matches!(def.kind, TargetKind::Ssh | TargetKind::Exedev) {
            cmd.resume_key.clone()
        } else {
            None
        },
        id: sess.id.clone(),
        target: sess.target.clone(),
        owner: sess.owner.clone(),
        shell: cmd.shell,
        project: actor.agent().map(|a| a.project.clone()),
        host_session: run.host_session.clone(),
        secrets: secrets
            .iter()
            .map(|(label, value)| (label.clone(), value.to_string()))
            .collect(),
    };
    state.executor.hosts.save_exec(&sess.id, &saved)?;
    drop(secrets);
    workspace_sync::finish_with(&sess, sync);
    tokio::spawn(attach(run, sess.clone(), ctl, false, false));
    state.executor.insert(sess.clone());
    drop(resources);
    let mut detail = json!({"session": sess.id, "kind": targets::kind_str(row.def.kind),
           "command": cmd_text(&cmd), "cwd": cmd.cwd, "pty": cmd.pty.is_some(),
           "env": cmd.env.keys().collect::<Vec<_>>()});
    if let Value::Object(m) = &mut detail {
        m.extend(on.detail);
    }
    let (target, project) = (row.def.name.clone(), row.def.project.clone());
    let label = on.label.unwrap_or_else(|| actor.label());
    state
        .db
        .write(label, move |t| {
            t.audit("target.exec", &target, project.as_deref(), detail)
        })
        .await?;
    Ok(sess)
}

/// What a tool run adds to its command (TER-30, design D4): the output
/// cap, the stdin rule, and the redaction of the output.
pub struct ToolRun {
    /// `stdin = true` in the tool: the caller's stdin goes to the command,
    /// at most `max_output` bytes. Else each stdin control is dropped.
    pub stdin: bool,
    /// The cap of stdout and stderr together, in raw bytes. Past it the
    /// process is killed and the session ends `truncated`.
    pub max_output: usize,
    /// The run's own secret values (`env_secrets`): `(label, value)`. They
    /// are redacted first, also a value that the daemon matcher leaves out
    /// as weak.
    pub secrets: Vec<(String, zeroize::Zeroizing<String>)>,
    /// The view of the daemon matcher for the output (the project of the
    /// caller).
    pub view: terrarium_core::redact::View,
    /// Called once at the end, with how the run ended.
    pub on_end: Box<dyn FnOnce(ToolEnd) + Send>,
}

/// The end of a tool run, for its audit row and event.
#[derive(Debug, Clone)]
pub struct ToolEnd {
    pub end: End,
    /// The raw output bytes that the command printed, up to the cap.
    pub bytes: u64,
    pub truncated: bool,
}

/// Start a tool run (TER-30, design D4): `cmd` on target `name` for
/// `actor`, which has a grant on the tool, not the target. So there is no
/// target policy check here; each other check of [`start_exec_as`] stays:
/// the session cap of the agent, the instance of the target, the resource
/// budget (`holder`), and the host session. The output goes through
/// [`attach_tool`].
pub async fn start_tool_exec(
    state: &Shared,
    actor: &Actor,
    name: &str,
    cmd: RemoteCmd,
    run: ToolRun,
) -> ApiResult<Arc<ExecSession>> {
    let n = name.to_owned();
    let row = state
        .db
        .read(move |c| {
            // TER-623: a tool defined before its target became a release
            // target (or its machine one) does not run there.
            crate::store::jobs::check_not_release_target(c, &n, "running a tool on the target")?;
            targets::get(c, &n)
        })
        .await?;
    check_cap(state, actor)?;
    let def = resolved_def(state, &row, Some(actor), None).await?;
    let mut cmd = cmd;
    cmd.shell = false;
    cmd.pty = None;
    cmd.holder = holder_of(actor);
    cmd.operator = actor.is_operator();
    let resources = state.executor.target_admission(&def).await?;
    let (running, report) = crate::run_kill::start_registered(state, &def, &cmd, &resources)?;
    let (sess, ctl) = ExecSession::new(
        name,
        actor.agent().map(|a| a.id.clone()),
        "tool",
        state.executor.buffer_limit,
    );
    sess.takes_stdin.store(run.stdin, Ordering::SeqCst);
    sess.set_kill_report(report);
    let daemon = state.db.redactor().get();
    tokio::spawn(attach_tool(running, sess.clone(), ctl, run, daemon));
    state.executor.insert(sess.clone());
    drop(resources);
    Ok(sess)
}

/// The shortest value of a tool run's own secrets that its output
/// redaction matches. Shorter values (1 to 3 bytes) would hide most of the
/// output; a definition should not use such a secret.
pub const TOOL_SECRET_MIN: usize = 4;

/// The redactors of one output stream: the run's own secrets, then the
/// daemon matcher.
struct ToolRedact {
    own: terrarium_core::redact::StreamRedactor,
    daemon: terrarium_core::redact::StreamRedactor,
}

impl ToolRedact {
    fn push(&mut self, b: &[u8]) -> Vec<u8> {
        let mid = self.own.push(b);
        self.daemon.push(&mid)
    }

    fn finish(&mut self) -> Vec<u8> {
        let mid = self.own.finish();
        let mut out = self.daemon.push(&mid);
        out.extend(self.daemon.finish());
        out
    }
}

/// The matcher of a run's own secret values (`(label, value)`): each
/// value of at least [`TOOL_SECRET_MIN`] bytes, also a weak one. `None`
/// with no value.
pub(crate) fn own_redactor(
    secrets: &[(String, zeroize::Zeroizing<String>)],
) -> Option<Arc<terrarium_core::redact::Redactor>> {
    use terrarium_core::redact::{Entry, Redactor};
    if secrets.is_empty() {
        return None;
    }
    let r = Redactor::build_min(
        secrets.iter().flat_map(|(label, v)| {
            std::iter::once(v.as_str())
                .chain(
                    v.lines()
                        .filter(|line| v.contains("PRIVATE KEY") && !line.trim().is_empty()),
                )
                .map(move |value| Entry {
                    label: label.clone(),
                    scope: None,
                    value: value.as_bytes(),
                })
        }),
        |_| true,
        TOOL_SECRET_MIN,
    )
    .unwrap_or_else(|_| Redactor::empty());
    (!r.is_empty()).then(|| Arc::new(r))
}

/// Forward a tool run into its session: the cap, the redaction of each
/// stream, and the stdin rule ([`ToolRun`]).
async fn attach_tool(
    mut run: Running,
    sess: Arc<ExecSession>,
    mut ctl: mpsc::UnboundedReceiver<Control>,
    tool: ToolRun,
    daemon: Arc<terrarium_core::redact::Redactor>,
) {
    use terrarium_core::redact::{Redactor, StreamRedactor, View};
    // The run's own values go also when they are short or weak: the
    // shortest pattern is TOOL_SECRET_MIN, not MIN_LEN.
    let own = Arc::new(
        Redactor::build_min(
            tool.secrets
                .iter()
                .map(|(label, v)| terrarium_core::redact::Entry {
                    label: label.clone(),
                    scope: None,
                    value: v.as_bytes().to_vec(),
                }),
            |_| true,
            TOOL_SECRET_MIN,
        )
        .unwrap_or_else(|_| Redactor::empty()),
    );
    let redact = |own: &Arc<Redactor>| ToolRedact {
        own: StreamRedactor::new(own.clone(), View::All),
        daemon: StreamRedactor::new(daemon.clone(), tool.view.clone()),
    };
    let mut out = redact(&own);
    let mut err = redact(&own);
    let cap = tool.max_output;
    let (mut bytes, mut truncated, mut stdin_bytes) = (0u64, false, 0usize);
    // `stdin = false` closes stdin at the start (design D4, step 7).
    if !tool.stdin {
        let _ = run.ctl.send(Control::Eof);
    }
    let mut ctl_open = true;
    let end = loop {
        tokio::select! {
            ev = run.events.recv() => match ev {
                None => break End::error("lost", "the run ended without an end"),
                Some(Ev::Started) => {}
                Some(Ev::Data(s, b)) => {
                    if truncated {
                        continue;
                    }
                    let room = cap.saturating_sub(bytes as usize);
                    let take = b.len().min(room);
                    bytes += take as u64;
                    let r = if s == Stream::Stderr { &mut err } else { &mut out };
                    let safe = r.push(&b[..take]);
                    sess.push(s, safe).await;
                    if take < b.len() {
                        truncated = true;
                        let _ = run.ctl.send(Control::Kill);
                    }
                }
                Some(Ev::Result(v)) => sess.set_result(v),
                Some(Ev::End(e)) => break e,
            },
            c = ctl.recv(), if ctl_open => match c {
                None => ctl_open = false,
                Some(Control::Kill) => {
                    let _ = run.ctl.send(Control::Kill);
                    break End::error("killed", "the session was closed");
                }
                Some(Control::Stdin(b)) if tool.stdin => {
                    if stdin_bytes + b.len() <= cap {
                        stdin_bytes += b.len();
                        let _ = run.ctl.send(Control::Stdin(b));
                    }
                }
                Some(Control::Eof) if tool.stdin => { let _ = run.ctl.send(Control::Eof); }
                Some(c @ Control::Signal(_)) => { let _ = run.ctl.send(c); }
                // No stdin for this tool, no pty, no copy: dropped.
                Some(_) => {}
            },
        }
    };
    for (s, r) in [(Stream::Stdout, &mut out), (Stream::Stderr, &mut err)] {
        let rest = r.finish();
        sess.push(s, rest).await;
    }
    let end = if truncated {
        End::error(
            "truncated",
            format!("the output reached max_output ({cap} bytes); the run was killed"),
        )
    } else {
        end
    };
    sess.set_result(json!({"bytes": bytes, "truncated": truncated}));
    sess.finish(end.clone());
    (tool.on_end)(ToolEnd {
        end,
        bytes,
        truncated,
    });
}

/// How long the redaction of an exec stream holds back the end of its
/// output with no new output (TER-33): then the held bytes go out,
/// redacted. A shell prompt or a progress line shows after this delay, not
/// only with the next output. A secret split across two writes further
/// apart than this is not found whole (PLAN §16b.6).
pub const REDACT_FLUSH_IDLE: Duration = Duration::from_millis(200);

/// The redaction of one output stream of an exec: the run's own values
/// first (`terrarium run` secrets, TER-142), then the daemon matcher.
struct ExecRedact {
    own: Option<terrarium_core::redact::StreamRedactor>,
    daemon: terrarium_core::redact::StreamRedactor,
}

impl ExecRedact {
    fn new(
        own: Option<&Arc<terrarium_core::redact::Redactor>>,
        daemon: &Arc<terrarium_core::redact::Redactor>,
        view: &terrarium_core::redact::View,
    ) -> Self {
        use terrarium_core::redact::{StreamRedactor, View};
        Self {
            own: own.map(|o| StreamRedactor::new(o.clone(), View::All)),
            daemon: StreamRedactor::new(daemon.clone(), view.clone()),
        }
    }

    fn held(&self) -> usize {
        self.own.as_ref().map_or(0, |o| o.held()) + self.daemon.held()
    }

    fn push(&mut self, b: &[u8]) -> Vec<u8> {
        match &mut self.own {
            Some(o) => {
                let mid = o.push(b);
                self.daemon.push(&mid)
            }
            None => self.daemon.push(b),
        }
    }

    fn finish(&mut self) -> Vec<u8> {
        let mut out = match &mut self.own {
            Some(o) => {
                let mid = o.finish();
                self.daemon.push(&mid)
            }
            None => Vec::new(),
        };
        out.extend(self.daemon.finish());
        out
    }
}

/// Redact the stdout and stderr of `run` as streams ([`StreamRedactor`]),
/// for `view`, so that a secret split across two output chunks is found
/// (TER-33). `own`: the run's own secret values, matched first (TER-142).
/// With no secret in either matcher, `run` is returned as it is.
///
/// [`StreamRedactor`]: terrarium_core::redact::StreamRedactor
pub(crate) fn redact_run(
    run: Running,
    own: Option<Arc<terrarium_core::redact::Redactor>>,
    redactor: Arc<terrarium_core::redact::Redactor>,
    view: terrarium_core::redact::View,
) -> Running {
    if redactor.is_empty() && own.is_none() {
        return run;
    }
    let Running {
        mut events,
        ctl,
        host_session,
    } = run;
    let (tx, rx) = mpsc::channel(64);
    tokio::spawn(async move {
        let mut out = ExecRedact::new(own.as_ref(), &redactor, &view);
        let mut err = ExecRedact::new(own.as_ref(), &redactor, &view);
        let flush = |out: &mut ExecRedact, err: &mut ExecRedact| {
            [
                (Stream::Stdout, out.finish()),
                (Stream::Stderr, err.finish()),
            ]
            .into_iter()
            .filter(|(_, b)| !b.is_empty())
            .collect::<Vec<_>>()
        };
        loop {
            let waiting = out.held() > 0 || err.held() > 0;
            let ev = if waiting {
                match tokio::time::timeout(REDACT_FLUSH_IDLE, events.recv()).await {
                    Ok(ev) => ev,
                    Err(_) => {
                        for (s, b) in flush(&mut out, &mut err) {
                            if tx.send(Ev::Data(s, b)).await.is_err() {
                                return;
                            }
                        }
                        continue;
                    }
                }
            } else {
                events.recv().await
            };
            let send = match ev {
                Some(Ev::Data(Stream::Stdout, b)) => vec![Ev::Data(Stream::Stdout, out.push(&b))],
                Some(Ev::Data(Stream::Stderr, b)) => vec![Ev::Data(Stream::Stderr, err.push(&b))],
                Some(Ev::End(e)) => {
                    let mut v: Vec<Ev> = flush(&mut out, &mut err)
                        .into_iter()
                        .map(|(s, b)| Ev::Data(s, b))
                        .collect();
                    v.push(Ev::End(e));
                    v
                }
                None => {
                    for (s, b) in flush(&mut out, &mut err) {
                        let _ = tx.send(Ev::Data(s, b)).await;
                    }
                    return;
                }
                Some(other) => vec![other],
            };
            let end = send.iter().any(|e| matches!(e, Ev::End(_)));
            for e in send {
                if matches!(&e, Ev::Data(_, b) if b.is_empty()) {
                    continue;
                }
                if tx.send(e).await.is_err() {
                    return;
                }
            }
            if end {
                return;
            }
        }
    });
    Running {
        events: rx,
        ctl,
        host_session,
    }
}

/// The body of `POST /api/targets/{name}/cp`.
#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CpBody {
    /// `push`: client to target. `pull`: target to client.
    pub direction: CpDirection,
    /// The absolute path on the target.
    pub path: String,
    /// The file mode of a push, for example 420 for `0644`.
    #[serde(default)]
    pub mode: Option<u32>,
    /// A `lab_from_workspace` target: the holder whose instance (an agent
    /// key). An agent caller uses its own.
    #[serde(default)]
    pub agent: Option<String>,
}

/// `POST /api/targets/{name}/cp`: start a copy in a new session.
pub async fn start_cp(
    state: &Shared,
    actor: &Actor,
    name: &str,
    mut body: CpBody,
) -> ApiResult<Arc<ExecSession>> {
    let row = usable(state, actor, name).await?;
    if path_kind(state, &row.def).key(&body.path).is_none() {
        return Err(ApiError::invalid(format!(
            "`path` must be absolute and without `..`, not `{}`",
            body.path
        )));
    }
    check_cap(state, actor)?;
    no_exec(&row.def)?;
    let def = resolved_def(state, &row, Some(actor), body.agent.as_deref()).await?;
    if path_kind(state, &def) == terrarium_host::protocol::PathKind::Windows
        && let Some(a) = actor.agent()
    {
        let kind = path_kind(state, &def);
        let root = kind
            .workspace(&a.project, &a.name)
            .ok_or_else(|| ApiError::invalid("invalid Windows workspace name"))?;
        if !kind.inside(&root, &body.path, true) {
            return Err(ApiError::invalid(format!("path must be inside {root}")));
        }
    }
    let sync = workspace_sync::prepare(
        state,
        actor,
        &def,
        body.agent.as_deref(),
        row.def.kind == TargetKind::Exedev,
    )
    .await?;
    let push = body.direction == CpDirection::Push;
    if let Some(sync) = &sync {
        body.path = sync.translate(&body.path);
    }
    let (sess, ctl) = ExecSession::new(
        name,
        actor.agent().map(|a| a.id.clone()),
        if push { "push" } else { "pull" },
        state.executor.buffer_limit,
    );
    workspace_sync::finish_with(&sess, sync);
    match def.kind {
        TargetKind::Host => {
            let host = host_of(&def)?;
            let run_as = windows_run_as(state, &def)?;
            let (path, mode) = (PathBuf::from(&body.path), body.mode);
            let hs = if push {
                state.executor.hosts.open(host, move |session| {
                    DaemonFrame::CpPush(CpPush {
                        windows_run_as: run_as,
                        session,
                        path,
                        mode,
                        mkdirs: true,
                    })
                })?
            } else {
                state.executor.hosts.open(host, move |session| {
                    DaemonFrame::CpPull(CpPull {
                        session,
                        path,
                        windows_run_as: run_as,
                    })
                })?
            };
            tokio::spawn(attach(pump_host(hs), sess.clone(), ctl, push, !push));
        }
        // `resolved_def` made an exedev target the ssh target of its VM.
        TargetKind::Docker | TargetKind::Ssh | TargetKind::Exedev => {
            let st = state.clone();
            let d = def.clone();
            let start = move |c: RemoteCmd| start_cmd(&st, &d, &c);
            if push {
                tokio::spawn(push_via_shell(
                    start,
                    sess.clone(),
                    ctl,
                    body.path.clone(),
                    body.mode,
                ));
            } else {
                let run = start(RemoteCmd {
                    argv: vec!["cat".into(), "--".into(), body.path.clone()],
                    ..RemoteCmd::default()
                })?;
                tokio::spawn(pull_via_cat(run, sess.clone(), ctl));
            }
        }
        TargetKind::Vmlab => {
            machine_of(&def)?;
            let stage = lab_dir(&def)?
                .join(".terrarium-stage")
                .join(ulid::Ulid::new().to_string());
            tokio::spawn(vmlab_cp(
                state.clone(),
                def.clone(),
                sess.clone(),
                ctl,
                push,
                stage,
                body.path.clone(),
                body.mode,
            ));
        }
        TargetKind::Vnc | TargetKind::Rdp | TargetKind::Desktop => no_exec(&def)?,
    }
    state.executor.insert(sess.clone());
    audit(
        state,
        actor,
        &row,
        "target.cp",
        json!({"session": sess.id, "direction": body.direction, "path": body.path}),
    )
    .await?;
    Ok(sess)
}

/// A vmlab copy: `cp_*` to a staging file in the lab dir, `vmlab.cp`, and
/// a clean-up of the staging file.
#[allow(clippy::too_many_arguments)]
async fn vmlab_cp(
    state: Shared,
    def: TargetDef,
    sess: Arc<ExecSession>,
    ctl: mpsc::UnboundedReceiver<Control>,
    push: bool,
    stage: PathBuf,
    guest: String,
    mode: Option<u32>,
) {
    let Ok(host) = host_of(&def).map(str::to_owned) else {
        return sess.finish(End::error("bad_request", "no host"));
    };
    let lab = def.lab.clone().unwrap_or_default();
    let machine = def.machine.clone().unwrap_or_default();
    let cp_frame = {
        let (lab, machine, stage, guest) =
            (lab.clone(), machine.clone(), stage.clone(), guest.clone());
        let direction = if push {
            CpDirection::Push
        } else {
            CpDirection::Pull
        };
        move |session| {
            DaemonFrame::VmlabCp(VmlabCp {
                session,
                lab,
                machine,
                direction,
                host_path: stage,
                guest_path: guest,
                timeout_secs: Some(VERB_TIMEOUT),
            })
        }
    };
    let cleanup = |state: Shared, host: String, stage: PathBuf| async move {
        let rm = ExecRequest {
            windows_run_as: None,
            session: 0,
            cmd: "rm".into(),
            args: vec!["-f".into(), "--".into(), stage.display().to_string()],
            cwd: None,
            env: BTreeMap::new(),
            pty: None,
            timeout_secs: Some(60),
            limits: None,
        };
        if let Ok(hs) = state.executor.hosts.open(&host, move |session| {
            DaemonFrame::Exec(ExecRequest { session, ..rm })
        }) {
            pump_host(hs).collect(4096).await;
        }
    };
    if push {
        let stage2 = stage.clone();
        let hs = state.executor.hosts.open(&host, move |session| {
            DaemonFrame::CpPush(CpPush {
                windows_run_as: None,
                session,
                path: stage2,
                mode,
                mkdirs: true,
            })
        });
        let hs = match hs {
            Ok(h) => h,
            Err(e) => return sess.finish(End::error("offline", e.to_string())),
        };
        let inner = sess_shadow(&sess);
        attach(pump_host(hs), inner.0.clone(), ctl, true, false).await;
        let first = inner.0.lock().end.clone();
        let Some(End::Cp { bytes, sha256 }) = first else {
            cleanup(state, host, stage).await;
            return sess.finish(first.unwrap_or_else(|| End::error("lost", "no end")));
        };
        let c = match state.executor.hosts.open(&host, cp_frame) {
            Ok(h) => pump_host(h).collect(64 * 1024).await,
            Err(e) => {
                cleanup(state, host, stage).await;
                return sess.finish(End::error("offline", e.to_string()));
            }
        };
        cleanup(state, host, stage).await;
        if c.end().success() {
            sess.finish(End::Cp { bytes, sha256 });
        } else {
            sess.finish(End::error(
                "io",
                format!("vmlab cp failed: {}", c.stderr_tail()),
            ));
        }
    } else {
        let c = match state.executor.hosts.open(&host, cp_frame) {
            Ok(h) => pump_host(h).collect(64 * 1024).await,
            Err(e) => return sess.finish(End::error("offline", e.to_string())),
        };
        if !c.end().success() {
            cleanup(state, host, stage).await;
            return sess.finish(End::error(
                "io",
                format!("vmlab cp failed: {}", c.stderr_tail()),
            ));
        }
        let stage2 = stage.clone();
        match state.executor.hosts.open(&host, move |session| {
            DaemonFrame::CpPull(CpPull {
                windows_run_as: None,
                session,
                path: stage2,
            })
        }) {
            Ok(h) => attach(pump_host(h), sess.clone(), ctl, false, true).await,
            Err(e) => sess.finish(End::error("offline", e.to_string())),
        }
        cleanup(state, host, stage).await;
    }
}

/// A private session that takes the controls of a first step, so that the
/// caller can run more steps before it ends the client's session.
fn sess_shadow(s: &ExecSession) -> (Arc<ExecSession>, mpsc::UnboundedReceiver<Control>) {
    ExecSession::new(&s.target, s.owner.clone(), s.kind, s.limit)
}

// --------------------------------------------------------------- lifecycle

/// Extra values for a lifecycle verb.
#[derive(Debug, Clone, Default, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct LifecycleOpts {
    /// Explicit recreation retains the verified original workspace binds.
    #[serde(skip)]
    pub recreate_workspaces: Option<Vec<String>>,
    /// The snapshot name for `snapshot` and `restore`. Default `terrarium`.
    #[serde(default)]
    pub name: Option<String>,
    /// `restore --discard-guest-changes` (vmlab).
    #[serde(default)]
    pub discard_guest_changes: bool,
    /// `snapshot` and `restore`: the target's `machine`, or the whole lab.
    /// Default: `machine` when the target has one, else `lab`.
    #[serde(default)]
    pub scope: Option<SnapshotScope>,
    /// `down` removes an ephemeral target (docker rm, vmlab destroy) even
    /// with `keep`.
    #[serde(default)]
    pub destroy: bool,
    /// A `lab_from_workspace` target: the holder whose instance the verb
    /// acts on (an agent key). An agent caller acts on its own instance.
    #[serde(default)]
    pub agent: Option<String>,
    /// The agent (ULID) that starts now, or gets a lease: its workspace is
    /// attached with those of the running holders. Not in the API body.
    #[serde(skip)]
    pub starting: Option<String>,
    /// The agent (ULID) that leaves: its workspace is not attached.
    #[serde(skip)]
    pub leaving: Option<String>,
    /// Internal: the verb runs on the lab dir of an instance; the caller
    /// did the running-targets check.
    #[serde(skip)]
    pub inner: bool,
}

fn state_of(v: &Value) -> String {
    v["state"].as_str().unwrap_or("unknown").to_owned()
}

/// Read the actual legacy container settings, never infer them from an edited definition.
pub async fn docker_baseline(
    state: &Shared,
    def: &TargetDef,
) -> ApiResult<Option<targets::DockerSettings>> {
    let container = container_name(def);
    let inspected = verb(state, def, move |session| {
        DaemonFrame::DockerInspect(DockerInspect {
            session,
            container,
            timeout_secs: Some(60),
        })
    })
    .await?;
    let result = inspected
        .result
        .as_ref()
        .ok_or_else(|| verb_failed("docker.inspect", &inspected))?;
    if state_of(result) == "absent" {
        return Ok(None);
    }
    let image = result["image"].as_str().ok_or_else(|| {
        ApiError::conflict(
            "cannot read container image; update the host agent before editing this legacy target",
        )
    })?;
    let limits = result["container_limits"].as_object().ok_or_else(|| {
        ApiError::conflict(
            "cannot read container limits; update the host agent before editing this legacy target",
        )
    })?;
    let positive = |key: &str| limits.get(key).and_then(Value::as_u64).filter(|v| *v > 0);
    let mut values = serde_json::Map::new();
    let nano = positive("cpu").or_else(|| {
        positive("cpu_quota")
            .and_then(|q| q.checked_mul(1_000_000_000))
            .zip(positive("cpu_period"))
            .map(|(q, p)| q / p)
    });
    if let Some(nano) = nano {
        if nano % 1_000_000 != 0 {
            return Err(ApiError::conflict(
                "legacy container CPU limit cannot be represented exactly",
            ));
        }
        let millis = nano / 1_000_000;
        values.insert(
            "cpu".into(),
            format!("{}.{:03}", millis / 1000, millis % 1000).into(),
        );
    }
    if let Some(memory) = positive("memory") {
        values.insert("memory".into(), memory.into());
        let total = match limits.get("memory_swap").and_then(Value::as_i64) {
            Some(-1) => {
                return Err(ApiError::conflict(
                    "legacy container has unlimited swap; cannot capture its limits exactly",
                ));
            }
            Some(0) | None => memory.saturating_mul(2),
            Some(n) => {
                u64::try_from(n).map_err(|_| ApiError::conflict("invalid legacy swap limit"))?
            }
        };
        values.insert("swap".into(), total.saturating_sub(memory).into());
    }
    if let Some(pids) = positive("pids") {
        values.insert("pids".into(), pids.into());
    }
    if let Some(weight) = positive("blkio_weight") {
        values.insert(
            "io_weight".into(),
            (1 + (weight.clamp(10, 1000) - 10) * 9999 / 990).into(),
        );
    }
    let limits = if values.is_empty() {
        None
    } else {
        Some(serde_json::from_value(Value::Object(values))?)
    };
    Ok(Some(targets::DockerSettings {
        image: Some(image.to_owned()),
        limits,
    }))
}

async fn docker_state(state: &Shared, def: &TargetDef) -> ApiResult<String> {
    let container = container_name(def);
    let c = verb(state, def, move |session| {
        DaemonFrame::DockerInspect(DockerInspect {
            session,
            container,
            timeout_secs: Some(60),
        })
    })
    .await?;
    // A missing container exits non-zero, with `{"state": "absent"}`.
    match &c.result {
        Some(r) => Ok(state_of(r)),
        None if c.end().success() => Ok("unknown".into()),
        None => Err(verb_failed("docker.inspect", &c)),
    }
}

/// The target's host agent said that it has no agent workspaces (its
/// `work_dir` is not a directory there: a remote host, TER-41).
fn host_has_no_workspaces(state: &Shared, def: &TargetDef) -> bool {
    def.host
        .as_deref()
        .and_then(|h| state.executor.hosts.capabilities(h))
        .is_some_and(|c| c.no_workspaces)
}

/// The `workspace_note` of an `up` on a host with no workspaces.
fn no_workspace_note(def: &TargetDef) -> String {
    format!(
        "host `{}` has no agent workspaces (its work_dir is not a directory there): the \
         container runs with no /work mount, so commands see no workspace; check a branch out \
         in the container (`terrarium repo checkout --target {}`)",
        def.host.as_deref().unwrap_or("?"),
        def.name
    )
}

async fn docker_run(state: &Shared, def: &TargetDef, workspaces: &[String]) -> ApiResult<()> {
    let workspaces: Vec<WorkspaceMount> = workspaces
        .iter()
        .map(|p| WorkspaceMount { path: p.clone() })
        .collect();
    let Some(image) = def.image.clone() else {
        return Err(ApiError::conflict(format!(
            "container `{}` does not exist and the target has no `image`",
            container_name(def)
        )));
    };
    let name = container_name(def);
    let mut labels = BTreeMap::new();
    labels.insert("terrarium.target".to_owned(), def.name.clone());
    let limits = docker_limits(state, def)?;
    let reservation = crate::reservations::docker_budget(state, def)?;
    verb_ok(state, def, "docker.run", move |session| {
        DaemonFrame::DockerRun(DockerRun {
            session,
            image,
            name: Some(name),
            detach: true,
            rm: false,
            // tini as PID 1 ends `sleep infinity` on SIGTERM, so `down`
            // does not wait for the stop grace.
            init: true,
            env: BTreeMap::new(),
            volumes: Vec::new(),
            workspaces,
            workdir: None,
            network: None,
            user: None,
            labels,
            command: vec!["sleep".into(), "infinity".into()],
            pty: None,
            timeout_secs: Some(VERB_TIMEOUT),
            limits,
            reservation,
        })
    })
    .await?;
    let mut rt = runtime_of(state, &def.name).await?;
    rt.recreate_required = false;
    rt.docker_settings = (!def.ephemeral).then(|| targets::DockerSettings::of(def));
    store_runtime(state, &def.name, rt).await?;
    Ok(())
}

/// The limits of a docker target's container (docs/api.md §17.2,
/// "Resource limits"): one budget for the container, which all holders
/// share. A host agent that cannot apply them is a `409 conflict`: the
/// container never runs without them.
fn docker_limits(state: &Shared, def: &TargetDef) -> ApiResult<Option<Limits>> {
    let Some(tl) = &def.limits else {
        return Ok(None);
    };
    let limits = tl.resolve().map_err(ApiError::invalid)?;
    if limits.is_empty() {
        return Ok(None);
    }
    let host = host_of(def)?;
    let Some(caps) = state.executor.hosts.capabilities(host) else {
        return Err(crate::hosts::HostError::Offline(host.to_owned()).into());
    };
    limits
        .docker_support(caps.limits.as_ref())
        .map_err(|r| ApiError::conflict(terrarium_core::limits::refusal(&r)))?;
    Ok(Some(limits))
}

async fn docker_start(state: &Shared, def: &TargetDef) -> ApiResult<()> {
    {
        let _admission = state.executor.target_resource_lock(def).lock_owned().await;
        crate::reservations::before_docker_start(state, def).await?;
    }
    let container = container_name(def);
    verb_ok(state, def, "docker.start", move |session| {
        DaemonFrame::DockerStart(DockerStart {
            session,
            container,
            timeout_secs: Some(120),
        })
    })
    .await?;
    Ok(())
}

async fn docker_rm(state: &Shared, def: &TargetDef) -> ApiResult<()> {
    let container = container_name(def);
    verb_ok(state, def, "docker.rm", move |session| {
        DaemonFrame::DockerRm(DockerRm {
            session,
            container,
            force: true,
            volumes: true,
            timeout_secs: Some(120),
        })
    })
    .await?;
    Ok(())
}

/// Explicit recreation overrides. Neither option accepts mount sources.
#[derive(Debug, Default, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RecreateOpts {
    #[serde(default)]
    pub force: bool,
    /// Exact names of volumes whose attachment may be discarded, not deleted.
    #[serde(default)]
    pub discard_volumes: Vec<String>,
}

/// Explicit consent to lose the container's writable layer, never volume data.
pub async fn recreate(
    state: &Shared,
    def: &TargetDef,
    opts: &RecreateOpts,
    audit_actor: &str,
) -> ApiResult<Value> {
    if def.kind != TargetKind::Docker || def.container.is_some() || def.image.is_none() {
        return Err(ApiError::invalid(
            "recreate needs a managed Docker target with an image",
        ));
    }
    host_online(state, def)?;
    let _resource_verb = {
        let _admission = state.executor.target_resource_lock(def).lock_owned().await;
        state.executor.begin_resource_verb(def)?
    };
    if state.executor.live_on(&def.name) > 0 && !opts.force {
        return Err(ApiError::conflict(
            "recreate refused: exec sessions are live on the target",
        ));
    }
    let st = docker_state(state, def).await?;
    if st == "absent" {
        return Err(ApiError::conflict("no container yet; up creates it"));
    }
    if st != "stopped" && !(st == "running" && opts.force) {
        return Err(ApiError::conflict(
            "recreate refused: bring the container down first",
        ));
    }
    // Check capability and capacity before discarding the old writable layer.
    docker_limits(state, def)?;
    check_running_cap(state, def, None).await?;
    let mut preserved = Vec::new();
    if st == "stopped" || st == "running" {
        let container = container_name(def);
        let inspect_container = container.clone();
        let inspected = verb_ok(state, def, "docker.inspect", move |session| {
            DaemonFrame::DockerInspect(DockerInspect {
                session,
                container: inspect_container,
                timeout_secs: Some(60),
            })
        })
        .await?;
        let mounts = inspected
            .result
            .as_ref()
            .and_then(|r| r["mounts"].as_array())
            .ok_or_else(|| {
                ApiError::conflict(
                    "recreate refused: host did not report container mounts; update the host agent",
                )
            })?;
        let rt = runtime_of(state, &def.name).await?;
        let verified = inspected
            .result
            .as_ref()
            .and_then(|r| r["workspace_mounts"].as_object());
        let mut dropped = BTreeMap::new();
        let mut retained = Vec::new();
        for mount in mounts {
            if mount["Type"] == "volume" {
                let name = mount["Name"].as_str().filter(|s| !s.is_empty());
                let destination = mount["Destination"].as_str().filter(|s| s.starts_with('/'));
                let (Some(name), Some(destination)) = (name, destination) else {
                    return Err(ApiError::conflict(
                        "recreate refused: incomplete volume metadata",
                    ));
                };
                dropped
                    .entry(name.to_owned())
                    .or_insert_with(Vec::new)
                    .push(destination.to_owned());
            } else {
                retained.push(mount.clone());
            }
        }
        let requested: std::collections::BTreeSet<_> =
            opts.discard_volumes.iter().cloned().collect();
        let actual: std::collections::BTreeSet<_> = dropped.keys().cloned().collect();
        if requested != actual || requested.len() != opts.discard_volumes.len() {
            return Err(ApiError::conflict(format!(
                "recreate refused: --discard-volumes must name exactly the volumes whose attachments would be lost: {}",
                json!(dropped)
            )).with_details(json!({"volumes": dropped})));
        }
        if retained.len() != rt.attached.len()
            || rt.attached.iter().any(|path| {
                !verified
                    .and_then(|v| v.get(path))
                    .is_some_and(|mount| retained.contains(mount))
            })
            || retained.iter().any(|mount| {
                !rt.attached
                    .iter()
                    .any(|path| verified.and_then(|v| v.get(path)) == Some(mount))
            })
        {
            return Err(ApiError::conflict(
                "recreate refused: container has foreign mounts or volumes; only verified Terrarium workspace binds can be preserved",
            ));
        }
        preserved = rt.attached;
        let container_id = inspected
            .result
            .as_ref()
            .and_then(|r| r["id"].as_str())
            .filter(|id| !id.is_empty())
            .ok_or_else(|| ApiError::conflict("recreate refused: missing container identity"))?
            .to_owned();
        // Recheck immediately before removal. A changed container or mount set
        // needs fresh consent. Never trust mount sources supplied by a caller.
        let check_container = container.clone();
        let checked = verb_ok(state, def, "docker.inspect", move |session| {
            DaemonFrame::DockerInspect(DockerInspect {
                session,
                container: check_container,
                timeout_secs: Some(60),
            })
        })
        .await?;
        let before = inspected.result.as_ref().ok_or_else(|| {
            ApiError::conflict("recreate refused: missing container inspection result")
        })?;
        let after = checked.result.as_ref();
        if !after.is_some_and(|after| {
            before["id"] == after["id"]
                && before["mounts"] == after["mounts"]
                && before["workspace_mounts"] == after["workspace_mounts"]
                && before["state"] == after["state"]
        }) || (!opts.force && state.executor.live_on(&def.name) > 0)
        {
            return Err(ApiError::conflict(
                "recreate refused: container changed during inspection; retry",
            ));
        }
        let force = opts.force;
        let removed_id = container_id.clone();
        verb_ok(state, def, "docker.rm", move |session| {
            DaemonFrame::DockerRm(DockerRm {
                session,
                // Pin removal to the inspected container, not a reusable name.
                container: container_id,
                force,
                volumes: false,
                timeout_secs: Some(120),
            })
        })
        .await?;
        // Removal is irreversible even if starting the replacement fails.
        // Persist the verified consent and outcome before attempting that start.
        let (target, project) = (def.name.clone(), def.project.clone());
        let discarded = opts.discard_volumes.clone();
        state
            .db
            .write(audit_actor, move |t| {
                t.audit(
                    "target.recreate.removed",
                    &target,
                    project.as_deref(),
                    json!({
                        "container_id": removed_id,
                        "force": force,
                        "discarded_volumes": discarded,
                        "outcome": "removed",
                    }),
                )
            })
            .await?;
    }
    let lifecycle_opts = LifecycleOpts {
        recreate_workspaces: Some(preserved),
        ..LifecycleOpts::default()
    };
    let mut out = lifecycle(state, def, LifecycleVerb::Up, &lifecycle_opts).await?;
    out["verb"] = "recreate".into();
    out["force"] = json!(opts.force);
    out["discarded_volumes"] = json!(opts.discard_volumes);
    Ok(out)
}

async fn vmlab_state(state: &Shared, def: &TargetDef) -> ApiResult<String> {
    let lab = lab_of(def)?;
    let c = verb_ok(state, def, "vmlab.status", move |session| {
        DaemonFrame::VmlabStatus(VmlabStatus {
            session,
            lab,
            verbose: false,
            timeout_secs: Some(120),
        })
    })
    .await?;
    let v = c.result.unwrap_or(Value::Null);
    // `not running`: the lab is not in `vmlab lab list`. A full `down`
    // and a `destroy` both unlist it. An ephemeral target without `keep`
    // is destroyed on its down, so its lab is gone (`absent`); else the
    // lab is down with its clones and snapshots (`stopped`).
    if v["running"] == false {
        return Ok(if def.ephemeral && !def.keep {
            "absent"
        } else {
            "stopped"
        }
        .into());
    }
    Ok(match &def.machine {
        Some(m) => v["machines"]
            .as_array()
            .and_then(|ms| ms.iter().find(|x| x["name"] == m.as_str()))
            .map_or("absent".into(), state_of),
        None => "running".into(),
    })
}

fn vmlab_lc(def: &TargetDef, force: bool) -> impl FnOnce(u32) -> VmlabLifecycle + use<> {
    let lab = def.lab.clone().unwrap_or_default();
    let machines: Vec<String> = def.machine.iter().cloned().collect();
    move |session| VmlabLifecycle {
        session,
        lab,
        machines,
        force,
        timeout_secs: Some(VERB_TIMEOUT),
    }
}

/// Run a lifecycle verb on a target. The answer is
/// `{target, verb, state, snapshot?, result?}`; the state is stored.
pub async fn lifecycle(
    state: &Shared,
    def: &TargetDef,
    verb_kind: LifecycleVerb,
    opts: &LifecycleOpts,
) -> ApiResult<Value> {
    use LifecycleVerb as V;
    let snapshot = opts.name.clone().unwrap_or_else(|| "terrarium".into());
    if let Some(n) = &opts.name {
        check_snapshot_name(n)?;
    }
    let unsupported = |what: &str| {
        Err(ApiError::invalid(format!(
            "a `{}` target has no `{what}`",
            targets::kind_str(def.kind)
        )))
    };
    if is_instance_kind(def) && !opts.inner {
        // Boxed: an instance verb runs `lifecycle` on the instance's lab dir.
        return Box::pin(instance_lifecycle(state, def, verb_kind, opts)).await;
    }
    // Publish a short-lived intent under the host admission lock. Resource
    // changes refuse this host while a verb runs, without holding a mutex
    // across image pulls or blocking work on any other host.
    let _resource_verb = if def.kind == TargetKind::Docker {
        let _admission = if verb_kind == V::Up {
            state.executor.target_admission(def).await?
        } else {
            state.executor.target_resource_lock(def).lock_owned().await
        };
        if verb_kind == V::Up {
            crate::reservations::lifecycle_admission(state, def)?;
        }
        Some(state.executor.begin_resource_verb(def)?)
    } else {
        None
    };
    if verb_kind == V::Up && def.kind != TargetKind::Docker {
        let _admission = state.executor.target_admission(def).await?;
        crate::reservations::lifecycle_admission(state, def)?;
    }
    let mut add_snapshot = None;
    let mut result = None;
    let mut attach_error: Option<String> = None;
    let mut workspace_note: Option<String> = None;
    let mut attached: Option<Vec<String>> = None;
    let new_state: String = match def.kind {
        TargetKind::Host | TargetKind::Ssh => match verb_kind {
            V::Snapshot | V::Restore => return unsupported("snapshot"),
            _ => match (&def.kind, &def.host) {
                (TargetKind::Host, Some(h)) if state.executor.hosts.is_online(h) => {
                    "running".into()
                }
                _ => "unknown".into(),
            },
        },
        // Display-only targets: `up` and `down` change nothing; each verb
        // reports what the host agent sees (docs/api.md §17.3).
        TargetKind::Vnc | TargetKind::Rdp | TargetKind::Desktop => match verb_kind {
            V::Snapshot | V::Restore => return unsupported("snapshot"),
            _ => {
                let online = def
                    .host
                    .as_deref()
                    .is_some_and(|h| state.executor.hosts.is_online(h));
                if !online {
                    "unknown".into()
                } else if def.kind == TargetKind::Desktop {
                    let on = def
                        .host
                        .as_deref()
                        .and_then(|h| state.executor.hosts.capabilities(h))
                        .is_some_and(|c| c.display.enabled);
                    if on { "running" } else { "unknown" }.into()
                } else {
                    let address = targets::display_address(def)?;
                    let c = verb_ok(state, def, "probe", move |session| {
                        DaemonFrame::Probe(Probe {
                            session,
                            address,
                            timeout_secs: Some(5),
                        })
                    })
                    .await?;
                    let r = c.result.unwrap_or_default();
                    let reachable = r["reachable"] == true;
                    result = Some(r);
                    if reachable { "running" } else { "unknown" }.into()
                }
            }
        },
        TargetKind::Docker => {
            if !matches!(verb_kind, V::Status) {
                host_online(state, def)?;
            }
            match verb_kind {
                V::Status => docker_state(state, def).await?,
                V::Up => {
                    // The workspaces of the running holders (and the agent
                    // that starts), bound at `/work/<project>/<agent>`. An
                    // existing container (`container`) gets none.
                    let bare = host_has_no_workspaces(state, def);
                    if bare {
                        workspace_note = Some(no_workspace_note(def));
                    }
                    let want = if let Some(preserved) = &opts.recreate_workspaces {
                        preserved.clone()
                    } else if def.container.is_none() && !bare {
                        attach_set(
                            state,
                            &def.name,
                            opts.starting.as_deref(),
                            opts.leaving.as_deref(),
                        )
                        .await?
                    } else {
                        Vec::new()
                    };
                    let now = runtime_of(state, &def.name).await?.attached;
                    let fits = want.iter().all(|w| now.contains(w));
                    match docker_state(state, def).await?.as_str() {
                        "running" if fits => {}
                        "running" => {
                            // Docker workspace changes always refuse recreation.
                            if let Some(why) = recreate_refusal(state, def) {
                                attach_error = Some(attach_refused(def, &now, &want, &why))
                            }
                        }
                        "absent" => {
                            check_running_cap(state, def, None).await?;
                            docker_run(state, def, &want).await?;
                            attached = Some(want);
                        }
                        // An ephemeral target starts fresh from its image.
                        "stopped" if def.ephemeral && def.image.is_some() => {
                            check_running_cap(state, def, None).await?;
                            docker_rm(state, def).await?;
                            docker_run(state, def, &want).await?;
                            attached = Some(want);
                        }
                        // A permanent target keeps its files over down and up.
                        "stopped" => {
                            check_running_cap(state, def, None).await?;
                            docker_start(state, def).await?;
                            if !fits && def.container.is_none() {
                                attach_error = Some(attach_refused(
                                    def,
                                    &now,
                                    &want,
                                    "it is permanent: a new container would lose its files",
                                ));
                            }
                        }
                        s => {
                            return Err(ApiError::conflict(format!(
                                "container `{}` is {s}; start it on the host (`docker start`)",
                                container_name(def)
                            )));
                        }
                    }
                    "running".into()
                }
                V::Down | V::Destroy => {
                    if verb_kind == V::Destroy || opts.destroy || (def.ephemeral && !def.keep) {
                        docker_rm(state, def).await?;
                        attached = Some(Vec::new());
                        "absent".into()
                    } else {
                        let container = container_name(def);
                        verb_ok(state, def, "docker.stop", move |session| {
                            DaemonFrame::DockerStop(DockerStop {
                                session,
                                container,
                                grace_secs: Some(10),
                                timeout_secs: Some(120),
                            })
                        })
                        .await?;
                        "stopped".into()
                    }
                }
                V::Snapshot | V::Restore => return unsupported("snapshot"),
            }
        }
        TargetKind::Exedev => match verb_kind {
            V::Status => crate::exedev::status(state, def).await?,
            V::Up => crate::exedev::up(state, def).await?,
            V::Down | V::Destroy => crate::exedev::destroy(state, def).await?,
            V::Snapshot | V::Restore => return unsupported("snapshot"),
        },
        TargetKind::Vmlab => {
            host_online(state, def)?;
            match verb_kind {
                V::Status => vmlab_state(state, def).await?,
                V::Up => {
                    if !opts.inner {
                        let row_state = state
                            .db
                            .read({
                                let n = def.name.clone();
                                move |c| targets::get(c, &n)
                            })
                            .await?
                            .state;
                        if row_state != "running" {
                            check_running_cap(state, def, None).await?;
                        }
                    }
                    let f = vmlab_lc(def, false);
                    verb_ok(state, def, "vmlab.up", move |s| DaemonFrame::VmlabUp(f(s))).await?;
                    "running".into()
                }
                V::Down | V::Destroy => {
                    if verb_kind == V::Destroy || opts.destroy || (def.ephemeral && !def.keep) {
                        let f = vmlab_lc(def, true);
                        verb_ok(state, def, "vmlab.destroy", move |s| {
                            let mut l = f(s);
                            l.machines.clear();
                            DaemonFrame::VmlabDestroy(l)
                        })
                        .await?;
                        "absent".into()
                    } else {
                        let f = vmlab_lc(def, false);
                        verb_ok(state, def, "vmlab.down", move |s| {
                            DaemonFrame::VmlabDown(f(s))
                        })
                        .await?;
                        "stopped".into()
                    }
                }
                V::Snapshot | V::Restore => {
                    let (lab, machine, name) = (
                        lab_of(def)?,
                        snapshot_machine(def, opts.scope)?,
                        snapshot.clone(),
                    );
                    let discard = opts.discard_guest_changes;
                    let take = verb_kind == V::Snapshot;
                    let c = verb_ok(
                        state,
                        def,
                        if take {
                            "vmlab.snapshot_take"
                        } else {
                            "vmlab.snapshot_restore"
                        },
                        move |session| {
                            let s = VmlabSnapshot {
                                session,
                                lab,
                                name,
                                machine,
                                discard_guest_changes: discard && !take,
                                timeout_secs: Some(VERB_TIMEOUT),
                            };
                            if take {
                                DaemonFrame::VmlabSnapshotTake(s)
                            } else {
                                DaemonFrame::VmlabSnapshotRestore(s)
                            }
                        },
                    )
                    .await?;
                    result = c.result;
                    if take {
                        add_snapshot = Some(snapshot.clone());
                    }
                    vmlab_state(state, def)
                        .await
                        .unwrap_or_else(|_| "unknown".into())
                }
            }
        }
    };
    let (name, st, snap) = (def.name.clone(), new_state.clone(), add_snapshot.clone());
    let project = def.project.clone();
    let reconcile = verb_kind == V::Status;
    state
        .db
        .write("system", move |t| {
            let before = targets::get(t, &name)?;
            targets::set_state(t, &name, &st, snap.as_deref())?;
            if reconcile {
                reconciled(t, &before, &st, project.as_deref())?;
            }
            if let Some(a) = attached {
                let mut rt = targets::get(t, &name)?.runtime;
                rt.attached = a;
                if st == "absent" {
                    rt.docker_settings = None;
                    rt.recreate_required = false;
                }
                targets::set_runtime(t, &name, &rt)?;
            }
            Ok(())
        })
        .await?;
    let mut out = json!({"target": def.name, "verb": verb_kind, "state": new_state});
    if verb_kind == V::Up && runtime_of(state, &def.name).await?.recreate_required {
        out["recreate_note"] = "Started the existing container with its old image and limits; recreate is pending. Explicit recreation loses the writable layer and refuses foreign mounts or volumes".into();
    }
    if let Some(e) = attach_error {
        out["attach_error"] = e.into();
    }
    if let Some(n) = workspace_note {
        out["workspace_note"] = n.into();
    }
    if matches!(verb_kind, V::Snapshot | V::Restore) {
        out["snapshot"] = snapshot.into();
        out["scope"] = json!(scope_of(def, opts.scope));
    }
    if let Some(r) = result {
        out["result"] = r;
    }
    Ok(out)
}

/// A `status` that found the target other than the row said: the audit
/// row `target.reconcile` and a `target` event (docs/api.md §17.3). Nothing
/// when the state is the same, and the snapshots did not go.
fn reconciled(
    t: &mut crate::db::Tx<'_>,
    before: &TargetRow,
    state: &str,
    project: Option<&str>,
) -> ApiResult<()> {
    let cleared = state == "absent" && !before.snapshots.is_empty();
    if before.state == state && !cleared {
        return Ok(());
    }
    let name = &before.def.name;
    let detail = json!({"from": before.state, "state": state,
                        "snapshots_cleared": if cleared { json!(before.snapshots) } else { json!([]) }});
    t.audit("target.reconcile", name, project, detail.clone())?;
    let mut data = detail;
    data["change"] = "reconciled".into();
    data["target"] = name.clone().into();
    let mut draft = crate::events::EventDraft::new(
        "target",
        format!("target {name}: {} -> {state}", before.state),
        data,
    );
    if let Some(p) = project {
        draft = draft.project(p.to_owned());
    }
    t.emit(draft)?;
    Ok(())
}

/// Reconcile the docker and vmlab targets of `host` with what runs there
/// (a `status` of each), after the host agent connects. A lab instance
/// target (`lab_repo`, `lab_from_workspace`) keeps its runtime record: its
/// instances come and go with their holders.
pub async fn reconcile_host(state: &Shared, host: &str) {
    let rows = match state.db.read(targets::list).await {
        Ok(r) => r,
        Err(e) => {
            warn!(host, "cannot list the targets to reconcile: {e}");
            return;
        }
    };
    for r in rows {
        if r.def.host.as_deref() != Some(host)
            || !matches!(r.def.kind, TargetKind::Docker | TargetKind::Vmlab)
            || is_instance_kind(&r.def)
        {
            continue;
        }
        let opts = LifecycleOpts::default();
        if let Err(e) = lifecycle(state, &r.def, LifecycleVerb::Status, &opts).await {
            debug!(host, target = %r.def.name, "the reconcile status failed: {e}");
        }
    }
}

fn host_online(state: &Shared, def: &TargetDef) -> ApiResult<()> {
    let h = host_of(def)?;
    if state.executor.hosts.is_online(h) {
        Ok(())
    } else {
        Err(crate::hosts::HostError::Offline(h.to_owned()).into())
    }
}

/// The snapshot that `snapshot_before_task` takes after `up` and restores
/// on an agent stop (PLAN §19).
pub const CLEAN_SNAPSHOT: &str = "terrarium-clean";

/// The target kinds that can take a snapshot. Only vmlab can.
fn snapshots_supported(def: &TargetDef) -> bool {
    def.kind == TargetKind::Vmlab
}

fn clean_opts() -> LifecycleOpts {
    LifecycleOpts {
        name: Some(CLEAN_SNAPSHOT.into()),
        ..LifecycleOpts::default()
    }
}

// ---------------------------------------------------------------- snapshots

/// Which machines a snapshot verb acts on (docs/api.md §17.3): the
/// target's `machine` (`vmlab snapshot ... --vm <machine>`), or every
/// machine of the lab (no `--vm`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SnapshotScope {
    Machine,
    Lab,
}

/// The scope of a snapshot verb: the one asked for, else `machine` when
/// the target has one, else `lab`.
pub fn scope_of(def: &TargetDef, asked: Option<SnapshotScope>) -> SnapshotScope {
    asked.unwrap_or(if def.machine.is_some() {
        SnapshotScope::Machine
    } else {
        SnapshotScope::Lab
    })
}

/// `--vm` of a snapshot verb: the machine, or `None` for the whole lab.
fn snapshot_machine(def: &TargetDef, asked: Option<SnapshotScope>) -> ApiResult<Option<String>> {
    match scope_of(def, asked) {
        SnapshotScope::Lab => Ok(None),
        SnapshotScope::Machine => def.machine.clone().map(Some).ok_or_else(|| {
            ApiError::invalid(format!(
                "target `{}` has no `machine`: use the scope `lab`",
                def.name
            ))
        }),
    }
}

/// A snapshot name goes onto the vmlab command line as a positional.
fn check_snapshot_name(n: &str) -> ApiResult<()> {
    if n.is_empty() || n.starts_with('-') || n.chars().any(|c| c.is_whitespace() || c.is_control())
    {
        return Err(ApiError::invalid(format!("bad snapshot name `{n}`")));
    }
    Ok(())
}

/// The query of `GET .../snapshots` and `DELETE .../snapshots/{snap}`.
#[derive(Debug, Clone, Default, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SnapshotQuery {
    #[serde(default)]
    pub scope: Option<SnapshotScope>,
    /// A `lab_from_workspace` target: the holder whose instance. An agent
    /// caller acts on its own instance.
    #[serde(default)]
    pub agent: Option<String>,
    /// `DELETE`: delete `terrarium-clean` while `snapshot_before_task` is on.
    #[serde(default)]
    pub force: bool,
}

/// The vmlab definition of a snapshot list or delete: the instance lab dir
/// for a lab instance target, and the holder for a `lab_from_workspace`
/// target. A target without snapshots is `422 invalid`.
async fn snapshot_def(
    state: &Shared,
    row: &TargetRow,
    agent: Option<&str>,
) -> ApiResult<(TargetDef, Option<HolderInfo>)> {
    if !snapshots_supported(&row.def) {
        return Err(ApiError::invalid(format!(
            "a `{}` target has no `snapshot`",
            targets::kind_str(row.def.kind)
        )));
    }
    host_online(state, &row.def)?;
    let holder = if row.def.lab_from_workspace {
        Some(instance_holder(state, &row.def, None, agent).await?)
    } else {
        None
    };
    let def = resolved_def(state, row, None, holder.as_ref().map(|h| h.id.as_str())).await?;
    Ok((def, holder))
}

/// The machines of a running lab, from `vmlab status`. `None`: the lab
/// does not run, so vmlab names no machines.
async fn lab_machines(state: &Shared, def: &TargetDef) -> ApiResult<Option<Vec<String>>> {
    let lab = lab_of(def)?;
    let c = verb_ok(state, def, "vmlab.status", move |session| {
        DaemonFrame::VmlabStatus(VmlabStatus {
            session,
            lab,
            verbose: false,
            timeout_secs: Some(120),
        })
    })
    .await?;
    let v = c.result.unwrap_or(Value::Null);
    if v["running"] == false {
        return Ok(None);
    }
    Ok(Some(
        v["machines"]
            .as_array()
            .map(|ms| {
                ms.iter()
                    .filter_map(|m| m["name"].as_str().map(str::to_owned))
                    .collect()
            })
            .unwrap_or_default(),
    ))
}

/// The snapshots of one machine: `vmlab snapshot list <machine>`.
async fn machine_snapshots(
    state: &Shared,
    def: &TargetDef,
    machine: &str,
) -> ApiResult<Vec<Value>> {
    let (lab, m) = (lab_of(def)?, machine.to_owned());
    let c = verb_ok(state, def, "vmlab.snapshot_list", move |session| {
        DaemonFrame::VmlabSnapshotList(VmlabSnapshotList {
            session,
            lab,
            machine: m,
            timeout_secs: Some(120),
        })
    })
    .await?;
    Ok(match c.result {
        Some(Value::Array(rows)) => rows,
        _ => Vec::new(),
    })
}

/// The machines a snapshot list or delete reads: the target's machine, or
/// every machine of the lab (`None` when the lab does not run).
async fn scope_machines(
    state: &Shared,
    def: &TargetDef,
    scope: SnapshotScope,
) -> ApiResult<Option<Vec<String>>> {
    match snapshot_machine(def, Some(scope))? {
        Some(m) => Ok(Some(vec![m])),
        None => lab_machines(state, def).await,
    }
}

/// The snapshots on each machine, merged by name in the order the host
/// gives them: `{name, kind, taken, machines}`. `kind` and `taken` come
/// from the first machine that has the name.
fn merge_snapshots(per_machine: &[(String, Vec<Value>)]) -> Vec<Value> {
    let mut out: Vec<Value> = Vec::new();
    for (machine, rows) in per_machine {
        for r in rows {
            let Some(name) = r["name"].as_str() else {
                continue;
            };
            match out.iter_mut().find(|x| x["name"] == name) {
                Some(x) => {
                    if let Some(ms) = x["machines"].as_array_mut() {
                        ms.push(machine.clone().into());
                    }
                }
                None => {
                    let mut x = r.clone();
                    x["machines"] = json!([machine]);
                    out.push(x);
                }
            }
        }
    }
    out
}

/// `GET /api/targets/{name}/snapshots` (docs/api.md §17.3): the snapshots
/// that the host reports, each with `managed` (Terrarium took it: the name
/// is in the target's `snapshots`). The list reconciles the stored names: a
/// managed name that the host no longer has goes (`dropped`), with the
/// audit row `target.reconcile` and a `target` event. A name that
/// Terrarium did not take is not added. A `lab_from_workspace` target is
/// not reconciled: its stored names cover every holder's instance. A lab
/// that does not run names no machines, so a lab-wide list then is empty
/// and drops nothing.
pub async fn list_snapshots(
    state: &Shared,
    row: &TargetRow,
    q: &SnapshotQuery,
) -> ApiResult<Value> {
    let (def, holder) = snapshot_def(state, row, q.agent.as_deref()).await?;
    let scope = scope_of(&def, q.scope);
    let machines = scope_machines(state, &def, scope).await?;
    let mut per_machine = Vec::new();
    for m in machines.iter().flatten() {
        per_machine.push((m.clone(), machine_snapshots(state, &def, m).await?));
    }
    let mut snaps = merge_snapshots(&per_machine);
    let authoritative = machines.as_ref().is_some_and(|m| !m.is_empty());
    let dropped: Vec<String> = if authoritative && !row.def.lab_from_workspace {
        row.snapshots
            .iter()
            .filter(|s| !snaps.iter().any(|x| x["name"] == s.as_str()))
            .cloned()
            .collect()
    } else {
        Vec::new()
    };
    if !dropped.is_empty() {
        let (name, project, gone) = (
            row.def.name.clone(),
            row.def.project.clone(),
            dropped.clone(),
        );
        state
            .db
            .write("system", move |t| {
                targets::remove_snapshots(t, &name, &gone)?;
                let detail = json!({"snapshots_dropped": gone});
                t.audit(
                    "target.reconcile",
                    &name,
                    project.as_deref(),
                    detail.clone(),
                )?;
                let mut data = detail;
                data["change"] = "snapshots_dropped".into();
                data["target"] = name.clone().into();
                let mut draft = crate::events::EventDraft::new(
                    "target",
                    format!(
                        "target {name}: the host has no snapshot {}",
                        gone.join(", ")
                    ),
                    data,
                );
                if let Some(p) = project {
                    draft = draft.project(p);
                }
                t.emit(draft)?;
                Ok(())
            })
            .await?;
    }
    for x in &mut snaps {
        let managed = x["name"]
            .as_str()
            .is_some_and(|n| row.snapshots.iter().any(|s| s == n));
        x["managed"] = managed.into();
    }
    let mut out = json!({
        "target": row.def.name,
        "scope": scope,
        "machines": machines.clone().unwrap_or_default(),
        "snapshots": snaps,
        "dropped": dropped,
    });
    if scope == SnapshotScope::Lab {
        out["lab_running"] = machines.is_some().into();
    }
    if let Some(h) = &holder {
        out["agent"] = h.name.clone().into();
    }
    Ok(out)
}

/// `DELETE /api/targets/{name}/snapshots/{snap}` (docs/api.md §17.3):
/// `vmlab snapshot delete <machine> <snap>` on the target's machine, or on
/// each machine of the lab that has the snapshot. `terrarium-clean` of a
/// target with `snapshot_before_task` needs `force`. The name leaves the
/// stored `snapshots`. The audit row `target.snapshot_delete` and a
/// `target` event record it.
pub async fn delete_snapshot(
    state: &Shared,
    actor: &Actor,
    row: &TargetRow,
    snap: &str,
    q: &SnapshotQuery,
) -> ApiResult<Value> {
    check_snapshot_name(snap)?;
    if snap == CLEAN_SNAPSHOT && row.def.snapshot_before_task && !q.force {
        return Err(ApiError::conflict(format!(
            "target `{}` has `snapshot_before_task`: the daemon restores `{CLEAN_SNAPSHOT}` \
             when an agent stops. Turn the flag off first, or delete with force=true",
            row.def.name
        )));
    }
    let (def, holder) = snapshot_def(state, row, q.agent.as_deref()).await?;
    let scope = scope_of(&def, q.scope);
    // vmlab 0.34 answers "deleted" with exit 0 for a snapshot that does
    // not exist, so only the machines whose list has it get the delete.
    let all = match scope {
        SnapshotScope::Machine => scope_machines(state, &def, scope)
            .await?
            .unwrap_or_default(),
        SnapshotScope::Lab => lab_machines(state, &def).await?.ok_or_else(|| {
            ApiError::conflict(format!(
                "the lab of target `{}` does not run, so vmlab names no machines: \
                     bring it up, or delete with the scope `machine`",
                row.def.name
            ))
        })?,
    };
    let mut machines = Vec::new();
    for m in all {
        if machine_snapshots(state, &def, &m)
            .await?
            .iter()
            .any(|r| r["name"] == snap)
        {
            machines.push(m);
        }
    }
    if machines.is_empty() {
        return Err(ApiError::not_found(format!(
            "no machine of target `{}` has the snapshot `{snap}`",
            row.def.name
        )));
    }
    for m in &machines {
        let (lab, machine, name) = (lab_of(&def)?, m.clone(), snap.to_owned());
        verb_ok(state, &def, "vmlab.snapshot_delete", move |session| {
            DaemonFrame::VmlabSnapshotDelete(VmlabSnapshotDelete {
                session,
                lab,
                machine,
                name,
                timeout_secs: Some(VERB_TIMEOUT),
            })
        })
        .await?;
    }
    let mut out = json!({
        "target": row.def.name,
        "verb": "snapshot_delete",
        "snapshot": snap,
        "scope": scope,
        "machines": machines,
    });
    if let Some(h) = &holder {
        out["agent"] = h.name.clone().into();
    }
    let (name, project, detail) = (row.def.name.clone(), row.def.project.clone(), out.clone());
    let snap = snap.to_owned();
    state
        .db
        .write(actor.label(), move |t| {
            targets::remove_snapshots(t, &name, std::slice::from_ref(&snap))?;
            t.audit(
                "target.snapshot_delete",
                &name,
                project.as_deref(),
                detail.clone(),
            )?;
            let mut data = detail;
            data["change"] = "snapshot_deleted".into();
            let mut draft = crate::events::EventDraft::new(
                "target",
                format!("target {name}: snapshot {snap} deleted"),
                data,
            );
            if let Some(p) = project {
                draft = draft.project(p);
            }
            t.emit(draft)?;
            Ok(())
        })
        .await?;
    Ok(out)
}

// ------------------------------------------------- workspaces and instances
//
// PLAN §11 and §19, "mode 1": an agent's workspace lives on the host under
// the work root, and the host agent attaches it into the targets that the
// agent holds, at `/work/<project>/<agent>`. A docker target binds it at
// `docker run`; a vmlab target syncs it with an `@dev` decorator in the
// lab file of its instance (`vmlab.prepare`). Neither can add a workspace
// to a running container or lab. The daemon may replace an ephemeral lab
// instance when no session is live. It never replaces a running Docker
// container for workspace changes, since background processes may remain
// after exec sessions end. A workspace that cannot join gets
// "attach needs a restart of the target".

/// A `lab_repo` or `lab_from_workspace` target: the daemon makes its lab
/// instance on the host with `vmlab.prepare`.
pub fn is_instance_kind(def: &TargetDef) -> bool {
    def.kind == TargetKind::Vmlab && (def.lab_repo.is_some() || def.lab_from_workspace)
}

/// A target that takes workspaces: a docker target with `image` (not an
/// existing `container`), and a vmlab instance target.
pub fn attaches(def: &TargetDef) -> bool {
    match def.kind {
        TargetKind::Docker => def.container.is_none(),
        TargetKind::Vmlab => is_instance_kind(def),
        _ => false,
    }
}

/// The live exec sessions on `target`, of any owner.
impl Executor {
    pub fn live_on(&self, target: &str) -> usize {
        self.table()
            .values()
            .filter(|s| s.target == target && !s.is_done())
            .count()
    }
}

async fn runtime_of(state: &Shared, target: &str) -> ApiResult<targets::Runtime> {
    let n = target.to_owned();
    Ok(state.db.read(move |c| targets::get(c, &n)).await?.runtime)
}

async fn store_runtime(state: &Shared, target: &str, rt: targets::Runtime) -> ApiResult<()> {
    let n = target.to_owned();
    state
        .db
        .write("system", move |t| targets::set_runtime(t, &n, &rt))
        .await
}

/// One agent as a holder of a target: its ULID, `project/name`, and its
/// workspace relative to the work root (`None`: not under the work root).
#[derive(Debug, Clone)]
pub struct HolderInfo {
    pub id: String,
    pub name: String,
    pub rel: Option<String>,
}

/// Load an agent by its ULID or key, move its workspace to the work root
/// when it is still in the old place, and make the dir.
async fn holder_info(state: &Shared, key: &str, context: Option<&str>) -> ApiResult<HolderInfo> {
    let (k, ctx) = (key.to_owned(), context.map(str::to_owned));
    let v = state
        .db
        .read(move |c| crate::store::agents::find(c, &k, ctx.as_deref()))
        .await?;
    let ws = crate::workspace::ensure_generic(state, &v.agent, false).await?;
    Ok(HolderInfo {
        id: v.agent.id.to_string(),
        name: format!("{}/{}", v.agent.project, v.agent.name),
        rel: ws.and_then(|w| crate::workspace::rel(&state.work_dir, &w)),
    })
}

/// The workspaces to attach to `target`: those of its running holders and
/// of `starting`, without `leaving`. Sorted and unique.
async fn attach_set(
    state: &Shared,
    target: &str,
    starting: Option<&str>,
    leaving: Option<&str>,
) -> ApiResult<Vec<String>> {
    let mut ids = running_holders(state, target, None).await?;
    if let Some(s) = starting
        && !ids.iter().any(|i| i == s)
    {
        ids.push(s.to_owned());
    }
    ids.retain(|i| Some(i.as_str()) != leaving);
    let mut out = Vec::new();
    for id in ids {
        match holder_info(state, &id, None).await {
            Ok(HolderInfo { rel: Some(r), .. }) => out.push(r),
            Ok(_) => {}
            Err(e) => warn!(agent = %id, "a holder of {target}: {e}"),
        }
    }
    out.sort();
    out.dedup();
    Ok(out)
}

/// Why the daemon may not make a new container (or lab instance) for a
/// new set of workspaces, or `None` when it may: an ephemeral target with
/// no live session. Docker containers are never replaced for mount changes:
/// processes can outlive the exec session that started them.
fn recreate_refusal(state: &Shared, def: &TargetDef) -> Option<String> {
    if !def.ephemeral {
        return Some("it is permanent: a new container would lose its files".into());
    }
    if def.kind == TargetKind::Docker {
        return Some(
            "a workspace change must not replace a running container or stop its processes".into(),
        );
    }
    let n = state.executor.live_on(&def.name);
    (n > 0).then(|| format!("{n} exec session(s) are live on it"))
}

fn attach_refused(def: &TargetDef, now: &[String], want: &[String], why: &str) -> String {
    let missing: Vec<&String> = want.iter().filter(|w| !now.contains(w)).collect();
    format!(
        "attach needs a restart of the target: `{}` runs with the workspaces [{}], and [{}] \
         cannot join a running {}; {why}. Stop the other holders, or `down` and `up` the \
         target, then start again",
        def.name,
        now.join(", "),
        missing
            .iter()
            .map(|s| s.as_str())
            .collect::<Vec<_>>()
            .join(", "),
        if def.kind == TargetKind::Docker {
            "container"
        } else {
            "lab"
        }
    )
}

/// The `max_running_targets` check of the target's host, before a target
/// (or a lab instance, `instance`: the holder ULID) starts. It counts the
/// docker and vmlab targets (and instances) on the host that the store
/// shows as running, without this one.
async fn check_running_cap(
    state: &Shared,
    def: &TargetDef,
    instance: Option<&str>,
) -> ApiResult<()> {
    let Some(host) = def.host.clone() else {
        return Ok(());
    };
    let Some(max) = state.executor.hosts.max_running_targets(&host) else {
        return Ok(());
    };
    let (me, inst) = (def.name.clone(), instance.map(str::to_owned));
    let h = host.clone();
    let running = state
        .db
        .read(move |c| {
            let mut n = 0usize;
            for r in targets::list(c)? {
                if r.def.host.as_deref() != Some(h.as_str())
                    || !matches!(r.def.kind, TargetKind::Docker | TargetKind::Vmlab)
                {
                    continue;
                }
                if r.def.lab_from_workspace {
                    n += r
                        .runtime
                        .instances
                        .iter()
                        .filter(|(k, i)| {
                            i.state == "running"
                                && !(r.def.name == me && Some(k.as_str()) == inst.as_deref())
                        })
                        .count();
                } else if r.state == "running" && r.def.name != me {
                    n += 1;
                }
            }
            Ok(n)
        })
        .await?;
    if running >= max as usize {
        return Err(ApiError::new(
            Code::RateLimited,
            format!(
                "host `{host}` runs {running} targets, and its max_running_targets is {max}; \
                 bring one down first"
            ),
        ));
    }
    Ok(())
}

/// `def` with `lab` set to the lab dir of an instance, as the vmlab verbs
/// take it.
fn with_lab(def: &TargetDef, lab: &str) -> TargetDef {
    let mut d = def.clone();
    d.lab = Some(PathBuf::from(lab));
    d.lab_repo = None;
    d.lab_from_workspace = false;
    d
}

/// The instance name on the host: the target, or `<target>.<agent ULID>`
/// for a `lab_from_workspace` target.
pub fn instance_name(def: &TargetDef, holder: Option<&str>) -> String {
    match holder {
        Some(h) if def.lab_from_workspace => format!("{}.{h}", def.name),
        _ => def.name.clone(),
    }
}

/// The holder whose instance a verb on a `lab_from_workspace` target acts
/// on: the agent caller itself, else `agent` of the body.
async fn instance_holder(
    state: &Shared,
    def: &TargetDef,
    actor: Option<&Actor>,
    agent: Option<&str>,
) -> ApiResult<HolderInfo> {
    let key = match (actor.and_then(Actor::agent), agent) {
        (Some(a), _) => a.id.clone(),
        (None, Some(k)) => k.to_owned(),
        (None, None) => {
            return Err(ApiError::invalid(format!(
                "`{}` gives each holder its own lab instance: name the holder with `agent`",
                def.name
            )));
        }
    };
    holder_info(state, &key, def.project.as_deref()).await
}

/// The definition that the exec, copy, and snapshot frames use: for a
/// vmlab instance target, `lab` is the lab dir of the instance. A target
/// that is not up has none: `409 conflict`.
pub async fn resolved_def(
    state: &Shared,
    row: &TargetRow,
    actor: Option<&Actor>,
    agent: Option<&str>,
) -> ApiResult<TargetDef> {
    if row.def.kind == TargetKind::Exedev {
        return crate::exedev::exec_def(state, &row.def);
    }
    if !is_instance_kind(&row.def) {
        return Ok(row.def.clone());
    }
    let lab = if row.def.lab_from_workspace {
        let h = instance_holder(state, &row.def, actor, agent).await?;
        row.runtime.instances.get(&h.id).and_then(|i| i.lab.clone())
    } else {
        row.runtime.lab.clone()
    };
    match lab {
        Some(l) => Ok(with_lab(&row.def, &l)),
        None => Err(ApiError::conflict(format!(
            "target `{}` has no lab instance on its host; bring it up first \
             (POST /api/targets/{}/up)",
            row.def.name, row.def.name
        ))),
    }
}

/// The source of a `vmlab.prepare` for a target, and the credential env of
/// a repo URL.
async fn lab_source(
    state: &Shared,
    def: &TargetDef,
    holder: Option<&HolderInfo>,
) -> ApiResult<(LabSource, BTreeMap<String, String>)> {
    let path = def.lab_path.clone().unwrap_or_default();
    if def.lab_from_workspace {
        let rel = holder.and_then(|h| h.rel.clone()).ok_or_else(|| {
            ApiError::invalid(format!(
                "`{}`: the holder's workspace is not under the work root",
                def.name
            ))
        })?;
        return Ok((
            LabSource::Workspace {
                workspace: rel,
                path,
            },
            BTreeMap::new(),
        ));
    }
    let (project, name) = (
        def.project.clone().unwrap_or_default(),
        def.lab_repo.clone().unwrap_or_default(),
    );
    let repo = state
        .db
        .read({
            let (p, n) = (project.clone(), name.clone());
            move |c| crate::store::repos::find_by_name(c, &p, &n)
        })
        .await?
        .ok_or_else(|| {
            ApiError::invalid(format!(
                "`lab_repo` `{name}` is not a repo of project `{project}`"
            ))
        })?;
    let git_ref = def
        .lab_ref
        .clone()
        .or_else(|| repo.default_branch.clone())
        .unwrap_or_else(|| "main".into());
    let local = crate::sandbox::local_repo_path(&repo.url, &state.repos_dir).and_then(|p| {
        p.strip_prefix(&state.repos_dir)
            .ok()
            .map(|r| r.to_string_lossy().into_owned())
    });
    let (url, env) = match &local {
        Some(_) => (None, BTreeMap::new()),
        None => {
            let secret = match &repo.credential {
                Some(c) => Some(crate::secrets::load(state, repo.owner(), c).await?),
                None => None,
            };
            (Some(repo.url.clone()), checkout_env(secret.as_ref()))
        }
    };
    Ok((
        LabSource::Repo {
            local,
            url,
            git_ref,
            path,
        },
        env,
    ))
}

/// `vmlab.prepare`: the lab instance with `workspace` attached. Returns
/// the `result` value (`lab`, `attached`, `head`, `dev`).
async fn prepare_instance(
    state: &Shared,
    def: &TargetDef,
    holder: Option<&HolderInfo>,
    workspace: Option<&str>,
) -> ApiResult<Value> {
    let (source, env) = lab_source(state, def, holder).await?;
    let instance = instance_name(def, holder.map(|h| h.id.as_str()));
    let machine = def.machine.clone();
    let workspaces: Vec<WorkspaceMount> = workspace
        .map(|w| WorkspaceMount { path: w.to_owned() })
        .into_iter()
        .collect();
    let c = verb_ok(state, def, "vmlab.prepare", move |session| {
        DaemonFrame::VmlabPrepare(VmlabPrepare {
            session,
            instance,
            source,
            machine,
            workspaces,
            env,
            timeout_secs: Some(VERB_TIMEOUT),
        })
    })
    .await?;
    c.result
        .ok_or_else(|| ApiError::internal("vmlab.prepare gave no result"))
}

/// `vmlab.release` of an instance.
async fn release_instance(state: &Shared, def: &TargetDef, instance: String) -> ApiResult<()> {
    verb_ok(state, def, "vmlab.release", move |session| {
        DaemonFrame::VmlabRelease(VmlabRelease {
            session,
            instance,
            timeout_secs: Some(VERB_TIMEOUT),
        })
    })
    .await?;
    Ok(())
}

/// A lifecycle verb on a vmlab instance target (`lab_repo` or
/// `lab_from_workspace`). `up` makes the instance with `vmlab.prepare` when
/// it is not running, then runs `vmlab up` in it; a destroy runs `vmlab
/// destroy` and `vmlab.release` (the worktree goes too). The other verbs go
/// to the lab dir of the instance.
async fn instance_lifecycle(
    state: &Shared,
    def: &TargetDef,
    verb_kind: LifecycleVerb,
    opts: &LifecycleOpts,
) -> ApiResult<Value> {
    use LifecycleVerb as V;
    host_online(state, def)?;
    let per_holder = def.lab_from_workspace;
    let holder = if per_holder {
        let key = opts
            .agent
            .as_deref()
            .or(opts.starting.as_deref())
            .or(opts.leaving.as_deref());
        Some(instance_holder(state, def, None, key).await?)
    } else {
        None
    };
    let hid = holder.as_ref().map(|h| h.id.clone());
    let mut rt = runtime_of(state, &def.name).await?;
    let (lab_now, attached_now) = match &hid {
        Some(h) => rt
            .instances
            .get(h)
            .map(|i| (i.lab.clone(), i.attached.clone()))
            .unwrap_or_default(),
        None => (rt.lab.clone(), rt.attached.clone()),
    };
    let inner = LifecycleOpts {
        inner: true,
        ..opts.clone()
    };
    let instance = instance_name(def, hid.as_deref());
    let mut extra = serde_json::Map::new();
    let mut lab_after = lab_now.clone();
    let mut attached_after = attached_now.clone();
    let mut head_after = rt.head.clone();
    let st = match (verb_kind, lab_now.as_deref()) {
        (V::Status, None) => "absent".to_owned(),
        (V::Status, Some(l)) => {
            lifecycle(state, &with_lab(def, l), V::Status, &inner).await?["state"]
                .as_str()
                .unwrap_or("unknown")
                .to_owned()
        }
        (V::Up, _) => {
            let cur = match lab_now.as_deref() {
                Some(l) => vmlab_state(state, &with_lab(def, l))
                    .await
                    .unwrap_or_else(|_| "unknown".into()),
                None => "absent".into(),
            };
            // The workspace to sync: the holder's own for a per-holder
            // instance; else the attached one while its agent holds the
            // target, else the first of the holders.
            let set = match &holder {
                Some(h) => h.rel.iter().cloned().collect(),
                None => {
                    attach_set(
                        state,
                        &def.name,
                        opts.starting.as_deref(),
                        opts.leaving.as_deref(),
                    )
                    .await?
                }
            };
            let want: Option<String> = attached_now
                .iter()
                .find(|a| set.contains(a))
                .cloned()
                .or_else(|| set.first().cloned());
            let fits = want.as_ref().is_none_or(|w| attached_now.contains(w));
            let mut fresh = cur != "running";
            if cur == "running" && !fits {
                match recreate_refusal(state, def) {
                    None => {
                        if let Some(l) = lab_now.as_deref() {
                            let d = LifecycleOpts {
                                destroy: true,
                                ..inner.clone()
                            };
                            lifecycle(state, &with_lab(def, l), V::Destroy, &d).await?;
                        }
                        fresh = true;
                    }
                    Some(why) => {
                        extra.insert(
                            "attach_error".into(),
                            attach_refused(def, &attached_now, &set, &why).into(),
                        );
                    }
                }
            }
            if fresh {
                check_running_cap(state, def, hid.as_deref()).await?;
                let p = prepare_instance(state, def, holder.as_ref(), want.as_deref()).await?;
                lab_after = p["lab"].as_str().map(str::to_owned);
                attached_after = want.iter().cloned().collect();
                if p.get("head").is_some_and(Value::is_string) {
                    head_after = p["head"].as_str().map(str::to_owned);
                }
                extra.insert("prepare".into(), p);
            }
            let l = lab_after
                .clone()
                .ok_or_else(|| ApiError::internal("vmlab.prepare gave no lab dir"))?;
            lifecycle(state, &with_lab(def, &l), V::Up, &inner).await?;
            // The others of the set that the one workspace of a machine
            // leaves out.
            let left: Vec<String> = set
                .iter()
                .filter(|s| !attached_after.contains(s))
                .cloned()
                .collect();
            if !left.is_empty() && !extra.contains_key("attach_error") {
                extra.insert(
                    "attach_error".into(),
                    format!(
                        "a vmlab machine syncs one workspace: `{}` has [{}]; [{}] is not attached",
                        def.name,
                        attached_after.join(", "),
                        left.join(", ")
                    )
                    .into(),
                );
            }
            "running".to_owned()
        }
        (V::Down | V::Destroy, None) => "absent".to_owned(),
        (V::Down | V::Destroy, Some(l)) => {
            let out = lifecycle(state, &with_lab(def, l), verb_kind, &inner).await?;
            let s = out["state"].as_str().unwrap_or("unknown").to_owned();
            if s == "absent" {
                release_instance(state, def, instance.clone()).await?;
                lab_after = None;
                attached_after.clear();
                head_after = None;
            }
            s
        }
        (V::Snapshot | V::Restore, None) => {
            return Err(ApiError::conflict(format!(
                "target `{}` has no lab instance; bring it up first",
                def.name
            )));
        }
        (V::Snapshot | V::Restore, Some(l)) => {
            let out = lifecycle(state, &with_lab(def, l), verb_kind, &inner).await?;
            for k in ["snapshot", "scope", "result"] {
                if let Some(v) = out.get(k) {
                    extra.insert(k.into(), v.clone());
                }
            }
            out["state"].as_str().unwrap_or("unknown").to_owned()
        }
    };
    match &hid {
        Some(h) => {
            if st == "absent" {
                rt.instances.remove(h);
            } else {
                let i = rt.instances.entry(h.clone()).or_default();
                i.agent_name = holder.as_ref().map(|x| x.name.clone()).unwrap_or_default();
                i.lab = lab_after;
                i.state = st.clone();
                i.attached = attached_after;
            }
        }
        None => {
            rt.lab = lab_after;
            rt.attached = attached_after;
            rt.head = head_after;
        }
    }
    let (name, s2) = (def.name.clone(), st.clone());
    state
        .db
        .write("system", move |t| {
            targets::set_runtime(t, &name, &rt)?;
            targets::set_state(t, &name, &s2, None)
        })
        .await?;
    let mut out = json!({"target": def.name, "verb": verb_kind, "state": st});
    if let Some(h) = &holder {
        out["agent"] = h.name.clone().into();
    }
    for (k, v) in extra {
        out[k] = v;
    }
    Ok(out)
}

/// Remove what a target runs on its host before the target is deleted:
/// the container of an ephemeral docker target, and the lab instances of a
/// vmlab instance target. Best effort: a failure is logged.
pub async fn teardown(state: &Shared, row: &TargetRow) {
    if row.def.kind == TargetKind::Exedev {
        // A failed `rm` keeps the VM's record, which outlives the target:
        // the sweep removes the VM on the account it was made on.
        if let Err(e) = crate::exedev::destroy(state, &row.def).await {
            warn!(target = %row.def.name, "cannot remove the exe.dev VM; the sweep will: {e}");
        }
        return;
    }
    if !is_instance_kind(&row.def)
        || !row
            .def
            .host
            .as_deref()
            .is_some_and(|h| state.executor.hosts.is_online(h))
    {
        return;
    }
    let opts = LifecycleOpts {
        destroy: true,
        ..LifecycleOpts::default()
    };
    let holders: Vec<Option<String>> = if row.def.lab_from_workspace {
        row.runtime.instances.keys().cloned().map(Some).collect()
    } else {
        vec![None]
    };
    for h in holders {
        let o = LifecycleOpts {
            agent: h.clone(),
            ..opts.clone()
        };
        if let Err(e) = instance_lifecycle(state, &row.def, LifecycleVerb::Destroy, &o).await {
            warn!(target = %row.def.name, holder = ?h, "cannot destroy the lab instance: {e}");
        }
    }
}

/// Bring an ephemeral target up before an agent starts (PLAN §19). A
/// permanent target is not brought up.
///
/// With `snapshot_before_task` on a kind that has snapshots (vmlab), the
/// daemon takes the snapshot [`CLEAN_SNAPSHOT`] after `up` and before any
/// exec, when the target has none yet. A kept target keeps its snapshot
/// over a stop, so it is taken one time. A destroy clears it, so an
/// ephemeral target without `keep` gets a new one on each start. A
/// permanent target gets it on the first agent start that finds it running.
///
/// Call sites: `RunnerSupervisor::start` ([`on_agent_start`]), for each
/// target the agent holds, before the harness launches; and a lease grant
/// to a running agent (`crate::leases`).
pub async fn ensure_up(state: &Shared, target: &str) -> ApiResult<Option<Value>> {
    ensure_up_for(state, target, "agent_start").await
}

/// [`ensure_up`] with the `reason` of the `target.up` audit row
/// (`agent_start`, or `lease_grant`).
pub async fn ensure_up_for(state: &Shared, target: &str, reason: &str) -> ApiResult<Option<Value>> {
    ensure_up_as(state, target, reason, None).await
}

/// [`ensure_up_for`] for the agent `agent` (a ULID): its workspace is
/// attached, and a `lab_from_workspace` target brings up its own instance.
pub async fn ensure_up_as(
    state: &Shared,
    target: &str,
    reason: &str,
    agent: Option<&str>,
) -> ApiResult<Option<Value>> {
    let n = target.to_owned();
    let row = state.db.read(move |c| targets::get(c, &n)).await?;
    let wants_snapshot = row.def.snapshot_before_task
        && snapshots_supported(&row.def)
        && !row.snapshots.iter().any(|s| s == CLEAN_SNAPSHOT);
    if !row.def.ephemeral {
        if wants_snapshot {
            let st = lifecycle(
                state,
                &row.def,
                LifecycleVerb::Status,
                &LifecycleOpts::default(),
            )
            .await?;
            if st["state"] == "running" {
                take_clean(state, &row).await?;
            } else {
                warn!(target = %row.def.name, state = %st["state"],
                      "snapshot_before_task: the permanent target is not running; no snapshot");
            }
        }
        // A permanent target that runs already: the agent's workspace must
        // be in it, or the answer says why not.
        if let Some(a) = agent {
            return attach_join(state, target, a, reason).await;
        }
        return Ok(None);
    }
    let opts = LifecycleOpts {
        starting: agent.map(str::to_owned),
        ..LifecycleOpts::default()
    };
    let out = lifecycle(state, &row.def, LifecycleVerb::Up, &opts).await?;
    audit_system(state, &row, "target.up", reason, &out).await?;
    if wants_snapshot {
        take_clean(state, &row).await?;
    }
    Ok(Some(out))
}

async fn take_clean(state: &Shared, row: &TargetRow) -> ApiResult<Value> {
    let out = lifecycle(state, &row.def, LifecycleVerb::Snapshot, &clean_opts()).await?;
    audit_system(state, row, "target.snapshot", "snapshot_before_task", &out).await?;
    Ok(out)
}

/// The audit row of a lifecycle verb that the daemon runs by itself (an
/// ephemeral target on an agent start or stop). The API handler writes the
/// row of a verb that a caller sends.
async fn audit_system(
    state: &Shared,
    row: &TargetRow,
    action: &str,
    reason: &str,
    out: &Value,
) -> ApiResult<()> {
    let mut detail = out.clone();
    detail["reason"] = reason.into();
    let (target, project, action) = (
        row.def.name.clone(),
        row.def.project.clone(),
        action.to_owned(),
    );
    state
        .db
        .write("system", move |t| {
            t.audit(&action, &target, project.as_deref(), detail)
        })
        .await
}

/// Remove an ephemeral target after its agent stops, unless `keep = true`
/// (PLAN §19). A permanent target is not touched.
///
/// A kept target with `snapshot_before_task` and the snapshot
/// [`CLEAN_SNAPSHOT`] is restored to it instead, so the next agent finds
/// it clean. It stays up, as a kept target does. A target that is
/// destroyed needs no restore.
///
/// Call sites: the supervisor's stop path ([`on_agent_stop`]:
/// `RunnerSupervisor::stop` and the exit of a harness that will not
/// restart), for each target the agent holds; and the end of a lease that
/// leaves no running holder (`crate::leases`).
pub async fn maybe_down(state: &Shared, target: &str) -> ApiResult<Option<Value>> {
    maybe_down_for(state, target, "agent_stop").await
}

/// [`maybe_down`] with the `reason` of the audit row (`agent_stop`, or
/// `lease_end`).
pub async fn maybe_down_for(
    state: &Shared,
    target: &str,
    reason: &str,
) -> ApiResult<Option<Value>> {
    maybe_down_as(state, target, reason, None).await
}

/// [`maybe_down_for`] for the agent `agent` (a ULID): a
/// `lab_from_workspace` target removes the agent's own instance.
pub async fn maybe_down_as(
    state: &Shared,
    target: &str,
    reason: &str,
    agent: Option<&str>,
) -> ApiResult<Option<Value>> {
    let n = target.to_owned();
    let row = state.db.read(move |c| targets::get(c, &n)).await?;
    if !row.def.ephemeral {
        return Ok(None);
    }
    let opts = LifecycleOpts {
        leaving: agent.map(str::to_owned),
        ..LifecycleOpts::default()
    };
    if row.def.keep {
        let restore = row.def.snapshot_before_task
            && snapshots_supported(&row.def)
            && row.snapshots.iter().any(|s| s == CLEAN_SNAPSHOT);
        if !restore {
            return Ok(None);
        }
        let o = LifecycleOpts {
            leaving: opts.leaving.clone(),
            ..clean_opts()
        };
        let out = lifecycle(state, &row.def, LifecycleVerb::Restore, &o).await?;
        audit_system(state, &row, "target.restore", reason, &out).await?;
        return Ok(Some(out));
    }
    let out = lifecycle(state, &row.def, LifecycleVerb::Destroy, &opts).await?;
    audit_system(state, &row, "target.destroy", reason, &out).await?;
    Ok(Some(out))
}

/// A holder joins a target that other running agents hold: its workspace
/// must be attached. A running ephemeral lab instance with no live session
/// gets a new instance with it; Docker containers are never replaced for
/// workspace changes. Otherwise the answer has
/// `attach_error`. A target that is not running, or takes no workspace, is
/// not touched: the next `up` attaches it.
pub async fn attach_join(
    state: &Shared,
    target: &str,
    agent: &str,
    reason: &str,
) -> ApiResult<Option<Value>> {
    let n = target.to_owned();
    let row = state.db.read(move |c| targets::get(c, &n)).await?;
    if !attaches(&row.def) || row.def.lab_from_workspace || row.state != "running" {
        return Ok(None);
    }
    // A host with no workspaces mounts none: nothing to attach (TER-41).
    if host_has_no_workspaces(state, &row.def) {
        return Ok(None);
    }
    let h = holder_info(state, agent, None).await?;
    let Some(rel) = h.rel else { return Ok(None) };
    if row.runtime.attached.contains(&rel) {
        return Ok(None);
    }
    if !row.def.ephemeral {
        // A permanent target is not run again for a workspace.
        let want: Vec<String> = row
            .runtime
            .attached
            .iter()
            .cloned()
            .chain(std::iter::once(rel))
            .collect();
        return Ok(Some(json!({
            "target": row.def.name, "verb": "up", "state": row.state,
            "attach_error": attach_refused(&row.def, &row.runtime.attached, &want,
                "it is permanent: a new container would lose its files"),
        })));
    }
    let opts = LifecycleOpts {
        starting: Some(agent.to_owned()),
        ..LifecycleOpts::default()
    };
    let out = lifecycle(state, &row.def, LifecycleVerb::Up, &opts).await?;
    audit_system(state, &row, "target.attach", reason, &out).await?;
    Ok(Some(out))
}

/// A holder leaves a target that other running agents still hold: its
/// workspace goes out of the target. Docker and vmlab cannot unmount from a
/// running container or lab. An ephemeral lab instance with no live
/// session gets a new one without it. Otherwise the workspace stays attached
/// until the target runs again (a log line says so).
pub async fn detach(
    state: &Shared,
    target: &str,
    agent: &str,
    reason: &str,
) -> ApiResult<Option<Value>> {
    let n = target.to_owned();
    let row = state.db.read(move |c| targets::get(c, &n)).await?;
    if !attaches(&row.def) || row.def.lab_from_workspace || row.state != "running" {
        return Ok(None);
    }
    let h = holder_info(state, agent, None).await?;
    let Some(rel) = h.rel else { return Ok(None) };
    if !row.runtime.attached.contains(&rel) {
        return Ok(None);
    }
    if let Some(why) = recreate_refusal(state, &row.def) {
        warn!(target = %row.def.name, workspace = %rel,
              "the workspace stays attached until the target runs again: {why}");
        return Ok(None);
    }
    // Docker always returned above: only lab instances can be replaced.
    let d = LifecycleOpts {
        destroy: true,
        leaving: Some(agent.to_owned()),
        ..LifecycleOpts::default()
    };
    lifecycle(state, &row.def, LifecycleVerb::Destroy, &d).await?;
    let u = LifecycleOpts {
        leaving: Some(agent.to_owned()),
        ..LifecycleOpts::default()
    };
    let out = lifecycle(state, &row.def, LifecycleVerb::Up, &u).await?;
    audit_system(state, &row, "target.detach", reason, &out).await?;
    Ok(Some(out))
}

/// The agents that hold `target` now and run: a running agent whose
/// `policy.targets` lists it, or that holds an active lease on it. `except`
/// is left out (the agent that starts or stops).
pub async fn running_holders(
    state: &Shared,
    target: &str,
    except: Option<&str>,
) -> ApiResult<Vec<String>> {
    let t = target.to_owned();
    let ids = state
        .db
        .read(move |c| {
            let mut stmt = c.prepare("SELECT id, policy FROM agents ORDER BY id")?;
            let mut ids: Vec<String> = stmt
                .query_map([], |r| Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?)))?
                .collect::<Result<Vec<_>, _>>()?
                .into_iter()
                .filter(|(_, p)| {
                    crate::store::json_col::<terrarium_core::config::Policy>(p)
                        .targets
                        .contains(&t)
                })
                .map(|(id, _)| id)
                .collect();
            for l in leases::active_on(c, &t)? {
                if !ids.contains(&l.agent) {
                    ids.push(l.agent);
                }
            }
            Ok(ids)
        })
        .await?;
    let mut out = Vec::new();
    for id in ids {
        if Some(id.as_str()) == except {
            continue;
        }
        let Ok(aid) = id.parse::<terrarium_core::AgentId>() else {
            continue;
        };
        if state
            .supervisor
            .status(aid)
            .await
            .is_some_and(|s| s.is_live() || s == terrarium_core::AgentStatus::Restarting)
        {
            out.push(id);
        }
    }
    Ok(out)
}

/// The targets an agent holds: its `policy.targets`, and the targets of
/// its active leases.
async fn held_targets(state: &Shared, agent: &str, policy_targets: &[String]) -> Vec<String> {
    let mut out = policy_targets.to_vec();
    let a = agent.to_owned();
    match state.db.read(move |c| leases::active_of(c, &a)).await {
        Ok(ls) => {
            for l in ls {
                if !out.contains(&l.target) {
                    out.push(l.target);
                }
            }
        }
        Err(e) => warn!(agent = %agent, "cannot read the leases: {e}"),
    }
    out
}

/// A target that gives each holder its own instance.
async fn per_holder(state: &Shared, target: &str) -> ApiResult<TargetRow> {
    let n = target.to_owned();
    state.db.read(move |c| targets::get(c, &n)).await
}

/// [`ensure_up`] for every target the agent holds (its policy and its
/// leases), unless another running agent holds it: then it is up already,
/// and the agent's workspace joins it ([`attach_join`]). A
/// `lab_from_workspace` target brings up the agent's own instance.
/// Failures, and an `attach_error`, are logged and returned; they do not
/// stop the others.
pub async fn on_agent_start(
    state: &Shared,
    agent: &str,
    policy_targets: &[String],
) -> Vec<(String, ApiError)> {
    let mut errors = Vec::new();
    for t in held_targets(state, agent, policy_targets).await {
        let row = match per_holder(state, &t).await {
            Ok(r) => r,
            Err(e) => {
                errors.push((t.clone(), e));
                continue;
            }
        };
        let res = if row.def.lab_from_workspace {
            ensure_up_as(state, &t, "agent_start", Some(agent)).await
        } else {
            match running_holders(state, &t, Some(agent)).await {
                Ok(h) if !h.is_empty() => attach_join(state, &t, agent, "agent_start").await,
                Ok(_) => ensure_up_as(state, &t, "agent_start", Some(agent)).await,
                Err(e) => Err(e),
            }
        };
        match res {
            Ok(Some(out)) => {
                if let Some(msg) = out["attach_error"].as_str() {
                    warn!(target = %t, "{msg}");
                    errors.push((t.clone(), ApiError::conflict(msg)));
                }
            }
            Ok(None) => {}
            Err(e) => {
                warn!(target = %t, "cannot bring an ephemeral target up: {e}");
                errors.push((t.clone(), e));
            }
        }
    }
    errors
}

/// [`maybe_down`] for every target the agent holds (its policy and its
/// leases), unless another running agent still holds it: then the agent's
/// workspace leaves it ([`detach`]). A `lab_from_workspace` target removes
/// the agent's own instance.
pub async fn on_agent_stop(
    state: &Shared,
    agent: &str,
    policy_targets: &[String],
) -> Vec<(String, ApiError)> {
    let mut errors = Vec::new();
    let holder = agent.to_owned();
    if let Err(e) = state
        .db
        .write_full("system", move |t| {
            crate::store::live_workspaces::end_holder_authority(t, &holder, None, true)
        })
        .await
    {
        errors.push(("live-workspaces".into(), e));
    }
    for t in held_targets(state, agent, policy_targets).await {
        let row = match per_holder(state, &t).await {
            Ok(r) => r,
            Err(e) => {
                errors.push((t.clone(), e));
                continue;
            }
        };
        let res = if row.def.lab_from_workspace {
            maybe_down_as(state, &t, "agent_stop", Some(agent)).await
        } else {
            match running_holders(state, &t, Some(agent)).await {
                Ok(h) if !h.is_empty() => detach(state, &t, agent, "agent_stop").await,
                Ok(_) => maybe_down_as(state, &t, "agent_stop", Some(agent)).await,
                Err(e) => Err(e),
            }
        };
        if let Err(e) = res {
            warn!(target = %t, "cannot remove an ephemeral target: {e}");
            errors.push((t.clone(), e));
        }
    }
    errors
}

// ------------------------------------------------------- checkout on target

/// The script of a checkout on a target. Positional args: URL, dest,
/// branch, depth. The secret comes in the env: `TERRARIUM_GIT_USERNAME` and
/// `TERRARIUM_GIT_PASSWORD` for a token (a credential helper of `-c` reads
/// them; the helper text holds no secret), or `TERRARIUM_GIT_SSH_KEY`
/// (base64 on one line, [`checkout_script_env`]; decoded with `base64 -d`
/// into a 0600 file in a 0700 temp dir for `GIT_SSH_COMMAND`, removed at
/// exit). ssh reads no config file (`-F /dev/null`), and its
/// known hosts are a file in that dir, seeded from
/// `TERRARIUM_GIT_KNOWN_HOSTS` ([`git_known_hosts`]).
///
/// The agent can write the repo on the target, so the script treats its
/// config as hostile (secret manager review S9, the rules of `crate::git`):
///
/// - Each git command runs with the `-c` overrides of `crate::git`
///   (no hooks, no fsmonitor, no helper from a config, only the safe
///   transports), with `GIT_CONFIG_GLOBAL=/dev/null`,
///   `GIT_CONFIG_NOSYSTEM=1`, no askpass, and a `GIT_SSH_COMMAND` that wins
///   over `core.sshCommand`. The env vars that point git at another repo or
///   config are unset.
/// - Before it touches an existing repo, the script checks the names in
///   `.git/config` against the allowlist of `crate::git::config_allowed`
///   (by name only), and refuses a `.git/commondir`. A key outside the
///   list stops the update: `not_updated`, with the key in `note`.
/// - The secret is in the env for the fetch or the clone only. The script
///   removes it (and the key file) before `status` and `merge`, which can
///   run filters from the repo config.
pub const CHECKOUT_SCRIPT: &str = r#"set -eu
url=$1; dest=$2; branch=$3; depth=$4
case "$dest" in /*) ;; *) dest="$HOME/work/$dest" ;; esac
unset GIT_DIR GIT_WORK_TREE GIT_INDEX_FILE GIT_OBJECT_DIRECTORY GIT_ALTERNATE_OBJECT_DIRECTORIES \
  GIT_CONFIG GIT_CONFIG_PARAMETERS GIT_CONFIG_COUNT GIT_NAMESPACE GIT_CREDENTIAL_HELPER \
  GIT_TEMPLATE_DIR GIT_EXEC_PATH GIT_EXTERNAL_DIFF GIT_PAGER GIT_EDITOR GIT_PROXY_COMMAND \
  GIT_SSH GIT_SSH_VARIANT GIT_CEILING_DIRECTORIES GIT_DISCOVERY_ACROSS_FILESYSTEM
export GIT_CONFIG_GLOBAL=/dev/null GIT_CONFIG_NOSYSTEM=1 GIT_TERMINAL_PROMPT=0
export GIT_ASKPASS=false SSH_ASKPASS=false LC_ALL=C
# ssh reads no config file (`-F /dev/null`: a planted ~/.ssh/config
# cannot redirect the host or run a ProxyCommand), and keeps its host keys
# and the key file in a 0700 dir of its own: TERRARIUM_GIT_SSH_DIR (the
# job's dir), else a new temp dir. TERRARIUM_GIT_KNOWN_HOSTS seeds the
# known hosts (the daemon's).
sshdir=${TERRARIUM_GIT_SSH_DIR:-}
sshtmp=
if [ -n "$sshdir" ]; then
  mkdir -p -m 700 -- "$sshdir" && chmod 700 -- "$sshdir" || { echo "cannot make $sshdir"; exit 2; }
else
  sshdir=$(mktemp -d) || exit 2
  sshtmp=$sshdir
fi
printf '%s\n' "${TERRARIUM_GIT_KNOWN_HOSTS:-}" > "$sshdir/known_hosts"
unset TERRARIUM_GIT_KNOWN_HOSTS TERRARIUM_GIT_SSH_DIR
sshbase="ssh -F /dev/null -o UserKnownHostsFile=$sshdir/known_hosts -o GlobalKnownHostsFile=/dev/null -o BatchMode=yes -o StrictHostKeyChecking=accept-new"
export GIT_SSH_COMMAND="$sshbase"
helper=
keyfile=
trap 'if [ -n "$keyfile" ]; then rm -f "$keyfile"; fi; if [ -n "$sshtmp" ]; then rm -rf "$sshtmp"; fi' EXIT
if [ -n "${TERRARIUM_GIT_PASSWORD:-}" ]; then
  helper='!f() { test "$1" = get || exit 0; echo "username=$TERRARIUM_GIT_USERNAME"; echo "password=$TERRARIUM_GIT_PASSWORD"; }; f'
fi
if [ -n "${TERRARIUM_GIT_SSH_KEY:-}" ]; then
  keyfile=$sshdir/key
  # The key comes base64-encoded on one line (TER-577), as the env of an ssh
  # target does: a docker target then keeps it in its 0600 env file.
  (umask 077 && printf '%s' "$TERRARIUM_GIT_SSH_KEY" | base64 -d > "$keyfile") \
    || { echo "cannot decode the SSH key: the target needs base64 -d" >&2; exit 2; }
  export GIT_SSH_COMMAND="$sshbase -i $keyfile -o IdentitiesOnly=yes"
fi
unset TERRARIUM_GIT_SSH_KEY
G() {
  git -c core.hooksPath=/dev/null -c core.fsmonitor=false -c core.untrackedCache=false \
    -c credential.helper= -c protocol.allow=never -c protocol.https.allow=always \
    -c protocol.http.allow=always -c protocol.ssh.allow=always -c protocol.file.allow=always \
    -c protocol.git.allow=never -c protocol.ext.allow=never -c protocol.fd.allow=never \
    -c protocol.ftp.allow=never -c protocol.ftps.allow=never -c fetch.recurseSubmodules=false \
    -c submodule.recurse=false -c gc.auto=0 -c maintenance.auto=false \
    -c merge.verifySignatures=false -c advice.detachedHead=false -c core.pager=cat \
    ${safe:+-c "safe.directory=$safe"} "$@"
}
g() {
  if [ -n "$helper" ]; then G -c "credential.helper=$helper" "$@"; else G "$@"; fi
}
forget() {
  unset TERRARIUM_GIT_PASSWORD TERRARIUM_GIT_USERNAME
  helper=
  if [ -n "$keyfile" ]; then rm -f "$keyfile"; fi
  keyfile=
  export GIT_SSH_COMMAND="$sshbase"
}
check_config() {
  if [ -e "$dest/.git/commondir" ]; then echo ".git/commondir"; return; fi
  names=$(git config --file "$dest/.git/config" --name-only --list 2>/dev/null) || { echo ".git/config"; return; }
  printf '%s\n' "$names" | while IFS= read -r k; do
    [ -n "$k" ] || continue
    l=$(printf '%s' "$k" | tr 'A-Z' 'a-z')
    case "$l" in
      remote.*:*|remote.*/*) echo "$k"; exit ;;
      core.repositoryformatversion|core.filemode|core.bare|core.logallrefupdates|core.ignorecase) ;;
      core.precomposeunicode|core.symlinks|core.autocrlf|core.eol|core.safecrlf|core.quotepath) ;;
      remote.*.url|remote.*.fetch|remote.*.pushurl|remote.*.push|remote.*.prune|remote.*.tagopt) ;;
      branch.*.remote|branch.*.merge|branch.*.rebase|branch.*.pushremote|branch.*.description) ;;
      credential.helper|credential.usehttppath|user.*|advice.*|color.*) ;;
      push.default|push.autosetupremote|push.followtags|pull.rebase|pull.ff) ;;
      init.defaultbranch|extensions.objectformat|commit.gpgsign) ;;
      *) echo "$k"; exit ;;
    esac
  done
}
note=
safe=
if [ -d "$dest/.git" ]; then
  # Another UID can own the repo: on a native host the sessions of one
  # agent run as different pool UIDs (TER-635). Its config is checked
  # below and runs with the overrides of G, so trust this one path.
  safe=$(cd "$dest" && pwd -P)
  bad=$(check_config)
  if [ -n "$bad" ]; then
    forget
    action=not_updated; note="the repo config has \`$bad\`, which the checkout does not allow; remove it"
  else
    before=$(G -C "$dest" rev-parse -q --verify HEAD || true)
    if [ -n "$branch" ]; then ref=$branch; else ref=$(G -C "$dest" symbolic-ref --short -q HEAD || echo HEAD); fi
    if [ -n "$depth" ]; then g -C "$dest" fetch --depth "$depth" -- "$url" "$ref"; else g -C "$dest" fetch -- "$url" "$ref"; fi
    forget
    if [ -n "$(G -C "$dest" status --porcelain)" ]; then
      action=not_updated; note="the work tree has local changes"
    elif G -C "$dest" merge --ff-only -q FETCH_HEAD >/dev/null 2>&1; then
      after=$(G -C "$dest" rev-parse -q --verify HEAD || true)
      if [ "$before" = "$after" ]; then action=up_to_date; else action=updated; fi
    else
      action=not_updated; note="the branch has diverged"
    fi
  fi
else
  mkdir -p "$(dirname "$dest")"
  set --
  if [ -n "$depth" ]; then set -- "$@" --depth "$depth"; fi
  if [ -n "$branch" ]; then set -- "$@" --branch "$branch"; fi
  g clone "$@" -- "$url" "$dest"
  forget
  action=cloned
fi
if [ -n "${TERRARIUM_COMMIT_HOOK:-}" ] && [ "$action" != not_updated ]; then
  [ ! -L "$dest/.git" ] && [ ! -L "$dest/.git/hooks" ] || exit 2
  mkdir -p "$dest/.git/hooks"
  hook="$dest/.git/hooks/prepare-commit-msg"
  if [ -e "$hook" ] && ! grep -q '^# terrarium managed provenance hook$' "$hook"; then
    [ ! -e "$hook.repo" ] && [ ! -L "$hook.repo" ] || exit 2
    mv "$hook" "$hook.repo"
  fi
  hook_tmp=$(mktemp "$dest/.git/hooks/.terrarium-commit.XXXXXX")
  printf '%s' "$TERRARIUM_COMMIT_HOOK" > "$hook_tmp"
  chmod 755 "$hook_tmp"
  mv -f "$hook_tmp" "$dest/.git/hooks/prepare-commit-msg"
  G -C "$dest" config user.name "$TERRARIUM_COMMIT_NAME"
  G -C "$dest" config user.email "$TERRARIUM_COMMIT_EMAIL"
  if [ -n "${TERRARIUM_PUSH_HOOK:-}" ]; then
    push_hook="$dest/.git/hooks/pre-push"
    if [ -e "$push_hook" ] && ! grep -q '^# Written by terrariumd' "$push_hook"; then
      [ ! -e "$push_hook.repo" ] && [ ! -L "$push_hook.repo" ] || exit 2
      mv "$push_hook" "$push_hook.repo"
    fi
    push_tmp=$(mktemp "$dest/.git/hooks/.terrarium-push.XXXXXX")
    printf '%s' "$TERRARIUM_PUSH_HOOK" > "$push_tmp"
    chmod 755 "$push_tmp"
    mv -f "$push_tmp" "$push_hook"
    G -C "$dest" config terrarium.provenance "$TERRARIUM_PROVENANCE"
    G -C "$dest" config terrarium.provenanceWarnOnly "$TERRARIUM_PROVENANCE_WARN_ONLY"
    G -C "$dest" config terrarium.repoId "$TERRARIUM_REPO_ID"
  fi
fi
head=$(G -C "$dest" rev-parse -q --verify HEAD || true)
cur=$(G -C "$dest" symbolic-ref --short -q HEAD || true)
printf 'TERRARIUM_CHECKOUT\taction=%s\thead=%s\tbranch=%s\tpath=%s\tnote=%s\n' "$action" "$head" "$cur" "$dest" "$note"
"#;

/// The env of a checkout for a secret.
pub fn checkout_env(secret: Option<&Secret>) -> BTreeMap<String, String> {
    let mut env = BTreeMap::new();
    match secret {
        Some(Secret::Token { username, token }) => {
            env.insert("TERRARIUM_GIT_USERNAME".into(), username.clone());
            env.insert("TERRARIUM_GIT_PASSWORD".into(), token.to_string());
        }
        Some(Secret::SshKey(k)) => {
            env.insert("TERRARIUM_GIT_SSH_KEY".into(), k.trim_end().to_owned());
        }
        // A login is for vnc and rdp targets; git gets no credential.
        Some(Secret::Login { .. } | Secret::Other(_)) | None => {}
    }
    env
}

/// The env of [`CHECKOUT_SCRIPT`] for a secret: [`checkout_env`], with the
/// SSH key base64-encoded on one line (TER-577). A multi-line value on a
/// docker target would go into the env of the host's `docker` CLI; one line
/// goes into the 0600 env file. The vmlab prepare reads [`checkout_env`] on
/// the host agent and keeps the plain key.
pub fn checkout_script_env(secret: Option<&Secret>) -> BTreeMap<String, String> {
    let mut env = checkout_env(secret);
    if let Some(k) = env.get_mut("TERRARIUM_GIT_SSH_KEY") {
        *k = base64::engine::general_purpose::STANDARD.encode(format!("{k}\n"));
    }
    env
}

/// The checkout command, without the secret: that goes in `env`.
pub fn checkout_cmd(
    url: &str,
    dest: &str,
    branch: Option<&str>,
    depth: Option<u32>,
    secret: Option<&Secret>,
) -> ApiResult<RemoteCmd> {
    if url.is_empty()
        || url.starts_with('-')
        || url.split('/').next().is_some_and(|s| s.contains("::"))
    {
        return Err(ApiError::conflict(format!(
            "repo URL `{url}` cannot be cloned on a target"
        )));
    }
    if let Some(b) = branch
        && (b.is_empty()
            || b.starts_with(['-', '.', '/'])
            || b.contains("..")
            || b.chars()
                .any(|c| c.is_whitespace() || c.is_control() || "~^:?*[\\".contains(c)))
    {
        return Err(ApiError::invalid(format!("bad branch name `{b}`")));
    }
    if !dest.starts_with('/') || dest.split('/').any(|c| c == "..") {
        return Err(ApiError::invalid(format!("bad checkout path `{dest}`")));
    }
    Ok(RemoteCmd {
        argv: vec![
            "sh".into(),
            "-c".into(),
            CHECKOUT_SCRIPT.into(),
            "sh".into(),
            url.into(),
            dest.into(),
            branch.unwrap_or_default().into(),
            depth.map(|d| d.to_string()).unwrap_or_default(),
        ],
        env: checkout_script_env(secret),
        timeout_secs: Some(VERB_TIMEOUT),
        ..RemoteCmd::default()
    })
}

/// Parse the last line of the checkout script.
pub fn parse_checkout(stdout: &str) -> Option<Value> {
    let line = stdout
        .lines()
        .rev()
        .find(|l| l.starts_with("TERRARIUM_CHECKOUT\t"))?;
    let mut out = serde_json::Map::new();
    for kv in line.split('\t').skip(1) {
        let (k, v) = kv.split_once('=')?;
        let v = if v.is_empty() {
            Value::Null
        } else {
            Value::String(v.to_owned())
        };
        out.insert(k.to_owned(), v);
    }
    if out.get("note").is_some_and(Value::is_null) {
        out.remove("note");
    }
    Some(Value::Object(out))
}

/// Clone or update a repo on a target, through an exec on the host agent
/// (or ssh). The secret goes in the env of that one run.
#[allow(clippy::too_many_arguments)]
pub async fn checkout_on_target(
    state: &Shared,
    actor: &Actor,
    row: &TargetRow,
    repo: &crate::store::repos::Repo,
    url: &str,
    dest: &str,
    branch: Option<&str>,
    depth: Option<u32>,
    secret: Option<&Secret>,
) -> ApiResult<Value> {
    no_exec(&row.def)?;
    if row.def.kind == TargetKind::Vmlab {
        return Err(ApiError::invalid(
            "a checkout on a vmlab target is not supported; vmlab exec takes no env for the credential",
        ));
    }
    let mut cmd = checkout_cmd(url, dest, branch, depth, secret)?;
    let _root_guard = crate::target_roots::lock_card(state, actor, &row.def.name).await?;
    crate::target_roots::prepare(state, actor, &row.def, Some(dest)).await?;
    if let Some(a) = actor.agent() {
        let p = crate::git_identity::for_repo(state, &a.id, repo).await?;
        cmd.env.insert("TERRARIUM_REPO_ID".into(), repo.id.clone());
        cmd.env.insert(
            "TERRARIUM_PUSH_HOOK".into(),
            crate::git::push_deny_hook(&repo.push_deny),
        );
        cmd.env.insert(
            "TERRARIUM_PROVENANCE".into(),
            (!p.trailers.is_empty()).to_string(),
        );
        cmd.env.insert(
            "TERRARIUM_PROVENANCE_WARN_ONLY".into(),
            p.warn_only.to_string(),
        );
        cmd.env.insert("TERRARIUM_COMMIT_HOOK".into(), p.hook());
        cmd.env
            .insert("TERRARIUM_COMMIT_NAME".into(), p.identity.name);
        cmd.env
            .insert("TERRARIUM_COMMIT_EMAIL".into(), p.identity.email);
    }
    if let Some(k) = git_known_hosts(state) {
        cmd.env.insert("TERRARIUM_GIT_KNOWN_HOSTS".into(), k);
    }
    cmd.holder = holder_of(actor);
    cmd.operator = actor.is_operator();
    let c = start_cmd(state, &row.def, &cmd)?.collect(256 * 1024).await;
    if !c.end().success() {
        let e = verb_failed("git checkout on the target", &c);
        return Err(ApiError::invalid(e.message).with_details(json!({
            "command": "checkout", "status": c.end(), "stderr": c.stderr_tail(),
        })));
    }
    parse_checkout(&String::from_utf8_lossy(&c.stdout))
        .ok_or_else(|| ApiError::internal("the checkout script printed no result line"))
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    #[tokio::test(start_paused = true)]
    async fn cleanup_attempt_fakes_never_produce_verified_outcomes() {
        for value in [json!({}), json!("truncated"), json!({"error":"failed"}),
            json!({"pid":null,"left":[]}), json!({"pid":1,"left":[]}),
            json!({"pid":1,"left":[2]}), json!({"left":["bad"]})] {
            let (s, mut controls) = ExecSession::new("fake", Some("owner".into()), "exec", 1);
            let (_tx, rx) = tokio::sync::watch::channel(Some(value.clone()));
            match s.attempt_cleanup(rx).await {
                CleanupAttempt::LegacyReport(report) => assert_eq!(report,value),
                _ => panic!("legacy reports must remain diagnostics"),
            }
            assert!(matches!(controls.try_recv(), Ok(Control::Kill)));
            assert!(controls.try_recv().is_err());
        }
        let (s, mut controls) = ExecSession::new("fake", Some("owner".into()), "exec", 1);
        let (tx, rx) = tokio::sync::watch::channel(None);
        drop(tx);
        assert!(matches!(s.attempt_cleanup(rx).await,CleanupAttempt::MissingReport));
        assert!(matches!(controls.try_recv(), Ok(Control::Kill)));
        let (tx, rx) = tokio::sync::watch::channel(None);
        let pending = {
            let s = s.clone();
            tokio::spawn(async move { s.attempt_cleanup(rx).await })
        };
        tokio::task::yield_now().await;
        tokio::time::advance(Duration::from_secs(13)).await;
        assert!(matches!(pending.await.unwrap(),CleanupAttempt::Deadline));
        tx.send(Some(json!({"left":[]}))).unwrap_err(); // late collector is gone
        assert!(matches!(controls.try_recv(), Ok(Control::Kill)));
        assert!(controls.try_recv().is_err());
        let (tx, rx) = tokio::sync::watch::channel(Some(Value::Null));
        assert!(matches!(s.attempt_cleanup(rx).await,CleanupAttempt::MissingReport));
        drop(tx);
        drop(controls);
        let (_tx, rx) = tokio::sync::watch::channel(None);
        assert!(matches!(s.attempt_cleanup(rx).await,CleanupAttempt::DeliveryUnknown));
    }

    #[tokio::test]
    async fn cleanup_route_claim_is_live_wrapped_and_single_use() {
        let (s, mut controls) = ExecSession::new("fake", Some("owner".into()), "exec", 1);
        assert!(s.claim_cleanup().is_none()); // missing/restored report recipe
        let (_tx, rx) = tokio::sync::watch::channel(None);
        s.set_kill_report(Some(rx));
        let claimed = s.claim_cleanup();
        assert!(claimed.is_some());
        assert!(s.claim_cleanup().is_none());
        assert!(s.control(Control::Kill));
        assert!(matches!(controls.recv().await, Some(Control::Kill)));
        assert!(controls.try_recv().is_err());
        drop(controls);
        assert!(!s.control(Control::Kill));
        s.finish(End::error("fake", "done"));
        let (_tx, rx) = tokio::sync::watch::channel(None);
        s.set_kill_report(Some(rx));
        assert!(s.claim_cleanup().is_none());
    }

    #[tokio::test(start_paused = true)]
    async fn target_admission_waits_for_reconciliation_without_holding_its_lock() {
        let executor = Arc::new(Executor::default());
        let def: TargetDef = toml::from_str("kind = \"host\"\nhost = \"h\"\n").unwrap();
        executor.set_resource_pending("h", true);
        let waiting = {
            let executor = executor.clone();
            tokio::spawn(async move { executor.target_admission(&def).await })
        };
        tokio::task::yield_now().await;
        assert!(!waiting.is_finished());
        let reconcile = executor.resource_lock("h").try_lock_owned().unwrap();
        let epoch = executor.resource_epoch("h");
        assert!(executor.finish_resource_reconcile("h", epoch.as_deref()));
        drop(reconcile);
        tokio::time::advance(Duration::from_millis(50)).await;
        let guard = waiting.await.unwrap().unwrap();
        assert!(executor.resource_lock("h").try_lock_owned().is_err());
        drop(guard);
    }

    #[tokio::test(start_paused = true)]
    async fn target_admission_refuses_when_reconciliation_deadline_passes() {
        let executor = Executor::default();
        let def: TargetDef = toml::from_str("kind = \"host\"\nhost = \"h\"\n").unwrap();
        executor.set_resource_pending("h", true);
        let started = tokio::time::Instant::now();
        let error = executor.target_admission(&def).await.unwrap_err();
        assert!(error.to_string().contains("reconciliation is pending"));
        assert_eq!(started.elapsed(), Duration::from_secs(60));
        assert!(executor.resource_pending("h"));
        assert!(executor.resource_lock("h").try_lock_owned().is_ok());
    }

    #[tokio::test(start_paused = true)]
    async fn target_admission_deadline_includes_the_reconciliation_lock() {
        let executor = Executor::default();
        let def: TargetDef = toml::from_str("kind = \"host\"\nhost = \"h\"\n").unwrap();
        executor.set_resource_pending("h", true);
        let _reconcile = executor.resource_lock("h").lock_owned().await;
        let started = tokio::time::Instant::now();
        assert!(executor.target_admission(&def).await.is_err());
        assert_eq!(started.elapsed(), Duration::from_secs(60));
        assert!(executor.resource_pending("h"));
    }

    #[test]
    fn own_key_redaction_covers_individual_pem_lines() {
        let body = "dummy-base64-private-key-line-for-redaction";
        let value = format!("-----BEGIN PRIVATE KEY-----\n{body}\n-----END PRIVATE KEY-----\n");
        let redactor =
            super::own_redactor(&[("job/vm".into(), zeroize::Zeroizing::new(value))]).unwrap();
        assert!(!redactor.redact_str(body).contains(body));
        assert!(
            redactor
                .redact_str("ordinary build output")
                .contains("ordinary build output")
        );
    }
    use super::*;

    fn ssh_def() -> TargetDef {
        let mut d: TargetDef =
            toml::from_str("kind = \"ssh\"\naddress = \"build@box\"\nport = 2222\n").unwrap();
        d.name = "buildbox".into();
        d
    }

    #[test]
    fn snapshot_scope_defaults_to_the_machine_and_merges_by_name() {
        let mut d: TargetDef = toml::from_str(
            "kind = \"vmlab\"\nhost = \"h\"\nlab = \"/labs/ad\"\nmachine = \"dc01\"\n",
        )
        .unwrap();
        d.name = "ad".into();
        assert_eq!(scope_of(&d, None), SnapshotScope::Machine);
        assert_eq!(snapshot_machine(&d, None).unwrap().as_deref(), Some("dc01"));
        assert_eq!(
            snapshot_machine(&d, Some(SnapshotScope::Lab)).unwrap(),
            None
        );
        d.machine = None;
        assert_eq!(scope_of(&d, None), SnapshotScope::Lab);
        assert!(snapshot_machine(&d, Some(SnapshotScope::Machine)).is_err());
        for bad in ["", "-x", "a b", "a\tb", "a\u{7}"] {
            assert!(check_snapshot_name(bad).is_err(), "{bad:?}");
        }
        assert!(check_snapshot_name("terrarium-clean").is_ok());
        let merged = merge_snapshots(&[
            (
                "dc01".into(),
                vec![
                    json!({"name": "clean", "kind": "online", "taken": "t1"}),
                    json!({"name": "pre", "kind": "offline", "taken": "t2"}),
                ],
            ),
            (
                "client01".into(),
                vec![json!({"name": "clean", "kind": "offline", "taken": "t3"})],
            ),
        ]);
        assert_eq!(
            merged,
            vec![
                json!({"name": "clean", "kind": "online", "taken": "t1", "machines": ["dc01", "client01"]}),
                json!({"name": "pre", "kind": "offline", "taken": "t2", "machines": ["dc01"]}),
            ]
        );
    }

    #[test]
    fn ssh_command_line() {
        let cmd = RemoteCmd {
            argv: vec!["echo".into(), "it's".into()],
            cwd: Some("/srv/x y".into()),
            ..RemoteCmd::default()
        };
        let p = ssh_plan(
            "ssh",
            &ssh_def(),
            Some(Path::new("/k/id")),
            Path::new("/s/known_hosts"),
            &cmd,
        )
        .unwrap();
        assert_eq!(p.program, "ssh");
        assert_eq!(
            p.args,
            [
                "-o",
                "BatchMode=yes",
                "-o",
                "StrictHostKeyChecking=accept-new",
                "-o",
                "UserKnownHostsFile=/s/known_hosts",
                "-o",
                "ConnectTimeout=15",
                "-i",
                "/k/id",
                "-o",
                "IdentitiesOnly=yes",
                "-p",
                "2222",
                "-T",
                "--",
                "build@box",
                "cd '/srv/x y' && exec 'echo' 'it'\\''s'",
            ]
        );
        assert_eq!(p.stdin_prefix, None);
    }

    #[test]
    fn ssh_env_goes_through_stdin_not_the_command_line() {
        let mut env = BTreeMap::new();
        env.insert("TOKEN".to_owned(), "s3cr3t".to_owned());
        let cmd = RemoteCmd {
            argv: vec!["env".into()],
            env,
            pty: Some(PtySize::default()),
            ..RemoteCmd::default()
        };
        let p = ssh_plan("/usr/bin/ssh", &ssh_def(), None, Path::new("/kh"), &cmd).unwrap();
        assert!(p.args.contains(&"-tt".to_owned()));
        assert!(!p.args.iter().any(|a| a.contains("s3cr3t")));
        let line = String::from_utf8(p.stdin_prefix.unwrap()).unwrap();
        let decoded = base64::engine::general_purpose::STANDARD
            .decode(line.trim())
            .unwrap();
        assert_eq!(
            String::from_utf8(decoded).unwrap(),
            "export TOKEN='s3cr3t'\n"
        );
        assert!(
            p.args
                .last()
                .unwrap()
                .starts_with("IFS= read -r __terrarium_env")
        );
        assert!(p.args.last().unwrap().ends_with("&& exec 'env'"));
    }

    #[test]
    fn ssh_shell_and_refusals() {
        let cmd = RemoteCmd {
            shell: true,
            pty: Some(PtySize::default()),
            ..RemoteCmd::default()
        };
        let p = ssh_plan("ssh", &ssh_def(), None, Path::new("/kh"), &cmd).unwrap();
        assert_eq!(p.args.last().unwrap(), "exec \"${SHELL:-/bin/sh}\" -l");
        let mut bad = ssh_def();
        bad.address = Some("-oProxyCommand=x".into());
        assert!(ssh_plan("ssh", &bad, None, Path::new("/kh"), &cmd).is_err());
        let mut env = BTreeMap::new();
        env.insert("A B".into(), "1".into());
        let c = RemoteCmd {
            argv: vec!["x".into()],
            env,
            ..RemoteCmd::default()
        };
        assert!(ssh_plan("ssh", &ssh_def(), None, Path::new("/kh"), &c).is_err());
        let c = RemoteCmd {
            argv: vec!["x".into()],
            cwd: Some("rel".into()),
            ..RemoteCmd::default()
        };
        assert!(ssh_plan("ssh", &ssh_def(), None, Path::new("/kh"), &c).is_err());
    }

    /// TER-71 review round 2, F1: the git steps on a target get the repo
    /// host keys (with GitHub's published keys), not the target host keys.
    #[test]
    fn git_steps_get_the_repo_host_keys() {
        let state = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(state.path().join("ssh")).unwrap();
        std::fs::write(
            known_hosts(state.path()).unwrap(),
            "target-box ssh-ed25519 AAAAtargetonly\n",
        )
        .unwrap();
        let text = repo_known_hosts(state.path()).unwrap();
        // Each published key, by type and key: the file may hold it on a
        // line with a hashed host name (copied from ~/.ssh/known_hosts).
        fn key(l: &str) -> Vec<String> {
            l.split_whitespace()
                .skip(1)
                .take(2)
                .map(str::to_owned)
                .collect()
        }
        for k in crate::git::PUBLISHED_HOST_KEYS {
            assert!(
                text.lines().any(|l| key(l) == key(k)),
                "{k} missing in {text}"
            );
        }
        assert!(!text.contains("AAAAtargetonly"), "{text}");
    }

    #[test]
    fn checkout_secret_is_env_only() {
        let s = Secret::Token {
            username: "x-access-token".into(),
            token: zeroize::Zeroizing::new("ghp_SECRET".into()),
        };
        let c = checkout_cmd("https://h/r.git", "/r", Some("main"), Some(1), Some(&s)).unwrap();
        assert!(!c.argv.iter().any(|a| a.contains("ghp_SECRET")));
        assert_eq!(c.env["TERRARIUM_GIT_PASSWORD"], "ghp_SECRET");
        assert_eq!(&c.argv[4..], ["https://h/r.git", "/r", "main", "1"]);
        assert!(checkout_cmd("ext::sh -c x", "/r", None, None, None).is_err());
        assert!(checkout_cmd("https://h/r", "/r", Some("-x"), None, None).is_err());
        assert!(checkout_cmd("https://h/r", "r", None, None, None).is_err());
        assert!(checkout_cmd("https://h/r", "../r", None, None, None).is_err());
        let v = parse_checkout("noise\nTERRARIUM_CHECKOUT\taction=cloned\thead=abc\tbranch=main\tpath=/h/work/r\tnote=\n").unwrap();
        assert_eq!(
            v,
            json!({"action": "cloned", "head": "abc", "branch": "main", "path": "/h/work/r"})
        );
    }

    /// TER-577: the SSH key goes to the checkout script base64-encoded on
    /// one line, so a docker target keeps it in its env file and never in
    /// the env of the `docker` CLI. The script decodes it into the key file
    /// that ssh gets; the vmlab prepare keeps the plain key.
    #[test]
    fn checkout_sends_the_ssh_key_on_one_line() {
        use std::os::unix::fs::PermissionsExt;
        use std::process::Command;
        let key = "-----BEGIN OPENSSH PRIVATE KEY-----\nb3BlbnNzaC1rZXktdjE=\n-----END OPENSSH PRIVATE KEY-----\n";
        let s = Secret::SshKey(zeroize::Zeroizing::new(key.into()));
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path();
        let dest = root.join("dest");
        let c = checkout_cmd(
            "ssh://git@h.invalid/r.git",
            dest.to_str().unwrap(),
            None,
            None,
            Some(&s),
        )
        .unwrap();
        let v = c.env["TERRARIUM_GIT_SSH_KEY"].clone();
        assert!(!v.contains(['\n', '\r']));
        assert!(
            base64::engine::general_purpose::STANDARD
                .decode(&v)
                .unwrap()
                == key.as_bytes()
        );
        assert!(checkout_env(Some(&s))["TERRARIUM_GIT_SSH_KEY"] == key.trim_end());
        // A fake ssh first in PATH copies the key file that it gets.
        let bin = root.join("bin");
        std::fs::create_dir(&bin).unwrap();
        let got = root.join("got");
        let key_path = root.join("key-path");
        let mode = root.join("mode");
        std::fs::write(
            bin.join("ssh"),
            format!(
                "#!/bin/sh\nwhile [ $# -gt 0 ]; do if [ \"$1\" = -i ]; then cp \"$2\" {}; printf '%s' \"$2\" > {}; stat -c %a \"$2\" > {}; fi; shift; done\nexit 1\n",
                got.display(),
                key_path.display(),
                mode.display()
            ),
        )
        .unwrap();
        std::fs::set_permissions(bin.join("ssh"), std::fs::Permissions::from_mode(0o755)).unwrap();
        let path = format!(
            "{}:{}",
            bin.display(),
            std::env::var("PATH").unwrap_or_default()
        );
        let run = |env: &BTreeMap<String, String>| {
            Command::new(&c.argv[0])
                .args(&c.argv[1..])
                .env_clear()
                .env("PATH", &path)
                .env("HOME", root)
                .envs(env)
                .output()
                .unwrap()
        };
        let out = run(&c.env);
        assert!(!out.status.success());
        assert!(std::fs::read_to_string(&got).unwrap_or_default() == key);
        assert_eq!(std::fs::read_to_string(&mode).unwrap().trim(), "600");
        let removed = std::fs::read_to_string(&key_path).unwrap();
        assert!(!Path::new(&removed).exists());
        // A value that is not base64 stops the script before git runs.
        let mut bad = c.env.clone();
        bad.insert("TERRARIUM_GIT_SSH_KEY".into(), "not base64!".into());
        let out = run(&bad);
        assert!(!out.status.success());
        assert!(String::from_utf8_lossy(&out.stderr).contains("cannot decode the SSH key"));
        assert!(!String::from_utf8_lossy(&out.stdout).contains("cannot decode the SSH key"));
    }

    /// S9 (secret manager review, 2026-10-29): the agent can write the
    /// repo config on the target. A planted `core.fsmonitor` or hook must
    /// not run while the token is in the env of the checkout script.
    #[test]
    fn target_checkout_chains_existing_hook_on_repeated_updates() {
        use std::os::unix::fs::PermissionsExt;
        use std::process::Command;
        let tmp = tempfile::tempdir().unwrap();
        let origin = tmp.path().join("origin");
        let dest = tmp.path().join("dest");
        let git = |dir: &std::path::Path, args: &[&str]| {
            let out = Command::new("git")
                .current_dir(dir)
                .args(args)
                .env("GIT_AUTHOR_NAME", "t")
                .env("GIT_AUTHOR_EMAIL", "t@t")
                .env("GIT_COMMITTER_NAME", "t")
                .env("GIT_COMMITTER_EMAIL", "t@t")
                .env_remove("TERRARIUM_GIT_TRAILERS")
                .env_remove("TERRARIUM_GIT_TRAILERS_FILE")
                .output()
                .unwrap();
            assert!(out.status.success(), "{out:?}");
            String::from_utf8(out.stdout).unwrap()
        };
        std::fs::create_dir(&origin).unwrap();
        git(&origin, &["init", "-q"]);
        git(&origin, &["commit", "--allow-empty", "-m", "one"]);
        git(
            tmp.path(),
            &["clone", origin.to_str().unwrap(), dest.to_str().unwrap()],
        );
        let hook = dest.join(".git/hooks/prepare-commit-msg");
        std::fs::write(
            &hook,
            "#!/bin/sh\nprintf '\\nRepo-Note: yes\\n' >> \"$1\"\n",
        )
        .unwrap();
        std::fs::set_permissions(&hook, std::fs::Permissions::from_mode(0o755)).unwrap();
        let p = crate::git_identity::Provenance {
            repo: None,
            warn_only: false,
            identity: crate::git_identity::Identity::from_env(),
            trailers: "Terrarium-Agent: test/worker\n".into(),
        };
        for _ in 0..2 {
            let c = checkout_cmd(
                origin.to_str().unwrap(),
                dest.to_str().unwrap(),
                None,
                None,
                None,
            )
            .unwrap();
            let out = Command::new(&c.argv[0])
                .args(&c.argv[1..])
                .envs(c.env)
                .env("TERRARIUM_COMMIT_HOOK", p.hook())
                .env("TERRARIUM_COMMIT_NAME", &p.identity.name)
                .env("TERRARIUM_COMMIT_EMAIL", &p.identity.email)
                .output()
                .unwrap();
            assert!(out.status.success(), "{out:?}");
        }
        git(&dest, &["commit", "--allow-empty", "-m", "two"]);
        let message = git(&dest, &["show", "-s", "--format=%B"]);
        assert_eq!(message.matches("Repo-Note: yes").count(), 1);
        assert!(message.contains("Terrarium-Agent: test/worker"));
    }

    #[test]
    fn checkout_script_ignores_a_planted_repo_config() {
        use std::os::unix::fs::PermissionsExt;
        use std::process::Command;
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path();
        let git = |args: &[&str]| {
            let out = Command::new("git")
                .args(["-c", "commit.gpgsign=false", "-c", "user.name=t"])
                .args(["-c", "user.email=t@t.invalid"])
                .args(args)
                .env("GIT_CONFIG_GLOBAL", "/dev/null")
                .env("GIT_CONFIG_NOSYSTEM", "1")
                .output()
                .unwrap();
            assert!(out.status.success(), "{args:?}: {out:?}");
        };
        let origin = root.join("origin");
        let o = origin.to_str().unwrap();
        git(&["init", "-q", "-b", "main", o]);
        git(&["-C", o, "commit", "-q", "--allow-empty", "-m", "one"]);
        let dest = root.join("dest");
        let secret = Secret::Token {
            username: "x-access-token".into(),
            token: zeroize::Zeroizing::new("ghp_PLANTEDtoken".into()),
        };
        let run = || {
            let url = format!("file://{o}");
            let c = checkout_cmd(&url, dest.to_str().unwrap(), None, None, Some(&secret)).unwrap();
            let out = Command::new(&c.argv[0])
                .args(&c.argv[1..])
                .env_clear()
                .env("PATH", std::env::var("PATH").unwrap_or_default())
                .env("HOME", root)
                .envs(&c.env)
                .output()
                .unwrap();
            assert!(out.status.success(), "{out:?}");
            parse_checkout(&String::from_utf8_lossy(&out.stdout)).unwrap()
        };
        assert_eq!(run()["action"], "cloned");
        // The agent plants an fsmonitor and a hooks dir that write the env.
        let leak = root.join("leak");
        let spy = root.join("spy.sh");
        std::fs::write(&spy, format!("#!/bin/sh\nenv >> {}\n", leak.display())).unwrap();
        std::fs::set_permissions(&spy, std::fs::Permissions::from_mode(0o755)).unwrap();
        let hooks = root.join("hooks");
        std::fs::create_dir(&hooks).unwrap();
        for h in ["reference-transaction", "post-merge", "post-checkout"] {
            std::fs::copy(&spy, hooks.join(h)).unwrap();
        }
        let d = dest.to_str().unwrap();
        git(&["-C", d, "config", "core.fsmonitor", spy.to_str().unwrap()]);
        git(&["-C", d, "config", "core.hooksPath", hooks.to_str().unwrap()]);
        git(&["-C", o, "commit", "-q", "--allow-empty", "-m", "two"]);
        let v = run();
        let leaked = std::fs::read_to_string(&leak).unwrap_or_default();
        assert!(!leaked.contains("ghp_PLANTED"), "the token leaked: {v}");
        assert!(!leak.exists(), "a planted program ran: {v}");
        assert_eq!(v["action"], "not_updated", "{v}");
        assert!(
            v["note"]
                .as_str()
                .unwrap_or_default()
                .contains("core.fsmonitor"),
            "{v}"
        );
        // Without the planted keys the update goes on.
        git(&["-C", d, "config", "--unset", "core.fsmonitor"]);
        git(&["-C", d, "config", "--unset", "core.hooksPath"]);
        assert_eq!(run()["action"], "updated");
        assert!(!leak.exists());
    }

    #[test]
    fn frames_for_each_kind() {
        let mut d: TargetDef =
            toml::from_str("kind = \"docker\"\nhost = \"h\"\nimage = \"alpine\"\n").unwrap();
        d.name = "box".into();
        let cmd = RemoteCmd {
            argv: vec!["ls".into(), "-l".into()],
            cwd: Some("/w".into()),
            ..RemoteCmd::default()
        };
        let f = host_frame(&d, None, &cmd, None).unwrap()(5);
        let DaemonFrame::DockerExec(x) = f else {
            panic!()
        };
        assert_eq!(
            (x.container.as_str(), x.cmd.as_str(), x.args.as_slice()),
            ("terrarium-box", "ls", &["-l".to_owned()][..])
        );
        let mut v: TargetDef = toml::from_str(
            "kind = \"vmlab\"\nhost = \"h\"\nlab = \"/labs/w\"\nmachine = \"win\"\n",
        )
        .unwrap();
        v.name = "w".into();
        assert!(
            host_frame(&v, None, &cmd, None).is_err(),
            "vmlab takes no cwd"
        );
        let shell = RemoteCmd {
            shell: true,
            ..RemoteCmd::default()
        };
        assert!(matches!(
            host_frame(&v, None, &shell, None).unwrap()(1),
            DaemonFrame::VmlabShell(_)
        ));
        let mut h: TargetDef = toml::from_str("kind = \"host\"\nhost = \"h\"\n").unwrap();
        h.name = "h".into();
        let DaemonFrame::Exec(e) = host_frame(&h, Some("/bin/bash"), &shell, None).unwrap()(2)
        else {
            panic!()
        };
        assert_eq!(
            (e.cmd.as_str(), e.args.as_slice()),
            ("/bin/bash", &["-l".to_owned()][..])
        );
        assert_eq!(e.pty, Some(PtySize::default()));
    }

    #[test]
    fn exec_limits_name_the_holder_and_need_the_capability() {
        let mut h: TargetDef =
            toml::from_str("kind = \"host\"\nhost = \"h\"\n[limits]\ncpu = 2\nmemory = \"1G\"\n")
                .unwrap();
        h.name = "box".into();
        let mut caps = HostCapabilities {
            limits: Some(terrarium_core::limits::LimitsCapability {
                job_objects: false,
                systemd: true,
                controllers: vec!["cpu".into(), "memory".into()],
                reason: None,
                docker: false,
            }),
            ..HostCapabilities::default()
        };
        let l = exec_limits(&h, Some(&caps), Some("terra/w-1"))
            .unwrap()
            .unwrap();
        assert_eq!((l.target.as_str(), l.holder.as_str()), ("box", "terra/w-1"));
        assert_eq!(l.limits.cpu_millis, Some(2000));
        let op = exec_limits(&h, Some(&caps), None).unwrap().unwrap();
        assert_eq!(op.holder, "operator");
        let cmd = RemoteCmd {
            argv: vec!["true".into()],
            ..RemoteCmd::default()
        };
        let DaemonFrame::Exec(e) = host_frame(&h, None, &cmd, Some(l)).unwrap()(3) else {
            panic!()
        };
        assert_eq!(e.limits.map(|l| l.holder), Some("terra/w-1".to_owned()));
        // No capability (an older host agent), or a missing controller.
        caps.limits = None;
        let err = exec_limits(&h, Some(&caps), None).unwrap_err();
        assert_eq!(err.code, Code::Conflict);
        caps.limits = Some(terrarium_core::limits::LimitsCapability {
            systemd: true,
            controllers: vec!["memory".into()],
            ..Default::default()
        });
        let err = exec_limits(&h, Some(&caps), None).unwrap_err();
        assert!(err.message.contains("no cpu controller"), "{}", err.message);
        // An offline host agent: no session, and no frame without limits.
        assert!(exec_limits(&h, None, None).is_err());
        // Other kinds and targets with no limits get none.
        h.limits = None;
        assert!(exec_limits(&h, None, None).unwrap().is_none());
    }

    #[tokio::test]
    async fn session_buffer_acks_by_since_and_applies_backpressure() {
        let (s, _ctl) = ExecSession::new("t", None, "exec", 8);
        s.push(Stream::Stdout, b"hello".to_vec()).await;
        s.push(Stream::Stderr, b"world".to_vec()).await;
        // Over the limit: the next push waits until a read acks.
        let s2 = s.clone();
        let blocked = tokio::spawn(async move { s2.push(Stream::Stdout, b"!".to_vec()).await });
        tokio::time::sleep(Duration::from_millis(30)).await;
        assert!(!blocked.is_finished());
        let o = s.read(0, Duration::ZERO, 100).await.unwrap();
        assert_eq!(o.next, 10);
        assert_eq!(o.chunks.len(), 2);
        let o = s.read(7, Duration::from_secs(1), 100).await.unwrap();
        blocked.await.unwrap();
        assert_eq!(o.since, 7);
        assert!(
            s.read(3, Duration::ZERO, 10).await.is_err(),
            "acked bytes are gone"
        );
        s.finish(End::Exit {
            code: Some(0),
            signal: None,
            reason: "exited".into(),
        });
        let o = s.read(7, Duration::ZERO, 100).await.unwrap();
        assert!(o.done);
        assert_eq!(o.next, 11);
    }
}
