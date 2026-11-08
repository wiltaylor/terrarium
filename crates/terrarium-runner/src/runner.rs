//! The runner: one harness in a PTY, its status machine, and its socket.

use std::collections::VecDeque;
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
    Adapter, AgentId, AgentStatus, Answer, Event, EventRecord, HarnessSessionId, HookPayload,
    LaunchSpec, ScreenHint,
};
use tokio::sync::{Notify, broadcast, oneshot, watch};
use tokio::task::AbortHandle;

use crate::attach::AttachStream;
use crate::config::RunnerConfig;
use crate::error::{DeliveryError, Result, RunnerError};
use crate::events::EventLog;
use crate::protocol::{RequestContext, RequestHandler};
use crate::ptylog::PtyLog;
use crate::ready::{DefaultReadyDetector, ReadyDetector};
use crate::socket;

/// The capacity of the event broadcast channel. A slow subscriber that
/// falls this far behind sees `RecvError::Lagged`. The output channel takes
/// `RunnerConfig::output_capacity`.
const CHANNEL_CAPACITY: usize = 1024;

/// How long [`Runner::interrupt`] watches for the screen to settle. After
/// it, the status follows the hooks and the stall timeout again.
pub const INTERRUPT_WAIT: Duration = Duration::from_secs(30);
/// After an interrupt, a screen that the adapter reads as `NotReady` (text
/// left in the input box) ends the turn once it is quiet this long.
pub const INTERRUPT_SETTLE: Duration = Duration::from_secs(1);

/// After the harness exits, how long an attach stream waits for the reader
/// thread to pass on the last PTY bytes. A background process that keeps the
/// PTY open delays the end of the output.
pub(crate) const OUTPUT_DRAIN: Duration = Duration::from_secs(1);

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

    /// Parse `bytes` on an empty `rows` x `cols` screen, and take the
    /// snapshot on the runner's own path. For fakes and tests.
    pub fn parse(rows: u16, cols: u16, bytes: &[u8]) -> Self {
        let mut parser = vt100::Parser::new(rows, cols, 0);
        parser.process(bytes);
        snapshot(parser.screen())
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

    /// The number of the signal that ended the process, when the signal
    /// name is known. The name is the `strsignal` text of the PTY crate
    /// (`Killed`, `Hangup`), a `SIGKILL` style name, or `Signal 9`.
    pub fn signal_number(&self) -> Option<i32> {
        let name = self.signal.as_deref()?.trim();
        const TEXT: [(&str, i32); 15] = [
            ("Hangup", 1),
            ("Interrupt", 2),
            ("Quit", 3),
            ("Illegal instruction", 4),
            ("Trace/breakpoint trap", 5),
            ("Aborted", 6),
            ("Bus error", 7),
            ("Floating point exception", 8),
            ("Killed", 9),
            ("User defined signal 1", 10),
            ("Segmentation fault", 11),
            ("User defined signal 2", 12),
            ("Broken pipe", 13),
            ("Alarm clock", 14),
            ("Terminated", 15),
        ];
        if let Some((_, n)) = TEXT.iter().find(|(t, _)| *t == name) {
            return Some(*n);
        }
        if let Ok(sig) = name.parse::<nix::sys::signal::Signal>() {
            return Some(sig as i32);
        }
        name.strip_prefix("Signal ")
            .and_then(|n| n.trim().parse().ok())
    }

    /// The exit code that reflects this status, as a shell gives it: the
    /// code of the process, or 128 plus the signal number when a signal
    /// ended it (128 when the signal is not known).
    pub fn exit_code(&self) -> i32 {
        if self.signal.is_some() {
            128 + self.signal_number().unwrap_or(0)
        } else {
            i32::try_from(self.code.min(255)).unwrap_or(255)
        }
    }
}

/// A handle to one typed turn. It resolves when the prompt-submit hook
/// arrives or the adapter's `screen_hint` says `Busy`, or when the delivery
/// fails.
#[derive(Debug)]
pub struct Delivery {
    id: u64,
    rx: oneshot::Receiver<Result<(), DeliveryError>>,
}

impl Delivery {
    pub(crate) fn from_parts(id: u64, rx: oneshot::Receiver<Result<(), DeliveryError>>) -> Self {
        Self { id, rx }
    }

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
    /// The idle agent keeps unsubmitted text in its input box
    /// ([`terrarium_core::adapter::INPUT_PENDING_REASON`]).
    Input,
}

