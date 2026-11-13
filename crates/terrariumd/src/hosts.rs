//! The daemon side of the host agent protocol (`docs/host-protocol.md`):
//! the dial-in endpoint `GET /hosts/connect`, the live host registry, and
//! the session multiplexer.
//!
//! - [`connect`] checks the bearer token against the `hosts` table before
//!   the upgrade (`401` on a bad token), reads `hello`, and answers
//!   `welcome` with a durable resume token and the sessions it takes back.
//! - [`HostRegistry`] holds one entry per host name. Sessions live in the
//!   entry, not in the connection, so they survive a reconnect.
//! - [`HostRegistry::open`] starts a session with an ID from a counter for
//!   each host and returns a [`HostSession`]. The consumer reads
//!   [`SessionEvent`]s from it. Acks follow what the consumer has taken,
//!   so a slow consumer is backpressure on the host's process.
//! - The daemon sends `ack` and `release` after each terminal frame, `ping`
//!   every heartbeat, and closes a link that is silent for 3 heartbeats.
//! - A session the daemon started that is missing from a resumed `hello`,
//!   or that stays without a link for the resume grace, ends with
//!   [`SessionEvent::Lost`].
//! - [`HostListen`] reads `TERRARIUM_HOST_LISTEN`, `TERRARIUM_HOST_TLS_CERT`,
//!   and `TERRARIUM_HOST_TLS_KEY`. With a certificate the listener speaks
//!   TLS ([`TlsListener`], rustls) on `0.0.0.0:8082`; without one it is
//!   plaintext on `127.0.0.1:8082`, and a plaintext bind to another
//!   address logs a warning (security review F2).
//! - An enrol token is swapped for a long-lived token after `hello`; the
//!   new token goes in `welcome.token` (docs/host-protocol.md "Tokens").
//! - The registry keeps the last link error of each host (a bad `hello`, a
//!   spent enrol token, the reason of a disconnect) in memory, and
//!   [`offline_reason`] turns it and the row into the text that the API
//!   shows for an offline host.
//! - `welcome.metrics` asks for `metrics` frames. The registry keeps the
//!   last [`METRICS_HISTORY`] samples of each host in memory only: a
//!   restart of the daemon loses them (docs/api.md §17.1).

use std::collections::{HashMap, VecDeque};
use std::net::SocketAddr;
use std::path::PathBuf;
use std::sync::{Arc, Mutex, MutexGuard, OnceLock};
use std::time::{Duration, Instant};

use axum::extract::connect_info::{ConnectInfo, Connected};
use axum::extract::ws::{Message as WsMessage, WebSocket};
use axum::extract::{FromRequestParts, Request, State, WebSocketUpgrade};
use axum::http::header;
use axum::response::{IntoResponse, Response};
use futures_util::{SinkExt, StreamExt};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
pub use terrarium_host::protocol::METRICS_INTERVAL_SECS;
use terrarium_host::protocol::{
    DaemonFrame, DataFrame, ExitReason, Hello, HostCapabilities, HostFrame, HostMetrics,
    PROTOCOL_VERSION, ResumeAccept, SessionId, StreamTag, Welcome, decode_control, encode_control,
};
use tokio::sync::mpsc;
use tokio::task::JoinHandle;
use tracing::{debug, info, warn};

use crate::api::Shared;
use crate::error::{ApiError, Code};
use crate::events::EventDraft;
use crate::store::hosts::{self, Auth, Host};

pub mod resume;

pub(crate) fn resume_aad(id: &str) -> Vec<u8> {
    format!("host-resume|{id}").into_bytes()
}

/// The heartbeat interval in `welcome`.
pub const HEARTBEAT: Duration = Duration::from_secs(15);
/// A link is dead after this many silent heartbeats.
const MISSED_HEARTBEATS: u32 = 3;
/// How long the daemon keeps the sessions of a host with no link.
pub const RESUME_GRACE: Duration = Duration::from_secs(600);
/// How long the daemon waits for `hello`.
const HELLO_TIMEOUT: Duration = Duration::from_secs(15);
/// The daemon acks after the consumer has taken this many new bytes.
const ACK_EVERY: u64 = 256 * 1024;
/// The largest WebSocket message the daemon accepts from a host.
const MAX_MESSAGE: usize = 2 * 1024 * 1024;
const JOURNAL_RETRY_DEADLINE: Duration = Duration::from_secs(60);
const JOURNAL_RETRY_EVENTS: usize = 64;
const JOURNAL_RETRY_BYTES: usize = 4 * 1024 * 1024;
// Drain retained chunks without inserting gaps larger than the exec
// redactor's idle flush (200 ms). Sustained contention still backs off.
const JOURNAL_RETRY_TICK: Duration = Duration::from_millis(10);
const JOURNAL_RETRY_BACKOFF: Duration = Duration::from_millis(100);

// Metadata calls do not have a host event to retain. Retry the whole
// operation after its registry guard has gone, never sleeping under it.
fn retry_metadata<T, E>(
    mut operation: impl FnMut() -> Result<T, E>,
    busy: impl Fn(&E) -> bool,
) -> Result<T, E> {
    retry_metadata_for(&mut operation, busy, Duration::from_secs(5))
}

fn retry_metadata_for<T, E>(
    mut operation: impl FnMut() -> Result<T, E>,
    busy: impl Fn(&E) -> bool,
    timeout: Duration,
) -> Result<T, E> {
    let deadline = Instant::now() + timeout;
    let mut backoff = Duration::from_millis(1);
    loop {
        match operation() {
            Err(error) if busy(&error) && Instant::now() < deadline => {
                std::thread::sleep(backoff);
                backoff = (backoff * 2).min(Duration::from_millis(20));
            }
            result => return result,
        }
    }
}

fn retry_metadata_api<T>(
    operation: impl FnMut() -> crate::error::ApiResult<T>,
) -> crate::error::ApiResult<T> {
    retry_metadata(operation, |error| error.code == Code::ResourceBusy)
}

struct PendingJournal {
    events: VecDeque<SessionEvent>,
    bytes: usize,
    deadline: Instant,
    next: Instant,
    backoff: Duration,
    exhausted: bool,
}

impl PendingJournal {
    fn new() -> Self {
        let now = Instant::now();
        Self {
            events: VecDeque::new(),
            bytes: 0,
            deadline: now + JOURNAL_RETRY_DEADLINE,
            next: now + JOURNAL_RETRY_TICK,
            backoff: JOURNAL_RETRY_TICK,
            exhausted: false,
        }
    }

    fn push(&mut self, event: SessionEvent) -> bool {
        let bytes = serde_json::to_vec(&event).map_or(usize::MAX, |v| v.len());
        // The receive loop stops at the watermark. Retain the frame that
        // crossed it too, rather than losing data or closing a healthy link.
        self.bytes += bytes;
        self.events.push_back(event);
        true
    }
}
/// How long a TLS handshake on the host listener may take.
const TLS_HANDSHAKE_TIMEOUT: Duration = Duration::from_secs(15);
/// The default dial-in address with TLS: every interface.
pub const DEFAULT_TLS_LISTEN: &str = "0.0.0.0:8082";
/// The default dial-in address without TLS: loopback only.
pub const DEFAULT_PLAIN_LISTEN: &str = "127.0.0.1:8082";
/// The metrics samples kept for each host: 1 hour at one sample each
/// [`METRICS_INTERVAL_SECS`].
pub const METRICS_HISTORY: usize = 360;
/// The latest metrics sample is stale when it is older than this, or when
/// the host is offline.
pub const METRICS_STALE: Duration = Duration::from_secs(30);

/// What a session reports to its consumer.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub enum SessionEvent {
    Started {
        pid: Option<u32>,
    },
    Data {
        stream: StreamTag,
        bytes: Vec<u8>,
    },
    /// Structured output of a verb; it comes before the terminal event.
    Result(Value),
    Exit {
        code: Option<i32>,
        signal: Option<i32>,
        reason: ExitReason,
    },
    Error {
        code: String,
        message: String,
    },
    CpDone {
        bytes: u64,
        sha256: String,
    },
    /// The daemon gave the session up: the host lost it, or it stayed
    /// without a link past the resume grace, or the host was revoked.
    Lost {
        message: String,
    },
}

impl SessionEvent {
    pub fn is_terminal(&self) -> bool {
        matches!(
            self,
            Self::Exit { .. } | Self::Error { .. } | Self::CpDone { .. } | Self::Lost { .. }
        )
    }
}

/// Why a session could not start.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum HostError {
    #[error("host agent `{0}` is offline")]
    Offline(String),
    #[error("host agent `{0}` runs its maximum of {1} sessions")]
    Busy(String, u32),
    #[error("cannot persist host session state")]
    Persistence,
    #[error("host journal writer is busy")]
    JournalBusy,
    #[error("Windows run-as requires a Windows host with the windows_run_as capability")]
    WindowsRunAsUnsupported,
}

impl From<HostError> for ApiError {
    fn from(e: HostError) -> Self {
        match e {
            HostError::Offline(_) => ApiError::conflict(e.to_string()),
            HostError::Busy(..) => ApiError::new(Code::RateLimited, e.to_string()),
            HostError::Persistence => ApiError::internal(e.to_string()),
            HostError::JournalBusy => ApiError::new(Code::ResourceBusy, e.to_string()),
            HostError::WindowsRunAsUnsupported => ApiError::invalid(e.to_string()),
        }
    }
}

enum Outgoing {
    Frame(Box<DaemonFrame>),
    Data(DataFrame),
    /// Close the link, with a `goodbye` when a reason is given.
    Close(Option<(&'static str, String)>),
}

impl Outgoing {
    fn frame(f: DaemonFrame) -> Self {
        Self::Frame(Box::new(f))
    }
}

#[derive(Clone)]
struct Conn {
    id: u64,
    tx: mpsc::UnboundedSender<Outgoing>,
}

struct Slot {
    tx: mpsc::UnboundedSender<SessionEvent>,
    /// Output bytes received from the host.
    received: u64,
    /// The host holds state for the session: it sent `started`, or it
    /// offered the session in a resume.
    held: bool,
    events: u64,
    terminal: bool,
    ended_at: Option<i64>,
    output_events: VecDeque<(u64, usize)>,
    replay_start: u64,
    restored: Option<mpsc::UnboundedReceiver<SessionEvent>>,
}

impl Slot {
    fn prune_count(&self) -> usize {
        let mut bytes: usize = self.output_events.iter().map(|(_, bytes)| bytes).sum();
        let mut count = self.output_events.len();
        let mut prune = 0;
        for (_, len) in &self.output_events {
            if bytes <= crate::executor::SESSION_BUFFER && count <= 4096 {
                break;
            }
            bytes -= len;
            count -= 1;
            prune += 1;
        }
        prune
    }
}

#[derive(Serialize, Deserialize)]
struct SavedSlot {
    received: u64,
    held: bool,
    events: u64,
    terminal: bool,
    #[serde(default)]
    ended_at: Option<i64>,
}

#[derive(Serialize, Deserialize)]
struct SavedHost {
    token: Option<String>,
    next_id: u32,
    sessions: HashMap<SessionId, SavedSlot>,
}

/// The live state of a host, from its last `hello`.
#[derive(Debug, Clone, Serialize)]
pub struct LiveInfo {
    pub path_kind: terrarium_host::protocol::PathKind,
    pub host_id: String,
    pub version: String,
    pub os: String,
    pub arch: String,
    pub capabilities: HostCapabilities,
    pub max_sessions: u32,
    /// The `max_running_targets` of the host agent's config. `None`: an
    /// older host agent, no limit.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub max_running_targets: Option<u32>,
    pub connected_at: String,
}

#[derive(Default)]
struct Entry {
    /// Uncommitted events are never exposed or acknowledged. Discard only
    /// on detach, when the host takes responsibility for replay again.
    pending_journal: HashMap<SessionId, PendingJournal>,
    /// Terminal intentions survive a retry deadline without becoming
    /// resumable. Driven independently of any host link.
    pending_lost: HashMap<SessionId, SessionEvent>,
    lost_retry_running: bool,
    conn: Option<Conn>,
    resume_token: Option<String>,
    sessions: HashMap<SessionId, Slot>,
    next_id: u32,
    info: Option<LiveInfo>,
    /// Counts links, so a grace timer knows if the host came back.
    generation: u64,
    /// The last [`METRICS_HISTORY`] samples, oldest first. They stay when
    /// the link drops.
    metrics: VecDeque<MetricsSample>,
    /// The last thing that ended or refused a link, since the daemon
    /// started. A new link clears it.
    last_error: Option<LinkError>,
    /// The `hello.update_failed` that this daemon recorded already, so a
    /// reconnect does not record it again.
    update_failed_seen: Option<String>,
}

/// Why the last link of a host failed or ended, and when.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct LinkError {
    /// RFC 3339.
    pub at: String,
    pub message: String,
}

/// One `metrics` frame, with the time the daemon got it.
#[derive(Debug, Clone, Serialize)]
pub struct MetricsSample {
    /// When the daemon got the sample (RFC 3339). The host's own clock
    /// does not count.
    pub at: String,
    #[serde(skip)]
    pub received: Instant,
    #[serde(flatten)]
    pub metrics: HostMetrics,
}

/// The metrics of one host for the API.
#[derive(Debug, Clone, Serialize)]
pub struct MetricsView {
    /// No sample in the last [`METRICS_STALE`], or the host is offline.
    pub stale: bool,
    /// Oldest first.
    pub samples: Vec<MetricsSample>,
}

/// What `GET /api/hosts` shows of a host's link.
#[derive(Debug, Clone, Serialize)]
pub struct LiveView {
    pub online: bool,
    pub sessions: usize,
    #[serde(flatten)]
    pub info: Option<LiveInfo>,
}

/// The hosts that are or were connected since the daemon started.
pub struct HostRegistry {
    entries: Mutex<HashMap<String, Entry>>,
    /// Lock order: metadata, then entries, then a nonblocking DB attempt.
    /// Never acquire metadata while holding entries or the DB. A newer
    /// save/delete cannot be overtaken by an older retry.
    metadata: Mutex<()>,
    next_conn: std::sync::atomic::AtomicU64,
    pub heartbeat: Duration,
    pub resume_grace: Duration,
    journal: OnceLock<resume::Journal>,
    online_changed: tokio::sync::Notify,
    stopped: std::sync::atomic::AtomicBool,
    ownership_epoch: std::sync::atomic::AtomicU64,
}

impl Default for HostRegistry {
    fn default() -> Self {
        Self::new(HEARTBEAT, RESUME_GRACE)
    }
}

fn send_events(slots: impl IntoIterator<Item = Slot>, message: &str) {
    for s in slots {
        let _ = s.tx.send(SessionEvent::Lost {
            message: message.to_owned(),
        });
    }
}

impl HostRegistry {
    pub fn new(heartbeat: Duration, resume_grace: Duration) -> Self {
        Self {
            entries: Mutex::default(),
            metadata: Mutex::default(),
            next_conn: std::sync::atomic::AtomicU64::new(1),
            heartbeat,
            resume_grace,
            journal: OnceLock::new(),
            online_changed: tokio::sync::Notify::new(),
            stopped: std::sync::atomic::AtomicBool::new(false),
            ownership_epoch: std::sync::atomic::AtomicU64::new(0),
        }
    }

    /// Restore before the HTTP/host listeners and process-wait reconciliation.
    #[cfg(test)]
    fn restore(&self, state_dir: &std::path::Path) -> crate::error::ApiResult<()> {
        self.restore_journal(resume::Journal::new(state_dir))
    }

    pub fn restore_with_db(
        &self,
        state_dir: &std::path::Path,
        db: crate::db::Db,
    ) -> crate::error::ApiResult<()> {
        self.restore_journal(resume::Journal::for_registry(state_dir, db))
    }

    fn restore_journal(&self, journal: resume::Journal) -> crate::error::ApiResult<()> {
        let saved = journal.scan::<SavedHost>("host/")?;
        let mut entries = HashMap::new();
        for (key, saved) in saved {
            let host = key
                .strip_prefix("host/")
                .ok_or_else(|| ApiError::internal("invalid host journal key"))?
                .to_owned();
            let mut entry = Entry {
                resume_token: saved.token,
                next_id: saved.next_id,
                ..Entry::default()
            };
            for (id, saved) in saved.sessions {
                let (tx, rx) = mpsc::unbounded_channel();
                let mut output_events = VecDeque::new();
                let mut replay_bytes = 0;
                for (key, ev) in journal.scan::<SessionEvent>(&format!("event/{host}/{id}/"))? {
                    if let SessionEvent::Data { stream, bytes } = &ev {
                        replay_bytes += bytes.len() as u64;
                        if matches!(stream, StreamTag::Stdout | StreamTag::Stderr) {
                            let seq = key
                                .rsplit('/')
                                .next()
                                .and_then(|seq| seq.parse().ok())
                                .ok_or_else(|| ApiError::internal("bad replay event sequence"))?;
                            output_events.push_back((seq, bytes.len()));
                        }
                    }
                    let _ = tx.send(ev);
                }
                entry.sessions.insert(
                    id,
                    Slot {
                        tx,
                        received: saved.received,
                        held: saved.held,
                        events: saved.events,
                        terminal: saved.terminal,
                        ended_at: saved.ended_at,
                        output_events,
                        replay_start: saved.received.saturating_sub(replay_bytes),
                        restored: Some(rx),
                    },
                );
            }
            entries.insert(host, entry);
        }
        self.lock().extend(entries);
        self.journal
            .set(journal)
            .map_err(|_| ApiError::internal("host journal already restored"))?;
        Ok(())
    }

    fn persist(
        &self,
        host: &str,
        entry: &Entry,
        event: Option<(SessionId, &SessionEvent)>,
    ) -> crate::error::ApiResult<()> {
        self.persist_bound(host, entry, event, None)
    }

    fn persist_bound(
        &self,
        host: &str,
        entry: &Entry,
        event: Option<(SessionId, &SessionEvent)>,
        binding: Option<(&str, SessionId)>,
    ) -> crate::error::ApiResult<()> {
        self.persist_events(host, entry, &event.into_iter().collect::<Vec<_>>(), binding)
    }

    fn persist_events(
        &self,
        host: &str,
        entry: &Entry,
        events: &[(SessionId, &SessionEvent)],
        binding: Option<(&str, SessionId)>,
    ) -> crate::error::ApiResult<()> {
        if self.stopped.load(std::sync::atomic::Ordering::Acquire) {
            return Err(ApiError::internal("host registry is stopping"));
        }
        let Some(journal) = self.journal.get() else {
            return Ok(());
        };
        let saved = SavedHost {
            token: entry.resume_token.clone(),
            next_id: entry.next_id,
            sessions: entry
                .sessions
                .iter()
                .map(|(&id, s)| {
                    (
                        id,
                        SavedSlot {
                            received: s.received,
                            held: s.held,
                            events: s.events,
                            terminal: s.terminal,
                            ended_at: s.ended_at,
                        },
                    )
                })
                .collect(),
        };
        let encode = |v| {
            serde_json::to_value(v)
                .map_err(|_| ApiError::internal("cannot encode host resume state"))
        };
        let mut values = Vec::new();
        for (id, event) in events {
            let seq = entry.sessions[id].events;
            values.push((
                format!("event/{host}/{id}/{seq:020}"),
                serde_json::to_value(event)
                    .map_err(|_| ApiError::internal("cannot encode host session event"))?,
            ));
        }
        if let Some((key, id)) = binding {
            values.push((format!("bound/{key}"), json!((host, id))));
        }
        // The final host offset and every event share one journal commit.
        values.push((format!("host/{host}"), encode(&saved)?));
        let removed: Vec<_> = events
            .iter()
            .flat_map(|(id, _)| {
                let slot = &entry.sessions[id];
                slot.output_events
                    .iter()
                    .take(slot.prune_count())
                    .map(move |(seq, _)| format!("event/{host}/{id}/{seq:020}"))
            })
            .collect();
        journal.replace(&values, &removed, &[])?;
        if binding.is_some() {
            self.ownership_epoch
                .fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        }
        Ok(())
    }

