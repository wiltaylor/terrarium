//! The runner: one harness in a PTY, its status machine, and its socket.

use std::collections::VecDeque;
use std::fs::{File, OpenOptions};
use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, MutexGuard};
use std::time::{Duration, Instant};

use portable_pty::{ChildKiller, CommandBuilder, MasterPty, PtySize, native_pty_system};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use terrarium_core::paths::{ENV_AGENT, ENV_AGENT_SOCKET};
use terrarium_core::{
    Adapter, AgentId, AgentStatus, Answer, Event, EventRecord, HookPayload, LaunchSpec, ScreenHint,
};
use tokio::sync::{Notify, broadcast, oneshot, watch};
use tokio::task::AbortHandle;

use crate::config::RunnerConfig;
use crate::error::{DeliveryError, Result, RunnerError};
use crate::protocol::{RequestContext, RequestHandler};
use crate::ptylog::PtyLog;
use crate::ready::{DefaultReadyDetector, ReadyDetector};
use crate::socket;

/// The capacity of the output and event broadcast channels. A slow
/// subscriber that falls this far behind sees `RecvError::Lagged`.
const CHANNEL_CAPACITY: usize = 1024;

/// A chunk of raw PTY output, as sent to attach clients.
pub type OutputChunk = Arc<[u8]>;

/// A copy of the parsed screen at one moment.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ScreenSnapshot {
    pub rows: u16,
    pub cols: u16,
    /// One string per screen row, with trailing blanks removed.
    pub lines: Vec<String>,
    /// The cursor as `(row, col)`, from zero.
    pub cursor: (u16, u16),
    /// Escape sequences that draw this screen on an empty terminal. An attach
    /// client writes these first, then the live output.
    #[serde(skip)]
    pub formatted: Vec<u8>,
}

impl ScreenSnapshot {
    /// All lines joined with `\n`.
    pub fn text(&self) -> String {
        self.lines.join("\n")
    }
}

/// How the harness process ended.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ExitStatus {
    pub code: u32,
    /// The signal name, when a signal ended the process.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub signal: Option<String>,
}

impl ExitStatus {
    pub fn success(&self) -> bool {
        self.code == 0 && self.signal.is_none()
    }
}

/// A handle to one typed turn. It resolves when the prompt-submit hook
/// arrives, or when the delivery fails.
#[derive(Debug)]
pub struct Delivery {
    id: u64,
    rx: oneshot::Receiver<Result<(), DeliveryError>>,
}

impl Delivery {
    pub fn id(&self) -> u64 {
        self.id
    }

    /// Wait for the outcome.
    pub async fn wait(self) -> Result<(), DeliveryError> {
        self.rx.await.unwrap_or(Err(DeliveryError::Dropped))
    }
}

/// Why the agent is `blocked`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Block {
    Question,
    Stall,
}

struct PendingTurn {
    id: u64,
    text: String,
    attempts: u32,
    tx: oneshot::Sender<Result<(), DeliveryError>>,
}

struct InFlight {
    turn: PendingTurn,
    deadline: Instant,
}

struct State {
    status: AgentStatus,
    block: Option<Block>,
    turn_open: bool,
    last_output: Instant,
    /// The last PTY output or hook.
    last_activity: Instant,
    stop_requested: bool,
    exited: bool,
    queue: VecDeque<PendingTurn>,
    in_flight: Option<InFlight>,
}

pub(crate) struct Inner {
    config: RunnerConfig,
    adapter: Arc<dyn Adapter>,
    detector: Arc<dyn ReadyDetector>,
    handler: Arc<dyn RequestHandler>,
    writer: Arc<Mutex<Box<dyn Write + Send>>>,
    master: Mutex<Box<dyn MasterPty + Send>>,
    killer: Mutex<Box<dyn ChildKiller + Send + Sync>>,
    parser: Mutex<vt100::Parser>,
    output_tx: broadcast::Sender<OutputChunk>,
    events_tx: broadcast::Sender<EventRecord>,
    events_log: Mutex<File>,
    events_path: PathBuf,
    state: Mutex<State>,
    status_tx: watch::Sender<AgentStatus>,
    exit_tx: watch::Sender<Option<ExitStatus>>,
    wake: Notify,
    next_delivery: AtomicU64,
    socket_path: PathBuf,
    socket_task: Mutex<Option<AbortHandle>>,
}

