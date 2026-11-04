//! The daemon side of the host agent protocol (`docs/host-protocol.md`):
//! the dial-in endpoint `GET /hosts/connect`, the live host registry, and
//! the session multiplexer.
//!
//! - [`connect`] checks the bearer token against the `hosts` table before
//!   the upgrade (`401` on a bad token), reads `hello`, and answers
//!   `welcome` with a new resume token and the sessions it takes back.
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
use std::sync::{Arc, Mutex, MutexGuard};
use std::time::{Duration, Instant};

use axum::extract::connect_info::{ConnectInfo, Connected};
use axum::extract::ws::{Message as WsMessage, WebSocket};
use axum::extract::{FromRequestParts, Request, State, WebSocketUpgrade};
use axum::http::header;
use axum::response::{IntoResponse, Response};
use futures_util::{SinkExt, StreamExt};
use serde::Serialize;
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
#[derive(Debug, Clone, PartialEq)]
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
}

impl From<HostError> for ApiError {
    fn from(e: HostError) -> Self {
        match e {
            HostError::Offline(_) => ApiError::conflict(e.to_string()),
            HostError::Busy(..) => ApiError::new(Code::RateLimited, e.to_string()),
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
}

/// The live state of a host, from its last `hello`.
#[derive(Debug, Clone, Serialize)]
pub struct LiveInfo {
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
    next_conn: std::sync::atomic::AtomicU64,
    pub heartbeat: Duration,
    pub resume_grace: Duration,
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
            next_conn: std::sync::atomic::AtomicU64::new(1),
            heartbeat,
            resume_grace,
        }
    }