    /// Stage terminal reconciliation under entries, commit all rows once,
    /// then publish. No receiver or replacement connection sees a partial
    /// attach, including when a later row fails inside the transaction.
    fn reconcile_lost(
        &self,
        host: &str,
        entry: &mut Entry,
        lost: &[(SessionId, SessionEvent)],
        resume: &[ResumeAccept],
        token: Option<String>,
    ) -> crate::error::ApiResult<()> {
        let old_token = entry.resume_token.clone();
        let mut old = Vec::new();
        for (id, _) in lost {
            if let Some(s) = entry.sessions.get_mut(id) {
                old.push((*id, s.events, s.held, s.terminal, s.ended_at));
                s.events += 1;
                s.terminal = true;
                s.ended_at = Some(chrono::Utc::now().timestamp());
            }
        }
        for r in resume {
            if let Some(s) = entry.sessions.get_mut(&r.session) {
                old.push((r.session, s.events, s.held, s.terminal, s.ended_at));
                s.held = true;
            }
        }
        entry.resume_token = token;
        let events: Vec<_> = lost.iter().map(|(id, event)| (*id, event)).collect();
        if let Err(error) = self.persist_events(host, entry, &events, None) {
            entry.resume_token = old_token;
            for (id, events, held, terminal, ended_at) in old {
                if let Some(s) = entry.sessions.get_mut(&id) {
                    (s.events, s.held, s.terminal, s.ended_at) = (events, held, terminal, ended_at);
                }
            }
            return Err(error);
        }
        for (id, event) in lost {
            if let Some(s) = entry.sessions.get_mut(id) {
                let prune = s.prune_count();
                s.output_events.drain(..prune);
                let _ = s.tx.send(event.clone());
            }
            entry.pending_lost.remove(id);
            entry.pending_journal.remove(id);
        }
        Ok(())
    }

    /// Forget an explicitly closed or expired exec and its replay together.
    pub fn forget_exec(&self, id: &str) -> crate::error::ApiResult<()> {
        let _metadata = self.metadata.lock().unwrap_or_else(|p| p.into_inner());
        retry_metadata_api(|| self.try_forget_exec(id))
    }

    fn try_forget_exec(&self, id: &str) -> crate::error::ApiResult<()> {
        let Some(journal) = self.journal.get() else {
            return Ok(());
        };
        #[derive(Deserialize)]
        struct Binding {
            host_session: Option<(String, SessionId)>,
            #[serde(default)]
            remote_key: Option<String>,
        }
        let key = format!("exec/{id}");
        let mut entries = self.lock();
        if self.stopped.load(std::sync::atomic::Ordering::Acquire) {
            return Err(ApiError::internal("host registry is stopping"));
        }
        let binding = journal.load::<Binding>(&key)?;
        let mut values = Vec::new();
        let mut prefixes = Vec::new();
        let mut removed = vec![key.clone()];
        if let Some(remote) = binding.as_ref().and_then(|b| b.remote_key.as_ref()) {
            removed.push(format!("ssh/{remote}"));
        }
        let session = binding.and_then(|b| b.host_session);
        if let Some((host, session)) = &session {
            let host_key = format!("host/{host}");
            if let Some(mut saved) = journal.load::<SavedHost>(&host_key)? {
                saved.sessions.remove(session);
                values.push((
                    host_key,
                    serde_json::to_value(saved)
                        .map_err(|_| ApiError::internal("cannot encode host cleanup"))?,
                ));
            }
            prefixes.push(format!("event/{host}/{session}/"));
        }
        journal.replace(&values, &removed, &prefixes)?;
        if let Some((host, session)) = session
            && let Some(entry) = entries.get_mut(&host)
        {
            entry.sessions.remove(&session);
            entry.pending_journal.remove(&session);
            entry.pending_lost.remove(&session);
            if let Some(conn) = &entry.conn {
                let _ = conn
                    .tx
                    .send(Outgoing::frame(DaemonFrame::Release { session }));
            }
        }
        Ok(())
    }

    /// Discard checkpoints only after the queue stored the job's final result.
    pub fn forget_job(&self, job: &str) -> crate::error::ApiResult<()> {
        let _metadata = self.metadata.lock().unwrap_or_else(|p| p.into_inner());
        retry_metadata_api(|| self.try_forget_job(job))
    }

    fn try_forget_job(&self, job: &str) -> crate::error::ApiResult<()> {
        let Some(journal) = self.journal.get() else {
            return Ok(());
        };
        let mut entries = self.lock();
        let mut prefixes = vec![
            format!("prepare/{job}/"),
            format!("recipe/{job}/"),
            format!("ssh/recipe/{job}/"),
        ];
        let mut removed = Vec::new();
        for phase in ["recipe", "receive"] {
            let prefix = format!("bound/{phase}/{job}/");
            for (_, (host, id)) in journal.scan::<(String, SessionId)>(&prefix)? {
                removed.push((host, id));
            }
            prefixes.push(prefix);
        }
        if self.stopped.load(std::sync::atomic::Ordering::Acquire) {
            return Err(ApiError::internal("host registry is stopping"));
        }
        let mut hosts = HashMap::<String, SavedHost>::new();
        for (host, id) in &removed {
            if !hosts.contains_key(host)
                && let Some(saved) = journal.load::<SavedHost>(&format!("host/{host}"))?
            {
                hosts.insert(host.clone(), saved);
            }
            if let Some(saved) = hosts.get_mut(host) {
                saved.sessions.remove(id);
            }
            prefixes.push(format!("event/{host}/{id}/"));
        }
        let mut values = Vec::new();
        for (host, saved) in hosts {
            values.push((
                format!("host/{host}"),
                serde_json::to_value(saved)
                    .map_err(|_| ApiError::internal("cannot encode job session cleanup"))?,
            ));
        }
        journal.replace(&values, &[], &prefixes)?;
        for (host, id) in removed {
            if let Some(entry) = entries.get_mut(&host) {
                entry.sessions.remove(&id);
                entry.pending_journal.remove(&id);
                entry.pending_lost.remove(&id);
            }
        }
        Ok(())
    }

    /// Freeze journal writers before releasing the daemon's state lock.
    /// Close without goodbye so host children remain available for resume.
    pub fn shutdown(&self) {
        let entries = self.lock();
        self.stopped
            .store(true, std::sync::atomic::Ordering::Release);
        for entry in entries.values() {
            if let Some(conn) = &entry.conn {
                let _ = conn.tx.send(Outgoing::Close(None));
            }
        }
    }

    pub(crate) fn is_stopped(&self) -> bool {
        self.stopped.load(std::sync::atomic::Ordering::Acquire)
    }

    /// Claim the replay channel of a session from the previous daemon.
    pub fn reattach(self: &Arc<Self>, host: &str, id: SessionId) -> Option<HostSession> {
        let mut entries = self.lock();
        let slot = entries.get_mut(host)?.sessions.get_mut(&id)?;
        let rx = slot.restored.take()?;
        let start = slot.replay_start;
        Some(HostSession {
            registry: self.clone(),
            host: host.to_owned(),
            id,
            rx,
            finished: false,
            consumed: start,
            acked: start,
        })
    }

    pub fn spawn_resume_grace(self: &Arc<Self>) -> JoinHandle<()> {
        let hosts: Vec<_> = self
            .lock()
            .iter()
            .map(|(host, entry)| (host.clone(), entry.generation))
            .collect();
        let registry = Arc::downgrade(self);
        let grace = self.resume_grace;
        tokio::spawn(async move {
            tokio::time::sleep(grace).await;
            if let Some(registry) = registry.upgrade() {
                for (host, generation) in hosts {
                    registry.expire(&host, generation);
                }
            }
            loop {
                tokio::time::sleep(Duration::from_secs(30)).await;
                let Some(registry) = registry.upgrade() else {
                    return;
                };
                let hosts: Vec<_> = registry
                    .lock()
                    .iter()
                    .filter(|(_, e)| !e.pending_lost.is_empty())
                    .map(|(host, _)| host.clone())
                    .collect();
                for host in hosts {
                    registry.drive_lost(&host);
                }
                if let Err(error) = registry.prune_unowned(chrono::Utc::now().timestamp() - 300) {
                    warn!(%error, "cannot prune orphan host replay");
                }
            }
        })
    }

    /// Finished helper sessions have no durable owner. Keep a five-minute
    /// grace for a fast exec to acquire its owner before removing replay.
    fn prune_unowned(&self, before: i64) -> crate::error::ApiResult<()> {
        let _metadata = self.metadata.lock().unwrap_or_else(|p| p.into_inner());
        retry_metadata_api(|| self.try_prune_unowned(before))
    }

    fn try_prune_unowned(&self, before: i64) -> crate::error::ApiResult<()> {
        let Some(journal) = self.journal.get() else {
            return Ok(());
        };
        let epoch = self
            .ownership_epoch
            .load(std::sync::atomic::Ordering::SeqCst);
        #[derive(Deserialize)]
        struct Binding {
            host_session: Option<(String, SessionId)>,
        }
        let mut owned: std::collections::HashSet<_> = journal
            .scan::<Binding>("exec/")?
            .into_iter()
            .filter_map(|(_, saved)| saved.host_session)
            .collect();
        owned.extend(
            journal
                .scan::<(String, SessionId)>("bound/")?
                .into_iter()
                .map(|(_, saved)| saved),
        );
        let mut entries = self.lock();
        // A new owner may have appeared while the scan ran without this lock.
        // Leave cleanup to the next tick rather than deleting its replay.
        if epoch
            != self
                .ownership_epoch
                .load(std::sync::atomic::Ordering::SeqCst)
        {
            return Ok(());
        }
        if self.stopped.load(std::sync::atomic::Ordering::Acquire) {
            return Ok(());
        }
        for (host, entry) in entries.iter_mut() {
            let ids: Vec<_> = entry
                .sessions
                .iter()
                .filter(|(id, slot)| {
                    slot.terminal
                        && slot.ended_at.unwrap_or(0) <= before
                        && !owned.contains(&(host.clone(), **id))
                })
                .map(|(id, _)| *id)
                .collect();
            if ids.is_empty() {
                continue;
            }
            let key = format!("host/{host}");
            let Some(mut saved) = journal.load::<SavedHost>(&key)? else {
                continue;
            };
            for id in &ids {
                saved.sessions.remove(id);
            }
            let value = serde_json::to_value(saved)
                .map_err(|_| ApiError::internal("cannot encode orphan cleanup"))?;
            let prefixes: Vec<_> = ids.iter().map(|id| format!("event/{host}/{id}/")).collect();
            journal.replace(&[(key, value)], &[], &prefixes)?;
            for id in ids {
                entry.sessions.remove(&id);
                entry.pending_journal.remove(&id);
                entry.pending_lost.remove(&id);
            }
        }
        Ok(())
    }

    fn lock(&self) -> MutexGuard<'_, HashMap<String, Entry>> {
        self.entries.lock().unwrap_or_else(|e| e.into_inner())
    }

    pub fn save_exec(&self, id: &str, value: &impl Serialize) -> crate::error::ApiResult<()> {
        let _metadata = self.metadata.lock().unwrap_or_else(|p| p.into_inner());
        retry_metadata_api(|| self.try_save_exec(id, value))
    }

    fn try_save_exec(&self, id: &str, value: &impl Serialize) -> crate::error::ApiResult<()> {
        let _entries = self.lock();
        if self.stopped.load(std::sync::atomic::Ordering::Acquire) {
            return Err(ApiError::internal("host registry is stopping"));
        }
        if let Some(journal) = self.journal.get() {
            journal.save(&format!("exec/{id}"), value)?;
        }
        self.ownership_epoch
            .fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        Ok(())
    }

    /// Is the host connected now?
    pub fn is_online(&self, host: &str) -> bool {
        self.lock().get(host).is_some_and(|e| e.conn.is_some())
    }

    pub async fn wait_online(&self, host: &str) -> bool {
        tokio::time::timeout(self.resume_grace, async {
            loop {
                let changed = self.online_changed.notified();
                tokio::pin!(changed);
                changed.as_mut().enable();
                if self.is_online(host) {
                    return;
                }
                changed.await;
            }
        })
        .await
        .is_ok()
    }

    /// The live state of a host.
    pub fn view(&self, host: &str) -> LiveView {
        let g = self.lock();
        match g.get(host) {
            Some(e) => LiveView {
                online: e.conn.is_some(),
                sessions: e.sessions.values().filter(|s| !s.terminal).count(),
                info: e.info.clone(),
            },
            None => LiveView {
                online: false,
                sessions: 0,
                info: None,
            },
        }
    }

    /// Keep the reason that a link of `host` failed or ended.
    pub fn note_error(&self, host: &str, message: impl Into<String>) {
        self.lock().entry(host.to_owned()).or_default().last_error = Some(LinkError {
            at: crate::db::now(),
            message: message.into(),
        });
    }

    /// True the first time this daemon sees `version` as the failed
    /// self-update of `host`.
    fn first_update_failure(&self, host: &str, version: &str) -> bool {
        let mut g = self.lock();
        let e = g.entry(host.to_owned()).or_default();
        if e.update_failed_seen.as_deref() == Some(version) {
            return false;
        }
        e.update_failed_seen = Some(version.to_owned());
        true
    }

    /// The last link error of `host` since the daemon started.
    pub fn last_error(&self, host: &str) -> Option<LinkError> {
        self.lock().get(host).and_then(|e| e.last_error.clone())
    }

    /// The `max_running_targets` of a host from its last `hello`.
    pub fn max_running_targets(&self, host: &str) -> Option<u32> {
        self.lock()
            .get(host)
            .and_then(|e| e.info.as_ref().and_then(|i| i.max_running_targets))
    }

    /// Keep a `metrics` sample that the daemon got at `received`.
    pub fn record_metrics(&self, host: &str, metrics: HostMetrics, received: Instant) {
        let mut g = self.lock();
        let Some(e) = g.get_mut(host) else {
            return;
        };
        while e.metrics.len() >= METRICS_HISTORY {
            e.metrics.pop_front();
        }
        e.metrics.push_back(MetricsSample {
            at: crate::db::now(),
            received,
            metrics,
        });
    }

    /// The samples of a host from the last `window`, oldest first. `None`
    /// when the host has no samples.
    pub fn metrics(&self, host: &str, window: Duration) -> Option<MetricsView> {
        let g = self.lock();
        let e = g.get(host)?;
        let last = e.metrics.back()?;
        let now = Instant::now();
        let stale =
            e.conn.is_none() || now.saturating_duration_since(last.received) > METRICS_STALE;
        let samples = e
            .metrics
            .iter()
            .filter(|s| now.saturating_duration_since(s.received) <= window)
            .cloned()
            .collect();
        Some(MetricsView { stale, samples })
    }

    /// The latest sample of a host, and whether it is stale.
    pub fn latest_metrics(&self, host: &str) -> Option<(MetricsSample, bool)> {
        let g = self.lock();
        let e = g.get(host)?;
        let last = e.metrics.back()?;
        let stale = e.conn.is_none()
            || Instant::now().saturating_duration_since(last.received) > METRICS_STALE;
        Some((last.clone(), stale))
    }

    /// The capabilities of a host from its last `hello`.
    pub fn capabilities(&self, host: &str) -> Option<HostCapabilities> {
        self.lock()
            .get(host)
            .and_then(|e| e.info.as_ref().map(|i| i.capabilities.clone()))
    }

    pub fn supports_windows_run_as(&self, host: &str) -> bool {
        self.lock().get(host).is_some_and(|e| {
            e.conn.is_some()
                && e.info
                    .as_ref()
                    .is_some_and(|i| i.os == "windows" && i.capabilities.windows_run_as)
        })
    }

    /// Native path syntax from the last hello. Container paths stay POSIX.
    pub fn path_kind(&self, host: &str) -> terrarium_host::protocol::PathKind {
        self.lock()
            .get(host)
            .and_then(|e| e.info.as_ref().map(|i| i.path_kind))
            .unwrap_or_default()
    }

    fn send(&self, host: &str, out: Outgoing) -> bool {
        match self.lock().get(host).and_then(|e| e.conn.clone()) {
            Some(c) => c.tx.send(out).is_ok(),
            None => false,
        }
    }

    /// Start a session: pick an ID, then send the frame that `make` builds.
    pub fn open(
        self: &Arc<Self>,
        host: &str,
        make: impl FnOnce(SessionId) -> DaemonFrame,
    ) -> Result<HostSession, HostError> {
        self.open_bound(host, None, make)
    }

    pub fn open_bound(
        self: &Arc<Self>,
        host: &str,
        binding: Option<&str>,
        make: impl FnOnce(SessionId) -> DaemonFrame,
    ) -> Result<HostSession, HostError> {
        let _metadata = self.metadata.lock().unwrap_or_else(|p| p.into_inner());
        let mut make = Some(make);
        let mut prepared = None;
        retry_metadata(
            || self.try_open_bound(host, binding, &mut make, &mut prepared),
            |error| matches!(error, HostError::JournalBusy),
        )
    }

    fn try_open_bound(
        self: &Arc<Self>,
        host: &str,
        binding: Option<&str>,
        make: &mut Option<impl FnOnce(SessionId) -> DaemonFrame>,
        prepared: &mut Option<(SessionId, DaemonFrame)>,
    ) -> Result<HostSession, HostError> {
        let mut g = self.lock();
        let e = g
            .get_mut(host)
            .filter(|e| e.conn.is_some())
            .ok_or_else(|| HostError::Offline(host.to_owned()))?;
        let max = e.info.as_ref().map_or(32, |i| i.max_sessions.max(1));
        if e.sessions.values().filter(|s| !s.terminal).count() >= max as usize {
            return Err(HostError::Busy(host.to_owned(), max));
        }
        let previous_next = e.next_id;
        let id = loop {
            e.next_id = e.next_id.wrapping_add(1);
            if e.next_id != 0 && !e.sessions.contains_key(&e.next_id) {
                break e.next_id;
            }
        };
        let (tx, rx) = mpsc::unbounded_channel();
        // Windows capability checks need the frame before persistence.
        // Keep the FnOnce result across retries rather than consuming it again.
        if prepared.is_none() {
            let Some(make) = make.take() else {
                e.next_id = previous_next;
                return Err(HostError::Persistence);
            };
            *prepared = Some((id, make(id)));
        }
        let Some((prepared_id, frame)) = prepared.as_ref() else {
            e.next_id = previous_next;
            return Err(HostError::Persistence);
        };
        if *prepared_id != id {
            e.next_id = previous_next;
            return Err(HostError::Persistence);
        }
        let has_run_as = match frame {
            DaemonFrame::Exec(req) => req.windows_run_as.is_some(),
            DaemonFrame::CpPush(req) => req.windows_run_as.is_some(),
            DaemonFrame::CpPull(req) => req.windows_run_as.is_some(),
            _ => false,
        };
        if has_run_as
            && !e
                .info
                .as_ref()
                .is_some_and(|i| i.os == "windows" && i.capabilities.windows_run_as)
        {
            e.next_id = previous_next;
            return Err(HostError::WindowsRunAsUnsupported);
        }
        e.sessions.insert(
            id,
            Slot {
                tx,
                received: 0,
                held: false,
                events: 0,
                terminal: false,
                ended_at: None,
                output_events: VecDeque::new(),
                replay_start: 0,
                restored: None,
            },
        );
        if let Err(error) = self.persist_bound(host, e, None, binding.map(|key| (key, id))) {
            e.sessions.remove(&id);
            e.next_id = previous_next;
            return Err(if error.code == Code::ResourceBusy {
                HostError::JournalBusy
            } else {
                HostError::Persistence
            });
        }
        let Some((_, frame)) = prepared.take() else {
            return Err(HostError::Persistence);
        };
        if let Some(c) = &e.conn {
            let _ = c.tx.send(Outgoing::frame(frame));
        }
        Ok(HostSession {
            registry: self.clone(),
            host: host.to_owned(),
            id,
            rx,
            finished: false,
            consumed: 0,
            acked: 0,
        })
    }