/// One harness in a PTY. Cloning the runner clones a handle to the same
/// harness. The background tasks keep running until the child exits: call
/// [`Runner::kill`] to end it.
#[derive(Clone)]
pub struct Runner {
    inner: Arc<Inner>,
}

fn lock<T>(m: &Mutex<T>) -> MutexGuard<'_, T> {
    m.lock().unwrap_or_else(std::sync::PoisonError::into_inner)
}

fn pty_err(e: impl std::fmt::Display) -> RunnerError {
    RunnerError::Pty(e.to_string())
}

impl Runner {
    /// Start `spec` in a PTY, bind the runner socket, and start the
    /// background tasks. Call this inside a Tokio runtime.
    ///
    /// The runner adds `TERRARIUM_SOCKET` and `TERRARIUM_AGENT` to the child
    /// env when `spec` does not set them. A sandbox that maps the socket to
    /// another path sets `TERRARIUM_SOCKET` in `spec` itself.
    pub async fn spawn(
        config: RunnerConfig,
        adapter: Arc<dyn Adapter>,
        spec: LaunchSpec,
        handler: Arc<dyn RequestHandler>,
    ) -> Result<Self> {
        let agent_dir = config.agent_dir();
        std::fs::create_dir_all(&agent_dir)
            .map_err(|e| RunnerError::io("create", &agent_dir, e))?;
        let socket_path = config.socket_path();
        let listener = socket::bind(&socket_path)?;

        let events_path = agent_dir.join("events.jsonl");
        let events_log = OpenOptions::new()
            .create(true)
            .append(true)
            .open(&events_path)
            .map_err(|e| RunnerError::io("open", &events_path, e))?;
        let pty_log = PtyLog::open(&agent_dir, config.log_max_bytes)?;

        let size = PtySize {
            rows: config.rows,
            cols: config.cols,
            pixel_width: 0,
            pixel_height: 0,
        };
        let pair = native_pty_system().openpty(size).map_err(pty_err)?;
        let cmd = command(&spec, &socket_path, config.agent);
        let mut child = pair.slave.spawn_command(cmd).map_err(pty_err)?;
        // The child holds its own copy of the slave. Drop ours, so the
        // reader sees EOF when the child exits.
        drop(pair.slave);
        let killer = child.clone_killer();
        let reader = pair.master.try_clone_reader().map_err(pty_err)?;
        let writer = pair.master.take_writer().map_err(pty_err)?;

        let now = Instant::now();
        let detector = config
            .ready_detector
            .clone()
            .unwrap_or_else(|| Arc::new(DefaultReadyDetector::default()));
        let (output_tx, _) = broadcast::channel(CHANNEL_CAPACITY);
        let (events_tx, _) = broadcast::channel(CHANNEL_CAPACITY);
        let parser = vt100::Parser::new(config.rows, config.cols, 0);
        let inner = Arc::new(Inner {
            adapter,
            detector,
            handler,
            writer: Arc::new(Mutex::new(writer)),
            master: Mutex::new(pair.master),
            killer: Mutex::new(killer),
            parser: Mutex::new(parser),
            output_tx,
            events_tx,
            events_log: Mutex::new(events_log),
            events_path,
            state: Mutex::new(State {
                status: AgentStatus::Starting,
                block: None,
                turn_open: false,
                last_output: now,
                last_activity: now,
                stop_requested: false,
                exited: false,
                queue: VecDeque::new(),
                in_flight: None,
            }),
            status_tx: watch::Sender::new(AgentStatus::Starting),
            exit_tx: watch::Sender::new(None),
            wake: Notify::new(),
            next_delivery: AtomicU64::new(1),
            socket_path,
            socket_task: Mutex::new(None),
            config,
        });
        inner.emit(Event::StatusChanged {
            from: AgentStatus::Stopped,
            to: AgentStatus::Starting,
            reason: Some(format!("spawned {}", spec.program)),
        });

        let task = tokio::spawn(socket::serve(listener, inner.clone()));
        *lock(&inner.socket_task) = Some(task.abort_handle());

        let r = inner.clone();
        std::thread::Builder::new()
            .name("terrarium-pty-reader".into())
            .spawn(move || r.read_loop(reader, pty_log))
            .map_err(|e| RunnerError::Other(format!("cannot start the reader thread: {e}")))?;

        let w = inner.clone();
        std::thread::Builder::new()
            .name("terrarium-pty-wait".into())
            .spawn(move || {
                let status = match child.wait() {
                    Ok(s) => ExitStatus {
                        code: s.exit_code(),
                        signal: s.signal().map(str::to_owned),
                    },
                    Err(e) => {
                        tracing::warn!("wait for the harness: {e}");
                        ExitStatus {
                            code: 1,
                            signal: None,
                        }
                    }
                };
                w.on_exit(status);
            })
            .map_err(|e| RunnerError::Other(format!("cannot start the wait thread: {e}")))?;

        tokio::spawn(inner.clone().drive());
        Ok(Self { inner })
    }