    fn lock(&self) -> MutexGuard<'_, HashMap<String, Entry>> {
        self.entries.lock().unwrap_or_else(|e| e.into_inner())
    }

    /// Is the host connected now?
    pub fn is_online(&self, host: &str) -> bool {
        self.lock().get(host).is_some_and(|e| e.conn.is_some())
    }

    /// The live state of a host.
    pub fn view(&self, host: &str) -> LiveView {
        let g = self.lock();
        match g.get(host) {
            Some(e) => LiveView {
                online: e.conn.is_some(),
                sessions: e.sessions.len(),
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
        let mut g = self.lock();
        let e = g
            .get_mut(host)
            .filter(|e| e.conn.is_some())
            .ok_or_else(|| HostError::Offline(host.to_owned()))?;
        let max = e.info.as_ref().map_or(32, |i| i.max_sessions.max(1));
        if e.sessions.len() >= max as usize {
            return Err(HostError::Busy(host.to_owned(), max));
        }
        let id = loop {
            e.next_id = e.next_id.wrapping_add(1);
            if e.next_id != 0 && !e.sessions.contains_key(&e.next_id) {
                break e.next_id;
            }
        };
        let (tx, rx) = mpsc::unbounded_channel();
        e.sessions.insert(
            id,
            Slot {
                tx,
                received: 0,
                held: false,
            },
        );
        if let Some(c) = &e.conn {
            let _ = c.tx.send(Outgoing::frame(make(id)));
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
    fn attach(&self, row: &Host, hello: &Hello, conn: Conn) -> (Welcome, Option<Conn>, usize, u64) {
        let token = format!("r-{}", ulid::Ulid::new());
        let mut g = self.lock();
        let e = g.entry(row.name.clone()).or_default();
        let old = e.conn.replace(conn);
        e.generation += 1;
        e.last_error = None;
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
            if offers.contains_key(&id) {
                if let Some(s) = e.sessions.get_mut(&id) {
                    s.held = true;
                    resume.push(ResumeAccept {
                        session: id,
                        received: s.received,
                    });
                }
            } else if let Some(s) = e.sessions.remove(&id) {
                lost.push(s);
            }
        }
        resume.sort_by_key(|r| r.session);
        send_events(lost, "the host agent no longer holds the session");
        e.resume_token = Some(token.clone());
        e.info = Some(LiveInfo {
            host_id: row.id.clone(),
            version: hello.version.clone(),
            os: hello.os.clone(),
            arch: hello.arch.clone(),
            capabilities: hello.capabilities.clone(),
            max_sessions: hello.max_sessions,
            max_running_targets: hello.max_running_targets,
            connected_at: crate::db::now(),
        });
        let n = resume.len();
        (
            Welcome {
                host_id: row.id.clone(),
                heartbeat_secs: self.heartbeat.as_secs().max(1),
                resume_token: token,
                resume,
                token: None,
                metrics: true,
            },
            old,
            n,
            e.generation,
        )
    }

    /// The link `conn_id` ended. True when it was the current link.
    fn detach(&self, host: &str, conn_id: u64) -> Option<u64> {
        let mut g = self.lock();
        let e = g.get_mut(host)?;
        if e.conn.as_ref().is_some_and(|c| c.id == conn_id) {
            e.conn = None;
            Some(e.generation)
        } else {
            None
        }
    }

    /// After the resume grace, give up the sessions of a host that did not
    /// come back.
    fn expire(&self, host: &str, generation: u64) {
        let mut g = self.lock();
        if let Some(e) = g.get_mut(host)
            && e.conn.is_none()
            && e.generation == generation
        {
            let slots: Vec<Slot> = e.sessions.drain().map(|(_, s)| s).collect();
            send_events(
                slots,
                "the host agent did not come back within the resume grace",
            );
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
        let Some(s) = g.get_mut(host).and_then(|e| e.sessions.get_mut(&f.session)) else {
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
        s.received += f.payload.len() as u64;
        let _ = s.tx.send(SessionEvent::Data {
            stream: f.stream,
            bytes: f.payload,
        });
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
        if matches!(event, SessionEvent::Started { .. })
            && let Some(s) = e.sessions.get_mut(&session)
        {
            s.held = true;
        }
        if !event.is_terminal() {
            match e.sessions.get(&session) {
                Some(s) => {
                    let _ = s.tx.send(event);
                }
                None => debug!(host, session, "frame for an unknown session"),
            }
            return;
        }
        let Some(s) = e.sessions.remove(&session) else {
            debug!(host, session, "terminal frame for an unknown session");
            return;
        };
        // An `error` for a session that never started holds no state on
        // the host: no `release` for it.
        let release = s.held || !matches!(event, SessionEvent::Error { .. });
        if let Some(c) = &e.conn {
            let _ = c.tx.send(Outgoing::frame(DaemonFrame::Ack {
                session,
                bytes: s.received,
            }));
            if release {
                let _ = c.tx.send(Outgoing::frame(DaemonFrame::Release { session }));
            }
        }
        let _ = s.tx.send(event);
    }

    /// The consumer dropped a session before its end: abandon it.
    fn abandon(&self, host: &str, session: SessionId) {
        let mut g = self.lock();
        if let Some(e) = g.get_mut(host)
            && e.sessions.remove(&session).is_some()
            && let Some(c) = &e.conn
        {
            let _ = c.tx.send(Outgoing::frame(DaemonFrame::Release { session }));
        }
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
        if !self.finished {
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

/// The router of the dial-in listener: only `/hosts/connect`.
pub fn router(state: Shared) -> axum::Router {
    axum::Router::new()
        .route(
            terrarium_host::protocol::CONNECT_PATH,
            axum::routing::get(connect),
        )
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
    let conn_id = reg
        .next_conn
        .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    let (tx, mut rx) = mpsc::unbounded_channel::<Outgoing>();
    let (mut welcome, old, resumed, _) = reg.attach(
        &row,
        &hello,
        Conn {
            id: conn_id,
            tx: tx.clone(),
        },
    );
    if let Some(old) = old {
        let _ = old.tx.send(Outgoing::Close(Some((
            "replaced",
            "a new connection with the same token took over".into(),
        ))));
    }
    welcome.token = new_token;
    match encode_control(&DaemonFrame::Welcome(welcome.clone())) {
        Ok(t) => {
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
    if terrarium_version::is_stale(&hello.version, terrarium_version::FULL) {
        warn!(
            host = %row.name,
            version = %hello.version,
            daemon = terrarium_version::FULL,
            "the host agent is built from another ref than the daemon: reinstall it"
        );
    }
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
        tokio::spawn(async move { crate::executor::reconcile_host(&state, &host).await });
    }

    let (mut sink, mut stream) = socket.split();
    let hb = reg.heartbeat.max(Duration::from_millis(10));
    let mut ticker = tokio::time::interval_at(tokio::time::Instant::now() + hb, hb);
    let mut last_rx = Instant::now();
    let mut nonce = 0u64;
    let why = loop {
        tokio::select! {
            msg = stream.next() => {
                last_rx = Instant::now();
                match msg {
                    Some(Ok(WsMessage::Text(t))) => match decode_control::<HostFrame>(t.as_str()) {
                        Ok(HostFrame::Ping { nonce }) => {
                            let _ = tx.send(Outgoing::frame(DaemonFrame::Pong { nonce }));
                        }
                        Ok(HostFrame::Pong { .. }) => {}
                        Ok(HostFrame::Hello(_)) => warn!(host = %row.name, "a second hello; ignored"),
                        Ok(HostFrame::Metrics(m)) => reg.record_metrics(&row.name, *m, Instant::now()),
                        Ok(f) => reg.on_session_frame(&row.name, f),
                        Err(e) => warn!(host = %row.name, "bad control frame: {e}"),
                    },
                    Some(Ok(WsMessage::Binary(b))) => match DataFrame::decode(&b) {
                        Ok(f) => reg.on_data(&row.name, f),
                        Err(e) => warn!(host = %row.name, "bad data frame: {e}"),
                    },
                    Some(Ok(WsMessage::Close(_))) | None => break "closed by the host",
                    Some(Ok(_)) => {}
                    Some(Err(e)) => {
                        debug!(host = %row.name, "link error: {e}");
                        break "link error";
                    }
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
                if last_rx.elapsed() > hb * MISSED_HEARTBEATS {
                    break "heartbeat timeout";
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