    /// The host sent `hello` on a new link. Returns the `welcome`, the link
    /// it replaces, and how many sessions it resumes.
    fn attach(
        &self,
        row: &Host,
        hello: &Hello,
        conn: Conn,
    ) -> crate::error::ApiResult<(Welcome, Option<Conn>, usize, u64)> {
        let _metadata = self.metadata.lock().unwrap_or_else(|p| p.into_inner());
        retry_metadata_api(|| self.try_attach(row, hello, conn.clone()))
    }

    fn try_attach(
        &self,
        row: &Host,
        hello: &Hello,
        conn: Conn,
    ) -> crate::error::ApiResult<(Welcome, Option<Conn>, usize, u64)> {
        let mut g = self.lock();
        let e = g.entry(row.name.clone()).or_default();
        // Keep the reconnect capability stable. Rotating it before welcome
        // is received can strand live sessions if either peer disconnects
        // after this commit but before the host saves the new token.
        // The separately authenticated host credential still gates attach.
        let token = e
            .resume_token
            .clone()
            .unwrap_or_else(|| format!("r-{}", ulid::Ulid::new()));
        let offers: HashMap<SessionId, u64> = match (&hello.resume, &e.resume_token) {
            (Some(r), Some(t)) if r.token == *t => {
                r.sessions.iter().map(|o| (o.session, o.sent)).collect()
            }
            _ => HashMap::new(),
        };
        let mut resume = Vec::new();
        let ids: Vec<SessionId> = e.sessions.keys().copied().collect();
        let mut lost = Vec::new();
        for id in ids {
            if e.sessions[&id].terminal {
                continue;
            }
            if offers.contains_key(&id) && !e.pending_lost.contains_key(&id) {
                if let Some(s) = e.sessions.get(&id) {
                    resume.push(ResumeAccept {
                        session: id,
                        received: s.received,
                    });
                }
            } else {
                lost.push((
                    id,
                    e.pending_lost
                        .get(&id)
                        .cloned()
                        .unwrap_or_else(|| SessionEvent::Lost {
                            message: "the host agent no longer holds the session".into(),
                        }),
                ));
            }
        }
        resume.sort_by_key(|r| r.session);
        lost.sort_by_key(|(id, _)| *id);
        let info = LiveInfo {
            host_id: row.id.clone(),
            version: hello.version.clone(),
            os: hello.os.clone(),
            arch: hello.arch.clone(),
            capabilities: hello.capabilities.clone(),
            path_kind: hello.path_kind,
            max_sessions: hello.max_sessions,
            max_running_targets: hello.max_running_targets,
            connected_at: crate::db::now(),
        };
        let n = resume.len();
        self.reconcile_lost(&row.name, e, &lost, &resume, Some(token.clone()))?;
        let old = e.conn.replace(conn);
        e.generation += 1;
        e.last_error = None;
        e.info = Some(info);
        // A replacement link replays from committed offsets, not from the
        // uncommitted queue owned by its predecessor.
        e.pending_journal.clear();
        self.online_changed.notify_waiters();
        Ok((
            Welcome {
                host_id: row.id.clone(),
                heartbeat_secs: self.heartbeat.as_secs().max(1),
                resume_token: token,
                resume,
                token: None,
                metrics: true,
                update: None,
            },
            old,
            n,
            e.generation,
        ))
    }

    /// The link `conn_id` ended. True when it was the current link.
    fn detach(&self, host: &str, conn_id: u64) -> Option<u64> {
        let mut g = self.lock();
        let e = g.get_mut(host)?;
        if e.conn.as_ref().is_some_and(|c| c.id == conn_id) {
            e.conn = None;
            e.pending_journal.clear();
            Some(e.generation)
        } else {
            None
        }
    }

    /// After the resume grace, give up the sessions of a host that did not
    /// come back.
    fn expire(self: &Arc<Self>, host: &str, generation: u64) {
        // Record the terminal intention before releasing entries, so attach
        // cannot resume it even if the writer stays busy past our deadline.
        let mut g = self.lock();
        if let Some(e) = g.get_mut(host)
            && e.conn.is_none()
            && e.generation == generation
        {
            for (&id, s) in &e.sessions {
                if !s.terminal {
                    e.pending_lost
                        .entry(id)
                        .or_insert_with(|| SessionEvent::Lost {
                            message: "the host agent did not come back within the resume grace"
                                .into(),
                        });
                }
            }
        }
        drop(g);
        self.drive_lost(host);
    }

    fn drive_lost(self: &Arc<Self>, host: &str) {
        {
            let mut g = self.lock();
            let Some(e) = g.get_mut(host) else { return };
            if e.pending_lost.is_empty() || e.lost_retry_running {
                return;
            }
            e.lost_retry_running = true;
        }
        let registry = self.clone();
        let host = host.to_owned();
        let work = move || {
            registry.finish_pending_lost(&host, JOURNAL_RETRY_DEADLINE);
            if let Some(e) = registry.lock().get_mut(&host) {
                e.lost_retry_running = false;
            }
        };
        if let Ok(runtime) = tokio::runtime::Handle::try_current() {
            runtime.spawn_blocking(work);
        } else {
            work();
        }
    }

    fn finish_pending_lost(&self, host: &str, timeout: Duration) {
        let _metadata = self.metadata.lock().unwrap_or_else(|p| p.into_inner());
        let result = retry_metadata_for(
            || {
                let mut g = self.lock();
                let Some(e) = g.get_mut(host) else {
                    return Ok(());
                };
                // Output already retained on a live link must finish first.
                if e.conn.is_some()
                    && e.pending_lost
                        .keys()
                        .any(|id| e.pending_journal.contains_key(id))
                {
                    return Err(ApiError::new(
                        Code::ResourceBusy,
                        "terminal event waits for retained output",
                    ));
                }
                let mut lost: Vec<_> = e
                    .pending_lost
                    .iter()
                    .filter(|(id, _)| e.sessions.get(id).is_some_and(|s| !s.terminal))
                    .map(|(id, event)| (*id, event.clone()))
                    .collect();
                lost.sort_by_key(|(id, _)| *id);
                let token = e.resume_token.clone();
                self.reconcile_lost(host, e, &lost, &[], token)?;
                e.pending_lost
                    .retain(|id, _| e.sessions.get(id).is_some_and(|s| !s.terminal));
                if let Some(c) = &e.conn {
                    for (session, _) in lost {
                        let _ = c.tx.send(Outgoing::frame(DaemonFrame::Release { session }));
                    }
                }
                Ok(())
            },
            |error| error.code == Code::ResourceBusy,
            timeout,
        );
        if let Err(error) = result {
            warn!(host, %error, "terminal host event retry exhausted; retained for next sweep");
        }
    }

    /// Revoke a host: `goodbye token_revoked`, close, and end its sessions.
    pub fn revoke(&self, host: &str) {
        let entry = self.lock().remove(host);
        if let Some(e) = entry {
            if let Some(c) = e.conn {
                let _ = c.tx.send(Outgoing::Close(Some((
                    "token_revoked",
                    "the host agent was deleted from the registry".into(),
                ))));
            }
            send_events(e.sessions.into_values(), "the host agent was revoked");
        }
    }

    /// Close the link of a host without a `goodbye`, as a network drop
    /// would. The sessions wait for the reconnect.
    pub fn drop_link(&self, host: &str) -> bool {
        self.send(host, Outgoing::Close(None))
    }

    fn on_data(&self, host: &str, f: DataFrame) {
        let mut g = self.lock();
        let Some(entry) = g.get_mut(host) else { return };
        if entry.pending_lost.contains_key(&f.session) {
            return;
        }
        let Some(s) = entry.sessions.get_mut(&f.session) else {
            debug!(host, session = f.session, "data for an unknown session");
            return;
        };
        if f.stream == StreamTag::Stdin {
            warn!(
                host,
                session = f.session,
                "host sent a stdin stream; ignored"
            );
            return;
        }
        if s.terminal {
            return;
        }
        let event = SessionEvent::Data {
            stream: f.stream,
            bytes: f.payload,
        };
        self.deliver(host, entry, f.session, event);
    }

    /// Save before exposing output to a consumer that can acknowledge it.
    fn deliver(&self, host: &str, entry: &mut Entry, id: SessionId, event: SessionEvent) -> bool {
        if let Some(pending) = entry.pending_journal.get_mut(&id) {
            pending.push(event);
            return false;
        }
        match self.try_deliver(host, entry, id, &event) {
            Ok(committed) => committed,
            Err(error) => {
                if error.code == Code::ResourceBusy {
                    let pending = entry
                        .pending_journal
                        .entry(id)
                        .or_insert_with(PendingJournal::new);
                    if pending.push(event) {
                        warn!(
                            host,
                            session = id,
                            "host journal busy; retaining event without acknowledgement"
                        );
                        return false;
                    }
                }
                if let Some(c) = &entry.conn {
                    let _ = c.tx.send(Outgoing::Close(None));
                }
                warn!(
                    host,
                    session = id,
                    "host journal commit failed; closing without acknowledgement"
                );
                false
            }
        }
    }

    fn try_deliver(
        &self,
        host: &str,
        entry: &mut Entry,
        id: SessionId,
        event: &SessionEvent,
    ) -> crate::error::ApiResult<bool> {
        let Some(s) = entry.sessions.get_mut(&id) else {
            return Ok(false);
        };
        if s.terminal {
            return Ok(false);
        }
        let old = (s.received, s.held, s.events, s.terminal, s.ended_at);
        s.events += 1;
        s.terminal = event.is_terminal();
        if s.terminal {
            s.ended_at = Some(chrono::Utc::now().timestamp());
        }
        if matches!(event, SessionEvent::Started { .. }) {
            s.held = true;
        }
        if let SessionEvent::Data { bytes, .. } = event {
            s.received += bytes.len() as u64;
        }
        let output = matches!(
            event,
            SessionEvent::Data {
                stream: StreamTag::Stdout | StreamTag::Stderr,
                ..
            }
        );
        if output && let SessionEvent::Data { bytes, .. } = event {
            s.output_events.push_back((s.events, bytes.len()));
        }
        if let Err(error) = self.persist(host, entry, Some((id, event))) {
            let Some(s) = entry.sessions.get_mut(&id) else {
                return Err(error);
            };
            (s.received, s.held, s.events, s.terminal, s.ended_at) = old;
            if output {
                s.output_events.pop_back();
            }
            return Err(error);
        }
        let Some(s) = entry.sessions.get_mut(&id) else {
            return Ok(false);
        };
        let prune = s.prune_count();
        s.output_events.drain(..prune);
        let _ = entry.sessions[&id].tx.send(event.clone());
        Ok(true)
    }

    /// Called on a blocking task. Writer contention waits here, without a
    /// registry guard, so heartbeat, ACK and other hosts keep running.
    fn retry_journal(&self, host: &str, conn_id: u64) {
        {
            let mut g = self.lock();
            let Some(e) = g.get_mut(host) else { return };
            if !e.conn.as_ref().is_some_and(|c| c.id == conn_id) {
                return;
            }
            if e.pending_journal
                .values()
                .any(|p| !p.exhausted && Instant::now() >= p.deadline)
            {
                for pending in e.pending_journal.values_mut() {
                    pending.exhausted = true;
                }
                warn!(
                    host,
                    "host journal made no progress; closing without acknowledgement"
                );
                if let Some(c) = &e.conn {
                    let _ = c.tx.send(Outgoing::Close(None));
                }
                return;
            }
            if e.pending_journal.is_empty()
                || e.pending_journal
                    .values()
                    .all(|p| p.exhausted || Instant::now() < p.next)
            {
                return;
            }
        }
        let result = if let Some(journal) = self.journal.get() {
            journal.with_writer(|conn| self.commit_pending(host, conn_id, Some(conn)))
        } else {
            self.commit_pending(host, conn_id, None)
        };
        if let Err(error) = &result
            && error.code == Code::ResourceBusy
        {
            let mut g = self.lock();
            if let Some(e) = g.get_mut(host)
                && e.conn.as_ref().is_some_and(|c| c.id == conn_id)
            {
                for pending in e.pending_journal.values_mut() {
                    pending.backoff = (pending.backoff * 2).min(JOURNAL_RETRY_BACKOFF);
                    pending.next = Instant::now() + pending.backoff;
                }
            }
        }
        if let Err(error) = result
            && error.code != Code::ResourceBusy
        {
            warn!(host, %error, "host journal commit failed; closing without acknowledgement");
            let g = self.lock();
            if let Some(e) = g.get(host)
                && let Some(c) = &e.conn
                && c.id == conn_id
            {
                let _ = c.tx.send(Outgoing::Close(None));
            }
        }
    }

    fn journal_backpressured(&self, host: &str) -> bool {
        self.lock().get(host).is_some_and(|e| {
            e.pending_journal
                .values()
                .any(|p| p.events.len() >= JOURNAL_RETRY_EVENTS || p.bytes >= JOURNAL_RETRY_BYTES)
        })
    }

    fn commit_pending(
        &self,
        host: &str,
        conn_id: u64,
        conn: Option<&mut rusqlite::Connection>,
    ) -> crate::error::ApiResult<()> {
        let mut g = self.lock();
        let Some(e) = g.get_mut(host) else {
            return Ok(());
        };
        if !e.conn.as_ref().is_some_and(|c| c.id == conn_id)
            || self.stopped.load(std::sync::atomic::Ordering::Acquire)
        {
            return Ok(());
        }
        let mut saved = SavedHost {
            token: e.resume_token.clone(),
            next_id: e.next_id,
            sessions: e
                .sessions
                .iter()
                .map(|(&id, s)| {
                    (
                        id,
                        SavedSlot {
                            received: s.received,
                            held: s.held,
                            events: s.events,
                            terminal: s.terminal,
                            ended_at: s.ended_at,
                        },
                    )
                })
                .collect(),
        };
        let mut values = Vec::new();
        let mut delivered = Vec::new();
        for (&id, pending) in &e.pending_journal {
            let Some(slot) = saved.sessions.get_mut(&id) else {
                continue;
            };
            for event in &pending.events {
                if slot.terminal {
                    break;
                }
                slot.events += 1;
                slot.terminal = event.is_terminal();
                if slot.terminal {
                    slot.ended_at = Some(chrono::Utc::now().timestamp());
                }
                if matches!(event, SessionEvent::Started { .. }) {
                    slot.held = true;
                }
                if let SessionEvent::Data { bytes, .. } = event {
                    slot.received += bytes.len() as u64;
                }
                values.push((
                    format!("event/{host}/{id}/{:020}", slot.events),
                    serde_json::to_value(event)
                        .map_err(|_| ApiError::internal("cannot encode host session event"))?,
                ));
                delivered.push((id, slot.events, event.clone()));
            }
        }
        if values.is_empty() {
            e.pending_journal.clear();
            return Ok(());
        }
        values.push((
            format!("host/{host}"),
            serde_json::to_value(&saved)
                .map_err(|_| ApiError::internal("cannot encode host resume state"))?,
        ));
        let mut removed = Vec::new();
        for (&id, s) in &e.sessions {
            let mut output = s.output_events.clone();
            for (event_id, seq, event) in &delivered {
                if *event_id == id
                    && let SessionEvent::Data {
                        stream: StreamTag::Stdout | StreamTag::Stderr,
                        bytes,
                    } = event
                {
                    output.push_back((*seq, bytes.len()));
                }
            }
            let mut bytes: usize = output.iter().map(|(_, n)| n).sum();
            while bytes > crate::executor::SESSION_BUFFER || output.len() > 4096 {
                let Some((seq, len)) = output.pop_front() else {
                    break;
                };
                bytes -= len;
                removed.push(format!("event/{host}/{id}/{seq:020}"));
            }
        }
        if let Some(journal) = self.journal.get()
            && let Some(conn) = conn
        {
            values.retain(|(key, _)| !removed.contains(key));
            journal.replace_on(conn, &values, &removed, &[])?;
        }
        // No live offset or consumer can observe the batch until commit.
        for (id, slot) in saved.sessions {
            if let Some(s) = e.sessions.get_mut(&id) {
                s.received = slot.received;
                s.events = slot.events;
                s.terminal = slot.terminal;
                s.ended_at = slot.ended_at;
                s.held = slot.held;
            }
        }
        e.pending_journal.clear();
        for (id, seq, event) in delivered {
            let Some(s) = e.sessions.get_mut(&id) else {
                continue;
            };
            if let SessionEvent::Data {
                stream: StreamTag::Stdout | StreamTag::Stderr,
                bytes,
            } = &event
            {
                s.output_events.push_back((seq, bytes.len()));
            }
            let prune = s.prune_count();
            s.output_events.drain(..prune);
            let _ = s.tx.send(event.clone());
            if event.is_terminal() {
                if matches!(event, SessionEvent::Lost { .. }) {
                    if let Some(c) = &e.conn {
                        let _ =
                            c.tx.send(Outgoing::frame(DaemonFrame::Release { session: id }));
                    }
                } else {
                    Self::ack_terminal(e, id, matches!(event, SessionEvent::Error { .. }));
                }
            }
        }
        Ok(())
    }

    /// A control frame about a session.
    fn on_session_frame(&self, host: &str, frame: HostFrame) {
        let (session, event) = match frame {
            HostFrame::Started { session, pid } => (session, SessionEvent::Started { pid }),
            HostFrame::Result { session, value } => (session, SessionEvent::Result(value)),
            HostFrame::Exit {
                session,
                code,
                signal,
                reason,
            } => (
                session,
                SessionEvent::Exit {
                    code,
                    signal,
                    reason,
                },
            ),
            HostFrame::Error {
                session,
                code,
                message,
            } => (
                session,
                SessionEvent::Error {
                    code: serde_json::to_value(code)
                        .ok()
                        .and_then(|v| v.as_str().map(str::to_owned))
                        .unwrap_or_else(|| "error".into()),
                    message,
                },
            ),
            HostFrame::CpDone {
                session,
                bytes,
                sha256,
            } => (session, SessionEvent::CpDone { bytes, sha256 }),
            HostFrame::Hello(_)
            | HostFrame::Ping { .. }
            | HostFrame::Pong { .. }
            | HostFrame::Metrics(_) => return,
        };
        let mut g = self.lock();
        let Some(e) = g.get_mut(host) else { return };
        let terminal = event.is_terminal();
        if e.pending_lost.contains_key(&session) {
            return;
        }
        let error = matches!(event, SessionEvent::Error { .. });
        if !self.deliver(host, e, session, event) || !terminal {
            return;
        }
        Self::ack_terminal(e, session, error);
    }

