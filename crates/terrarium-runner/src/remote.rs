//! [`RemoteRunner`]: the daemon side of a runner process. It speaks the
//! control protocol (`docs/runner.md`, "The control socket") and offers the
//! same surface as [`crate::Runner`], through [`RunnerHandle`].

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex, MutexGuard, OnceLock};
use std::time::{Duration, Instant};

use serde::de::DeserializeOwned;
use serde_json::{Value, json};
use terrarium_core::{AgentId, AgentStatus, Answer, EventRecord};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::net::UnixStream;
use tokio::net::unix::{OwnedReadHalf, OwnedWriteHalf};
use tokio::sync::{broadcast, mpsc, oneshot, watch};
use tokio::task::AbortHandle;

use crate::attach::{AttachEvent, AttachStream};
use crate::config::{control_socket_path, create_private_dir, private_dir};
use crate::control::{
    AnswerKeysParams, AnswerParams, CancelParams, ClientFrame, EventsParams, Hello, HelloParams,
    PROTOCOL_VERSION, ResizeParams, ServerFrame, StoppedByParams, TypeTurnParams, WireSnapshot,
    WriteParams, codes, decode, encode,
};
use crate::error::{DeliveryError, Result, RunnerError};
use crate::handle::RunnerHandle;
use crate::process::StoppedBy;
use crate::process::live_runner_pid;
use crate::protocol::{BoxFuture, NoHandler, RequestHandler, RpcError};
use crate::runner::{Delivery, ExitStatus, ScreenSnapshot};
use crate::spec::RunnerSpec;

/// The capacity of the event and output channels of a `RemoteRunner`. A
/// replay with `events_since` goes through the event channel, so it is
/// larger than the one of an in-process runner.
const EVENTS_CAPACITY: usize = 8192;
const OUTPUT_CAPACITY: usize = 1024;

/// The signal of the exit status when the connection closed and the runner
/// process left no `runner-exit.json`.
pub const LOST_SIGNAL: &str = "RUNNER_LOST";

fn lock<T>(m: &Mutex<T>) -> MutexGuard<'_, T> {
    m.lock().unwrap_or_else(std::sync::PoisonError::into_inner)
}

/// How to connect to a runner process.
#[derive(Clone)]
pub struct ConnectOptions {
    /// Answers the agent RPC requests that the runner forwards (method
    /// `http` and the others). The daemon passes its `AgentRpc`.
    pub handler: Arc<dyn RequestHandler>,
    /// Replay the events after this seq on the first
    /// [`RunnerHandle::subscribe_events`] receiver. `None`: from the seq
    /// that `hello` reports, so only new events.
    pub events_since: Option<u64>,
    /// A name for the runner log.
    pub client: String,
    pub connect_timeout: Duration,
    /// The capacity of the local output channel of each attach. A reader
    /// that falls this many chunks behind gets a resync
    /// (`AttachEvent::Resynced`). The default is 1024.
    pub attach_capacity: usize,
}

impl Default for ConnectOptions {
    fn default() -> Self {
        Self {
            handler: Arc::new(NoHandler),
            events_since: None,
            client: "terrariumd".into(),
            connect_timeout: Duration::from_secs(5),
            attach_capacity: OUTPUT_CAPACITY,
        }
    }
}

/// How to start a runner process.
#[derive(Clone)]
pub struct SpawnOptions {
    pub agent: AgentId,
    pub state_dir: PathBuf,
    pub runtime_dir: PathBuf,
    pub spec: RunnerSpec,
    /// Written to `<agent dir>/runner-argv.json` and passed as
    /// `--sandbox-argv-file`: it replaces the program and the arguments of
    /// `spec.launch`.
    pub sandbox_argv: Option<Vec<String>>,
    /// How long to wait for the control socket.
    pub start_timeout: Duration,
    pub connect: ConnectOptions,
}