    pub fn agent(&self) -> AgentId {
        self.inner.config.agent
    }

    /// The runner socket path.
    pub fn socket_path(&self) -> &Path {
        &self.inner.socket_path
    }

    /// The `events.jsonl` path.
    pub fn events_path(&self) -> &Path {
        &self.inner.events_path
    }

    pub fn status(&self) -> AgentStatus {
        lock(&self.inner.state).status
    }

    /// A receiver that always holds the current status.
    pub fn watch_status(&self) -> watch::Receiver<AgentStatus> {
        self.inner.status_tx.subscribe()
    }

    /// Every event from now on: adapter events from hooks, status changes,
    /// exits, and failed deliveries.
    pub fn subscribe_events(&self) -> broadcast::Receiver<EventRecord> {
        self.inner.events_tx.subscribe()
    }

    /// Raw PTY output from now on.
    pub fn subscribe_output(&self) -> broadcast::Receiver<OutputChunk> {
        self.inner.output_tx.subscribe()
    }

    /// The current screen and the output that follows it, with no gap and no
    /// overlap between the two.
    pub fn attach(&self) -> (ScreenSnapshot, broadcast::Receiver<OutputChunk>) {
        let parser = lock(&self.inner.parser);
        (snapshot(parser.screen()), self.inner.output_tx.subscribe())
    }

    pub fn screen(&self) -> ScreenSnapshot {
        snapshot(lock(&self.inner.parser).screen())
    }

    /// Write raw bytes to the PTY, as keystrokes.
    pub async fn write(&self, bytes: &[u8]) -> Result<()> {
        if lock(&self.inner.state).exited {
            return Err(RunnerError::Exited);
        }
        self.inner.write_bytes(bytes.to_vec()).await
    }

    /// Change the PTY size and the parsed screen size.
    pub fn resize(&self, rows: u16, cols: u16) -> Result<()> {
        lock(&self.inner.master)
            .resize(PtySize {
                rows,
                cols,
                pixel_width: 0,
                pixel_height: 0,
            })
            .map_err(pty_err)?;
        lock(&self.inner.parser).screen_mut().set_size(rows, cols);
        Ok(())
    }

    /// Queue a user turn. The runner types it when the agent is `idle` and
    /// the prompt is ready, and resolves the handle when the prompt-submit
    /// hook arrives. Turns go out one at a time, in order.
    pub fn type_turn(&self, text: impl Into<String>) -> Delivery {
        let (tx, rx) = oneshot::channel();
        let id = self.inner.next_delivery.fetch_add(1, Ordering::Relaxed);
        let delivery = Delivery { id, rx };
        let mut state = lock(&self.inner.state);
        if state.exited {
            let _ = tx.send(Err(DeliveryError::Exited));
            return delivery;
        }
        state.queue.push_back(PendingTurn {
            id,
            text: text.into(),
            attempts: 0,
            tx,
        });
        drop(state);
        self.inner.wake.notify_one();
        delivery
    }