    fn ack_terminal(e: &Entry, session: SessionId, error: bool) {
        let s = &e.sessions[&session];
        // An `error` for a session that never started holds no state on
        // the host: no `release` for it.
        let release = s.held || !error;
        if let Some(c) = &e.conn {
            let _ = c.tx.send(Outgoing::frame(DaemonFrame::Ack {
                session,
                bytes: s.received,
            }));
            if release {
                let _ = c.tx.send(Outgoing::frame(DaemonFrame::Release { session }));
            }
        }
    }

    /// The consumer dropped a session before its end: abandon it.
    fn abandon(self: &Arc<Self>, host: &str, session: SessionId) {
        let mut g = self.lock();
        if let Some(e) = g.get_mut(host)
            && e.sessions.get(&session).is_some_and(|s| !s.terminal)
        {
            e.pending_lost
                .entry(session)
                .or_insert_with(|| SessionEvent::Lost {
                    message: "the session consumer abandoned the run".into(),
                });
        }
        drop(g);
        self.drive_lost(host);
    }
}

/// One session on a host, as its consumer sees it. Dropping it before the
/// terminal event abandons the session: the host kills the process.
pub struct HostSession {
    registry: Arc<HostRegistry>,
    host: String,
    id: SessionId,
    rx: mpsc::UnboundedReceiver<SessionEvent>,
    finished: bool,
    consumed: u64,
    acked: u64,
}

impl HostSession {
    pub fn abandon(&mut self) {
        self.registry.abandon(&self.host, self.id);
        self.finished = true;
    }
    pub fn id(&self) -> SessionId {
        self.id
    }

    pub fn host(&self) -> &str {
        &self.host
    }

    /// The next event. `None` after the terminal event.
    pub async fn recv(&mut self) -> Option<SessionEvent> {
        if self.finished {
            return None;
        }
        let ev = match self.rx.recv().await {
            Some(ev) => ev,
            None => SessionEvent::Lost {
                message: "the host registry dropped the session".into(),
            },
        };
        match &ev {
            SessionEvent::Data { bytes, .. } => {
                self.consumed += bytes.len() as u64;
                if self.consumed - self.acked >= ACK_EVERY {
                    self.acked = self.consumed;
                    self.registry.send(
                        &self.host,
                        Outgoing::frame(DaemonFrame::Ack {
                            session: self.id,
                            bytes: self.consumed,
                        }),
                    );
                }
            }
            e if e.is_terminal() => self.finished = true,
            _ => {}
        }
        Some(ev)
    }

    fn frame(&self, f: DaemonFrame) -> bool {
        self.registry.send(&self.host, Outgoing::frame(f))
    }

    /// Bytes for stdin. False when the host is offline.
    pub fn stdin(&self, bytes: Vec<u8>) -> bool {
        self.registry.send(
            &self.host,
            Outgoing::Data(DataFrame::new(self.id, StreamTag::Stdin, bytes)),
        )
    }

    /// File bytes of a push.
    pub fn file(&self, bytes: Vec<u8>) -> bool {
        self.registry.send(
            &self.host,
            Outgoing::Data(DataFrame::new(self.id, StreamTag::File, bytes)),
        )
    }

    pub fn eof(&self) -> bool {
        self.frame(DaemonFrame::Eof { session: self.id })
    }

    pub fn resize(&self, rows: u16, cols: u16) -> bool {
        self.frame(DaemonFrame::Resize {
            session: self.id,
            rows,
            cols,
        })
    }

    pub fn signal(&self, signal: &str) -> bool {
        self.frame(DaemonFrame::Signal {
            session: self.id,
            signal: signal.to_owned(),
        })
    }

    pub fn cp_end(&self, bytes: u64, sha256: String) -> bool {
        self.frame(DaemonFrame::CpEnd {
            session: self.id,
            bytes,
            sha256,
        })
    }
}

impl Drop for HostSession {
    fn drop(&mut self) {
        if !self.finished && self.registry.journal.get().is_none() {
            self.registry.abandon(&self.host, self.id);
        }
    }
}

// ------------------------------------------------------------- the endpoint

fn unauthorized(m: &str) -> Response {
    ApiError::new(Code::Unauthorized, m).into_response()
}

/// `GET /hosts/connect`: the dial-in WebSocket of a host agent.
pub async fn connect(State(state): State<Shared>, req: Request) -> Response {
    let token = req
        .headers()
        .get(header::AUTHORIZATION)
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.strip_prefix("Bearer "))
        .map(str::trim)
        .map(str::to_owned);
    let Some(token) = token.filter(|t| !t.is_empty()) else {
        return unauthorized("a host agent sends `Authorization: Bearer <token>`");
    };
    let hash = hosts::token_hash(&token);
    let lookup = hash.clone();
    let (row, enrol) = match state
        .db
        .read(move |c| hosts::authenticate(c, &lookup))
        .await
    {
        Ok(Auth::Host(r)) => (r, None),
        Ok(Auth::Enrol(r)) => (r, Some(hash)),
        Ok(Auth::Expired) => {
            return unauthorized("the enrol token ran out; rotate the host token for a new one");
        }
        Ok(Auth::Unknown) => return unauthorized("unknown or revoked host token"),
        Err(e) => return e.into_response(),
    };
    let peer = peer_addr(&req);
    let (mut parts, _) = req.into_parts();
    let ws = match WebSocketUpgrade::from_request_parts(&mut parts, &()).await {
        Ok(ws) => ws,
        Err(e) => return ApiError::bad_request(e.body_text()).into_response(),
    };
    ws.max_message_size(MAX_MESSAGE)
        .max_frame_size(MAX_MESSAGE)
        .on_upgrade(move |socket| serve_link(state, socket, row, enrol, peer))
}

/// The peer address of a request on the host listener, as `ip:port`.
fn peer_addr(req: &Request) -> Option<String> {
    req.extensions()
        .get::<ConnectInfo<PeerAddr>>()
        .map(|c| c.0.0.to_string())
        .or_else(|| {
            req.extensions()
                .get::<ConnectInfo<SocketAddr>>()
                .map(|c| c.0.to_string())
        })
}

/// The connect info of the host listener: the TCP peer.
#[derive(Debug, Clone, Copy)]
pub struct PeerAddr(pub SocketAddr);

impl Connected<axum::serve::IncomingStream<'_, tokio::net::TcpListener>> for PeerAddr {
    fn connect_info(s: axum::serve::IncomingStream<'_, tokio::net::TcpListener>) -> Self {
        Self(*s.remote_addr())
    }
}

impl Connected<axum::serve::IncomingStream<'_, TlsListener>> for PeerAddr {
    fn connect_info(s: axum::serve::IncomingStream<'_, TlsListener>) -> Self {
        Self(*s.remote_addr())
    }
}

/// A TCP listener that does the rustls handshake. Each handshake runs in
/// its own task, so a slow or silent client cannot hold up the others.
pub struct TlsListener {
    rx: mpsc::Receiver<(
        tokio_rustls::server::TlsStream<tokio::net::TcpStream>,
        SocketAddr,
    )>,
    local: SocketAddr,
    task: JoinHandle<()>,
}

impl TlsListener {
    pub fn new(
        tcp: tokio::net::TcpListener,
        config: Arc<rustls::ServerConfig>,
    ) -> std::io::Result<Self> {
        let local = tcp.local_addr()?;
        let (tx, rx) = mpsc::channel(64);
        let acceptor = tokio_rustls::TlsAcceptor::from(config);
        let task = tokio::spawn(async move {
            loop {
                let (stream, peer) = match tcp.accept().await {
                    Ok(x) => x,
                    Err(e) => {
                        debug!("host listener accept: {e}");
                        tokio::time::sleep(Duration::from_millis(50)).await;
                        continue;
                    }
                };
                if tx.is_closed() {
                    break;
                }
                let (acceptor, tx) = (acceptor.clone(), tx.clone());
                tokio::spawn(async move {
                    match tokio::time::timeout(TLS_HANDSHAKE_TIMEOUT, acceptor.accept(stream)).await
                    {
                        Ok(Ok(tls)) => {
                            let _ = tx.send((tls, peer)).await;
                        }
                        Ok(Err(e)) => info!(%peer, "host listener: TLS handshake failed: {e}"),
                        Err(_) => info!(%peer, "host listener: TLS handshake timed out"),
                    }
                });
            }
        });
        Ok(Self { rx, local, task })
    }
}

impl Drop for TlsListener {
    fn drop(&mut self) {
        self.task.abort();
    }
}

impl axum::serve::Listener for TlsListener {
    type Io = tokio_rustls::server::TlsStream<tokio::net::TcpStream>;
    type Addr = SocketAddr;

    async fn accept(&mut self) -> (Self::Io, Self::Addr) {
        match self.rx.recv().await {
            Some(x) => x,
            None => std::future::pending().await,
        }
    }

    fn local_addr(&self) -> std::io::Result<Self::Addr> {
        Ok(self.local)
    }
}

/// The dial-in listener settings, from the environment.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HostListen {
    pub addr: String,
    /// The PEM certificate chain and key: TLS on the listener.
    pub tls: Option<(PathBuf, PathBuf)>,
}

impl HostListen {
    /// `TERRARIUM_HOST_LISTEN`, `TERRARIUM_HOST_TLS_CERT`, and
    /// `TERRARIUM_HOST_TLS_KEY`. `None`: the listener is off (an empty
    /// `TERRARIUM_HOST_LISTEN`). Unset or `auto`: `0.0.0.0:8082` with TLS,
    /// `127.0.0.1:8082` without.
    pub fn from_env() -> anyhow::Result<Option<Self>> {
        Self::from_vars(|k| std::env::var(k).ok())
    }

    /// [`Self::from_env`] with a lookup, for tests.
    pub fn from_vars(var: impl Fn(&str) -> Option<String>) -> anyhow::Result<Option<Self>> {
        let set = |k: &str| var(k).filter(|v| !v.trim().is_empty());
        let tls = match (
            set("TERRARIUM_HOST_TLS_CERT"),
            set("TERRARIUM_HOST_TLS_KEY"),
        ) {
            (Some(c), Some(k)) => Some((PathBuf::from(c), PathBuf::from(k))),
            (None, None) => None,
            _ => anyhow::bail!(
                "set both TERRARIUM_HOST_TLS_CERT and TERRARIUM_HOST_TLS_KEY, or neither"
            ),
        };
        // `auto` (what compose passes when the variable is not set) and
        // an unset variable pick the default for the TLS mode.
        let addr = match var("TERRARIUM_HOST_LISTEN").map(|a| a.trim().to_owned()) {
            Some(a) if a.is_empty() => return Ok(None),
            Some(a) if a != "auto" => a,
            _ if tls.is_some() => DEFAULT_TLS_LISTEN.to_owned(),
            _ => DEFAULT_PLAIN_LISTEN.to_owned(),
        };
        Ok(Some(Self { addr, tls }))
    }

    /// Bind and serve: TLS when a certificate is set, else plaintext.
    pub async fn serve(
        &self,
        state: Shared,
    ) -> anyhow::Result<(JoinHandle<()>, std::net::SocketAddr)> {
        match &self.tls {
            Some((cert, key)) => {
                let (config, fingerprint) = terrarium_host::tls::server_config(cert, key)?;
                info!(
                    cert = %cert.display(),
                    sha256 = %fingerprint,
                    "host agent dial-in uses TLS; pin with server_fingerprint_sha256"
                );
                serve_tls(state, &self.addr, config).await
            }
            None => {
                let (task, local) = serve(state, &self.addr).await?;
                if !local.ip().is_loopback() {
                    warn!(
                        addr = %local,
                        "INSECURE: the host agent dial-in is plaintext on a non-loopback \
                         address. Host tokens and all exec traffic cross the network in clear \
                         text, and a captured token can be replayed. Set TERRARIUM_HOST_TLS_CERT \
                         and TERRARIUM_HOST_TLS_KEY (just host::cert), or put a TLS proxy in \
                         front and bind TERRARIUM_HOST_LISTEN to 127.0.0.1"
                    );
                } else {
                    info!(
                        "host agent dial-in is plaintext on loopback; set TERRARIUM_HOST_TLS_CERT \
                         and TERRARIUM_HOST_TLS_KEY to accept remote host agents"
                    );
                }
                Ok((task, local))
            }
        }
    }
}

/// The router of the dial-in listener: `/hosts/connect`, and the host
/// agent binary with its SHA-256 (`/install/terrarium-host[.sha256]`) for
/// the self-update (docs/host-protocol.md, "Self-update").
pub fn router(state: Shared) -> axum::Router {
    axum::Router::new()
        .route(
            terrarium_host::protocol::CONNECT_PATH,
            axum::routing::get(connect),
        )
        .merge(crate::api::install::binary_routes())
        .with_state(state)
}

/// Bind a plaintext dial-in listener and serve on it in the background.
/// [`HostListen::serve`] picks this or [`serve_tls`].
pub async fn serve(
    state: Shared,
    addr: &str,
) -> anyhow::Result<(JoinHandle<()>, std::net::SocketAddr)> {
    use anyhow::Context;
    let listener = tokio::net::TcpListener::bind(addr)
        .await
        .with_context(|| format!("cannot bind the host listener {addr}"))?;
    let local = listener.local_addr()?;
    info!(addr = %local, "host agent dial-in listening on TCP (plaintext)");
    let app = router(state).into_make_service_with_connect_info::<PeerAddr>();
    Ok((
        tokio::spawn(async move {
            if let Err(e) = axum::serve(listener, app).await {
                tracing::error!("host listener failed: {e}");
            }
        }),
        local,
    ))
}

/// Bind a TLS dial-in listener and serve on it in the background.
pub async fn serve_tls(
    state: Shared,
    addr: &str,
    config: Arc<rustls::ServerConfig>,
) -> anyhow::Result<(JoinHandle<()>, std::net::SocketAddr)> {
    use anyhow::Context;
    let tcp = tokio::net::TcpListener::bind(addr)
        .await
        .with_context(|| format!("cannot bind the host listener {addr}"))?;
    let listener = TlsListener::new(tcp, config)?;
    let local = listener.local;
    info!(addr = %local, "host agent dial-in listening on TCP with TLS");
    let app = router(state).into_make_service_with_connect_info::<PeerAddr>();
    Ok((
        tokio::spawn(async move {
            if let Err(e) = axum::serve(listener, app).await {
                tracing::error!("host listener failed: {e}");
            }
        }),
        local,
    ))
}

async fn record(
    state: &Shared,
    draft: EventDraft,
    write: impl FnOnce(&mut crate::db::Tx<'_>) -> crate::error::ApiResult<()> + Send + 'static,
) {
    let r = state
        .db
        .write("system", move |t| {
            write(t)?;
            t.emit(draft)?;
            Ok(())
        })
        .await;
    if let Err(e) = r {
        warn!("cannot record a host event: {e}");
    }
}

/// The audit and the events of a self-update (docs/host-protocol.md,
/// "Self-update"): `host.updated` and the audit `host.update` when a host
/// agent of another build comes back with the daemon's build;
/// `host.update_failed` (event and audit) when it reports a new binary
/// that did not start.
async fn host_update_records(state: &Shared, row: &Host, hello: &Hello) {
    let daemon = terrarium_version::FULL;
    if let Some(old) = row.version.as_deref()
        && old != hello.version
        && terrarium_version::is_stale(old, daemon)
        && !terrarium_version::is_stale(&hello.version, daemon)
    {
        info!(host = %row.name, from = %old, to = %hello.version, "the host agent updated to the daemon's build");
        let (name, id, from, to) = (
            row.name.clone(),
            row.id.clone(),
            old.to_owned(),
            hello.version.clone(),
        );
        record(
            state,
            EventDraft::new(
                "host.updated",
                format!("host {} updated to {}", row.name, hello.version),
                json!({"host": row.name, "host_id": row.id, "from": old, "to": hello.version}),
            ),
            move |t| {
                t.audit(
                    "host.update",
                    &name,
                    None,
                    json!({"host_id": id, "from": from, "to": to}),
                )
            },
        )
        .await;
    }
    if let Some(failed) = hello.update_failed.as_deref()
        && state.executor.hosts.first_update_failure(&row.name, failed)
    {
        warn!(
            host = %row.name,
            failed,
            running = %hello.version,
            "the new host agent binary did not start; the host went back to the previous binary"
        );
        let (name, id, failed2, running) = (
            row.name.clone(),
            row.id.clone(),
            failed.to_owned(),
            hello.version.clone(),
        );
        record(
            state,
            EventDraft::new(
                "host.update_failed",
                format!(
                    "host {}: the update to {failed} did not start; it runs {} again",
                    row.name, hello.version
                ),
                json!({"host": row.name, "host_id": row.id, "failed": failed, "running": hello.version}),
            ),
            move |t| {
                t.audit(
                    "host.update_failed",
                    &name,
                    None,
                    json!({"host_id": id, "failed": failed2, "running": running}),
                )
            },
        )
        .await;
    }
}

async fn read_hello(socket: &mut WebSocket) -> Result<Hello, String> {
    let next = tokio::time::timeout(HELLO_TIMEOUT, async {
        loop {
            match socket.recv().await {
                Some(Ok(WsMessage::Text(t))) => return Ok(t),
                Some(Ok(WsMessage::Close(_))) | None => return Err("closed".to_owned()),
                Some(Ok(_)) => continue,
                Some(Err(e)) => return Err(e.to_string()),
            }
        }
    })
    .await
    .map_err(|_| "no hello in time".to_owned())??;
    match decode_control::<HostFrame>(next.as_str()) {
        Ok(HostFrame::Hello(h)) => Ok(*h),
        Ok(other) => Err(format!("expected hello, got {other:?}")),
        Err(e) => Err(e.to_string()),
    }
}

async fn goodbye(socket: &mut WebSocket, reason: &str, message: String) {
    let f = DaemonFrame::Goodbye {
        reason: reason.to_owned(),
        message: Some(message),
    };
    if let Ok(t) = encode_control(&f) {
        let _ = socket.send(WsMessage::Text(t.into())).await;
    }
    let _ = socket.send(WsMessage::Close(None)).await;
}

/// Handle one message read from a host link. Returns why the link ends,
/// or `None` to keep it.
fn on_link_message(
    reg: &HostRegistry,
    host: &str,
    tx: &mpsc::UnboundedSender<Outgoing>,
    msg: Option<Result<WsMessage, axum::Error>>,
) -> Option<&'static str> {
    match msg {
        Some(Ok(WsMessage::Text(t))) => match decode_control::<HostFrame>(t.as_str()) {
            Ok(HostFrame::Ping { nonce }) => {
                let _ = tx.send(Outgoing::frame(DaemonFrame::Pong { nonce }));
            }
            Ok(HostFrame::Pong { .. }) => {}
            Ok(HostFrame::Hello(_)) => warn!(host = %host, "a second hello; ignored"),
            Ok(HostFrame::Metrics(m)) => reg.record_metrics(host, *m, Instant::now()),
            Ok(f) => reg.on_session_frame(host, f),
            Err(e) => warn!(host = %host, "bad control frame: {e}"),
        },
        Some(Ok(WsMessage::Binary(b))) => match DataFrame::decode(&b) {
            Ok(f) => reg.on_data(host, f),
            Err(e) => warn!(host = %host, "bad data frame: {e}"),
        },
        Some(Ok(WsMessage::Close(_))) | None => return Some("closed by the host"),
        Some(Ok(_)) => {}
        Some(Err(e)) => {
            debug!(host = %host, "link error: {e}");
            return Some("link error");
        }
    }
    None
}