struct PendingTurn {
    id: u64,
    pending_id: String,
    text: String,
    attempts: u32,
    tx: oneshot::Sender<Result<(), DeliveryError>>,
}

/// A turn that has not started typing. IDs belong to one runner lifetime.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct WaitingTurn {
    pub id: String,
    pub text: String,
}

struct InFlight {
    turn: PendingTurn,
    deadline: Instant,
    /// The CR of the turn that waits for the adapter's
    /// [`Adapter::turn_submit_pause`], and when it goes out.
    submit: Option<(Instant, Vec<u8>)>,
    /// When the last CR went out (with the text, or on its own).
    entered: Instant,
    /// How many times the CR went out again because the text stayed in
    /// the input box ([`Adapter::input_pending`]).
    resubmits: u32,
}

/// When the input box still holds the typed turn this long after its CR,
/// the runner presses CR again (TER-194). The harness empties the box in
/// about 30 ms when it takes the turn (`docs/compat.md`).
pub const RESUBMIT_AFTER: Duration = Duration::from_secs(1);

/// The most CRs that the runner sends again for one typed turn, before it
/// empties the input box and types the turn again.
pub const MAX_RESUBMITS: u32 = 2;

/// After a write of a person ([`Runner::write_person`]: the terminal of
/// the agent, or `agent keys`), the text in the input box is that
/// person's, and the watchdog does not flag it, also when the agent was
/// busy at the write and goes idle later (TER-224). The mark ends when the
/// box of the agent that does not work reads empty for
/// [`INPUT_CLEAR_DEBOUNCE`], counted from this long after the last write:
/// the harness may draw the text late.
pub const PERSON_INPUT_SLACK: Duration = Duration::from_secs(2);

/// The input box of an agent that the watchdog blocked must read empty
/// this long before the agent goes back to `idle`. A redraw that hides the
/// text for a frame does not unblock it, so one stuck box gives one flag
/// and one mail (TER-224).
pub const INPUT_CLEAR_DEBOUNCE: Duration = Duration::from_secs(1);

struct State {
    status: AgentStatus,
    block: Option<Block>,
    turn_open: bool,
    last_output: Instant,
    /// The last PTY output or hook.
    last_activity: Instant,
    stop_requested: bool,
    /// A reversible idle-stop reservation. A lost control reply must not
    /// disable delivery forever. The supervisor also gates new work.
    idle_stop_until: Option<Instant>,
    /// A SIGKILL follows the SIGHUP of `kill` after `kill_grace`.
    kill_armed: bool,
    exited: bool,
    queue: VecDeque<PendingTurn>,
    in_flight: Option<InFlight>,
    /// The startup of the harness is complete: the adapter does not wait
    /// for a startup hook, the hook came, or its wait timed out
    /// ([`RunnerConfig::startup_hook_timeout`]). Until then the agent
    /// stays `starting`, also on a ready screen.
    startup_done: bool,
    /// The spawn time, for the startup hook timeout.
    spawned: Instant,
    /// [`Runner::interrupt`] sent the interrupt keys: until this deadline,
    /// a screen that settles (not busy, no question) ends the turn, also
    /// when the harness sends no stop hook (Claude Code sends none on
    /// Esc).
    interrupt: Option<Instant>,
    /// Since when an idle agent with no turn in flight has text in its
    /// input box ([`Adapter::input_pending`]), and whether the runner
    /// flagged it. No turn can go out while the box holds text.
    pending_input: Option<(Instant, bool)>,
    /// The input box holds text of a person ([`Runner::write_person`]):
    /// the last write of the person, and since when the box reads empty
    /// ([`PERSON_INPUT_SLACK`]).
    person_text: Option<(Instant, Option<Instant>)>,
    /// Since when the input box of the agent that the watchdog blocked
    /// reads empty ([`INPUT_CLEAR_DEBOUNCE`]).
    input_cleared: Option<Instant>,
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
    events_log: Mutex<EventLog>,
    events_path: PathBuf,
    harness_pid: Option<u32>,
    session: Mutex<Option<HarnessSessionId>>,
    state: Mutex<State>,
    status_tx: watch::Sender<AgentStatus>,
    exit_tx: watch::Sender<Option<ExitStatus>>,
    /// True when the reader thread has seen the end of the PTY output.
    output_done: watch::Sender<bool>,
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
        // The records go to `private/`, which the sandbox never binds.
        let private = config.private_dir();
        crate::config::create_private_dir(&private)?;
        let socket_path = config.socket_path();
        let listener = socket::bind(&socket_path)?;