impl SpawnOptions {
    pub fn new(
        agent: AgentId,
        state_dir: impl Into<PathBuf>,
        runtime_dir: impl Into<PathBuf>,
        spec: RunnerSpec,
    ) -> Self {
        Self {
            agent,
            state_dir: state_dir.into(),
            runtime_dir: runtime_dir.into(),
            spec,
            sandbox_argv: None,
            start_timeout: Duration::from_secs(10),
            connect: ConnectOptions::default(),
        }
    }
}

type DeliveryTx = oneshot::Sender<Result<(), DeliveryError>>;

enum Slot {
    Reply(oneshot::Sender<Result<Value, RpcError>>),
    Stream(mpsc::UnboundedSender<StreamMsg>),
    Turn(DeliveryTx),
}

enum StreamMsg {
    Item(Value),
    End(Option<RpcError>),
}

enum Outgoing {
    Frame(ClientFrame),
    /// Shut the write half: the runner process may exit.
    Close,
}

struct Calls {
    closed: bool,
    slots: HashMap<u64, Slot>,
}

/// What the reader task and the handle share.
struct Shared {
    out: mpsc::UnboundedSender<Outgoing>,
    calls: Mutex<Calls>,
    deliveries: Mutex<HashMap<u64, DeliveryTx>>,
    next_id: AtomicU64,
    status_tx: watch::Sender<AgentStatus>,
    events_tx: broadcast::Sender<EventRecord>,
    events_done: watch::Sender<bool>,
    exit_tx: watch::Sender<Option<ExitStatus>>,
    connected: AtomicBool,
    last_seq: AtomicU64,
    handler: Arc<dyn RequestHandler>,
    /// `runner-exit.json`, read when the connection closes with no exit.
    exit_file: OnceLock<PathBuf>,
    attach_capacity: usize,
}

fn control_err(e: RpcError) -> RunnerError {
    if e.code == codes::EXITED {
        RunnerError::Exited
    } else {
        RunnerError::Control(e)
    }
}

fn decode_value<T: DeserializeOwned>(v: Value) -> Result<T> {
    serde_json::from_value(v).map_err(|e| RunnerError::Other(format!("bad runner answer: {e}")))
}

impl Shared {
    fn next_id(&self) -> u64 {
        self.next_id.fetch_add(1, Ordering::Relaxed)
    }

    /// Register a slot. False: the connection is closed.
    fn register(&self, id: u64, slot: Slot) -> std::result::Result<(), Slot> {
        let mut calls = lock(&self.calls);
        if calls.closed {
            return Err(slot);
        }
        calls.slots.insert(id, slot);
        Ok(())
    }

    fn take(&self, id: u64) -> Option<Slot> {
        lock(&self.calls).slots.remove(&id)
    }

    fn send_call(&self, id: u64, method: &str, params: Value) -> bool {
        self.out
            .send(Outgoing::Frame(ClientFrame::Call {
                id,
                method: method.to_owned(),
                params,
            }))
            .is_ok()
    }

    async fn call(&self, method: &str, params: Value) -> Result<Value> {
        let id = self.next_id();
        let (tx, rx) = oneshot::channel();
        if self.register(id, Slot::Reply(tx)).is_err() {
            return Err(RunnerError::Disconnected);
        }
        if !self.send_call(id, method, params) {
            self.take(id);
            return Err(RunnerError::Disconnected);
        }
        match rx.await {
            Ok(Ok(v)) => Ok(v),
            Ok(Err(e)) => Err(control_err(e)),
            Err(_) => Err(RunnerError::Disconnected),
        }
    }

    fn open_stream(
        &self,
        method: &str,
        params: Value,
    ) -> Result<(u64, mpsc::UnboundedReceiver<StreamMsg>)> {
        let id = self.next_id();
        let (tx, rx) = mpsc::unbounded_channel();
        if self.register(id, Slot::Stream(tx)).is_err() || !self.send_call(id, method, params) {
            self.take(id);
            return Err(RunnerError::Disconnected);
        }
        Ok((id, rx))
    }

    fn cancel(&self, call: u64) {
        let id = self.next_id();
        let params = serde_json::to_value(CancelParams { call }).unwrap_or(Value::Null);
        // The reply goes to no slot, so the reader drops it.
        let _ = self.send_call(id, "cancel", params);
    }