    /// Answer the question `request_id` (from `Event::Question` or a
    /// question notification) with the adapter's keystrokes. This does not
    /// wait for `idle`. A `blocked` agent goes back to `working` when a turn
    /// is open, else to `idle`.
    pub async fn answer(&self, request_id: &str, answer: &Answer) -> Result<()> {
        let bytes = self.inner.adapter.answer(request_id, answer);
        if bytes.is_empty() {
            return Err(RunnerError::Other(format!(
                "the {} adapter has no answer path",
                self.inner.adapter.id()
            )));
        }
        self.write(&bytes).await?;
        self.inner.unblock("answered");
        Ok(())
    }

    /// Answer a question with raw keys, then Enter, for a dialog the adapter
    /// does not know. It unblocks the agent like [`Runner::answer`].
    pub async fn answer_keys(&self, keys: &str) -> Result<()> {
        let mut bytes = keys.as_bytes().to_vec();
        bytes.push(b'\r');
        self.write(&bytes).await?;
        self.inner.unblock("answered");
        Ok(())
    }

    /// Stop the harness on purpose. The status becomes `stopped` when the
    /// child exits.
    pub fn kill(&self) -> Result<()> {
        {
            let mut state = lock(&self.inner.state);
            if state.exited {
                return Ok(());
            }
            state.stop_requested = true;
        }
        lock(&self.inner.killer)
            .kill()
            .map_err(|e| RunnerError::Pty(format!("kill: {e}")))
    }

    /// Wait for the child to exit.
    pub async fn wait(&self) -> ExitStatus {
        let mut rx = self.inner.exit_tx.subscribe();
        loop {
            if let Some(status) = rx.borrow_and_update().clone() {
                return status;
            }
            if rx.changed().await.is_err() {
                return ExitStatus {
                    code: 1,
                    signal: None,
                };
            }
        }
    }
}

fn command(spec: &LaunchSpec, socket: &Path, agent: AgentId) -> CommandBuilder {
    let mut cmd = CommandBuilder::new(&spec.program);
    cmd.args(&spec.args);
    cmd.cwd(&spec.cwd);
    if !spec.env.contains_key("TERM") {
        cmd.env("TERM", "xterm-256color");
    }
    if !spec.env.contains_key(ENV_AGENT_SOCKET) {
        cmd.env(ENV_AGENT_SOCKET, socket);
    }
    if !spec.env.contains_key(ENV_AGENT) {
        cmd.env(ENV_AGENT, agent.to_string());
    }
    for (k, v) in &spec.env {
        cmd.env(k, v);
    }
    cmd
}

fn snapshot(screen: &vt100::Screen) -> ScreenSnapshot {
    let (rows, cols) = screen.size();
    ScreenSnapshot {
        rows,
        cols,
        lines: screen
            .rows(0, cols)
            .map(|l| l.trim_end().to_owned())
            .collect(),
        cursor: screen.cursor_position(),
        formatted: screen.contents_formatted(),
    }
}

impl Inner {
    pub(crate) fn handler(&self) -> &dyn RequestHandler {
        self.handler.as_ref()
    }

    pub(crate) fn request_context(&self) -> RequestContext {
        RequestContext {
            agent: self.config.agent,
            name: self.config.name.clone(),
            project: self.config.project.clone(),
            harness: self.adapter.id(),
            status: lock(&self.state).status,
        }
    }

    fn unblock(&self, reason: &str) {
        let mut state = lock(&self.state);
        if state.status == AgentStatus::Blocked {
            state.block = None;
            let to = if state.turn_open {
                AgentStatus::Working
            } else {
                AgentStatus::Idle
            };
            self.transition(&mut state, to, reason);
        }
    }

    /// Append the event to `events.jsonl` and publish it.
    fn emit(&self, event: Event) {
        let record = EventRecord::now(self.config.agent, event);
        match serde_json::to_vec(&record) {
            Ok(mut line) => {
                line.push(b'\n');
                if let Err(e) = lock(&self.events_log).write_all(&line) {
                    tracing::warn!("write {}: {e}", self.events_path.display());
                }
            }
            Err(e) => tracing::warn!("serialise an event: {e}"),
        }
        let _ = self.events_tx.send(record);
    }