        let events_path = private.join("events.jsonl");
        let events_log = EventLog::open(&events_path)?;
        let pty_log = PtyLog::open(&private, config.log_max_bytes)?;

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
        let harness_pid = child.process_id();
        let reader = pair.master.try_clone_reader().map_err(pty_err)?;
        let writer = pair.master.take_writer().map_err(pty_err)?;

        let now = Instant::now();
        let detector = config
            .ready_detector
            .clone()
            .unwrap_or_else(|| Arc::new(DefaultReadyDetector::default()));
        let (output_tx, _) = broadcast::channel(config.output_capacity.max(1));
        let (events_tx, _) = broadcast::channel(CHANNEL_CAPACITY);
        let parser = vt100::Parser::new(config.rows, config.cols, 0);
        let startup_done = !adapter.waits_for_startup_hook();
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
            harness_pid,
            session: Mutex::new(None),
            state: Mutex::new(State {
                status: AgentStatus::Starting,
                block: None,
                turn_open: false,
                last_output: now,
                last_activity: now,
                stop_requested: false,
                idle_stop_until: None,
                kill_armed: false,
                exited: false,
                queue: VecDeque::new(),
                in_flight: None,
                startup_done,
                spawned: now,
                interrupt: None,
                pending_input: None,
                person_text: None,
                input_cleared: None,
            }),
            status_tx: watch::Sender::new(AgentStatus::Starting),
            exit_tx: watch::Sender::new(None),
            output_done: watch::Sender::new(false),
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

    /// The process ID of the harness (for a sandbox: of `bwrap`).
    pub fn harness_pid(&self) -> Option<u32> {
        self.inner.harness_pid
    }

    /// The harness session ID from the last `SessionStarted` event.
    pub fn session(&self) -> Option<HarnessSessionId> {
        lock(&self.inner.session).clone()
    }

    /// The seq of the last line of `events.jsonl`.
    pub fn last_seq(&self) -> u64 {
        lock(&self.inner.events_log).seq()
    }