    fn on_frame(self: &Arc<Self>, frame: ServerFrame) {
        match frame {
            ServerFrame::Reply { id, result, error } => {
                let outcome = match (result, error) {
                    (_, Some(e)) => Err(e),
                    (Some(v), None) => Ok(v),
                    (None, None) => Ok(Value::Null),
                };
                match self.take(id) {
                    Some(Slot::Reply(tx)) => {
                        let _ = tx.send(outcome);
                    }
                    Some(Slot::Turn(tx)) => match outcome {
                        Ok(v) => match v.get("delivery").and_then(Value::as_u64) {
                            Some(d) => {
                                lock(&self.deliveries).insert(d, tx);
                            }
                            None => {
                                let _ = tx.send(Err(DeliveryError::Dropped));
                            }
                        },
                        Err(e) => {
                            let err = if e.code == codes::EXITED {
                                DeliveryError::Exited
                            } else {
                                DeliveryError::Dropped
                            };
                            let _ = tx.send(Err(err));
                        }
                    },
                    // A stream call that failed at once.
                    Some(Slot::Stream(tx)) => {
                        let _ = tx.send(StreamMsg::End(outcome.err()));
                    }
                    None => {}
                }
            }
            ServerFrame::Stream { id, item } => {
                let gone = match lock(&self.calls).slots.get(&id) {
                    Some(Slot::Stream(tx)) => tx.send(StreamMsg::Item(item)).is_err(),
                    _ => false,
                };
                if gone {
                    self.take(id);
                    self.cancel(id);
                }
            }
            ServerFrame::End { id, error } => {
                if let Some(Slot::Stream(tx)) = self.take(id) {
                    let _ = tx.send(StreamMsg::End(error));
                }
            }
            ServerFrame::Delivery {
                delivery,
                ok,
                error,
            } => {
                if let Some(tx) = lock(&self.deliveries).remove(&delivery) {
                    let outcome = if ok {
                        Ok(())
                    } else {
                        Err(error
                            .map(DeliveryError::from)
                            .unwrap_or(DeliveryError::Dropped))
                    };
                    let _ = tx.send(outcome);
                }
            }
            ServerFrame::RpcRequest {
                id,
                method,
                params,
                context,
            } => {
                let me = self.clone();
                tokio::spawn(async move {
                    let outcome = me.handler.handle(&context, &method, params).await;
                    let (result, error) = match outcome {
                        Ok(v) => (Some(v), None),
                        Err(e) => (None, Some(e)),
                    };
                    let _ = me.out.send(Outgoing::Frame(ClientFrame::RpcResponse {
                        id,
                        result,
                        error,
                    }));
                });
            }
        }
    }

    /// The connection is gone: fail every open call.
    fn on_close(&self) {
        self.connected.store(false, Ordering::SeqCst);
        let slots: Vec<Slot> = {
            let mut calls = lock(&self.calls);
            calls.closed = true;
            calls.slots.drain().map(|(_, s)| s).collect()
        };
        for slot in slots {
            match slot {
                Slot::Reply(tx) => {
                    let _ = tx.send(Err(RpcError::new("disconnected", "disconnected")));
                }
                Slot::Stream(tx) => {
                    let _ = tx.send(StreamMsg::End(Some(RpcError::new(
                        "disconnected",
                        "disconnected",
                    ))));
                }
                Slot::Turn(tx) => {
                    let _ = tx.send(Err(DeliveryError::Dropped));
                }
            }
        }
        for (_, tx) in lock(&self.deliveries).drain() {
            let _ = tx.send(Err(DeliveryError::Dropped));
        }
        if self.exit_tx.borrow().is_none() {
            let status = self
                .exit_file
                .get()
                .and_then(|p| crate::safefs::read_to_string(p).ok())
                .and_then(|t| serde_json::from_str::<ExitStatus>(&t).ok())
                .unwrap_or(ExitStatus {
                    code: 1,
                    signal: Some(LOST_SIGNAL.into()),
                });
            self.exit_tx.send_replace(Some(status));
        }
        self.events_done.send_replace(true);
    }
}