    fn transition(&self, state: &mut State, to: AgentStatus, reason: &str) {
        let from = state.status;
        if from == to {
            return;
        }
        state.status = to;
        self.status_tx.send_replace(to);
        self.emit(Event::StatusChanged {
            from,
            to,
            reason: Some(reason.to_owned()),
        });
    }

    async fn write_bytes(&self, bytes: Vec<u8>) -> Result<()> {
        let writer = self.writer.clone();
        tokio::task::spawn_blocking(move || {
            let mut w = lock(&writer);
            w.write_all(&bytes)?;
            w.flush()
        })
        .await
        .map_err(|e| RunnerError::Other(format!("the write task failed: {e}")))?
        .map_err(|e| RunnerError::Pty(format!("write: {e}")))
    }

    /// The reader thread: PTY bytes go to `pty.log`, the screen, and the
    /// attach channel.
    fn read_loop(&self, mut reader: Box<dyn Read + Send>, mut log: PtyLog) {
        let mut buf = vec![0u8; 16 * 1024];
        loop {
            let n = match reader.read(&mut buf) {
                Ok(0) => break,
                Ok(n) => n,
                Err(e) if e.kind() == std::io::ErrorKind::Interrupted => continue,
                // Linux returns EIO when the child side closes.
                Err(_) => break,
            };
            let chunk = &buf[..n];
            if let Err(e) = log.write(chunk) {
                tracing::warn!("pty.log: {e}");
            }
            {
                let mut parser = lock(&self.parser);
                parser.process(chunk);
                let _ = self.output_tx.send(Arc::from(chunk));
            }
            self.on_output();
        }
    }

    fn on_output(&self) {
        let mut state = lock(&self.state);
        let now = Instant::now();
        state.last_output = now;
        state.last_activity = now;
        if state.block == Some(Block::Stall) && state.turn_open && !state.exited {
            state.block = None;
            self.transition(&mut state, AgentStatus::Working, "output resumed");
        }
    }

    /// A hook arrived on the socket.
    pub(crate) fn on_hook(&self, hook: String, body: Value) {
        let payload = HookPayload {
            harness: self.adapter.id(),
            hook,
            body,
        };
        let events = self.adapter.hook_event(&payload);
        let mut state = lock(&self.state);
        if state.exited {
            return;
        }
        state.last_activity = Instant::now();
        for event in events {
            self.emit(event.clone());
            match event {
                Event::TurnStart { .. } => {
                    state.turn_open = true;
                    state.block = None;
                    if let Some(f) = state.in_flight.take() {
                        let _ = f.turn.tx.send(Ok(()));
                    }
                    self.transition(&mut state, AgentStatus::Working, "prompt submitted");
                }
                Event::TurnEnd => {
                    state.turn_open = false;
                    state.block = None;
                    self.transition(&mut state, AgentStatus::Idle, "turn ended");
                }
                Event::Notification {
                    question: true,
                    message,
                } => {
                    state.block = Some(Block::Question);
                    self.transition(&mut state, AgentStatus::Blocked, &message);
                }
                Event::Question { question, .. } => {
                    state.block = Some(Block::Question);
                    self.transition(&mut state, AgentStatus::Blocked, &question);
                }
                Event::ToolUse { .. } | Event::ToolFinished { .. }
                    if state.status == AgentStatus::Blocked =>
                {
                    state.turn_open = true;
                    state.block = None;
                    self.transition(&mut state, AgentStatus::Working, "tool use");
                }
                _ => {}
            }
        }
        drop(state);
        self.wake.notify_one();
    }

    fn on_exit(&self, status: ExitStatus) {
        let mut state = lock(&self.state);
        state.exited = true;
        self.emit(Event::Exit {
            code: i32::try_from(status.code).ok(),
        });
        let (to, reason) = if state.stop_requested {
            (AgentStatus::Stopped, "stopped on request".to_owned())
        } else {
            (
                AgentStatus::Error,
                format!(
                    "exited with code {}{}",
                    status.code,
                    status
                        .signal
                        .as_deref()
                        .map(|s| format!(" ({s})"))
                        .unwrap_or_default()
                ),
            )
        };
        self.transition(&mut state, to, &reason);
        if let Some(f) = state.in_flight.take() {
            let _ = f.turn.tx.send(Err(DeliveryError::Exited));
        }
        for turn in state.queue.drain(..) {
            let _ = turn.tx.send(Err(DeliveryError::Exited));
        }
        drop(state);
        self.exit_tx.send_replace(Some(status));
        self.wake.notify_one();
    }