    /// The exit status, once the child has exited.
    pub fn exit_status(&self) -> Option<ExitStatus> {
        self.inner.exit_tx.borrow().clone()
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

    /// [`Runner::attach`] as an [`AttachStream`]. When the stream falls
    /// behind, it attaches again and gives `AttachEvent::Resynced` with a
    /// new snapshot. The stream ends after the harness exits and the last
    /// output is out.
    pub fn attach_stream(&self) -> (ScreenSnapshot, AttachStream) {
        let (snap, rx) = self.attach();
        (snap, AttachStream::local(self.clone(), rx))
    }

    /// Wait until the reader thread has passed on the last PTY output, up
    /// to `limit`. Call it after the harness exits.
    pub(crate) async fn output_drained(&self, limit: Duration) {
        let mut rx = self.inner.output_done.subscribe();
        let _ = tokio::time::timeout(limit, rx.wait_for(|d| *d)).await;
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

    /// [`Runner::write`] of the keys of a person: the terminal of the
    /// agent, or `agent keys`. Text that they leave in the input box is
    /// theirs, and the watchdog does not flag it ([`PERSON_INPUT_SLACK`],
    /// TER-224).
    pub async fn write_person(&self, bytes: &[u8]) -> Result<()> {
        lock(&self.inner.state).person_text = Some((Instant::now(), None));
        self.write(bytes).await
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
    /// hook arrives or the screen goes busy. Turns go out one at a time, in
    /// order.
    pub fn type_turn(&self, text: impl Into<String>) -> Delivery {
        let (tx, rx) = oneshot::channel();
        let id = self.inner.next_delivery.fetch_add(1, Ordering::Relaxed);
        let delivery = Delivery { id, rx };
        let mut state = lock(&self.inner.state);
        if state.exited || state.stop_requested {
            let _ = tx.send(Err(DeliveryError::Exited));
            return delivery;
        }
        state.queue.push_back(PendingTurn {
            id,
            pending_id: terrarium_core::AgentId::new().to_string(),
            text: text.into(),
            attempts: 0,
            tx,
        });
        drop(state);
        self.inner.wake.notify_one();
        delivery
    }

    /// Snapshot only untouched turns, under the dispatch lock.
    pub fn list_pending(&self) -> Vec<WaitingTurn> {
        lock(&self.inner.state)
            .queue
            .iter()
            .filter(|t| t.attempts == 0)
            .map(|t| WaitingTurn {
                id: t.pending_id.clone(),
                text: t.text.clone(),
            })
            .collect()
    }

    /// Refuse a turn once dispatch has taken it, including retries.
    pub fn cancel_pending(&self, id: &str) -> bool {
        let mut state = lock(&self.inner.state);
        let Some(index) = state
            .queue
            .iter()
            .position(|t| t.pending_id == id && t.attempts == 0)
        else {
            return false;
        };
        let turn = state.queue.remove(index).expect("pending turn");
        let _ = turn.tx.send(Err(DeliveryError::Dropped));
        true
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

    /// Interrupt the running turn: type the adapter's interrupt keys
    /// ([`Adapter::interrupt_keys`]), with
    /// [`INTERRUPT_GAP`](terrarium_core::adapter::INTERRUPT_GAP) between
    /// the writes. The harness keeps running. When the screen then settles
    /// (the prompt is ready, or the screen is quiet and not busy for
    /// [`INTERRUPT_SETTLE`]), the agent goes `idle` with the reason
    /// `interrupted`, also with no stop hook. A turn that waits in the
    /// queue goes out after that.
    ///
    /// False: no turn runs (the status is not `working` or `blocked`), and
    /// no key went out. An error when the adapter has no interrupt keys.
    pub async fn interrupt(&self) -> Result<bool> {
        let writes = self.inner.adapter.interrupt_keys();
        if writes.is_empty() {
            return Err(RunnerError::Other(format!(
                "the {} adapter has no interrupt keys",
                self.inner.adapter.id()
            )));
        }
        {
            let mut state = lock(&self.inner.state);
            if state.exited {
                return Err(RunnerError::Exited);
            }
            if !matches!(state.status, AgentStatus::Working | AgentStatus::Blocked) {
                return Ok(false);
            }
            state.interrupt = Some(Instant::now() + INTERRUPT_WAIT);
        }
        for (i, w) in writes.into_iter().enumerate() {
            if i > 0 {
                tokio::time::sleep(terrarium_core::adapter::INTERRUPT_GAP).await;
            }
            self.inner.write_bytes(w).await?;
        }
        self.inner.emit(Event::System {
            message: "interrupt: the interrupt keys went to the harness; the turn stops".into(),
        });
        self.inner.wake.notify_one();
        Ok(true)
    }

    /// Atomically claim an idle stop against queued and running turns.
    /// Idempotent while held. The caller must finish the stop with exit
    /// keys or `kill`, or release it. A lost reply expires after five
    /// seconds. New turns may queue, but cannot dispatch while held.
    pub fn claim_idle_stop(&self) -> bool {
        let mut state = lock(&self.inner.state);
        if state
            .idle_stop_until
            .is_some_and(|until| Instant::now() < until)
        {
            return true;
        }
        if state.exited
            || state.stop_requested
            || !state.status.is_resting()
            || state.turn_open
            || state.in_flight.is_some()
            || !state.queue.is_empty()
        {
            return false;
        }
        state.idle_stop_until = Some(Instant::now() + Duration::from_secs(5));
        true
    }

    /// Release a claim, including one whose reply was lost.
    pub fn release_idle_stop(&self) {
        lock(&self.inner.state).idle_stop_until = None;
        self.inner.wake.notify_one();
    }

    /// Stop the harness on purpose. The status becomes `stopped` when the
    /// child exits. The runner sends SIGHUP. When the harness has not exited
    /// after `RunnerConfig::kill_grace`, it sends SIGKILL to the process
    /// group of the harness.
    pub fn kill(&self) -> Result<()> {
        let arm = {
            let mut state = lock(&self.inner.state);
            if state.exited {
                return Ok(());
            }
            state.stop_requested = true;
            !std::mem::replace(&mut state.kill_armed, true)
        };
        let sent = lock(&self.inner.killer)
            .kill()
            .map_err(|e| RunnerError::Pty(format!("kill: {e}")));
        if arm && let Some(pid) = self.inner.harness_pid {
            let inner = self.inner.clone();
            let spawned = std::thread::Builder::new()
                .name("terrarium-pty-kill".into())
                .spawn(move || inner.escalate_kill(pid));
            if let Err(e) = spawned {
                tracing::warn!("cannot start the kill thread: {e}");
            }
        }
        sent
    }

    /// Stop the harness gracefully. An idle harness gets the adapter's exit
    /// keys (the final CR after `Adapter::exit_key_pause`) and
    /// `RunnerConfig::stop_grace` to exit. A busy harness, a
    /// harness with no exit keys, or one that does not exit in time gets
    /// [`Runner::kill`]. No queued turn goes out after this call. Returns
    /// the exit status.
    pub async fn stop(&self) -> ExitStatus {
        let writes = terrarium_core::adapter::exit_key_writes(
            &self.inner.adapter.exit_keys(),
            self.inner.adapter.exit_key_pause(),
        );
        let (exited, idle, again) = {
            let mut state = lock(&self.inner.state);
            let again = state.stop_requested;
            state.stop_requested = true;
            (
                state.exited,
                state.status == AgentStatus::Idle && state.in_flight.is_none() && !state.turn_open,
                again,
            )
        };
        // An exit, or a stop that is on the way already: that stop types
        // the exit keys once and kills on its timeout. A second set of keys
        // in the exit-key pause would join the first (`/exit/exit`).
        if exited || again {
            return self.wait().await;
        }
        if idle && !writes.is_empty() {
            match self.type_exit_keys(writes).await {
                Ok(()) => {
                    if let Ok(status) =
                        tokio::time::timeout(self.inner.config.stop_grace, self.wait()).await
                    {
                        return status;
                    }
                    tracing::info!("the harness did not exit on its exit keys; killing it");
                }
                Err(e) => tracing::debug!("cannot type the exit keys: {e}"),
            }
        }
        if let Err(e) = self.kill() {
            tracing::warn!("kill the harness: {e}");
        }
        self.wait().await
    }

    /// Type the writes of [`exit_key_writes`](terrarium_core::adapter::exit_key_writes),
    /// with the adapter's exit-key pause between them.
    async fn type_exit_keys(&self, writes: Vec<Vec<u8>>) -> Result<()> {
        let pause = self.inner.adapter.exit_key_pause();
        for (i, w) in writes.into_iter().enumerate() {
            if i > 0 {
                tokio::time::sleep(pause).await;
            }
            self.inner.write_bytes(w).await?;
        }
        Ok(())
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
    if spec.clear_env {
        cmd.env_clear();
    }
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
            roles: self.config.roles.clone(),
            harness: self.adapter.id(),
            status: lock(&self.state).status,
        }
    }

    fn unblock(&self, reason: &str) {
        let mut state = lock(&self.state);
        if state.status == AgentStatus::Blocked {
            state.block = None;
            // A new watch of the input box: text that is still there gets
            // flagged again after `submit_timeout` (TER-224).
            state.pending_input = None;
            state.input_cleared = None;
            let to = if state.turn_open {
                AgentStatus::Working
            } else {
                AgentStatus::Idle
            };
            self.transition(&mut state, to, reason);
        }
    }

    /// Give the event the next seq, append it to `events.jsonl`, and
    /// publish it. The publish happens under the log lock, so subscribers
    /// see the records in seq order.
    fn emit(&self, event: Event) {
        if let Event::SessionStarted {
            harness_session, ..
        } = &event
        {
            *lock(&self.session) = Some(harness_session.clone());
        }
        let mut record = EventRecord::now(self.config.agent, event);
        let mut log = lock(&self.events_log);
        if let Err(e) = log.append(&mut record) {
            tracing::warn!("write {}: {e}", self.events_path.display());
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
        self.output_done.send_replace(true);
    }

    /// The kill thread: SIGKILL the harness when SIGHUP did not end it.
    fn escalate_kill(&self, pid: u32) {
        use nix::sys::signal::{Signal, kill, killpg};
        use nix::unistd::Pid;
        let deadline = Instant::now() + self.config.kill_grace;
        while Instant::now() < deadline {
            if lock(&self.state).exited {
                return;
            }
            std::thread::sleep(Duration::from_millis(20));
        }
        let state = lock(&self.state);
        if state.exited {
            return;
        }
        let Ok(raw) = i32::try_from(pid) else { return };
        tracing::info!(pid, "the harness did not exit on SIGHUP; sending SIGKILL");
        // The harness leads its own session and process group (the PTY).
        if killpg(Pid::from_raw(raw), Signal::SIGKILL).is_err() {
            let _ = kill(Pid::from_raw(raw), Signal::SIGKILL);
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
        // A prompt that the harness made itself (Copilot's
        // `<system_notification>` after a background shell) is not a typed
        // turn: it must not confirm a delivery, and it is not a user turn.
        let system = self.adapter.is_system_prompt(&payload);
        let startup = self.adapter.is_startup_hook(&payload);
        let mut state = lock(&self.state);
        if state.exited {
            return;
        }
        state.last_activity = Instant::now();
        if startup && !state.startup_done {
            tracing::debug!("startup hook `{}`: the startup is complete", payload.hook);
            state.startup_done = true;
        }
        for event in events {
            let event = match event {
                Event::TurnStart { prompt } if system => {
                    tracing::debug!("a system prompt opens a turn; no delivery counts it");
                    self.emit(Event::Notification {
                        message: prompt.unwrap_or_else(|| "system prompt".to_owned()),
                        question: false,
                    });
                    // The harness works on it, so the turn is open, but an
                    // in-flight delivery still waits for its own sign.
                    state.turn_open = true;
                    state.block = None;
                    self.transition(&mut state, AgentStatus::Working, "system prompt");
                    continue;
                }
                e => e,
            };
            self.emit(event.clone());
            match event {
                Event::TurnStart { .. } => {
                    state.turn_open = true;
                    state.block = None;
                    state.interrupt = None;
                    if let Some(f) = state.in_flight.take() {
                        let _ = f.turn.tx.send(Ok(()));
                    }
                    self.transition(&mut state, AgentStatus::Working, "prompt submitted");
                }
                Event::TurnEnd => {
                    state.turn_open = false;
                    state.block = None;
                    state.interrupt = None;
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
        } else if status.success() {
            // The harness ended on its own and reported success, for
            // example after `/exit`. That is not a failure (PLAN §18).
            (AgentStatus::Stopped, "exited".to_owned())
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

    /// The screen hint, the prompt-ready answer, and the pending-input
    /// answer ([`Adapter::input_pending`]) for the current screen.
    fn probe(&self, quiet_for: Duration) -> (ScreenHint, bool, bool) {
        let parser = lock(&self.parser);
        let screen = parser.screen();
        let hint = self.adapter.screen_hint(screen);
        let ready = match hint {
            ScreenHint::PromptReady => true,
            ScreenHint::Busy | ScreenHint::Question | ScreenHint::NotReady => false,
            ScreenHint::Unknown => self.detector.prompt_ready(screen, quiet_for),
        };
        let pending = !ready && self.adapter.input_pending(screen);
        (hint, ready, pending)
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
        let (hint, ready, pending) = self.probe(quiet_for);

        let mut to_type = None;
        let mut clear = None;
        let mut to_submit = None;
        {
            let mut state = lock(&self.state);
            if state.exited {
                return false;
            }
            let now = Instant::now();
            if !state.startup_done
                && now.duration_since(state.spawned) >= self.config.startup_hook_timeout
            {
                state.startup_done = true;
                let secs = self.config.startup_hook_timeout.as_secs();
                tracing::warn!("no startup hook after {secs} s; the screen alone decides");
                self.emit(Event::Warning {
                    message: format!(
                        "the harness sent no startup hook in {secs} s; \
                         the prompt-ready screen counts alone"
                    ),
                });
            }
            // After an interrupt: a settled screen ends the turn.
            if let Some(deadline) = state.interrupt {
                let turn = matches!(state.status, AgentStatus::Working | AgentStatus::Blocked);
                let settled = match hint {
                    ScreenHint::PromptReady => true,
                    ScreenHint::NotReady => quiet_for >= INTERRUPT_SETTLE,
                    ScreenHint::Unknown => ready,
                    ScreenHint::Busy | ScreenHint::Question => false,
                };
                if !turn || now >= deadline {
                    state.interrupt = None;
                } else if settled && state.in_flight.is_none() {
                    state.interrupt = None;
                    state.turn_open = false;
                    state.block = None;
                    // Some harnesses put the stopped prompt back in the
                    // input box. Empty it, or the next turn never types.
                    if hint == ScreenHint::NotReady {
                        clear = Some(self.adapter.clear_input_keys());
                    }
                    self.transition(&mut state, AgentStatus::Idle, "interrupted");
                }
            }
            match state.status {
                // A harness that sends a startup hook can draw a ready
                // prompt before its startup is complete (pi 1.4.0). A turn
                // typed then does not submit.
                AgentStatus::Starting if ready && state.startup_done => {
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

            // The prompt-submit hook is one sign that the harness took the
            // turn. A busy screen is the other: the text left the input line
            // and a turn runs. Codex and Copilot send the hook late (the
            // first prompt also creates the session), so a retry on the
            // hook alone would type the text twice.
            if hint == ScreenHint::Busy
                && let Some(f) = state.in_flight.take()
            {
                tracing::debug!("turn {} submitted: the screen is busy", f.turn.id);
                let _ = f.turn.tx.send(Ok(()));
                state.turn_open = true;
                state.block = None;
                self.transition(
                    &mut state,
                    AgentStatus::Working,
                    "prompt submitted (screen busy)",
                );
            }

            // The CR of a turn goes out on its own after the paste
            // ([`Adapter::turn_submit_pause`]).
            if let Some(f) = state.in_flight.as_mut()
                && f.submit.as_ref().is_some_and(|(at, _)| now >= *at)
                && let Some((_, cr)) = f.submit.take()
            {
                f.entered = now;
                to_submit = Some(cr);
            } else if pending
                && let Some(f) = state.in_flight.as_mut()
                && f.submit.is_none()
                && f.resubmits < MAX_RESUBMITS
                && now.duration_since(f.entered) >= RESUBMIT_AFTER
            {
                // The text sits in the input box: the harness lost the CR.
                // Press it again, and do not type the text again.
                f.resubmits += 1;
                f.entered = now;
                tracing::debug!(
                    "turn {} is still in the input box; pressing Enter again ({})",
                    f.turn.id,
                    f.resubmits
                );
                to_submit = Some(b"\r".to_vec());
            }

            if state.in_flight.as_ref().is_some_and(|f| now >= f.deadline)
                && let Some(f) = state.in_flight.take()
            {
                // A CR for the old text must not follow the clear keys.
                to_submit = None;
                if f.turn.attempts < self.config.submit_attempts {
                    tracing::debug!(
                        "turn {} got no prompt-submit hook and no busy screen; typing it again",
                        f.turn.id
                    );
                    state.queue.push_front(f.turn);
                    // The text of the failed attempt can still be in the
                    // input box. Empty it first, or the prompt never reads
                    // as ready and the retry would join the old text.
                    clear = Some(self.adapter.clear_input_keys());
                } else {
                    let attempts = f.turn.attempts;
                    self.emit(Event::DeliveryFailed {
                        text: f.turn.text.clone(),
                        reason: format!(
                            "no prompt-submit hook and no busy screen after {attempts} attempts"
                        ),
                    });
                    let _ = f
                        .turn
                        .tx
                        .send(Err(DeliveryError::NoSubmitHook { attempts }));
                    // The text of the last attempt can still be in the
                    // input box. Empty it, or no later turn can go out.
                    if pending {
                        clear = Some(self.adapter.clear_input_keys());
                    }
                }
            }

            if state.in_flight.is_none()
                && !state.stop_requested
                && state
                    .idle_stop_until
                    .is_none_or(|until| Instant::now() >= until)
                && state.status == AgentStatus::Idle
                && ready
                && let Some(mut turn) = state.queue.pop_front()
            {
                turn.attempts += 1;
                let pause = self.adapter.turn_submit_pause();
                let mut writes = terrarium_core::adapter::exit_key_writes(
                    &self.adapter.type_turn(&turn.text),
                    pause,
                )
                .into_iter();
                let text = writes.next().unwrap_or_default();
                let submit = writes.next().map(|cr| (now + pause, cr));
                let wait = if submit.is_some() {
                    pause
                } else {
                    Duration::ZERO
                };
                state.in_flight = Some(InFlight {
                    turn,
                    deadline: now + wait + self.config.submit_timeout,
                    submit,
                    entered: now,
                    resubmits: 0,
                });
                to_type = Some(text);
            }

            // The mark of a person's text ends when the box of an agent
            // that does not work reads empty for INPUT_CLEAR_DEBOUNCE,
            // from PERSON_INPUT_SLACK after the last write (TER-224).
            if let Some((wrote, empty)) = state.person_text {
                if pending
                    || state.status == AgentStatus::Working
                    || now.duration_since(wrote) < PERSON_INPUT_SLACK
                {
                    state.person_text = Some((wrote, None));
                } else {
                    let empty = empty.unwrap_or(now);
                    state.person_text = (now.duration_since(empty) < INPUT_CLEAR_DEBOUNCE)
                        .then_some((wrote, Some(empty)));
                }
            }

            // The watchdog: an idle agent with text left in its input box
            // takes no turn, and nothing wakes it (TER-194). Flag it once:
            // a warning, and `blocked` with INPUT_PENDING_REASON, so the
            // daemon mails the leads (TER-208). Text that a person typed
            // is not flagged. A box that stays empty for
            // INPUT_CLEAR_DEBOUNCE makes the agent idle again (TER-224).
            if state.block == Some(Block::Input) {
                if pending {
                    state.input_cleared = None;
                } else if state.status == AgentStatus::Blocked {
                    let cleared = *state.input_cleared.get_or_insert(now);
                    if now.duration_since(cleared) >= INPUT_CLEAR_DEBOUNCE {
                        state.block = None;
                        state.pending_input = None;
                        state.input_cleared = None;
                        self.transition(
                            &mut state,
                            AgentStatus::Idle,
                            "no unsubmitted text in the input box",
                        );
                    }
                }
            } else if pending && state.in_flight.is_none() && state.status == AgentStatus::Idle {
                let (since, flagged) = *state.pending_input.get_or_insert((now, false));
                let typed = state.person_text.is_some();
                if !flagged && !typed && now.duration_since(since) >= self.config.submit_timeout {
                    state.pending_input = Some((since, true));
                    let secs = now.duration_since(since).as_secs();
                    tracing::warn!("unsubmitted text in the input box for {secs} s");
                    self.emit(Event::Warning {
                        message: format!(
                            "the input box of the idle agent holds unsubmitted text \
                             (for {secs} s); no turn can go out until it is submitted \
                             or cleared"
                        ),
                    });
                    state.block = Some(Block::Input);
                    state.input_cleared = None;
                    self.transition(
                        &mut state,
                        AgentStatus::Blocked,
                        terrarium_core::adapter::INPUT_PENDING_REASON,
                    );
                }
            } else {
                state.pending_input = None;
            }
        }

        if let Some(bytes) = clear.filter(|b| !b.is_empty())
            && let Err(e) = self.write_bytes(bytes).await
        {
            tracing::debug!("cannot clear the input line: {e}");
        }
        if let Some(bytes) = to_type.or(to_submit)
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

#[cfg(test)]
mod snapshot_tests {
    use super::ScreenSnapshot;

    #[test]
    fn the_snapshot_text_has_no_escape_codes() {
        let bytes = b"\x1b[2J\x1b[H\x1b[1;31mError:\x1b[0m disk full   \r\n\
            \x1b[38;5;208m> \x1b[7mAllow?\x1b[27m [y/n]\x1b[0m\r\n\x1b[5;3Hlow\x1b]0;title\x07";
        let s = ScreenSnapshot::parse(6, 30, bytes);
        assert_eq!((s.rows, s.cols), (6, 30));
        assert_eq!(s.lines[0], "Error: disk full");
        assert_eq!(s.lines[1], "> Allow? [y/n]");
        assert_eq!(s.lines[4], "  low");
        assert_eq!(s.cursor, (4, 5));
        let text = s.text();
        assert!(!text.contains('\x1b'), "{text:?}");
        assert!(text.lines().all(|l| l == l.trim_end()));
        // The formatted copy keeps the escape codes for an attach.
        assert!(s.formatted.contains(&0x1b));
    }
}