async fn write_loop(mut write: OwnedWriteHalf, mut rx: mpsc::UnboundedReceiver<Outgoing>) {
    while let Some(o) = rx.recv().await {
        match o {
            Outgoing::Frame(frame) => {
                let Ok(mut line) = serde_json::to_vec(&frame) else {
                    continue;
                };
                line.push(b'\n');
                if write.write_all(&line).await.is_err() {
                    break;
                }
            }
            Outgoing::Close => {
                let _ = write.shutdown().await;
                break;
            }
        }
    }
}

async fn read_loop(shared: Arc<Shared>, read: OwnedReadHalf) {
    let mut lines = BufReader::new(read).lines();
    loop {
        match lines.next_line().await {
            Ok(Some(line)) => {
                if line.trim().is_empty() {
                    continue;
                }
                match serde_json::from_str::<ServerFrame>(&line) {
                    Ok(frame) => shared.on_frame(frame),
                    Err(e) => tracing::warn!("bad frame from the runner: {e}"),
                }
            }
            Ok(None) => break,
            Err(e) => {
                tracing::debug!("runner control connection: {e}");
                break;
            }
        }
    }
    shared.on_close();
}

struct Inner {
    shared: Arc<Shared>,
    hello: Hello,
    control_path: PathBuf,
    first_events: Mutex<Option<broadcast::Receiver<EventRecord>>>,
    tasks: Mutex<Vec<AbortHandle>>,
}

impl Drop for Inner {
    fn drop(&mut self) {
        for t in lock(&self.tasks).drain(..) {
            t.abort();
        }
    }
}

/// A connection to a runner process. Cloning it clones a handle to the
/// same connection. Dropping the last handle closes the connection; the
/// runner process and the harness keep running.
#[derive(Clone)]
pub struct RemoteRunner {
    inner: Arc<Inner>,
}

impl RemoteRunner {
    /// Connect to the live runner process of `agent`, with no agent RPC
    /// handler. See [`RemoteRunner::connect_with`].
    pub async fn connect(runtime_dir: &Path, agent: AgentId) -> Result<Self> {
        Self::connect_with(runtime_dir, agent, ConnectOptions::default()).await
    }