    /// The screen hint and prompt-ready answer for the current screen.
    fn probe(&self, quiet_for: Duration) -> (ScreenHint, bool) {
        let parser = lock(&self.parser);
        let screen = parser.screen();
        let hint = self.adapter.screen_hint(screen);
        let ready = match hint {
            ScreenHint::PromptReady => true,
            ScreenHint::Busy | ScreenHint::Question | ScreenHint::NotReady => false,
            ScreenHint::Unknown => self.detector.prompt_ready(screen, quiet_for),
        };
        (hint, ready)
    }

    /// The driver task: prompt-ready, stalls, delivery, and timeouts.
    async fn drive(self: Arc<Self>) {
        loop {
            tokio::select! {
                _ = self.wake.notified() => {}
                _ = tokio::time::sleep(self.config.tick) => {}
            }
            if !self.tick().await {
                break;
            }
        }
        if let Some(task) = lock(&self.socket_task).take() {
            task.abort();
        }
        if let Err(e) = std::fs::remove_file(&self.socket_path) {
            tracing::debug!("remove {}: {e}", self.socket_path.display());
        }
    }

    /// One driver step. Returns false when the child has exited.
    async fn tick(&self) -> bool {
        let quiet_for = {
            let state = lock(&self.state);
            if state.exited {
                return false;
            }
            state.last_output.elapsed()
        };
        let (hint, ready) = self.probe(quiet_for);

        let mut to_type = None;
        {
            let mut state = lock(&self.state);
            if state.exited {
                return false;
            }
            let now = Instant::now();
            match state.status {
                AgentStatus::Starting if ready => {
                    self.transition(&mut state, AgentStatus::Idle, "prompt ready");
                }
                AgentStatus::Working if hint == ScreenHint::Question => {
                    state.block = Some(Block::Question);
                    self.transition(&mut state, AgentStatus::Blocked, "question on screen");
                }
                AgentStatus::Working
                    if now.duration_since(state.last_activity) >= self.config.stall_timeout =>
                {
                    state.block = Some(Block::Stall);
                    self.transition(&mut state, AgentStatus::Blocked, "stalled");
                }
                _ => {}
            }

            if state.in_flight.as_ref().is_some_and(|f| now >= f.deadline)
                && let Some(f) = state.in_flight.take()
            {
                if f.turn.attempts < self.config.submit_attempts {
                    tracing::debug!(
                        "turn {} got no prompt-submit hook; typing it again",
                        f.turn.id
                    );
                    state.queue.push_front(f.turn);
                } else {
                    let attempts = f.turn.attempts;
                    self.emit(Event::DeliveryFailed {
                        text: f.turn.text.clone(),
                        reason: format!("no prompt-submit hook after {attempts} attempts"),
                    });
                    let _ = f
                        .turn
                        .tx
                        .send(Err(DeliveryError::NoSubmitHook { attempts }));
                }
            }

            if state.in_flight.is_none()
                && state.status == AgentStatus::Idle
                && ready
                && let Some(mut turn) = state.queue.pop_front()
            {
                turn.attempts += 1;
                let bytes = self.adapter.type_turn(&turn.text);
                state.in_flight = Some(InFlight {
                    turn,
                    deadline: now + self.config.submit_timeout,
                });
                to_type = Some(bytes);
            }
        }

        if let Some(bytes) = to_type
            && let Err(e) = self.write_bytes(bytes).await
        {
            let mut state = lock(&self.state);
            if let Some(f) = state.in_flight.take() {
                self.emit(Event::DeliveryFailed {
                    text: f.turn.text.clone(),
                    reason: e.to_string(),
                });
                let _ = f.turn.tx.send(Err(DeliveryError::Write(e.to_string())));
            }
        }
        true
    }
}