async fn drive_link(
    socket: WebSocket,
    reg: &Arc<HostRegistry>,
    host: &str,
    conn_id: u64,
    tx: mpsc::UnboundedSender<Outgoing>,
    mut rx: mpsc::UnboundedReceiver<Outgoing>,
) -> &'static str {
    let (mut sink, mut stream) = socket.split();
    let hb = reg.heartbeat.max(Duration::from_millis(10));
    let mut ticker = tokio::time::interval_at(tokio::time::Instant::now() + hb, hb);
    let mut last_rx = Instant::now();
    let mut journal_retry = tokio::time::interval(JOURNAL_RETRY_TICK);
    let mut journal_task: Option<tokio::task::JoinHandle<()>> = None;
    let mut nonce = 0u64;
    let why = loop {
        tokio::select! {
            _ = journal_retry.tick() => {
                if journal_task.as_ref().is_none_or(|task| task.is_finished()) {
                    let registry = reg.clone();
                    let host = host.to_owned();
                    journal_task = Some(tokio::task::spawn_blocking(move || registry.retry_journal(&host, conn_id)));
                }
            },
            msg = stream.next(), if !reg.journal_backpressured(host) => {
                last_rx = Instant::now();
                if let Some(why) = on_link_message(reg, host, &tx, msg) {
                    break why;
                }
            }
            out = rx.recv() => {
                let msg = match out {
                    Some(Outgoing::Frame(f)) => match encode_control(&f) {
                        Ok(t) => WsMessage::Text(t.into()),
                        Err(e) => {
                            warn!("cannot encode a frame: {e}");
                            continue;
                        }
                    },
                    Some(Outgoing::Data(d)) => WsMessage::Binary(d.encode().into()),
                    Some(Outgoing::Close(reason)) => {
                        if let Some((reason, message)) = reason {
                            let f = DaemonFrame::Goodbye { reason: reason.to_owned(), message: Some(message) };
                            if let Ok(t) = encode_control(&f) {
                                let _ = sink.send(WsMessage::Text(t.into())).await;
                            }
                        }
                        let _ = sink.send(WsMessage::Close(None)).await;
                        break "closed by the daemon";
                    }
                    None => break "no sender",
                };
                if sink.send(msg).await.is_err() {
                    break "send failed";
                }
            }
            _ = ticker.tick() => {
                if reg.journal_backpressured(host) {
                    last_rx = Instant::now();
                }
                if last_rx.elapsed() > hb * MISSED_HEARTBEATS {
                    // A late wake-up of this task is not silence from the
                    // host: under load the ticker and a frame that arrived
                    // meanwhile become ready together, and `select!` may
                    // pick the ticker first (TER-1132). Count a waiting
                    // frame before closing the link.
                    match futures_util::FutureExt::now_or_never(stream.next()) {
                        Some(msg) => {
                            last_rx = Instant::now();
                            if let Some(why) = on_link_message(reg, host, &tx, msg) {
                                break why;
                            }
                        }
                        None => break "heartbeat timeout",
                    }
                }
                nonce += 1;
                if sink.send(WsMessage::Text(
                    encode_control(&DaemonFrame::Ping { nonce }).unwrap_or_default().into(),
                )).await.is_err() {
                    break "send failed";
                }
            }
        }
    };
    let _ = sink.close().await;
    why
}

async fn serve_link(
    state: Shared,
    mut socket: WebSocket,
    row: Host,
    enrol: Option<String>,
    peer: Option<String>,
) {
    let reg = state.executor.hosts.clone();
    let hello = match read_hello(&mut socket).await {
        Ok(h) => h,
        Err(e) => {
            warn!(host = %row.name, "bad hello: {e}");
            reg.note_error(&row.name, format!("refused: bad hello: {e}"));
            goodbye(&mut socket, "protocol", e).await;
            return;
        }
    };
    if hello.protocol != PROTOCOL_VERSION {
        let m = format!(
            "protocol {} is not supported; this daemon speaks {PROTOCOL_VERSION}",
            hello.protocol
        );
        reg.note_error(&row.name, format!("refused: {m}"));
        goodbye(&mut socket, "protocol", m).await;
        return;
    }
    if hello.host != row.name {
        let m = format!(
            "this token belongs to host `{}`, not `{}`",
            row.name, hello.host
        );
        reg.note_error(&row.name, format!("refused: {m}"));
        goodbye(&mut socket, "protocol", m).await;
        return;
    }
    // An enrol token: swap it for the long-lived token now. Only one dial
    // can win the swap.
    let new_token = match enrol {
        None => None,
        Some(hash) => {
            let name = row.name.clone();
            let from = peer.clone();
            let row_id_for_audit = row.id.clone();
            let r = state
                .db
                .write("system", move |t| {
                    let token = hosts::complete_enrol(t, &name, &hash)?;
                    if token.is_some() {
                        t.audit(
                            "host.enrol",
                            &name,
                            None,
                            json!({"host_id": row_id_for_audit, "from": from}),
                        )?;
                    }
                    Ok(token)
                })
                .await;
            match r {
                Ok(Some(t)) => Some(t),
                Ok(None) => {
                    reg.note_error(
                        &row.name,
                        "refused: the enrol token was used already or ran out",
                    );
                    goodbye(
                        &mut socket,
                        "token_revoked",
                        "the enrol token was used already or ran out".into(),
                    )
                    .await;
                    return;
                }
                Err(e) => {
                    warn!(host = %row.name, "cannot complete the enrol: {e}");
                    reg.note_error(
                        &row.name,
                        format!("refused: cannot complete the enrol: {e}"),
                    );
                    goodbye(&mut socket, "shutdown", "cannot complete the enrol".into()).await;
                    return;
                }
            }
        }
    };
    let enrolled = new_token.is_some();
    // A host agent of another build gets this daemon's binary
    // (docs/host-protocol.md, "Self-update").
    let stale = terrarium_version::is_stale(&hello.version, terrarium_version::FULL);
    let offer = if stale {
        crate::api::install::update_offer_for_platform(
            &state.web.install.dist_dir,
            &hello.os,
            &hello.arch,
        )
        .await
    } else {
        None
    };
    let conn_id = reg
        .next_conn
        .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    let (tx, rx) = mpsc::unbounded_channel::<Outgoing>();
    state.executor.set_resource_pending(&row.name, true);
    let attached = reg.attach(
        &row,
        &hello,
        Conn {
            id: conn_id,
            tx: tx.clone(),
        },
    );
    let (mut welcome, old, resumed, _) = match attached {
        Ok(attached) => attached,
        Err(e) => {
            warn!(host = %row.name, "cannot persist host welcome: {e}");
            return;
        }
    };
    if let Some(old) = old {
        let _ = old.tx.send(Outgoing::Close(Some((
            "replaced",
            "a new connection with the same token took over".into(),
        ))));
    }
    welcome.token = new_token;
    welcome.update = offer;
    match encode_control(&DaemonFrame::Welcome(welcome.clone())) {
        Ok(t) => {
            tracing::trace!("host transport frame send");
            if socket.send(WsMessage::Text(t.into())).await.is_err() {
                reg.detach(&row.name, conn_id);
                return;
            }
        }
        Err(e) => {
            warn!("cannot encode welcome: {e}");
            return;
        }
    }
    info!(host = %row.name, resumed, enrolled, peer = peer.as_deref().unwrap_or("-"), "host agent connected");
    if stale {
        match &welcome.update {
            Some(o) => info!(
                host = %row.name,
                version = %hello.version,
                daemon = terrarium_version::FULL,
                sha256 = %o.sha256,
                "the host agent is built from another ref than the daemon: offered it the daemon's build"
            ),
            None => warn!(
                host = %row.name,
                version = %hello.version,
                daemon = terrarium_version::FULL,
                "the host agent is built from another ref than the daemon, and this daemon has no \
                 terrarium-host binary for its arch to offer: reinstall it"
            ),
        }
    }
    host_update_records(&state, &row, &hello).await;
    let caps = serde_json::to_value(&hello.capabilities).unwrap_or(Value::Null);
    let summary = hello.capabilities.summary(&hello.os, &hello.arch);
    {
        let name = row.name.clone();
        let (caps2, os, arch, version) = (
            caps.clone(),
            hello.os.clone(),
            hello.arch.clone(),
            hello.version.clone(),
        );
        record(
            &state,
            EventDraft::new(
                "host.connected",
                format!("host {} connected", row.name),
                json!({"host": row.name, "host_id": row.id, "version": hello.version,
                       "os": hello.os, "arch": hello.arch, "capabilities": caps,
                       "summary": summary, "resumed": resumed, "enrolled": enrolled,
                       "from": peer}),
            ),
            move |t| {
                hosts::mark_online(t, &name, &caps2, &os, &arch, &version)?;
                hosts::mark_used(t, &name, peer.as_deref())
            },
        )
        .await;
    }

    // The stored target states can be stale (a lab destroyed by hand, a
    // container removed while the link was down): check each target of
    // the host, once the link serves the verbs.
    {
        let (state, host) = (state.clone(), row.name.clone());
        tokio::spawn(async move {
            crate::executor::reconcile_host(&state, &host).await;
            if let Err(error) = crate::reservations::reconcile_host(&state, &host).await {
                warn!(%host, %error, "reservation reconnect reconciliation pending");
            }
        });
    }

    let why = drive_link(socket, &reg, &row.name, conn_id, tx, rx).await;
    if let Some(generation) = reg.detach(&row.name, conn_id) {
        info!(host = %row.name, why, "host agent disconnected");
        reg.note_error(&row.name, format!("disconnected: {why}"));
        let name = row.name.clone();
        record(
            &state,
            EventDraft::new(
                "host.disconnected",
                format!("host {} disconnected: {why}", row.name),
                json!({"host": row.name, "host_id": row.id, "reason": why}),
            ),
            move |t| hosts::mark_offline(t, &name),
        )
        .await;
        let grace = reg.resume_grace;
        let host = row.name.clone();
        tokio::spawn(async move {
            tokio::time::sleep(grace).await;
            reg.expire(&host, generation);
        });
    }
}

/// Why `row` is offline, for `GET /api/hosts` (docs/api.md §17). `last`:
/// the last link error since the daemon started. `listen`: the host
/// listener of the daemon. `now`: RFC 3339, as [`crate::db::now`].
pub fn offline_reason(
    row: &Host,
    last: Option<&LinkError>,
    listen: Option<&HostListen>,
    now: &str,
) -> String {
    let name = &row.name;
    // A rotate after the error makes the error old news.
    let last = last.filter(|e| row.rotated_at.as_deref().is_none_or(|r| r < e.at.as_str()));
    if let Some(e) = last {
        return format!("{} (at {})", e.message, e.at);
    }
    if let Some(exp) = row.enrol_expires_at.as_deref() {
        return if exp <= now {
            format!(
                "the enrol token ran out at {exp} before the host agent used it; \
                 make a new one: terrarium admin host rotate {name} --enrol"
            )
        } else {
            format!(
                "waiting for the host agent to dial in with its enrol token (it runs out at {exp})"
            )
        };
    }
    let rotated_since_use = match (row.rotated_at.as_deref(), row.last_used_at.as_deref()) {
        (Some(r), Some(u)) => r > u,
        (Some(_), None) => true,
        _ => false,
    };
    if row.last_used_at.is_none() && row.last_seen.is_none() {
        let mut m = "never connected: the host agent has not reached the daemon. \
                     Check `daemon_url` in /etc/terrarium-host/config.toml and \
                     `journalctl -u terrarium-host` on the host"
            .to_owned();
        if let Some(l) = listen
            && l.tls.is_none()
            && loopback_addr(&l.addr)
        {
            m.push_str(&format!(
                ". The host listener is plaintext on {} (loopback only): \
                 a published container port cannot reach it. Set \
                 TERRARIUM_HOST_LISTEN=0.0.0.0:8082 and publish the port on \
                 loopback, or set up TLS",
                l.addr
            ));
        }
        return m;
    }
    if rotated_since_use {
        let at = row.rotated_at.as_deref().unwrap_or_default();
        return format!("the token was rotated at {at}: give the host agent the new token");
    }
    match row.last_seen.as_deref().or(row.last_used_at.as_deref()) {
        Some(t) => format!("not connected since {t}"),
        None => "not connected".to_owned(),
    }
}