    /// Connect to the live runner process of `agent` and send `hello`. The
    /// connection becomes the daemon connection: an older one closes.
    pub async fn connect_with(
        runtime_dir: &Path,
        agent: AgentId,
        opts: ConnectOptions,
    ) -> Result<Self> {
        let path = control_socket_path(runtime_dir, agent);
        let stream = tokio::time::timeout(opts.connect_timeout, UnixStream::connect(&path))
            .await
            .map_err(|_| RunnerError::Other(format!("connect {}: timed out", path.display())))?
            .map_err(|e| RunnerError::io("connect", &path, e))?;
        let (read, write) = stream.into_split();
        let (out, out_rx) = mpsc::unbounded_channel();
        let (events_tx, _) = broadcast::channel(EVENTS_CAPACITY);
        let shared = Arc::new(Shared {
            out,
            calls: Mutex::new(Calls {
                closed: false,
                slots: HashMap::new(),
            }),
            deliveries: Mutex::new(HashMap::new()),
            next_id: AtomicU64::new(1),
            status_tx: watch::Sender::new(AgentStatus::Starting),
            events_tx,
            events_done: watch::Sender::new(false),
            exit_tx: watch::Sender::new(None),
            connected: AtomicBool::new(true),
            last_seq: AtomicU64::new(0),
            handler: opts.handler.clone(),
            exit_file: OnceLock::new(),
            attach_capacity: opts.attach_capacity.max(1),
        });
        let mut tasks = vec![
            tokio::spawn(write_loop(write, out_rx)).abort_handle(),
            tokio::spawn(read_loop(shared.clone(), read)).abort_handle(),
        ];
        let abort = |tasks: &[AbortHandle]| tasks.iter().for_each(AbortHandle::abort);

        let hello = shared
            .call(
                "hello",
                serde_json::to_value(HelloParams {
                    client: opts.client.clone(),
                    protocol: PROTOCOL_VERSION,
                })
                .unwrap_or(Value::Null),
            )
            .await
            .and_then(decode_value::<Hello>);
        let hello = match hello {
            Ok(h) if h.agent == agent => h,
            Ok(h) => {
                abort(&tasks);
                return Err(RunnerError::Other(format!(
                    "{} belongs to agent {}, not {agent}",
                    path.display(),
                    h.agent
                )));
            }
            Err(e) => {
                abort(&tasks);
                return Err(e);
            }
        };
        if let Some(dir) = hello.events_path.parent() {
            let _ = shared.exit_file.set(dir.join("runner-exit.json"));
        }
        shared.status_tx.send_replace(hello.status);
        shared.last_seq.store(hello.seq, Ordering::SeqCst);
        if let Some(exit) = &hello.exit {
            shared.exit_tx.send_replace(Some(exit.clone()));
        }

        let first = shared.events_tx.subscribe();
        let since = opts.events_since.unwrap_or(hello.seq);
        let (_, mut events) = match shared.open_stream(
            "events",
            serde_json::to_value(EventsParams { since: Some(since) }).unwrap_or(Value::Null),
        ) {
            Ok(s) => s,
            Err(e) => {
                abort(&tasks);
                return Err(e);
            }
        };
        let s = shared.clone();
        tasks.push(
            tokio::spawn(async move {
                while let Some(StreamMsg::Item(v)) = events.recv().await {
                    match serde_json::from_value::<EventRecord>(v) {
                        Ok(rec) => {
                            if let Some(seq) = rec.seq {
                                s.last_seq.fetch_max(seq, Ordering::SeqCst);
                            }
                            let _ = s.events_tx.send(rec);
                        }
                        Err(e) => tracing::warn!("bad event from the runner: {e}"),
                    }
                }
                s.events_done.send_replace(true);
            })
            .abort_handle(),
        );

        let (_, mut status) = match shared.open_stream("watch_status", json!({})) {
            Ok(s) => s,
            Err(e) => {
                abort(&tasks);
                return Err(e);
            }
        };
        let s = shared.clone();
        tasks.push(
            tokio::spawn(async move {
                while let Some(StreamMsg::Item(v)) = status.recv().await {
                    if let Some(st) = v
                        .get("status")
                        .cloned()
                        .and_then(|v| serde_json::from_value::<AgentStatus>(v).ok())
                    {
                        s.status_tx.send_replace(st);
                    }
                }
            })
            .abort_handle(),
        );

        // `wait`: the exit status. After it and the last event, close the
        // connection, so the runner process need not linger.
        let s = shared.clone();
        tasks.push(
            tokio::spawn(async move {
                if let Ok(v) = s.call("wait", json!({})).await
                    && let Ok(status) = serde_json::from_value::<ExitStatus>(v)
                {
                    s.exit_tx.send_replace(Some(status));
                    let mut done = s.events_done.subscribe();
                    let _ = done.wait_for(|d| *d).await;
                    let _ = s.out.send(Outgoing::Close);
                }
            })
            .abort_handle(),
        );

        Ok(Self {
            inner: Arc::new(Inner {
                shared,
                hello,
                control_path: path,
                first_events: Mutex::new(Some(first)),
                tasks: Mutex::new(tasks),
            }),
        })
    }

