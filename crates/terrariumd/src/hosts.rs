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

use std::collections::HashMap;
use std::sync::{Arc, Mutex, MutexGuard};
use std::time::{Duration, Instant};

use axum::extract::ws::{Message as WsMessage, WebSocket};
use axum::extract::{FromRequestParts, Request, State, WebSocketUpgrade};
use axum::http::header;
use axum::response::{IntoResponse, Response};
use futures_util::{SinkExt, StreamExt};
use serde::Serialize;
use serde_json::{Value, json};
use terrarium_host::protocol::{
    DaemonFrame, DataFrame, ExitReason, Hello, HostCapabilities, HostFrame, PROTOCOL_VERSION,
    ResumeAccept, SessionId, StreamTag, Welcome, decode_control, encode_control,
};
use tokio::sync::mpsc;
use tokio::task::JoinHandle;
use tracing::{debug, info, warn};

use crate::api::Shared;
use crate::error::{ApiError, Code};
use crate::events::EventDraft;
use crate::store::hosts::{self, Host};

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
            connected_at: crate::db::now(),
        });
        let n = resume.len();
        (
            Welcome {
                host_id: row.id.clone(),
                heartbeat_secs: self.heartbeat.as_secs().max(1),
                resume_token: token,
                resume,
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
            HostFrame::Hello(_) | HostFrame::Ping { .. } | HostFrame::Pong { .. } => return,
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
    let row = match state.db.read(move |c| hosts::by_token_hash(c, &hash)).await {
        Ok(Some(r)) => r,
        Ok(None) => return unauthorized("unknown or revoked host token"),
        Err(e) => return e.into_response(),
    };
    let (mut parts, _) = req.into_parts();
    let ws = match WebSocketUpgrade::from_request_parts(&mut parts, &()).await {
        Ok(ws) => ws,
        Err(e) => return ApiError::bad_request(e.body_text()).into_response(),
    };
    ws.max_message_size(MAX_MESSAGE)
        .max_frame_size(MAX_MESSAGE)
        .on_upgrade(move |socket| serve_link(state, socket, row))
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

/// Bind the dial-in listener (`TERRARIUM_HOST_LISTEN`, default
/// `0.0.0.0:8082`) and serve on it in the background.
pub async fn serve(
    state: Shared,
    addr: &str,
) -> anyhow::Result<(JoinHandle<()>, std::net::SocketAddr)> {
    use anyhow::Context;
    let listener = tokio::net::TcpListener::bind(addr)
        .await
        .with_context(|| format!("cannot bind the host listener {addr}"))?;
    let local = listener.local_addr()?;
    info!(addr = %local, "host agent dial-in listening on TCP");
    let app = router(state);
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

async fn serve_link(state: Shared, mut socket: WebSocket, row: Host) {
    let reg = state.executor.hosts.clone();
    let hello = match read_hello(&mut socket).await {
        Ok(h) => h,
        Err(e) => {
            warn!(host = %row.name, "bad hello: {e}");
            goodbye(&mut socket, "protocol", e).await;
            return;
        }
    };
    if hello.protocol != PROTOCOL_VERSION {
        goodbye(
            &mut socket,
            "protocol",
            format!(
                "protocol {} is not supported; this daemon speaks {PROTOCOL_VERSION}",
                hello.protocol
            ),
        )
        .await;
        return;
    }
    if hello.host != row.name {
        goodbye(
            &mut socket,
            "protocol",
            format!(
                "this token belongs to host `{}`, not `{}`",
                row.name, hello.host
            ),
        )
        .await;
        return;
    }
    let conn_id = reg
        .next_conn
        .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    let (tx, mut rx) = mpsc::unbounded_channel::<Outgoing>();
    let (welcome, old, resumed, _) = reg.attach(
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
    info!(host = %row.name, resumed, "host agent connected");
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
                       "summary": summary, "resumed": resumed}),
            ),
            move |t| hosts::mark_online(t, &name, &caps2, &os, &arch, &version),
        )
        .await;
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