/// `127.0.0.1:8082`, `[::1]:8082`, `localhost:8082`.
fn loopback_addr(addr: &str) -> bool {
    let host = addr.rsplit_once(':').map_or(addr, |(h, _)| h);
    let host = host.trim_start_matches('[').trim_end_matches(']');
    host == "localhost"
        || host
            .parse::<std::net::IpAddr>()
            .is_ok_and(|ip| ip.is_loopback())
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::*;

    fn host_row() -> Host {
        Host {
            name: "lab1".into(),
            id: "h1".into(),
            created_by: "operator".into(),
            capabilities: json!({}),
            os: None,
            arch: None,
            version: None,
            status: "offline".into(),
            last_seen: None,
            created_at: "2026-11-01T10:00:00.000Z".into(),
            first_used_at: None,
            last_used_at: None,
            last_used_from: None,
            rotated_at: None,
            enrol_expires_at: None,
        }
    }

    const NOW: &str = "2026-11-03T12:00:00.000Z";

    #[test]
    fn a_host_that_never_connected_says_so() {
        let r = offline_reason(&host_row(), None, None, NOW);
        assert!(r.starts_with("never connected"), "{r}");
        assert!(!r.contains("loopback"), "{r}");
        // A plaintext loopback listener is the likely cause (#3).
        let listen = HostListen {
            addr: DEFAULT_PLAIN_LISTEN.into(),
            tls: None,
        };
        let r = offline_reason(&host_row(), None, Some(&listen), NOW);
        assert!(
            r.contains("plaintext on 127.0.0.1:8082 (loopback only)"),
            "{r}"
        );
        let open = HostListen {
            addr: "0.0.0.0:8082".into(),
            tls: None,
        };
        assert!(!offline_reason(&host_row(), None, Some(&open), NOW).contains("loopback"));
    }

    #[test]
    fn an_open_or_spent_enrol_token() {
        let mut row = host_row();
        row.enrol_expires_at = Some("2026-11-03T12:10:00.000Z".into());
        let r = offline_reason(&row, None, None, NOW);
        assert!(r.starts_with("waiting for the host agent"), "{r}");
        row.enrol_expires_at = Some("2026-11-03T11:50:00.000Z".into());
        let r = offline_reason(&row, None, None, NOW);
        assert!(r.contains("ran out at 2026-11-03T11:50:00.000Z"), "{r}");
        assert!(
            r.contains("terrarium admin host rotate lab1 --enrol"),
            "{r}"
        );
    }

    #[test]
    fn the_last_link_error_wins_until_a_rotate() {
        let mut row = host_row();
        row.last_used_at = Some("2026-11-03T09:00:00.000Z".into());
        row.last_seen = Some("2026-11-03T11:00:00.000Z".into());
        assert_eq!(
            offline_reason(&row, None, None, NOW),
            "not connected since 2026-11-03T11:00:00.000Z"
        );
        let e = LinkError {
            at: "2026-11-03T11:00:00.000Z".into(),
            message: "disconnected: heartbeat timeout".into(),
        };
        assert_eq!(
            offline_reason(&row, Some(&e), None, NOW),
            "disconnected: heartbeat timeout (at 2026-11-03T11:00:00.000Z)"
        );
        row.rotated_at = Some("2026-11-03T11:30:00.000Z".into());
        let r = offline_reason(&row, Some(&e), None, NOW);
        assert!(
            r.starts_with("the token was rotated at 2026-11-03T11:30"),
            "{r}"
        );
    }

    #[test]
    fn the_registry_keeps_the_last_error_until_a_link() {
        let reg = HostRegistry::default();
        assert!(reg.last_error("lab1").is_none());
        reg.note_error("lab1", "refused: bad hello: closed");
        assert_eq!(
            reg.last_error("lab1").unwrap().message,
            "refused: bad hello: closed"
        );
        assert!(!reg.is_online("lab1"));
    }

    #[test]
    fn loopback_addresses() {
        for a in ["127.0.0.1:8082", "[::1]:8082", "localhost:8082"] {
            assert!(loopback_addr(a), "{a}");
        }
        for a in ["0.0.0.0:8082", "10.0.0.2:8082", "[::]:8082"] {
            assert!(!loopback_addr(a), "{a}");
        }
    }

    fn sample(cores: u32) -> HostMetrics {
        HostMetrics {
            interval_secs: 10.0,
            cpu: Some(terrarium_host::protocol::CpuMetrics {
                busy_pct: 5.0,
                cores,
                load1: 0.1,
            }),
            ..HostMetrics::default()
        }
    }

    /// A registry with an entry for `host`, online or not.
    fn registry(host: &str, online: bool) -> HostRegistry {
        let reg = HostRegistry::default();
        let conn = online.then(|| Conn {
            id: 1,
            tx: mpsc::unbounded_channel().0,
        });
        reg.lock().insert(
            host.to_owned(),
            Entry {
                conn,
                ..Entry::default()
            },
        );
        reg
    }

    #[tokio::test]
    async fn reconnect_journal_bounds_output_tail_without_losing_exit_or_resume_offset() {
        let dir = tempfile::tempdir().unwrap();
        let db = crate::db::open(&dir.path().join("terrarium.db")).unwrap();
        crate::secrets::ensure_key(&db, dir.path()).unwrap();
        let reg = Arc::new(registry("lab1", true));
        reg.restore(dir.path()).unwrap();
        let session = reg
            .open("lab1", |_| DaemonFrame::Ping { nonce: 0 })
            .unwrap();
        let id = session.id();
        let chunk = crate::executor::SESSION_BUFFER / 4;
        for byte in 0..6 {
            reg.on_data(
                "lab1",
                DataFrame::new(id, StreamTag::Stdout, vec![byte; chunk]),
            );
        }
        reg.on_session_frame(
            "lab1",
            HostFrame::Exit {
                session: id,
                code: Some(23),
                signal: None,
                reason: ExitReason::Exited,
            },
        );
        drop(session);
        drop(reg);
        let restored = Arc::new(HostRegistry::default());
        restored.restore(dir.path()).unwrap();
        assert_eq!(
            restored.lock()["lab1"].sessions[&id].received,
            (6 * chunk) as u64
        );
        let mut session = restored.reattach("lab1", id).unwrap();
        assert_eq!(session.consumed, (2 * chunk) as u64);
        for byte in 2..6 {
            assert_eq!(
                session.recv().await,
                Some(SessionEvent::Data {
                    stream: StreamTag::Stdout,
                    bytes: vec![byte; chunk]
                })
            );
        }
        assert!(matches!(
            session.recv().await,
            Some(SessionEvent::Exit { code: Some(23), .. })
        ));
        assert_eq!(session.consumed, (6 * chunk) as u64);
    }

    #[tokio::test]
    async fn reconnect_journal_prunes_only_expired_unowned_terminal_sessions() {
        let dir = tempfile::tempdir().unwrap();
        let db = crate::db::open(&dir.path().join("terrarium.db")).unwrap();
        crate::secrets::ensure_key(&db, dir.path()).unwrap();
        let reg = Arc::new(registry("lab1", true));
        reg.restore(dir.path()).unwrap();
        let orphan = reg
            .open("lab1", |_| DaemonFrame::Ping { nonce: 0 })
            .unwrap();
        let owned = reg
            .open_bound("lab1", Some("recipe/job/1"), |_| DaemonFrame::Ping {
                nonce: 0,
            })
            .unwrap();
        for id in [orphan.id(), owned.id()] {
            reg.on_session_frame(
                "lab1",
                HostFrame::Exit {
                    session: id,
                    code: Some(0),
                    signal: None,
                    reason: ExitReason::Exited,
                },
            );
        }
        reg.prune_unowned(chrono::Utc::now().timestamp() - 300)
            .unwrap();
        assert!(reg.lock()["lab1"].sessions.contains_key(&orphan.id()));
        reg.prune_unowned(chrono::Utc::now().timestamp() + 1)
            .unwrap();
        assert!(!reg.lock()["lab1"].sessions.contains_key(&orphan.id()));
        assert!(reg.lock()["lab1"].sessions.contains_key(&owned.id()));
        reg.forget_job("job").unwrap();
        assert!(!reg.lock()["lab1"].sessions.contains_key(&owned.id()));
        let journal = resume::Journal::new(dir.path());
        assert!(
            journal
                .scan::<serde_json::Value>("event/")
                .unwrap()
                .is_empty()
        );
        assert!(
            journal
                .scan::<serde_json::Value>("bound/")
                .unwrap()
                .is_empty()
        );
    }

    #[tokio::test]
    async fn reconnect_journal_closed_exec_does_not_resurrect_or_delete_other_output() {
        let dir = tempfile::tempdir().unwrap();
        let db = crate::db::open(&dir.path().join("terrarium.db")).unwrap();
        crate::secrets::ensure_key(&db, dir.path()).unwrap();
        let reg = Arc::new(registry("lab1", true));
        reg.restore(dir.path()).unwrap();
        let first = reg
            .open("lab1", |_| DaemonFrame::Ping { nonce: 0 })
            .unwrap();
        let second = reg
            .open("lab1", |_| DaemonFrame::Ping { nonce: 0 })
            .unwrap();
        let (first_id, second_id) = (first.id(), second.id());
        let journal = resume::Journal::new(dir.path());
        journal
            .save("exec/closed", &json!({"host_session": ["lab1", first_id]}))
            .unwrap();
        for id in [first_id, second_id] {
            reg.on_data(
                "lab1",
                DataFrame::new(id, StreamTag::Stdout, b"retained".to_vec()),
            );
        }
        reg.forget_exec("closed").unwrap();
        assert!(
            journal
                .load::<serde_json::Value>("exec/closed")
                .unwrap()
                .is_none()
        );
        assert!(
            journal
                .scan::<SessionEvent>(&format!("event/lab1/{first_id}/"))
                .unwrap()
                .is_empty()
        );
        assert_eq!(
            journal
                .scan::<SessionEvent>(&format!("event/lab1/{second_id}/"))
                .unwrap()
                .len(),
            1
        );
        drop(first);
        drop(second);
        drop(reg);
        let restored = Arc::new(HostRegistry::default());
        restored.restore(dir.path()).unwrap();
        assert!(restored.reattach("lab1", first_id).is_none());
        assert!(restored.reattach("lab1", second_id).is_some());
    }

    #[tokio::test]
    async fn reconnect_journal_survives_an_undelivered_welcome() {
        let dir = tempfile::tempdir().unwrap();
        let db = crate::db::open(&dir.path().join("terrarium.db")).unwrap();
        crate::secrets::ensure_key(&db, dir.path()).unwrap();
        let reg = Arc::new(registry("lab1", true));
        reg.restore(dir.path()).unwrap();
        reg.lock().get_mut("lab1").unwrap().resume_token = Some("host-kept-token".into());
        let session = reg
            .open("lab1", |_| DaemonFrame::Ping { nonce: 0 })
            .unwrap();
        let id = session.id();
        let hello = Hello {
            path_kind: terrarium_host::protocol::PathKind::Posix,
            protocol: 1,
            host: "lab1".into(),
            version: "test".into(),
            os: "linux".into(),
            arch: "x86_64".into(),
            capabilities: HostCapabilities::default(),
            max_sessions: 8,
            max_running_targets: None,
            resume: Some(terrarium_host::protocol::Resume {
                token: "host-kept-token".into(),
                sessions: vec![terrarium_host::protocol::ResumeOffer {
                    session: id,
                    sent: 0,
                    finished: false,
                }],
            }),
            update_failed: None,
        };
        // Commit welcome, but simulate losing it before the host receives it.
        let (welcome, _, n, _) = reg
            .attach(
                &host_row(),
                &hello,
                Conn {
                    id: 2,
                    tx: mpsc::unbounded_channel().0,
                },
            )
            .unwrap();
        assert_eq!(n, 1);
        assert_eq!(welcome.resume_token, "host-kept-token");
        drop(session);
        drop(reg);
        let restored = HostRegistry::default();
        restored.restore(dir.path()).unwrap();
        let (_, _, n, _) = restored
            .attach(
                &host_row(),
                &hello,
                Conn {
                    id: 3,
                    tx: mpsc::unbounded_channel().0,
                },
            )
            .unwrap();
        assert_eq!(n, 1, "the host still holds the pre-welcome token");
        assert!(!restored.lock()["lab1"].sessions[&id].terminal);
    }

    #[tokio::test]
    async fn reconnect_journal_keeps_output_and_real_exit_after_registry_restart() {
        let dir = tempfile::tempdir().unwrap();
        let conn = crate::db::open(&dir.path().join("terrarium.db")).unwrap();
        crate::secrets::ensure_key(&conn, dir.path()).unwrap();
        let reg = Arc::new(registry("lab1", true));
        reg.restore(dir.path()).unwrap();
        reg.lock().get_mut("lab1").unwrap().resume_token = Some("kept-token".into());
        let session = reg
            .open("lab1", |_| DaemonFrame::Ping { nonce: 0 })
            .unwrap();
        let id = session.id();
        reg.on_session_frame(
            "lab1",
            HostFrame::Started {
                session: id,
                pid: Some(42),
            },
        );
        reg.on_data(
            "lab1",
            DataFrame::new(id, StreamTag::Stdout, b"kept output\n".to_vec()),
        );
        reg.on_session_frame(
            "lab1",
            HostFrame::Exit {
                session: id,
                code: Some(23),
                signal: None,
                reason: ExitReason::Exited,
            },
        );
        drop(session);
        drop(reg);
        let restored = Arc::new(HostRegistry::default());
        restored.restore(dir.path()).unwrap();
        assert_eq!(
            restored.lock()["lab1"].resume_token.as_deref(),
            Some("kept-token")
        );
        let mut session = restored.reattach("lab1", id).unwrap();
        assert!(matches!(
            session.recv().await,
            Some(SessionEvent::Started { pid: Some(42) })
        ));
        assert_eq!(
            session.recv().await,
            Some(SessionEvent::Data {
                stream: StreamTag::Stdout,
                bytes: b"kept output\n".to_vec(),
            })
        );
        assert!(matches!(
            session.recv().await,
            Some(SessionEvent::Exit { code: Some(23), .. })
        ));
        assert!(session.recv().await.is_none());
        assert!(restored.reattach("lab1", id).is_none());
    }

    #[tokio::test]
    async fn failed_journal_commit_does_not_expose_or_acknowledge_output() {
        let dir = tempfile::tempdir().unwrap();
        let conn = crate::db::open(&dir.path().join("terrarium.db")).unwrap();
        crate::secrets::ensure_key(&conn, dir.path()).unwrap();
        let reg = Arc::new(registry("lab1", true));
        reg.restore(dir.path()).unwrap();
        let mut session = reg
            .open("lab1", |_| DaemonFrame::Ping { nonce: 0 })
            .unwrap();
        conn.execute("DROP TABLE host_resume", []).unwrap();
        reg.on_data(
            "lab1",
            DataFrame::new(session.id(), StreamTag::Stdout, b"not saved".to_vec()),
        );
        assert_eq!(reg.lock()["lab1"].sessions[&session.id()].received, 0);
        assert!(session.rx.try_recv().is_err());
    }

    #[tokio::test]
    async fn busy_journal_retains_order_and_retries_exactly_once() {
        let dir = tempfile::tempdir().unwrap();
        let conn = crate::db::open(&dir.path().join("terrarium.db")).unwrap();
        crate::secrets::ensure_key(&conn, dir.path()).unwrap();
        let reg = Arc::new(registry("lab1", true));
        reg.restore(dir.path()).unwrap();
        let mut session = reg
            .open("lab1", |_| DaemonFrame::Ping { nonce: 0 })
            .unwrap();
        let (tx, mut outgoing) = mpsc::unbounded_channel();
        reg.lock().get_mut("lab1").unwrap().conn = Some(Conn { id: 1, tx });
        conn.execute_batch("BEGIN IMMEDIATE").unwrap();
        reg.on_data(
            "lab1",
            DataFrame::new(session.id(), StreamTag::Stdout, b"first".to_vec()),
        );
        reg.on_data(
            "lab1",
            DataFrame::new(session.id(), StreamTag::Stdout, b"second".to_vec()),
        );
        reg.on_session_frame(
            "lab1",
            HostFrame::Exit {
                session: session.id(),
                code: Some(0),
                signal: None,
                reason: ExitReason::Exited,
            },
        );
        assert!(
            outgoing.try_recv().is_err(),
            "no close, ACK or release on busy"
        );
        assert!(session.rx.try_recv().is_err());
        assert_eq!(reg.lock()["lab1"].sessions[&session.id()].received, 0);
        assert_eq!(
            reg.lock()["lab1"].pending_journal[&session.id()]
                .events
                .len(),
            3
        );
        let deadline = reg.lock()["lab1"].pending_journal[&session.id()].deadline;
        reg.lock()
            .get_mut("lab1")
            .unwrap()
            .pending_journal
            .get_mut(&session.id())
            .unwrap()
            .next = Instant::now();
        reg.retry_journal("lab1", 1);
        assert!(outgoing.try_recv().is_err());
        assert!(session.rx.try_recv().is_err());
        assert_eq!(
            reg.lock()["lab1"].pending_journal[&session.id()]
                .events
                .len(),
            3
        );
        assert_eq!(
            reg.lock()["lab1"].pending_journal[&session.id()].deadline,
            deadline
        );
        assert_eq!(
            reg.lock()["lab1"].pending_journal[&session.id()].backoff,
            Duration::from_millis(20)
        );
        conn.execute_batch("ROLLBACK").unwrap();
        reg.lock()
            .get_mut("lab1")
            .unwrap()
            .pending_journal
            .get_mut(&session.id())
            .unwrap()
            .next = Instant::now();
        reg.retry_journal("lab1", 1);
        for bytes in [b"first".as_slice(), b"second".as_slice()] {
            assert_eq!(
                session.rx.try_recv().unwrap(),
                SessionEvent::Data {
                    stream: StreamTag::Stdout,
                    bytes: bytes.to_vec()
                }
            );
        }
        assert!(matches!(
            session.rx.try_recv().unwrap(),
            SessionEvent::Exit { .. }
        ));
        assert!(session.rx.try_recv().is_err());
        assert!(
            matches!(outgoing.try_recv().unwrap(), Outgoing::Frame(f) if matches!(*f, DaemonFrame::Ack { bytes: 11, .. }))
        );
        assert!(
            matches!(outgoing.try_recv().unwrap(), Outgoing::Frame(f) if matches!(*f, DaemonFrame::Release { .. }))
        );
        reg.retry_journal("lab1", 1);
        assert!(outgoing.try_recv().is_err());
        let events = resume::Journal::new(dir.path())
            .scan::<SessionEvent>(&format!("event/lab1/{}/", session.id()))
            .unwrap();
        assert_eq!(events.len(), 3);
        assert_eq!(reg.lock()["lab1"].sessions[&session.id()].received, 11);
    }

    #[tokio::test]
    async fn journal_no_progress_closes_and_reconnect_replays() {
        let dir = tempfile::tempdir().unwrap();
        let conn = crate::db::open(&dir.path().join("terrarium.db")).unwrap();
        crate::secrets::ensure_key(&conn, dir.path()).unwrap();
        let reg = Arc::new(registry("lab1", true));
        reg.restore(dir.path()).unwrap();
        reg.lock().get_mut("lab1").unwrap().resume_token = Some("retry-token".into());
        let mut session = reg
            .open("lab1", |_| DaemonFrame::Ping { nonce: 0 })
            .unwrap();
        let (tx, mut outgoing) = mpsc::unbounded_channel();
        let mut pending = PendingJournal::new();
        pending.deadline = Instant::now();
        {
            let mut g = reg.lock();
            let e = g.get_mut("lab1").unwrap();
            e.conn = Some(Conn { id: 1, tx });
            e.pending_journal.insert(session.id(), pending);
        }
        reg.on_data(
            "lab1",
            DataFrame::new(session.id(), StreamTag::Stdout, b"replayed".to_vec()),
        );
        reg.retry_journal("lab1", 1);
        assert!(matches!(
            outgoing.try_recv().unwrap(),
            Outgoing::Close(None)
        ));
        assert!(session.rx.try_recv().is_err());
        assert!(reg.detach("lab1", 1).is_some());
        assert!(reg.lock()["lab1"].pending_journal.is_empty());
        let hello = Hello {
            path_kind: terrarium_host::protocol::PathKind::Posix,
            protocol: PROTOCOL_VERSION,
            host: "lab1".into(),
            version: "test".into(),
            os: "linux".into(),
            arch: "x86_64".into(),
            capabilities: HostCapabilities::default(),
            max_sessions: 8,
            max_running_targets: None,
            update_failed: None,
            resume: Some(terrarium_host::protocol::Resume {
                token: "retry-token".into(),
                sessions: vec![terrarium_host::protocol::ResumeOffer {
                    session: session.id(),
                    sent: 8,
                    finished: false,
                }],
            }),
        };
        let (welcome, _, count, _) = reg
            .attach(
                &host_row(),
                &hello,
                Conn {
                    id: 2,
                    tx: mpsc::unbounded_channel().0,
                },
            )
            .unwrap();
        assert_eq!(count, 1);
        assert_eq!(welcome.resume[0].received, 0);
        // Replayed data starts at the unchanged committed offset.
        reg.on_data(
            "lab1",
            DataFrame::new(session.id(), StreamTag::Stdout, b"replayed".to_vec()),
        );
        assert_eq!(
            session.rx.try_recv().unwrap(),
            SessionEvent::Data {
                stream: StreamTag::Stdout,
                bytes: b"replayed".to_vec()
            }
        );
        assert_eq!(reg.lock()["lab1"].sessions[&session.id()].received, 8);
        let mut pending = PendingJournal::new();
        assert!(pending.push(SessionEvent::Data {
            stream: StreamTag::Stdout,
            bytes: vec![0; JOURNAL_RETRY_BYTES]
        }));
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn journal_websocket_pauses_reads_but_keeps_outgoing_frames_live() {
        journal_websocket_replay(false).await;
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn journal_websocket_no_progress_closes_while_reads_are_paused() {
        journal_websocket_replay(true).await;
    }

    /// TER-1132: the link task wakes late under load with a host frame
    /// already waiting. The heartbeat check must count that frame and keep
    /// the link, not blame the host for the daemon's own delay.
    #[tokio::test(flavor = "current_thread")]
    async fn heartbeat_counts_a_frame_that_waits_while_the_daemon_is_late() {
        use tokio_tungstenite::tungstenite::Message;

        let mut registry = registry("lab1", true);
        registry.heartbeat = Duration::from_millis(20);
        let hb = registry.heartbeat;
        let reg = Arc::new(registry);
        let (ended_tx, mut ended) = mpsc::unbounded_channel();
        let app = axum::Router::new().route(
            "/",
            axum::routing::get(move |upgrade: WebSocketUpgrade| {
                let registry = reg.clone();
                let ended_tx = ended_tx.clone();
                async move {
                    upgrade.on_upgrade(move |socket| async move {
                        let (tx, rx) = mpsc::unbounded_channel();
                        let why = drive_link(socket, &registry, "lab1", 77, tx, rx).await;
                        let _ = ended_tx.send(why);
                    })
                }
            }),
        );
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let server = tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
        let (mut ws, _) = tokio_tungstenite::connect_async(format!("ws://{addr}/"))
            .await
            .unwrap();
        for nonce in 0..20 {
            ws.send(Message::Text(
                encode_control(&HostFrame::Ping { nonce }).unwrap().into(),
            ))
            .await
            .unwrap();
            // Block the only runtime thread past the deadline: the link
            // task cannot read the Ping until its ticker is overdue too.
            std::thread::sleep(hb * (MISSED_HEARTBEATS + 2));
            tokio::time::sleep(Duration::from_millis(1)).await;
            assert!(ended.try_recv().is_err(), "link ended after ping {nonce}");
        }
        ws.close(None).await.unwrap();
        let why = tokio::time::timeout(Duration::from_secs(5), ended.recv())
            .await
            .unwrap()
            .unwrap();
        assert_eq!(why, "closed by the host");
        server.abort();
    }

    async fn journal_websocket_replay(expire: bool) {
        use tokio_tungstenite::tungstenite::Message;

        let dir = tempfile::tempdir().unwrap();
        let conn = crate::db::open(&dir.path().join("terrarium.db")).unwrap();
        crate::secrets::ensure_key(&conn, dir.path()).unwrap();
        let db = crate::db::Db::new(conn, crate::events::EventHub::default());
        let mut registry = registry("lab1", true);
        registry.heartbeat = Duration::from_millis(50);
        let reg = Arc::new(registry);
        reg.restore_with_db(dir.path(), db.clone()).unwrap();
        let mut session = reg
            .open("lab1", |_| DaemonFrame::Ping { nonce: 0 })
            .unwrap();
        let session_id = session.id();
        let completed = reg
            .open("lab1", |_| DaemonFrame::Ping { nonce: 0 })
            .unwrap();
        reg.on_session_frame(
            "lab1",
            HostFrame::Exit {
                session: completed.id(),
                code: Some(0),
                signal: None,
                reason: ExitReason::Exited,
            },
        );
        reg.lock().get_mut("lab1").unwrap().resume_token = Some("websocket-replay".into());
        for offset in 0..8u32 {
            reg.on_data(
                "lab1",
                DataFrame::new(session_id, StreamTag::Stdout, offset.to_le_bytes().to_vec()),
            );
            session.recv().await.unwrap();
        }
        reg.detach("lab1", 1).unwrap();
        let route_reg = reg.clone();
        let app = axum::Router::new().route(
            "/",
            axum::routing::get(move |upgrade: WebSocketUpgrade| {
                let registry = route_reg.clone();
                async move {
                    let (tx, rx) = mpsc::unbounded_channel();
                    let mut hello = lost_test_hello("websocket-replay", &[session_id]);
                    hello.resume.as_mut().unwrap().sessions[0].sent = 512;
                    let (welcome, _, resumed, _) = registry
                        .attach(
                            &host_row(),
                            &hello,
                            Conn {
                                id: 77,
                                tx: tx.clone(),
                            },
                        )
                        .unwrap();
                    assert_eq!(resumed, 1);
                    assert_eq!(welcome.resume[0].received, 32);
                    upgrade.on_upgrade(move |mut socket| async move {
                        socket
                            .send(WsMessage::Text(
                                encode_control(&DaemonFrame::Welcome(welcome))
                                    .unwrap()
                                    .into(),
                            ))
                            .await
                            .unwrap();
                        drive_link(socket, &registry, "lab1", 77, tx, rx).await;
                    })
                }
            }),
        );
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let server = tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
        let (mut ws, _) = tokio_tungstenite::connect_async(format!("ws://{addr}/"))
            .await
            .unwrap();
        let welcome = ws.next().await.unwrap().unwrap();
        assert!(
            matches!(welcome, Message::Text(text) if matches!(decode_control::<DaemonFrame>(&text).unwrap(), DaemonFrame::Welcome(_)))
        );
        let writer_db = db.clone();
        let (acquired, ready) = tokio::sync::oneshot::channel();
        let (release, released) = std::sync::mpsc::channel();
        let writer = tokio::task::spawn_blocking(move || {
            writer_db
                .read_now(|_| {
                    acquired.send(()).unwrap();
                    released.recv_timeout(Duration::from_secs(10)).unwrap();
                    Ok(())
                })
                .unwrap()
        });
        ready.await.unwrap();
        for offset in 8..128u32 {
            ws.send(Message::Binary(
                DataFrame::new(
                    session.id(),
                    StreamTag::Stdout,
                    offset.to_le_bytes().to_vec(),
                )
                .encode()
                .into(),
            ))
            .await
            .unwrap();
        }
        ws.send(Message::Text(
            encode_control(&HostFrame::Exit {
                session: session.id(),
                code: Some(0),
                signal: None,
                reason: ExitReason::Exited,
            })
            .unwrap()
            .into(),
        ))
        .await
        .unwrap();
        ws.send(Message::Text(
            encode_control(&HostFrame::Ping { nonce: 999 })
                .unwrap()
                .into(),
        ))
        .await
        .unwrap();
        tokio::time::timeout(Duration::from_secs(3), async {
            while !reg.journal_backpressured("lab1") {
                tokio::time::sleep(Duration::from_millis(5)).await;
            }
        })
        .await
        .unwrap();
        // These concern an already committed terminal session. The transport
        // must send them even while incoming output is backpressured.
        reg.send(
            "lab1",
            Outgoing::frame(DaemonFrame::Ack {
                session: completed.id(),
                bytes: 0,
            }),
        );
        reg.send(
            "lab1",
            Outgoing::frame(DaemonFrame::Release {
                session: completed.id(),
            }),
        );
        let mut heartbeats = 0;
        let mut ack = false;
        let mut released_frame = false;
        tokio::time::timeout(Duration::from_secs(3), async {
            while heartbeats < 5 || !ack || !released_frame {
                match ws.next().await.unwrap().unwrap() {
                    Message::Text(text) => match decode_control::<DaemonFrame>(&text).unwrap() {
                        DaemonFrame::Ping { .. } => heartbeats += 1,
                        DaemonFrame::Ack { session, bytes: 0 } if session == completed.id() => {
                            ack = true
                        }
                        DaemonFrame::Release { session } if session == completed.id() => {
                            released_frame = true
                        }
                        other => panic!("unexpected frame while paused: {other:?}"),
                    },
                    other => panic!("unexpected websocket frame: {other:?}"),
                }
            }
        })
        .await
        .unwrap();
        // Paused longer than three heartbeat intervals without a Pong. The
        // outgoing Pings keep the real host's receive deadline alive too.
        assert_eq!(
            reg.lock()["lab1"].pending_journal[&session.id()]
                .events
                .len(),
            JOURNAL_RETRY_EVENTS
        );
        assert_eq!(reg.lock()["lab1"].sessions[&session.id()].received, 32);
        assert!(session.rx.try_recv().is_err());
        if expire {
            // Simulate expiration of the 60 s no-progress bound while the
            // writer remains held and the receive gate remains closed.
            reg.lock()
                .get_mut("lab1")
                .unwrap()
                .pending_journal
                .get_mut(&session_id)
                .unwrap()
                .deadline = Instant::now();
            tokio::time::timeout(Duration::from_secs(3), async {
                loop {
                    match ws.next().await.unwrap().unwrap() {
                        Message::Text(text) => assert!(matches!(
                            decode_control::<DaemonFrame>(&text).unwrap(),
                            DaemonFrame::Ping { .. }
                        )),
                        Message::Close(_) => break,
                        other => panic!("unexpected frame at deadline: {other:?}"),
                    }
                }
            })
            .await
            .unwrap();
            assert!(reg.lock()["lab1"].pending_journal[&session_id].exhausted);
            let mut closes = 0;
            let (tx, mut rx) = mpsc::unbounded_channel();
            reg.lock().get_mut("lab1").unwrap().conn = Some(Conn { id: 77, tx });
            for _ in 0..3 {
                reg.retry_journal("lab1", 77);
                while let Ok(frame) = rx.try_recv() {
                    if matches!(frame, Outgoing::Close(_)) {
                        closes += 1;
                    }
                }
            }
            assert_eq!(closes, 0, "expired queue emits only the first close");
            assert!(session.rx.try_recv().is_err());
            release.send(()).unwrap();
            writer.await.unwrap();
            server.abort();
            return;
        }
        release.send(()).unwrap();
        writer.await.unwrap();
        let consumer = tokio::spawn(async move {
            for offset in 8..128u32 {
                assert_eq!(
                    session.recv().await.unwrap(),
                    SessionEvent::Data {
                        stream: StreamTag::Stdout,
                        bytes: offset.to_le_bytes().to_vec()
                    }
                );
            }
            assert!(matches!(
                session.recv().await,
                Some(SessionEvent::Exit { .. })
            ));
        });
        let journal = resume::Journal::with_db(dir.path(), db);
        let mut final_ack = false;
        let mut final_release = false;
        let mut pong = false;
        tokio::time::timeout(Duration::from_secs(10), async {
            while !final_ack || !final_release || !pong {
                match ws.next().await.unwrap().unwrap() {
                    Message::Text(text) => match decode_control::<DaemonFrame>(&text).unwrap() {
                        DaemonFrame::Ping { nonce } => ws
                            .send(Message::Text(
                                encode_control(&HostFrame::Pong { nonce }).unwrap().into(),
                            ))
                            .await
                            .unwrap(),
                        DaemonFrame::Ack { session, bytes } => {
                            let saved = journal.load::<SavedHost>("host/lab1").unwrap().unwrap();
                            assert_eq!(bytes, 512);
                            assert_eq!(saved.sessions[&session].received, bytes);
                            final_ack = true;
                        }
                        DaemonFrame::Release { .. } => final_release = true,
                        DaemonFrame::Pong { nonce: 999 } => pong = true,
                        other => panic!("unexpected frame after resume: {other:?}"),
                    },
                    other => panic!("unexpected websocket frame: {other:?}"),
                }
            }
        })
        .await
        .unwrap();
        consumer.await.unwrap();
        let events = journal
            .scan::<SessionEvent>(&format!("event/lab1/{session_id}/"))
            .unwrap();
        assert_eq!(events.len(), 129);
        for (offset, (_, event)) in events.iter().take(128).enumerate() {
            assert_eq!(
                event,
                &SessionEvent::Data {
                    stream: StreamTag::Stdout,
                    bytes: (offset as u32).to_le_bytes().to_vec()
                }
            );
        }
        assert_eq!(reg.lock()["lab1"].conn.as_ref().unwrap().id, 77);
        ws.close(None).await.unwrap();
        server.abort();
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn journal_batch_partial_write_failure_rolls_back_all_sessions() {
        let dir = tempfile::tempdir().unwrap();
        let conn = crate::db::open(&dir.path().join("terrarium.db")).unwrap();
        crate::secrets::ensure_key(&conn, dir.path()).unwrap();
        let db = crate::db::Db::new(conn, crate::events::EventHub::default());
        let reg = Arc::new(registry("lab1", true));
        reg.restore_with_db(dir.path(), db.clone()).unwrap();
        let mut sessions: Vec<_> = (0..3)
            .map(|_| {
                reg.open("lab1", |_| DaemonFrame::Ping { nonce: 0 })
                    .unwrap()
            })
            .collect();
        let (tx, mut outgoing) = mpsc::unbounded_channel();
        {
            let mut g = reg.lock();
            let entry = g.get_mut("lab1").unwrap();
            entry.conn = Some(Conn { id: 77, tx });
            for session in &sessions {
                let mut pending = PendingJournal::new();
                pending.next = Instant::now();
                pending.push(SessionEvent::Data {
                    stream: StreamTag::Stdout,
                    bytes: b"first".to_vec(),
                });
                pending.push(SessionEvent::Data {
                    stream: StreamTag::Stderr,
                    bytes: b"second".to_vec(),
                });
                pending.push(SessionEvent::Exit {
                    code: Some(0),
                    signal: None,
                    reason: ExitReason::Exited,
                });
                entry.pending_journal.insert(session.id(), pending);
            }
        }
        // AFTER the second event insert of whichever session the HashMap
        // visits first, so this fails after writes, not at lock acquisition.
        db.read_now(|conn| {
            conn.execute_batch("CREATE TRIGGER fail_journal_batch AFTER INSERT ON host_resume WHEN NEW.id LIKE 'event/lab1/%/00000000000000000002' BEGIN SELECT RAISE(ABORT, 'injected partial batch failure'); END")?;
            Ok(())
        }).unwrap();
        reg.retry_journal("lab1", 77);
        assert!(matches!(
            outgoing.try_recv().unwrap(),
            Outgoing::Close(None)
        ));
        assert!(
            outgoing.try_recv().is_err(),
            "no ACK or Release after failed commit"
        );
        let journal = resume::Journal::with_db(dir.path(), db);
        assert!(
            journal
                .scan::<SessionEvent>("event/lab1/")
                .unwrap()
                .is_empty()
        );
        let saved = journal.load::<SavedHost>("host/lab1").unwrap().unwrap();
        for session in &mut sessions {
            assert!(
                session.rx.try_recv().is_err(),
                "failed batch is not delivered"
            );
            let g = reg.lock();
            let entry = &g["lab1"];
            let slot = &entry.sessions[&session.id()];
            assert_eq!((slot.received, slot.events, slot.terminal), (0, 0, false));
            let pending = &entry.pending_journal[&session.id()];
            assert_eq!(pending.events.len(), 3);
            assert!(matches!(
                pending.events.back(),
                Some(SessionEvent::Exit { .. })
            ));
            let slot = &saved.sessions[&session.id()];
            assert_eq!((slot.received, slot.events, slot.terminal), (0, 0, false));
        }
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn journal_batch_six_streams_survive_writer_contention() {
        let dir = tempfile::tempdir().unwrap();
        let conn = crate::db::open(&dir.path().join("terrarium.db")).unwrap();
        crate::secrets::ensure_key(&conn, dir.path()).unwrap();
        let db = crate::db::Db::new(conn, crate::events::EventHub::default());
        let reg = Arc::new(registry("lab1", true));
        reg.restore_with_db(dir.path(), db.clone()).unwrap();
        let mut sessions: Vec<_> = (0..6)
            .map(|_| {
                reg.open("lab1", |_| DaemonFrame::Ping { nonce: 0 })
                    .unwrap()
            })
            .collect();
        let (tx, mut outgoing) = mpsc::unbounded_channel();
        reg.lock().get_mut("lab1").unwrap().conn = Some(Conn { id: 77, tx });
        let writer_db = db.clone();
        let (acquired, ready) = tokio::sync::oneshot::channel();
        let writer = tokio::task::spawn_blocking(move || {
            writer_db
                .read_now(|_| {
                    acquired.send(()).unwrap();
                    std::thread::sleep(Duration::from_secs(2));
                    Ok(())
                })
                .unwrap();
            for _ in 0..500 {
                writer_db
                    .read_now(|_| {
                        std::thread::sleep(Duration::from_millis(1));
                        Ok(())
                    })
                    .unwrap();
                std::thread::sleep(Duration::from_millis(1));
            }
        });
        ready.await.unwrap();
        let journal = resume::Journal::with_db(dir.path(), db.clone());
        let payload = |offset: u32| {
            let mut bytes = vec![0; 256];
            bytes[..4].copy_from_slice(&offset.to_le_bytes());
            bytes
        };
        let mut acks = 0;
        let mut sent = 0u32;
        while sent < 2000 {
            let count = (2000 - sent).min(64);
            // Exercise a batch across all six sessions on one connection.
            for offset in sent..sent + count {
                for session in &sessions {
                    reg.on_data(
                        "lab1",
                        DataFrame::new(session.id(), StreamTag::Stdout, payload(offset)),
                    );
                }
            }
            if sent == 0 {
                assert!(reg.journal_backpressured("lab1"));
                assert!(outgoing.try_recv().is_err());
                assert!(sessions.iter_mut().all(|s| s.rx.try_recv().is_err()));
            }
            let deadline = Instant::now() + Duration::from_secs(10);
            while !reg.lock()["lab1"].pending_journal.is_empty() {
                assert!(Instant::now() < deadline);
                let registry = reg.clone();
                tokio::task::spawn_blocking(move || registry.retry_journal("lab1", 77))
                    .await
                    .unwrap();
                tokio::time::sleep(Duration::from_millis(10)).await;
                assert!(outgoing.try_recv().is_err(), "no close during contention");
            }
            let committed = journal.load::<SavedHost>("host/lab1").unwrap().unwrap();
            for session in &mut sessions {
                for offset in sent..sent + count {
                    assert!(
                        committed.sessions[&session.id()].received >= u64::from(offset + 1) * 256
                    );
                    assert_eq!(
                        session.recv().await.unwrap(),
                        SessionEvent::Data {
                            stream: StreamTag::Stdout,
                            bytes: payload(offset)
                        }
                    );
                }
            }
            while let Ok(out) = outgoing.try_recv() {
                match out {
                    Outgoing::Frame(frame) => {
                        if let DaemonFrame::Ack { session, bytes } = *frame {
                            assert!(bytes <= committed.sessions[&session].received);
                            acks += 1;
                        }
                    }
                    Outgoing::Close(_) => panic!("writer contention closed the connection"),
                    _ => {}
                }
            }
            sent += count;
            assert_eq!(reg.lock()["lab1"].conn.as_ref().unwrap().id, 77);
        }
        writer.await.unwrap();
        for session in &mut sessions {
            reg.on_session_frame(
                "lab1",
                HostFrame::Exit {
                    session: session.id(),
                    code: Some(0),
                    signal: None,
                    reason: ExitReason::Exited,
                },
            );
            assert!(matches!(
                session.recv().await,
                Some(SessionEvent::Exit { .. })
            ));
            let events = journal
                .scan::<SessionEvent>(&format!("event/lab1/{}/", session.id()))
                .unwrap();
            assert_eq!(events.len(), 2001);
            for (offset, (_, event)) in events.iter().take(2000).enumerate() {
                assert_eq!(
                    event,
                    &SessionEvent::Data {
                        stream: StreamTag::Stdout,
                        bytes: payload(offset as u32)
                    }
                );
            }
        }
        let committed = journal.load::<SavedHost>("host/lab1").unwrap().unwrap();
        while let Ok(out) = outgoing.try_recv() {
            match out {
                Outgoing::Frame(frame) => {
                    if let DaemonFrame::Ack { session, bytes } = *frame {
                        assert_eq!(bytes, 512000);
                        assert!(bytes <= committed.sessions[&session].received);
                        acks += 1;
                    }
                }
                Outgoing::Close(_) => panic!("writer contention closed the connection"),
                _ => {}
            }
        }
        assert_eq!(acks, 12);
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn main_writer_contention_queues_without_blocking_or_closing() {
        let dir = tempfile::tempdir().unwrap();
        let conn = crate::db::open(&dir.path().join("terrarium.db")).unwrap();
        crate::secrets::ensure_key(&conn, dir.path()).unwrap();
        let db = crate::db::Db::new(conn, crate::events::EventHub::default());
        let reg = Arc::new(registry("lab1", true));
        reg.restore_with_db(dir.path(), db.clone()).unwrap();
        let mut session = reg
            .open("lab1", |_| DaemonFrame::Ping { nonce: 0 })
            .unwrap();
        let (tx, mut outgoing) = mpsc::unbounded_channel();
        reg.lock().get_mut("lab1").unwrap().conn = Some(Conn { id: 1, tx });
        for byte in 0..16u8 {
            let (acquired, ready) = tokio::sync::oneshot::channel();
            let (release, released) = std::sync::mpsc::channel();
            let writer_db = db.clone();
            let writer = tokio::spawn(async move {
                writer_db
                    .write("system", move |t| {
                        t.execute("UPDATE host_resume SET sealed=sealed WHERE id='unused'", [])?;
                        acquired.send(()).unwrap();
                        released.recv_timeout(Duration::from_secs(5)).unwrap();
                        Ok(())
                    })
                    .await
                    .unwrap();
            });
            ready.await.unwrap();
            // The writer cannot release its mutex until this returns. A
            // blocking journal would deadlock until the writer test times out.
            reg.on_data(
                "lab1",
                DataFrame::new(session.id(), StreamTag::Stdout, vec![byte]),
            );
            reg.on_data(
                "lab1",
                DataFrame::new(session.id(), StreamTag::Stdout, vec![byte + 16]),
            );
            assert!(session.rx.try_recv().is_err());
            assert!(outgoing.try_recv().is_err());
            assert_eq!(
                reg.lock()["lab1"].pending_journal[&session.id()]
                    .events
                    .len(),
                2
            );
            release.send(()).unwrap();
            writer.await.unwrap();
            reg.lock()
                .get_mut("lab1")
                .unwrap()
                .pending_journal
                .get_mut(&session.id())
                .unwrap()
                .next = Instant::now();
            reg.retry_journal("lab1", 1);
            for expected in [byte, byte + 16] {
                assert_eq!(
                    session.rx.try_recv().unwrap(),
                    SessionEvent::Data {
                        stream: StreamTag::Stdout,
                        bytes: vec![expected]
                    }
                );
            }
            assert!(outgoing.try_recv().is_err());
        }
        assert_eq!(reg.lock()["lab1"].sessions[&session.id()].received, 32);
        let journal = resume::Journal::with_db(dir.path(), db);
        assert_eq!(
            journal
                .scan::<SessionEvent>(&format!("event/lab1/{}/", session.id()))
                .unwrap()
                .len(),
            32
        );
    }

    fn hold_writer(
        db: crate::db::Db,
    ) -> (std::sync::mpsc::Sender<()>, std::thread::JoinHandle<()>) {
        let (ready, acquired) = std::sync::mpsc::channel();
        let (release, released) = std::sync::mpsc::channel();
        let thread = std::thread::spawn(move || {
            db.read_now(|_| {
                ready.send(()).unwrap();
                released.recv_timeout(Duration::from_secs(30)).unwrap();
                Ok(())
            })
            .unwrap();
        });
        acquired.recv_timeout(Duration::from_secs(5)).unwrap();
        (release, thread)
    }

    #[test]
    fn metadata_lock_is_taken_before_registry_lock() {
        let reg = registry("lab1", true);
        let _metadata = reg.metadata.lock().unwrap();
        let entries = reg.lock();
        assert!(entries.contains_key("lab1"));
    }

    fn metadata_retry_started(reg: &HostRegistry) {
        let deadline = Instant::now() + Duration::from_secs(5);
        while reg.metadata.try_lock().is_ok() {
            assert!(Instant::now() < deadline);
            std::thread::yield_now();
        }
        // The retry keeps its metadata intention, but not the registry lock.
        let guard = reg.lock();
        assert!(guard.contains_key("lab1"));
    }

    #[test]
    fn contended_metadata_retries_in_order_and_rechecks_current_state() {
        let dir = tempfile::tempdir().unwrap();
        let conn = crate::db::open(&dir.path().join("terrarium.db")).unwrap();
        crate::secrets::ensure_key(&conn, dir.path()).unwrap();
        let db = crate::db::Db::new(conn, crate::events::EventHub::default());
        let reg = Arc::new(registry("lab1", true));
        reg.restore_with_db(dir.path(), db.clone()).unwrap();
        let (tx, mut outgoing) = mpsc::unbounded_channel();
        reg.lock().get_mut("lab1").unwrap().conn = Some(Conn { id: 1, tx });
        let journal = resume::Journal::with_db(dir.path(), db.clone());
        let (release, writer) = hold_writer(db.clone());
        let r = reg.clone();
        let calls = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let frame_calls = calls.clone();
        let opening = std::thread::spawn(move || {
            r.open_bound("lab1", Some("test/open"), |_| {
                frame_calls.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                DaemonFrame::Ping { nonce: 42 }
            })
        });
        metadata_retry_started(&reg);
        assert!(reg.lock()["lab1"].sessions.is_empty());
        assert_eq!(reg.lock()["lab1"].next_id, 0);
        assert!(outgoing.try_recv().is_err());
        release.send(()).unwrap();
        writer.join().unwrap();
        let session = opening.join().unwrap().unwrap();
        assert_eq!(calls.load(std::sync::atomic::Ordering::SeqCst), 1);
        assert!(matches!(outgoing.try_recv().unwrap(), Outgoing::Frame(_)));
        assert_eq!(
            journal.load::<(String, u32)>("bound/test/open").unwrap(),
            Some(("lab1".into(), session.id()))
        );

        let (release, writer) = hold_writer(db.clone());
        let r = reg.clone();
        let id = session.id();
        let older = std::thread::spawn(move || {
            r.save_exec(
                "test",
                &json!({"host_session": ["lab1", id], "revision": 1}),
            )
        });
        metadata_retry_started(&reg);
        let r = reg.clone();
        let newer = std::thread::spawn(move || {
            r.save_exec(
                "test",
                &json!({"host_session": ["lab1", id], "revision": 2}),
            )
        });
        release.send(()).unwrap();
        writer.join().unwrap();
        older.join().unwrap().unwrap();
        newer.join().unwrap().unwrap();
        assert_eq!(
            journal.load::<Value>("exec/test").unwrap().unwrap()["revision"],
            2
        );

        let other = reg
            .open("lab1", |_| DaemonFrame::Ping { nonce: 0 })
            .unwrap();
        outgoing.try_recv().unwrap();
        let (release, writer) = hold_writer(db.clone());
        let r = reg.clone();
        let deleting = std::thread::spawn(move || r.forget_exec("test"));
        metadata_retry_started(&reg);
        assert!(reg.lock()["lab1"].sessions.contains_key(&id));
        assert!(outgoing.try_recv().is_err());
        release.send(()).unwrap();
        writer.join().unwrap();
        // This event may commit before or after deletion. Either order must
        // preserve it instead of writing the metadata retry's old snapshot.
        reg.on_data(
            "lab1",
            DataFrame::new(other.id(), StreamTag::Stdout, b"new".to_vec()),
        );
        deleting.join().unwrap().unwrap();
        if !reg.lock()["lab1"].pending_journal.is_empty() {
            reg.lock()
                .get_mut("lab1")
                .unwrap()
                .pending_journal
                .get_mut(&other.id())
                .unwrap()
                .next = Instant::now();
            reg.retry_journal("lab1", 1);
        }
        let saved = journal.load::<SavedHost>("host/lab1").unwrap().unwrap();
        assert!(!saved.sessions.contains_key(&id));
        assert_eq!(saved.sessions[&other.id()].received, 3);
        assert!(journal.load::<Value>("exec/test").unwrap().is_none());
    }

    #[test]
    fn metadata_deadline_leaves_no_partial_registry_or_journal_state() {
        let dir = tempfile::tempdir().unwrap();
        let conn = crate::db::open(&dir.path().join("terrarium.db")).unwrap();
        crate::secrets::ensure_key(&conn, dir.path()).unwrap();
        let db = crate::db::Db::new(conn, crate::events::EventHub::default());
        let reg = Arc::new(registry("lab1", true));
        reg.restore_with_db(dir.path(), db.clone()).unwrap();
        let session = reg
            .open("lab1", |_| DaemonFrame::Ping { nonce: 0 })
            .unwrap();
        reg.save_exec("kept", &json!({"host_session": ["lab1", session.id()]}))
            .unwrap();
        let next_id = reg.lock()["lab1"].next_id;
        db.read_now(|_| {
            assert!(matches!(
                reg.open_bound("lab1", Some("never"), |_| DaemonFrame::Ping { nonce: 0 }),
                Err(HostError::JournalBusy)
            ));
            assert_eq!(
                reg.save_exec("new", &1).unwrap_err().code,
                Code::ResourceBusy
            );
            assert_eq!(
                reg.forget_exec("kept").unwrap_err().code,
                Code::ResourceBusy
            );
            assert_eq!(reg.lock()["lab1"].sessions.len(), 1);
            assert_eq!(reg.lock()["lab1"].next_id, next_id);
            Ok(())
        })
        .unwrap();
        let journal = resume::Journal::with_db(dir.path(), db);
        assert!(journal.load::<Value>("exec/kept").unwrap().is_some());
        assert!(journal.load::<Value>("exec/new").unwrap().is_none());
        assert!(journal.load::<Value>("bound/never").unwrap().is_none());
        assert_eq!(
            journal
                .load::<SavedHost>("host/lab1")
                .unwrap()
                .unwrap()
                .sessions
                .len(),
            1
        );
    }

    #[test]
    fn contended_reconnect_commits_missing_session_without_an_undriven_queue() {
        let dir = tempfile::tempdir().unwrap();
        let conn = crate::db::open(&dir.path().join("terrarium.db")).unwrap();
        crate::secrets::ensure_key(&conn, dir.path()).unwrap();
        let db = crate::db::Db::new(conn, crate::events::EventHub::default());
        let reg = Arc::new(registry("lab1", true));
        reg.restore_with_db(dir.path(), db.clone()).unwrap();
        let mut session = reg
            .open("lab1", |_| DaemonFrame::Ping { nonce: 0 })
            .unwrap();
        reg.detach("lab1", 1).unwrap();
        let hello = Hello {
            path_kind: terrarium_host::protocol::PathKind::Posix,
            protocol: PROTOCOL_VERSION,
            host: "lab1".into(),
            version: "test".into(),
            os: "linux".into(),
            arch: "x86_64".into(),
            capabilities: HostCapabilities::default(),
            max_sessions: 8,
            max_running_targets: None,
            update_failed: None,
            resume: None,
        };
        let (release, writer) = hold_writer(db);
        let r = reg.clone();
        let attaching = std::thread::spawn(move || {
            r.attach(
                &host_row(),
                &hello,
                Conn {
                    id: 2,
                    tx: mpsc::unbounded_channel().0,
                },
            )
        });
        metadata_retry_started(&reg);
        assert!(reg.lock()["lab1"].conn.is_none());
        assert!(!reg.lock()["lab1"].sessions[&session.id()].terminal);
        assert!(reg.lock()["lab1"].pending_journal.is_empty());
        assert!(session.rx.try_recv().is_err());
        release.send(()).unwrap();
        writer.join().unwrap();
        attaching.join().unwrap().unwrap();
        assert!(reg.is_online("lab1"));
        assert!(matches!(
            session.rx.try_recv().unwrap(),
            SessionEvent::Lost { .. }
        ));
        assert!(session.rx.try_recv().is_err());
        assert!(reg.lock()["lab1"].pending_journal.is_empty());
    }

    fn lost_test_hello(token: &str, ids: &[SessionId]) -> Hello {
        Hello {
            path_kind: terrarium_host::protocol::PathKind::Posix,
            protocol: PROTOCOL_VERSION,
            host: "lab1".into(),
            version: "test".into(),
            os: "linux".into(),
            arch: "x86_64".into(),
            capabilities: HostCapabilities::default(),
            max_sessions: 8,
            max_running_targets: None,
            update_failed: None,
            resume: Some(terrarium_host::protocol::Resume {
                token: token.into(),
                sessions: ids
                    .iter()
                    .map(|id| terrarium_host::protocol::ResumeOffer {
                        session: *id,
                        sent: 0,
                        finished: false,
                    })
                    .collect(),
            }),
        }
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn offline_expiry_and_abandon_retry_without_reconnect_exactly_once() {
        for expiry in [true, false] {
            let dir = tempfile::tempdir().unwrap();
            let conn = crate::db::open(&dir.path().join("terrarium.db")).unwrap();
            crate::secrets::ensure_key(&conn, dir.path()).unwrap();
            let db = crate::db::Db::new(conn, crate::events::EventHub::default());
            let reg = Arc::new(registry("lab1", true));
            reg.restore_with_db(dir.path(), db.clone()).unwrap();
            reg.lock().get_mut("lab1").unwrap().resume_token = Some("offline-token".into());
            let mut session = reg
                .open("lab1", |_| DaemonFrame::Ping { nonce: 0 })
                .unwrap();
            let generation = reg.detach("lab1", 1).unwrap();
            let (release, writer) = hold_writer(db.clone());
            if expiry {
                reg.expire("lab1", generation);
            } else {
                session.abandon();
            }
            assert!(reg.lock()["lab1"].pending_journal.is_empty());
            assert!(reg.lock()["lab1"].pending_lost.contains_key(&session.id()));
            assert!(!reg.lock()["lab1"].sessions[&session.id()].terminal);
            assert!(session.rx.try_recv().is_err());
            release.send(()).unwrap();
            writer.join().unwrap();
            let event = tokio::time::timeout(Duration::from_secs(5), session.rx.recv())
                .await
                .unwrap()
                .unwrap();
            assert!(matches!(event, SessionEvent::Lost { .. }));
            assert!(session.rx.try_recv().is_err());
            reg.finish_pending_lost("lab1", Duration::ZERO);
            assert!(session.rx.try_recv().is_err());
            let journal = resume::Journal::with_db(dir.path(), db);
            assert_eq!(
                journal
                    .scan::<SessionEvent>(&format!("event/lab1/{}/", session.id()))
                    .unwrap()
                    .len(),
                1
            );
            let hello = lost_test_hello("offline-token", &[session.id()]);
            let (welcome, _, count, _) = reg
                .attach(
                    &host_row(),
                    &hello,
                    Conn {
                        id: 2,
                        tx: mpsc::unbounded_channel().0,
                    },
                )
                .unwrap();
            assert_eq!(count, 0);
            assert!(welcome.resume.is_empty());
            assert!(session.rx.try_recv().is_err());
        }
    }

    #[test]
    fn expired_retry_deadline_keeps_a_nonresumable_intention_for_the_sweep() {
        let dir = tempfile::tempdir().unwrap();
        let conn = crate::db::open(&dir.path().join("terrarium.db")).unwrap();
        crate::secrets::ensure_key(&conn, dir.path()).unwrap();
        let db = crate::db::Db::new(conn, crate::events::EventHub::default());
        let reg = Arc::new(registry("lab1", true));
        reg.restore_with_db(dir.path(), db.clone()).unwrap();
        reg.lock().get_mut("lab1").unwrap().resume_token = Some("offline-token".into());
        let mut session = reg
            .open("lab1", |_| DaemonFrame::Ping { nonce: 0 })
            .unwrap();
        reg.detach("lab1", 1).unwrap();
        reg.lock().get_mut("lab1").unwrap().pending_lost.insert(
            session.id(),
            SessionEvent::Lost {
                message: "expired".into(),
            },
        );
        let hello = lost_test_hello("offline-token", &[session.id()]);
        db.read_now(|_| {
            reg.finish_pending_lost("lab1", Duration::ZERO);
            assert!(reg.lock()["lab1"].pending_lost.contains_key(&session.id()));
            assert_eq!(
                reg.try_attach(
                    &host_row(),
                    &hello,
                    Conn {
                        id: 2,
                        tx: mpsc::unbounded_channel().0
                    }
                )
                .err()
                .unwrap()
                .code,
                Code::ResourceBusy
            );
            assert!(reg.lock()["lab1"].conn.is_none());
            assert!(session.rx.try_recv().is_err());
            Ok(())
        })
        .unwrap();
        // This is the same operation driven by the independent 30-second sweep.
        reg.finish_pending_lost("lab1", Duration::ZERO);
        assert!(matches!(
            session.rx.try_recv().unwrap(),
            SessionEvent::Lost { .. }
        ));
        assert!(reg.lock()["lab1"].pending_lost.is_empty());
        let (welcome, _, count, _) = reg
            .attach(
                &host_row(),
                &hello,
                Conn {
                    id: 2,
                    tx: mpsc::unbounded_channel().0,
                },
            )
            .unwrap();
        assert_eq!(count, 0);
        assert!(welcome.resume.is_empty());
        assert!(session.rx.try_recv().is_err());
    }

    #[test]
    fn attach_reconciliation_rolls_back_all_missing_sessions_on_any_row_failure() {
        for final_host_row in [false, true] {
            let dir = tempfile::tempdir().unwrap();
            let conn = crate::db::open(&dir.path().join("terrarium.db")).unwrap();
            crate::secrets::ensure_key(&conn, dir.path()).unwrap();
            let db = crate::db::Db::new(conn, crate::events::EventHub::default());
            let reg = Arc::new(registry("lab1", true));
            reg.restore_with_db(dir.path(), db.clone()).unwrap();
            let mut first = reg
                .open("lab1", |_| DaemonFrame::Ping { nonce: 0 })
                .unwrap();
            let mut second = reg
                .open("lab1", |_| DaemonFrame::Ping { nonce: 0 })
                .unwrap();
            let failing_row = if final_host_row {
                "host/lab1".to_owned()
            } else {
                format!("event/lab1/{}/{:020}", second.id(), 1)
            };
            db.read_now(|c| {
                c.execute_batch(&format!("CREATE TRIGGER fail_attach BEFORE INSERT ON host_resume WHEN NEW.id='{failing_row}' BEGIN SELECT RAISE(ABORT, 'injected attach failure'); END"))?;
                Ok(())
            }).unwrap();
            let mut hello = lost_test_hello("unused", &[]);
            hello.resume = None;
            assert!(
                reg.attach(
                    &host_row(),
                    &hello,
                    Conn {
                        id: 2,
                        tx: mpsc::unbounded_channel().0
                    }
                )
                .is_err()
            );
            assert_eq!(reg.lock()["lab1"].conn.as_ref().unwrap().id, 1);
            assert!(reg.lock()["lab1"].resume_token.is_none());
            for id in [first.id(), second.id()] {
                assert!(!reg.lock()["lab1"].sessions[&id].terminal);
                assert_eq!(reg.lock()["lab1"].sessions[&id].events, 0);
            }
            assert!(first.rx.try_recv().is_err());
            assert!(second.rx.try_recv().is_err());
            let journal = resume::Journal::with_db(dir.path(), db.clone());
            assert!(
                journal
                    .scan::<SessionEvent>("event/lab1/")
                    .unwrap()
                    .is_empty()
            );
            let saved = journal.load::<SavedHost>("host/lab1").unwrap().unwrap();
            assert!(
                saved
                    .sessions
                    .values()
                    .all(|s| !s.terminal && s.events == 0)
            );
            db.read_now(|c| {
                c.execute_batch("DROP TRIGGER fail_attach")?;
                Ok(())
            })
            .unwrap();
            reg.attach(
                &host_row(),
                &hello,
                Conn {
                    id: 2,
                    tx: mpsc::unbounded_channel().0,
                },
            )
            .unwrap();
            assert_eq!(reg.lock()["lab1"].conn.as_ref().unwrap().id, 2);
            assert!(matches!(
                first.rx.try_recv().unwrap(),
                SessionEvent::Lost { .. }
            ));
            assert!(matches!(
                second.rx.try_recv().unwrap(),
                SessionEvent::Lost { .. }
            ));
            assert!(first.rx.try_recv().is_err());
            assert!(second.rx.try_recv().is_err());
            assert_eq!(
                journal.scan::<SessionEvent>("event/lab1/").unwrap().len(),
                2
            );
        }
    }

    #[test]
    fn the_history_keeps_the_last_hour() {
        let reg = registry("lab1", true);
        assert!(reg.latest_metrics("lab1").is_none());
        assert!(reg.metrics("lab1", Duration::from_secs(3600)).is_none());
        let now = Instant::now();
        for i in 0..(METRICS_HISTORY as u32 + 5) {
            let back = Duration::from_secs(10) * (METRICS_HISTORY as u32 + 4 - i);
            reg.record_metrics("lab1", sample(i), now.checked_sub(back).unwrap_or(now));
        }
        let all = reg.metrics("lab1", Duration::from_secs(3600)).unwrap();
        assert_eq!(all.samples.len(), METRICS_HISTORY);
        // The 5 oldest went out; oldest first.
        assert_eq!(all.samples[0].metrics.cpu.as_ref().unwrap().cores, 5);
        let (last, stale) = reg.latest_metrics("lab1").unwrap();
        assert_eq!(last.metrics.cpu.unwrap().cores, METRICS_HISTORY as u32 + 4);
        assert!(!stale);
        assert!(!all.stale);
        // 15 minutes at 10 s: 91 samples (0 s to 900 s back). The window
        // has 5 s more, because `metrics` reads the clock after `now`.
        let recent = reg
            .metrics("lab1", Duration::from_secs(15 * 60 + 5))
            .unwrap();
        assert_eq!(recent.samples.len(), 91);
        // A sample for a host that never connected is dropped.
        reg.record_metrics("nope", sample(1), now);
        assert!(reg.latest_metrics("nope").is_none());
    }

    #[test]
    fn old_or_offline_samples_are_stale() {
        let reg = registry("lab1", true);
        let old = Instant::now().checked_sub(METRICS_STALE + Duration::from_secs(1));
        reg.record_metrics("lab1", sample(2), old.unwrap_or_else(Instant::now));
        if old.is_some() {
            assert!(reg.latest_metrics("lab1").unwrap().1);
        }
        let reg = registry("lab2", false);
        reg.record_metrics("lab2", sample(2), Instant::now());
        assert!(reg.latest_metrics("lab2").unwrap().1);
        assert!(reg.metrics("lab2", Duration::from_secs(60)).unwrap().stale);
    }

    #[test]
    fn a_sample_serializes_flat_with_its_time() {
        let reg = registry("lab1", true);
        reg.record_metrics("lab1", sample(8), Instant::now());
        let (s, _) = reg.latest_metrics("lab1").unwrap();
        let v = serde_json::to_value(&s).unwrap();
        assert!(v["at"].is_string());
        assert_eq!(v["cpu"]["cores"], 8);
        assert!(v.get("received").is_none());
        assert!(v.get("metrics").is_none());
    }
}