    /// Start `bin` (the `terrarium-runner` binary) detached, wait for its
    /// control socket, and connect. The process runs in its own session
    /// (it calls `setsid`), with stdin from `/dev/null` and stdout and
    /// stderr appended to `<state>/agents/<ulid>/private/runner.log`. The
    /// spec goes to `<state>/agents/<ulid>/private/runner.json` (mode
    /// `0600`; the directory is `0700` and the sandbox never binds it). A thread reaps
    /// the process when it exits while this daemon runs.
    pub async fn spawn_process(bin: &Path, opts: SpawnOptions) -> Result<Self> {
        let agent = opts.agent;
        if let Some(pid) = live_runner_pid(&opts.runtime_dir, agent) {
            return Err(RunnerError::AlreadyRunning {
                agent: agent.to_string(),
                pid: pid.to_string(),
            });
        }
        let dir = private_dir(&opts.state_dir, agent);
        create_private_dir(&dir)?;
        let spec_path = dir.join("runner.json");
        opts.spec.write(&spec_path)?;
        let log_path = dir.join("runner.log");
        // No symlink follow for the private files (`docs/security-review.md`, F11).
        let log = crate::safefs::open_append(&log_path)?;
        let err_log = log
            .try_clone()
            .map_err(|e| RunnerError::io("open", &log_path, e))?;

        let mut cmd = std::process::Command::new(bin);
        cmd.arg("--agent")
            .arg(agent.to_string())
            .arg("--state-dir")
            .arg(&opts.state_dir)
            .arg("--runtime-dir")
            .arg(&opts.runtime_dir)
            .arg("--spec")
            .arg(&spec_path);
        if let Some(argv) = &opts.sandbox_argv {
            let argv_path = dir.join("runner-argv.json");
            let bytes = serde_json::to_vec(argv).map_err(|e| RunnerError::Other(e.to_string()))?;
            crate::safefs::write_private(&argv_path, &bytes)?;
            cmd.arg("--sandbox-argv-file").arg(&argv_path);
        }
        cmd.stdin(std::process::Stdio::null())
            .stdout(log)
            .stderr(err_log);
        let mut child = cmd.spawn().map_err(|e| RunnerError::io("spawn", bin, e))?;
        let (died_tx, died_rx) = watch::channel(None::<String>);
        std::thread::Builder::new()
            .name("terrarium-runner-reap".into())
            .spawn(move || {
                let how = match child.wait() {
                    Ok(s) => s.to_string(),
                    Err(e) => format!("wait failed: {e}"),
                };
                died_tx.send_replace(Some(how));
            })
            .map_err(|e| RunnerError::Other(format!("cannot start the reaper thread: {e}")))?;

        let control = control_socket_path(&opts.runtime_dir, agent);
        let deadline = Instant::now() + opts.start_timeout;
        loop {
            if let Some(how) = died_rx.borrow().clone() {
                return Err(RunnerError::Other(format!(
                    "the runner process ended before it was ready ({how}); see {}",
                    log_path.display()
                )));
            }
            if control.exists() {
                match Self::connect_with(&opts.runtime_dir, agent, opts.connect.clone()).await {
                    Ok(r) => return Ok(r),
                    Err(e) if Instant::now() >= deadline => return Err(e),
                    Err(_) => {}
                }
            }
            if Instant::now() >= deadline {
                return Err(RunnerError::Other(format!(
                    "the runner process did not open {} in {} ms; see {}",
                    control.display(),
                    opts.start_timeout.as_millis(),
                    log_path.display()
                )));
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    }

    /// What the runner reported at connect time.
    pub fn hello(&self) -> &Hello {
        &self.inner.hello
    }

    /// `<run>/runners/<ulid>.sock`.
    pub fn control_path(&self) -> &Path {
        &self.inner.control_path
    }

    /// False once the control connection is closed.
    pub fn is_connected(&self) -> bool {
        self.inner.shared.connected.load(Ordering::SeqCst)
    }

    /// The highest event seq that this connection has received. Keep it
    /// for `ConnectOptions::events_since` on the next connect.
    pub fn last_seq(&self) -> u64 {
        self.inner.shared.last_seq.load(Ordering::SeqCst)
    }

    async fn call_unit(&self, method: &str, params: Value) -> Result<()> {
        self.inner.shared.call(method, params).await.map(|_| ())
    }

    fn track(&self, task: AbortHandle) {
        let mut tasks = lock(&self.inner.tasks);
        tasks.retain(|t| !t.is_finished());
        tasks.push(task);
    }
}

/// Open an `attach` stream and read its snapshot.
async fn open_attach(
    shared: &Shared,
) -> Result<(u64, ScreenSnapshot, mpsc::UnboundedReceiver<StreamMsg>)> {
    let (id, mut rx) = shared.open_stream("attach", json!({}))?;
    let snapshot = match rx.recv().await {
        Some(StreamMsg::Item(mut v)) => {
            let snap = decode_value::<WireSnapshot>(v["snapshot"].take());
            match snap.and_then(|s| s.into_snapshot().map_err(RunnerError::Control)) {
                Ok(s) => s,
                Err(e) => {
                    shared.cancel(id);
                    return Err(e);
                }
            }
        }
        Some(StreamMsg::End(Some(e))) => return Err(control_err(e)),
        Some(StreamMsg::End(None)) => return Err(RunnerError::Exited),
        None => return Err(RunnerError::Disconnected),
    };
    Ok((id, snapshot, rx))
}

/// Pass the output of the `attach` stream `id` to `tx`. When the runner ends
/// the stream with `lagged`, or the reader of `tx` falls `attach_capacity`
/// chunks behind, wait for room, attach again, and send
/// [`AttachEvent::Resynced`] with the new snapshot.
async fn forward_attach(
    shared: Arc<Shared>,
    mut id: u64,
    mut rx: mpsc::UnboundedReceiver<StreamMsg>,
    tx: mpsc::Sender<AttachEvent>,
) {
    loop {
        let lagged = match rx.recv().await {
            Some(StreamMsg::Item(v)) => {
                let Some(text) = v.get("output").and_then(Value::as_str) else {
                    continue;
                };
                let Ok(bytes) = decode(text) else { continue };
                match tx.try_send(AttachEvent::Output(Arc::from(bytes))) {
                    Ok(()) => false,
                    Err(mpsc::error::TrySendError::Full(_)) => {
                        // Stop the old stream: its output is no use now.
                        shared.cancel(id);
                        true
                    }
                    Err(mpsc::error::TrySendError::Closed(_)) => {
                        // No reader is left.
                        shared.cancel(id);
                        return;
                    }
                }
            }
            Some(StreamMsg::End(Some(e))) if e.code == codes::LAGGED => true,
            Some(StreamMsg::End(_)) | None => return,
        };
        if !lagged {
            continue;
        }
        let Ok(permit) = tx.reserve().await else {
            return;
        };
        match open_attach(&shared).await {
            Ok((new_id, snapshot, new_rx)) => {
                tracing::debug!("the attach stream fell behind; attached again");
                permit.send(AttachEvent::Resynced(snapshot));
                id = new_id;
                rx = new_rx;
            }
            Err(e) => {
                tracing::debug!("attach again after a lag: {e}");
                return;
            }
        }
    }
}

impl RunnerHandle for RemoteRunner {
    fn agent(&self) -> AgentId {
        self.inner.hello.agent
    }

    fn socket_path(&self) -> &Path {
        &self.inner.hello.hook_socket
    }

    fn events_path(&self) -> &Path {
        &self.inner.hello.events_path
    }

    fn status(&self) -> AgentStatus {
        *self.inner.shared.status_tx.borrow()
    }

    fn watch_status(&self) -> watch::Receiver<AgentStatus> {
        self.inner.shared.status_tx.subscribe()
    }

    fn subscribe_events(&self) -> broadcast::Receiver<EventRecord> {
        lock(&self.inner.first_events)
            .take()
            .unwrap_or_else(|| self.inner.shared.events_tx.subscribe())
    }

    fn screen(&self) -> BoxFuture<'_, Result<ScreenSnapshot>> {
        Box::pin(async move {
            let v = self.inner.shared.call("screen", json!({})).await?;
            decode_value::<WireSnapshot>(v)?
                .into_snapshot()
                .map_err(RunnerError::Control)
        })
    }

    fn attach(&self) -> BoxFuture<'_, Result<(ScreenSnapshot, AttachStream)>> {
        Box::pin(async move {
            let shared = self.inner.shared.clone();
            let (id, snapshot, rx) = open_attach(&shared).await?;
            let (tx, out) = mpsc::channel::<AttachEvent>(shared.attach_capacity);
            let task = tokio::spawn(forward_attach(shared, id, rx, tx));
            self.track(task.abort_handle());
            Ok((snapshot, AttachStream::remote(out)))
        })
    }

    fn write<'a>(&'a self, bytes: &'a [u8]) -> BoxFuture<'a, Result<()>> {
        Box::pin(async move {
            let p = serde_json::to_value(WriteParams {
                data: encode(bytes),
            })
            .unwrap_or(Value::Null);
            self.call_unit("write", p).await
        })
    }

    fn resize(&self, rows: u16, cols: u16) -> BoxFuture<'_, Result<()>> {
        Box::pin(async move {
            let p = serde_json::to_value(ResizeParams { rows, cols }).unwrap_or(Value::Null);
            self.call_unit("resize", p).await
        })
    }

    /// The `Delivery` id is local to this connection: it is the call id of
    /// the `type_turn` call, not the delivery id of the runner.
    fn type_turn(&self, text: String) -> Delivery {
        let shared = &self.inner.shared;
        let id = shared.next_id();
        let (tx, rx) = oneshot::channel();
        let delivery = Delivery::from_parts(id, rx);
        let p = serde_json::to_value(TypeTurnParams { text }).unwrap_or(Value::Null);
        match shared.register(id, Slot::Turn(tx)) {
            Ok(()) => {
                if !shared.send_call(id, "type_turn", p)
                    && let Some(Slot::Turn(tx)) = shared.take(id)
                {
                    let _ = tx.send(Err(DeliveryError::Dropped));
                }
            }
            Err(Slot::Turn(tx)) => {
                let _ = tx.send(Err(DeliveryError::Dropped));
            }
            Err(_) => {}
        }
        delivery
    }

    fn answer<'a>(&'a self, request_id: &'a str, answer: &'a Answer) -> BoxFuture<'a, Result<()>> {
        Box::pin(async move {
            let p = serde_json::to_value(AnswerParams {
                request_id: request_id.to_owned(),
                answer: answer.clone(),
            })
            .unwrap_or(Value::Null);
            self.call_unit("answer", p).await
        })
    }

    fn answer_keys<'a>(&'a self, keys: &'a str) -> BoxFuture<'a, Result<()>> {
        Box::pin(async move {
            let p = serde_json::to_value(AnswerKeysParams {
                keys: keys.to_owned(),
            })
            .unwrap_or(Value::Null);
            self.call_unit("answer_keys", p).await
        })
    }

    /// A runner process from before the `interrupt` call answers
    /// `method_not_found` (`RunnerError::Control`).
    fn interrupt(&self) -> BoxFuture<'_, Result<bool>> {
        Box::pin(async move {
            let v = self.inner.shared.call("interrupt", json!({})).await?;
            Ok(v.get("sent").and_then(Value::as_bool).unwrap_or(false))
        })
    }

    fn kill(&self) -> BoxFuture<'_, Result<()>> {
        Box::pin(self.call_unit("kill", json!({})))
    }

    fn set_stopped_by(&self, by: StoppedBy) -> BoxFuture<'_, Result<()>> {
        Box::pin(async move {
            let p = serde_json::to_value(StoppedByParams { stopped_by: by }).unwrap_or(Value::Null);
            self.call_unit("set_stopped_by", p).await
        })
    }

    /// When the connection closes before the runner reports the exit, the
    /// status comes from `runner-exit.json`, else it is code 1 with the
    /// signal [`LOST_SIGNAL`].
    fn wait(&self) -> BoxFuture<'_, ExitStatus> {
        Box::pin(async move {
            let mut rx = self.inner.shared.exit_tx.subscribe();
            match rx.wait_for(Option::is_some).await {
                Ok(s) => s.clone().unwrap_or(ExitStatus {
                    code: 1,
                    signal: Some(LOST_SIGNAL.into()),
                }),
                Err(_) => ExitStatus {
                    code: 1,
                    signal: Some(LOST_SIGNAL.into()),
                },
            }
        })
    }
}
