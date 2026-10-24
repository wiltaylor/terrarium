//! The dial-out loop: connect, `hello`/`welcome`, heartbeats, the session
//! multiplexer, acks, replay, and reconnect with backoff.
//!
//! Sessions live in the [`Client`], not in a connection. When the link
//! drops, every session keeps running and buffers its output in its
//! [`Outbox`]. The next `hello` offers them to the daemon with the resume
//! token; the `welcome` says which ones the daemon takes back and from which
//! byte to replay.

use std::collections::HashMap;
use std::hash::{BuildHasher, Hasher};
use std::path::PathBuf;
use std::sync::{Arc, Mutex, MutexGuard};
use std::time::{Duration, Instant};

use futures_util::{SinkExt, StreamExt};
use tokio::sync::mpsc;
use tokio_tungstenite::tungstenite::client::IntoClientRequest;
use tokio_tungstenite::tungstenite::http::header::{AUTHORIZATION, HeaderValue};
use tokio_tungstenite::tungstenite::protocol::WebSocketConfig;
use tokio_tungstenite::tungstenite::{self, Message};

use crate::config::PathPolicy;
use crate::outbox::{Link, Outbox, Outgoing};
use crate::protocol::{
    CpPull, CpPush, DaemonFrame, DataFrame, ErrorCode, ExitReason, Hello, HostCapabilities,
    HostFrame, PROTOCOL_VERSION, Resume, ResumeOffer, SessionId, StreamTag, Welcome,
    decode_control, encode_control, parse_signal,
};
use crate::session::{self, SessionInput};
use crate::transfer::{self, PushFile};
use crate::verb::{BuildError, VerbPlan};
use crate::{docker, vmlab};

/// How many heartbeat intervals may pass with nothing received.
const MISSED_HEARTBEATS: u32 = 3;
/// Queued push chunks before the reader waits for the disk.
const PUSH_QUEUE: usize = 64;

/// Everything the client needs. `main` builds it from the config file.
#[derive(Debug, Clone)]
pub struct ClientOptions {
    /// The full WebSocket URL, for example `wss://d:8082/hosts/connect`.
    pub url: String,
    pub token: String,
    pub name: String,
    pub max_sessions: u32,
    pub capabilities: HostCapabilities,
    pub buffer_limit: usize,
    /// How long a session lives with no daemon connection.
    pub resume_grace: Duration,
    pub backoff_min: Duration,
    pub backoff_max: Duration,
    /// Limit for the TCP+TLS+upgrade and for the `welcome`.
    pub handshake_timeout: Duration,
    pub policy: PathPolicy,
    pub docker_bin: String,
    pub vmlab_bin: String,
    /// A TLS config for `wss://`. `None` uses the system roots.
    /// [`crate::Config::transport`] builds it, with pinning and `ca_file`.
    pub tls: Option<Arc<rustls::ClientConfig>>,
    /// Allow `ws://` to a host that is not loopback. Off by default: the
    /// client refuses to send the token in clear text over a network.
    pub allow_insecure: bool,
    /// Where the token lives. When the daemon hands out a new long-lived
    /// token (`welcome.token`, after an enrol token), the client writes it
    /// here, so the next start uses it.
    pub token_file: Option<PathBuf>,
}

impl ClientOptions {
    pub fn new(url: impl Into<String>, token: impl Into<String>, name: impl Into<String>) -> Self {
        Self {
            url: url.into(),
            token: token.into(),
            name: name.into(),
            max_sessions: 32,
            capabilities: HostCapabilities::default(),
            buffer_limit: crate::protocol::DEFAULT_BUFFER_LIMIT,
            resume_grace: Duration::from_secs(600),
            backoff_min: Duration::from_secs(1),
            backoff_max: Duration::from_secs(60),
            handshake_timeout: Duration::from_secs(15),
            policy: PathPolicy::default(),
            docker_bin: "docker".into(),
            vmlab_bin: "vmlab".into(),
            tls: None,
            allow_insecure: false,
            token_file: None,
        }
    }
}

/// Exponential backoff with jitter: each delay is uniform in
/// `[base/2, base]`, and `base` doubles up to the maximum.
#[derive(Debug, Clone)]
pub struct Backoff {
    min: Duration,
    max: Duration,
    base: Duration,
}

impl Backoff {
    pub fn new(min: Duration, max: Duration) -> Self {
        let min = min.max(Duration::from_millis(1));
        Self {
            min,
            max: max.max(min),
            base: min,
        }
    }

    pub fn reset(&mut self) {
        self.base = self.min;
    }

    pub fn next_delay(&mut self) -> Duration {
        let base = self.base;
        self.base = (self.base * 2).min(self.max);
        let half = base / 2;
        let span = (base - half).as_nanos() as u64;
        let r = if span == 0 {
            0
        } else {
            random_u64() % (span + 1)
        };
        half + Duration::from_nanos(r)
    }
}

fn random_u64() -> u64 {
    // RandomState is seeded from the OS; good enough for jitter.
    let mut h = std::collections::hash_map::RandomState::new().build_hasher();
    h.write_u128(
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_nanos())
            .unwrap_or_default(),
    );
    h.finish()
}

#[derive(Debug, thiserror::Error)]
pub enum ClientError {
    #[error("websocket: {0}")]
    Ws(#[from] tungstenite::Error),
    #[error("bad daemon URL or token: {0}")]
    Request(String),
    #[error("timed out: {0}")]
    Timeout(&'static str),
    #[error("protocol: {0}")]
    Protocol(String),
    #[error("daemon said goodbye: {reason}")]
    Goodbye { reason: String },
    #[error("connection closed")]
    Closed,
    #[error(transparent)]
    Insecure(#[from] crate::tls::TlsError),
}

enum PushMsg {
    Data(Vec<u8>),
    End { bytes: u64, sha256: String },
    Abort,
}

struct Entry {
    outbox: Arc<Outbox>,
    input: Option<mpsc::UnboundedSender<SessionInput>>,
    push: Option<mpsc::Sender<PushMsg>>,
    detached_since: Option<Instant>,
}

#[derive(Default)]
struct Shared {
    sessions: Mutex<HashMap<SessionId, Entry>>,
    resume_token: Mutex<Option<String>>,
    /// The bearer token for the next dial. It starts as the configured
    /// token and changes when the daemon sends `welcome.token`.
    bearer: Mutex<String>,
}

/// The host agent's client. Cheap to clone.
#[derive(Clone)]
pub struct Client {
    opts: Arc<ClientOptions>,
    shared: Arc<Shared>,
}

impl Client {
    pub fn new(opts: ClientOptions) -> Self {
        let shared = Shared {
            bearer: Mutex::new(opts.token.clone()),
            ..Shared::default()
        };
        Self {
            opts: Arc::new(opts),
            shared: Arc::new(shared),
        }
    }

    /// The bearer token that the next dial sends.
    pub fn bearer(&self) -> String {
        self.shared
            .bearer
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .clone()
    }

    /// The daemon swapped an enrol token for a long-lived one. Store it in
    /// `token_file` first, then use it.
    fn adopt_token(&self, token: &str) {
        match &self.opts.token_file {
            Some(path) => match crate::config::write_secret(path, token) {
                Ok(()) => tracing::info!(
                    path = %path.display(),
                    "enrolled: stored the long-lived host token"
                ),
                Err(e) => tracing::error!(
                    path = %path.display(),
                    error = %e,
                    "enrolled, but cannot store the long-lived host token; it lives in memory \
                     only, and the next start fails: rotate the token on the daemon"
                ),
            },
            None => tracing::error!(
                "enrolled, but no token_file is set; the long-lived host token lives in memory \
                 only, and the next start fails: set token_file and rotate the token"
            ),
        }
        *self.shared.bearer.lock().unwrap_or_else(|e| e.into_inner()) = token.to_owned();
    }

    /// Refuse plaintext to a host that is not loopback, unless allowed.
    fn check_transport(&self) -> Result<(), ClientError> {
        let url = self.opts.url.to_ascii_lowercase();
        if url.starts_with("ws://") || url.starts_with("http://") {
            let host = crate::tls::url_host(&self.opts.url).unwrap_or_default();
            if !crate::tls::is_loopback_host(&host) && !self.opts.allow_insecure {
                return Err(crate::tls::TlsError::Insecure { host }.into());
            }
        }
        Ok(())
    }

    fn sessions(&self) -> MutexGuard<'_, HashMap<SessionId, Entry>> {
        self.shared
            .sessions
            .lock()
            .unwrap_or_else(|e| e.into_inner())
    }

    fn token(&self) -> MutexGuard<'_, Option<String>> {
        self.shared
            .resume_token
            .lock()
            .unwrap_or_else(|e| e.into_inner())
    }

    /// The number of sessions held (live and finished but not released).
    pub fn session_count(&self) -> usize {
        self.sessions().len()
    }

    /// Dial, serve, and redial for ever.
    pub async fn run(self) {
        ensure_crypto_provider();
        let gc = tokio::spawn(self.clone().reap_detached());
        let mut backoff = Backoff::new(self.opts.backoff_min, self.opts.backoff_max);
        loop {
            match self.connect_once().await {
                Err(ClientError::Insecure(e)) => {
                    tracing::error!(error = %e, "not connecting");
                    gc.abort();
                    return;
                }
                Ok(()) => {
                    tracing::warn!("daemon connection ended");
                    backoff.reset();
                }
                Err(e) => tracing::warn!(error = %e, "daemon connection failed"),
            }
            let delay = backoff.next_delay();
            tracing::info!(?delay, "reconnecting");
            tokio::time::sleep(delay).await;
            if gc.is_finished() {
                tracing::error!("session reaper stopped");
            }
        }
    }

    fn hello(&self) -> Hello {
        let token = self.token().clone();
        let resume = token.map(|token| {
            let mut sessions: Vec<ResumeOffer> = self
                .sessions()
                .iter()
                .map(|(id, e)| ResumeOffer {
                    session: *id,
                    sent: e.outbox.sent(),
                    finished: e.outbox.is_finished(),
                })
                .collect();
            sessions.sort_by_key(|o| o.session);
            Resume { token, sessions }
        });
        Hello {
            protocol: PROTOCOL_VERSION,
            host: self.opts.name.clone(),
            version: terrarium_core::VERSION.to_owned(),
            os: std::env::consts::OS.to_owned(),
            arch: std::env::consts::ARCH.to_owned(),
            capabilities: self.opts.capabilities.clone(),
            max_sessions: self.opts.max_sessions,
            resume,
        }
    }

    /// One connection, from dial to drop. `Ok` means the daemon welcomed us.
    pub async fn connect_once(&self) -> Result<(), ClientError> {
        self.check_transport()?;
        let mut req = self
            .opts
            .url
            .as_str()
            .into_client_request()
            .map_err(|e| ClientError::Request(e.to_string()))?;
        let auth = HeaderValue::from_str(&format!("Bearer {}", self.bearer()))
            .map_err(|e| ClientError::Request(e.to_string()))?;
        req.headers_mut().insert(AUTHORIZATION, auth);
        let connector = self
            .opts
            .tls
            .clone()
            .map(tokio_tungstenite::Connector::Rustls);
        let ws_config = WebSocketConfig::default()
            .max_message_size(Some(64 << 20))
            .max_frame_size(Some(16 << 20));
        let (ws, _) = tokio::time::timeout(
            self.opts.handshake_timeout,
            tokio_tungstenite::connect_async_tls_with_config(
                req,
                Some(ws_config),
                false,
                connector,
            ),
        )
        .await
        .map_err(|_| ClientError::Timeout("connect"))??;
        let (mut sink, mut stream) = ws.split();

        let hello = HostFrame::Hello(Box::new(self.hello()));
        sink.send(Message::Text(text(&hello)?.into())).await?;

        let welcome = tokio::time::timeout(self.opts.handshake_timeout, async {
            loop {
                match stream.next().await {
                    Some(Ok(Message::Text(t))) => match decode_control::<DaemonFrame>(&t) {
                        Ok(DaemonFrame::Welcome(w)) => return Ok(w),
                        Ok(DaemonFrame::Goodbye { reason, .. }) => {
                            return Err(ClientError::Goodbye { reason });
                        }
                        Ok(other) => {
                            return Err(ClientError::Protocol(format!(
                                "expected welcome, got {other:?}"
                            )));
                        }
                        Err(e) => return Err(ClientError::Protocol(e.to_string())),
                    },
                    Some(Ok(Message::Close(_))) | None => return Err(ClientError::Closed),
                    Some(Ok(_)) => continue,
                    Some(Err(e)) => return Err(e.into()),
                }
            }
        })
        .await
        .map_err(|_| ClientError::Timeout("welcome"))??;
        tracing::info!(host_id = %welcome.host_id, resumed = welcome.resume.len(), "connected");

        let (tx, mut rx) = mpsc::unbounded_channel::<Outgoing>();
        self.apply_welcome(&welcome, &tx);

        let mut writer = tokio::spawn(async move {
            while let Some(item) = rx.recv().await {
                let msg = match item {
                    Outgoing::Data(d) => Message::Binary(d.encode().into()),
                    Outgoing::Control(f) => match encode_control(&f) {
                        Ok(t) => Message::Text(t.into()),
                        Err(e) => {
                            tracing::error!(error = %e, "cannot encode a frame");
                            continue;
                        }
                    },
                };
                if sink.send(msg).await.is_err() {
                    break;
                }
            }
            let _ = sink.close().await;
        });

        let hb = Duration::from_secs(welcome.heartbeat_secs.max(1));
        let mut ticker = tokio::time::interval_at(tokio::time::Instant::now() + hb, hb);
        let mut last_rx = Instant::now();
        let mut nonce = 0u64;
        let result = loop {
            tokio::select! {
                msg = stream.next() => {
                    last_rx = Instant::now();
                    match msg {
                        Some(Ok(Message::Text(t))) => {
                            if let Err(e) = self.on_text(&t, &tx).await {
                                break Err(e);
                            }
                        }
                        Some(Ok(Message::Binary(b))) => self.on_binary(&b, &tx).await,
                        Some(Ok(Message::Close(_))) | None => break Ok(()),
                        Some(Ok(_)) => {}
                        Some(Err(e)) => break Err(e.into()),
                    }
                }
                _ = ticker.tick() => {
                    if last_rx.elapsed() > hb * MISSED_HEARTBEATS {
                        break Err(ClientError::Timeout("heartbeat"));
                    }
                    nonce += 1;
                    let _ = tx.send(Outgoing::Control(HostFrame::Ping { nonce }));
                }
                _ = &mut writer => break Ok(()),
            }
        };
        self.detach_all();
        drop(tx);
        // Give the writer a moment to flush a close, then stop it.
        if tokio::time::timeout(Duration::from_millis(200), &mut writer)
            .await
            .is_err()
        {
            writer.abort();
        }
        result
    }

    fn apply_welcome(&self, welcome: &Welcome, tx: &Link) {
        *self.token() = Some(welcome.resume_token.clone());
        if let Some(t) = welcome.token.as_deref().filter(|t| !t.is_empty()) {
            self.adopt_token(t);
        }
        let accepted: HashMap<SessionId, u64> = welcome
            .resume
            .iter()
            .map(|r| (r.session, r.received))
            .collect();
        self.sessions().retain(|id, e| match accepted.get(id) {
            Some(received) => {
                tracing::info!(session = id, received, "resuming session");
                e.outbox.attach(tx.clone(), *received);
                e.detached_since = None;
                true
            }
            None => {
                // Dropping the entry drops the input sender, which kills a
                // live process.
                tracing::info!(session = id, "daemon did not resume session; ending it");
                false
            }
        });
    }

    fn detach_all(&self) {
        let now = Instant::now();
        for e in self.sessions().values_mut() {
            e.outbox.detach();
            e.detached_since.get_or_insert(now);
            if let Some(push) = e.push.take() {
                let _ = push.try_send(PushMsg::Abort);
            }
        }
    }

    /// Kill sessions that stayed detached past the resume grace.
    async fn reap_detached(self) {
        let tick =
            (self.opts.resume_grace / 4).clamp(Duration::from_millis(50), Duration::from_secs(5));
        loop {
            tokio::time::sleep(tick).await;
            for (id, e) in self.sessions().iter_mut() {
                let expired = e
                    .detached_since
                    .is_some_and(|t| t.elapsed() >= self.opts.resume_grace);
                if expired
                    && !e.outbox.is_finished()
                    && let Some(input) = &e.input
                {
                    tracing::warn!(session = id, "detached past the resume grace; killing");
                    let _ = input.send(SessionInput::Kill(ExitReason::DetachedTimeout));
                }
            }
        }
    }

    async fn on_binary(&self, bytes: &[u8], tx: &Link) {
        let frame = match DataFrame::decode(bytes) {
            Ok(f) => f,
            Err(e) => {
                tracing::warn!(error = %e, "bad data frame");
                return;
            }
        };
        match frame.stream {
            StreamTag::Stdin => {
                let input = self
                    .sessions()
                    .get(&frame.session)
                    .and_then(|e| e.input.clone());
                match input {
                    Some(i) => {
                        let _ = i.send(SessionInput::Stdin(frame.payload));
                    }
                    None => unknown(tx, frame.session),
                }
            }
            StreamTag::File => {
                let push = self
                    .sessions()
                    .get(&frame.session)
                    .and_then(|e| e.push.clone());
                match push {
                    Some(p) => {
                        let _ = p.send(PushMsg::Data(frame.payload)).await;
                    }
                    None => unknown(tx, frame.session),
                }
            }
            StreamTag::Stdout | StreamTag::Stderr => {
                tracing::warn!(
                    session = frame.session,
                    "daemon sent an output stream; ignored"
                );
            }
        }
    }

    async fn on_text(&self, text: &str, tx: &Link) -> Result<(), ClientError> {
        let frame: DaemonFrame = match decode_control(text) {
            Ok(f) => f,
            Err(e) => {
                let session = serde_json::from_str::<serde_json::Value>(text)
                    .ok()
                    .and_then(|v| v["session"].as_u64())
                    .and_then(|s| u32::try_from(s).ok())
                    .unwrap_or(0);
                tracing::warn!(session, error = %e, "bad control frame");
                send_error(tx, session, ErrorCode::BadRequest, e.to_string());
                return Ok(());
            }
        };
        let o = &self.opts;
        let docker_ok = o.capabilities.docker.present;
        let vmlab_ok = o.capabilities.vmlab.present;
        let p = &o.policy;
        let (db, vb) = (o.docker_bin.as_str(), o.vmlab_bin.as_str());
        use DaemonFrame as F;
        match frame {
            F::Welcome(_) => tracing::warn!("unexpected welcome; ignored"),
            F::Goodbye { reason, message } => {
                tracing::warn!(%reason, message = message.as_deref().unwrap_or(""), "goodbye");
                return Err(ClientError::Goodbye { reason });
            }
            F::Ping { nonce } => {
                let _ = tx.send(Outgoing::Control(HostFrame::Pong { nonce }));
            }
            F::Pong { .. } => {}
            F::Ack { session, bytes } => {
                if let Some(e) = self.sessions().get(&session) {
                    e.outbox.ack(bytes);
                }
            }
            F::Release { session } => {
                if self.sessions().remove(&session).is_none() {
                    tracing::debug!(session, "release of an unknown session");
                }
            }
            F::Eof { session } => self.input(tx, session, SessionInput::Eof),
            F::Resize {
                session,
                rows,
                cols,
            } => self.input(
                tx,
                session,
                SessionInput::Resize(crate::protocol::PtySize { rows, cols }),
            ),
            F::Signal { session, signal } => match parse_signal(&signal) {
                Some(sig) => self.input(tx, session, SessionInput::Signal(sig)),
                None => send_error(
                    tx,
                    session,
                    ErrorCode::BadRequest,
                    format!("unknown signal {signal:?}"),
                ),
            },
            F::CpEnd {
                session,
                bytes,
                sha256,
            } => {
                let push = self
                    .sessions()
                    .get_mut(&session)
                    .and_then(|e| e.push.take());
                match push {
                    Some(p) => {
                        let _ = p.send(PushMsg::End { bytes, sha256 }).await;
                    }
                    None => unknown(tx, session),
                }
            }
            F::CpPush(req) => self.start_push(req, tx).await,
            F::CpPull(req) => self.start_pull(req, tx),
            F::Exec(req) => {
                let id = req.session;
                self.start(id, crate::verb::plan_exec(&req, p), tx);
            }
            F::DockerRun(r) => self.start(
                r.session,
                need(docker_ok, "docker").and_then(|_| docker::plan_run(&r, db, p)),
                tx,
            ),
            F::DockerExec(r) => self.start(
                r.session,
                need(docker_ok, "docker").and_then(|_| docker::plan_exec(&r, db)),
                tx,
            ),
            F::DockerStop(r) => self.start(
                r.session,
                need(docker_ok, "docker").and_then(|_| docker::plan_stop(&r, db)),
                tx,
            ),
            F::DockerRm(r) => self.start(
                r.session,
                need(docker_ok, "docker").and_then(|_| docker::plan_rm(&r, db)),
                tx,
            ),
            F::DockerInspect(r) => self.start(
                r.session,
                need(docker_ok, "docker").and_then(|_| docker::plan_inspect(&r, db)),
                tx,
            ),
            F::DockerPs(r) => self.start(
                r.session,
                need(docker_ok, "docker").and_then(|_| docker::plan_ps(&r, db)),
                tx,
            ),
            F::DockerVolumeCreate(r) => self.start(
                r.session,
                need(docker_ok, "docker").and_then(|_| docker::plan_volume_create(&r, db)),
                tx,
            ),
            F::DockerVolumeRm(r) => self.start(
                r.session,
                need(docker_ok, "docker").and_then(|_| docker::plan_volume_rm(&r, db)),
                tx,
            ),
            F::VmlabUp(r) => self.start(
                r.session,
                need(vmlab_ok, "vmlab").and_then(|_| vmlab::plan_up(&r, vb, p)),
                tx,
            ),
            F::VmlabDown(r) => self.start(
                r.session,
                need(vmlab_ok, "vmlab").and_then(|_| vmlab::plan_down(&r, vb, p)),
                tx,
            ),
            F::VmlabDestroy(r) => self.start(
                r.session,
                need(vmlab_ok, "vmlab").and_then(|_| vmlab::plan_destroy(&r, vb, p)),
                tx,
            ),
            F::VmlabStatus(r) => self.start(
                r.session,
                need(vmlab_ok, "vmlab").and_then(|_| vmlab::plan_status(&r, vb, p)),
                tx,
            ),
            F::VmlabExec(r) => self.start(
                r.session,
                need(vmlab_ok, "vmlab").and_then(|_| vmlab::plan_exec(&r, vb, p)),
                tx,
            ),
            F::VmlabShell(r) => self.start(
                r.session,
                need(vmlab_ok, "vmlab").and_then(|_| vmlab::plan_shell(&r, vb, p)),
                tx,
            ),
            F::VmlabCp(r) => self.start(
                r.session,
                need(vmlab_ok, "vmlab").and_then(|_| vmlab::plan_cp(&r, vb, p)),
                tx,
            ),
            F::VmlabSnapshotTake(r) => self.start(
                r.session,
                need(vmlab_ok, "vmlab").and_then(|_| vmlab::plan_snapshot_take(&r, vb, p)),
                tx,
            ),
            F::VmlabSnapshotRestore(r) => self.start(
                r.session,
                need(vmlab_ok, "vmlab").and_then(|_| vmlab::plan_snapshot_restore(&r, vb, p)),
                tx,
            ),
            F::VmlabSnapshotList(r) => self.start(
                r.session,
                need(vmlab_ok, "vmlab").and_then(|_| vmlab::plan_snapshot_list(&r, vb, p)),
                tx,
            ),
        }
        Ok(())
    }

    fn input(&self, tx: &Link, session: SessionId, input: SessionInput) {
        let sender = self.sessions().get(&session).and_then(|e| e.input.clone());
        match sender {
            Some(s) => {
                let _ = s.send(input);
            }
            None => unknown(tx, session),
        }
    }

    /// Check the session ID and the session limit. Errors go straight to
    /// the link: a session that never started holds no entry.
    fn admit(&self, session: SessionId, tx: &Link) -> bool {
        if session == 0 {
            send_error(tx, 0, ErrorCode::BadRequest, "session 0 is reserved".into());
            return false;
        }
        let sessions = self.sessions();
        if sessions.contains_key(&session) {
            drop(sessions);
            send_error(
                tx,
                session,
                ErrorCode::DuplicateSession,
                format!("session {session} is in use"),
            );
            return false;
        }
        let live = sessions
            .values()
            .filter(|e| !e.outbox.is_finished())
            .count();
        if live >= self.opts.max_sessions as usize {
            drop(sessions);
            send_error(
                tx,
                session,
                ErrorCode::Busy,
                format!("{live} sessions are running (max_sessions)"),
            );
            return false;
        }
        true
    }

    fn new_outbox(&self, session: SessionId, tx: &Link) -> Arc<Outbox> {
        let outbox = Arc::new(Outbox::new(session, self.opts.buffer_limit));
        outbox.attach(tx.clone(), 0);
        outbox
    }

    fn start(&self, session: SessionId, plan: Result<VerbPlan, BuildError>, tx: &Link) {
        if !self.admit(session, tx) {
            return;
        }
        let plan = match plan {
            Ok(p) => p,
            Err(e) => return send_error(tx, session, e.code, e.message),
        };
        let outbox = self.new_outbox(session, tx);
        let program = plan.exec.program.clone();
        let running = match session::spawn(plan.exec, outbox.clone()) {
            Ok(r) => r,
            Err(e) => {
                outbox.detach();
                return send_error(tx, session, ErrorCode::SpawnFailed, e.to_string());
            }
        };
        tracing::info!(session, %program, pid = running.pid, "session started");
        outbox.send_now(HostFrame::Started {
            session,
            pid: running.pid,
        });
        self.sessions().insert(
            session,
            Entry {
                outbox: outbox.clone(),
                input: Some(running.input),
                push: None,
                detached_since: None,
            },
        );
        let parser = plan.parser;
        let done = running.done;
        tokio::spawn(async move {
            let info = match done.await {
                Ok(info) => info,
                Err(e) => {
                    tracing::error!(session, error = %e, "session task failed");
                    outbox.finish(vec![HostFrame::Error {
                        session,
                        code: ErrorCode::Io,
                        message: format!("session task failed: {e}"),
                    }]);
                    return;
                }
            };
            tracing::info!(session, code = info.code, signal = info.signal, reason = ?info.reason, "session ended");
            let mut frames = Vec::new();
            if let Some(parser) = parser {
                frames.push(HostFrame::Result {
                    session,
                    value: parser.parse(&info.stdout, info.code),
                });
            }
            frames.push(HostFrame::Exit {
                session,
                code: info.code,
                signal: info.signal,
                reason: info.reason,
            });
            outbox.finish(frames);
        });
    }

    async fn start_push(&self, req: CpPush, tx: &Link) {
        let session = req.session;
        if !self.admit(session, tx) {
            return;
        }
        let path = match self.opts.policy.check(&req.path) {
            Ok(p) => p,
            Err(m) => return send_error(tx, session, ErrorCode::PathDenied, m),
        };
        let file = match PushFile::open(&path, req.mode, req.mkdirs, session).await {
            Ok(f) => f,
            Err(e) => return send_error(tx, session, e.code(), e.to_string()),
        };
        let outbox = self.new_outbox(session, tx);
        outbox.send_now(HostFrame::Started { session, pid: None });
        let (ptx, mut prx) = mpsc::channel(PUSH_QUEUE);
        self.sessions().insert(
            session,
            Entry {
                outbox: outbox.clone(),
                input: None,
                push: Some(ptx),
                detached_since: None,
            },
        );
        tokio::spawn(async move {
            let mut file = Some(file);
            let mut failure: Option<transfer::TransferError> = None;
            let frame = loop {
                match prx.recv().await {
                    Some(PushMsg::Data(d)) => {
                        if failure.is_none()
                            && let Some(f) = file.as_mut()
                            && let Err(e) = f.write(&d).await
                        {
                            failure = Some(e);
                        }
                    }
                    Some(PushMsg::End { bytes, sha256 }) => {
                        let Some(f) = file.take() else {
                            break interrupted(session);
                        };
                        if let Some(e) = failure.take() {
                            f.abort().await;
                            break HostFrame::Error {
                                session,
                                code: e.code(),
                                message: e.to_string(),
                            };
                        }
                        break match f.finish(bytes, &sha256).await {
                            Ok((bytes, sha256)) => HostFrame::CpDone {
                                session,
                                bytes,
                                sha256,
                            },
                            Err(e) => HostFrame::Error {
                                session,
                                code: e.code(),
                                message: e.to_string(),
                            },
                        };
                    }
                    Some(PushMsg::Abort) | None => {
                        if let Some(f) = file.take() {
                            f.abort().await;
                        }
                        break interrupted(session);
                    }
                }
            };
            outbox.finish(vec![frame]);
        });
    }

    fn start_pull(&self, req: CpPull, tx: &Link) {
        let session = req.session;
        if !self.admit(session, tx) {
            return;
        }
        let path = match self.opts.policy.check(&req.path) {
            Ok(p) => p,
            Err(m) => return send_error(tx, session, ErrorCode::PathDenied, m),
        };
        let outbox = self.new_outbox(session, tx);
        outbox.send_now(HostFrame::Started { session, pid: None });
        self.sessions().insert(
            session,
            Entry {
                outbox: outbox.clone(),
                input: None,
                push: None,
                detached_since: None,
            },
        );
        tokio::spawn(async move {
            let frame = match transfer::pull(&path, &outbox).await {
                Ok((bytes, sha256)) => HostFrame::CpDone {
                    session,
                    bytes,
                    sha256,
                },
                Err(e) => HostFrame::Error {
                    session,
                    code: e.code(),
                    message: e.to_string(),
                },
            };
            outbox.finish(vec![frame]);
        });
    }
}

fn need(present: bool, tool: &str) -> Result<(), BuildError> {
    if present {
        Ok(())
    } else {
        Err(BuildError {
            code: ErrorCode::Unsupported,
            message: format!("{tool} is not available on this host"),
        })
    }
}

fn interrupted(session: SessionId) -> HostFrame {
    HostFrame::Error {
        session,
        code: ErrorCode::Interrupted,
        message: "the connection dropped during the push; the partial file was removed".into(),
    }
}

fn text(frame: &HostFrame) -> Result<String, ClientError> {
    encode_control(frame).map_err(|e| ClientError::Protocol(e.to_string()))
}

fn send_error(tx: &Link, session: SessionId, code: ErrorCode, message: String) {
    let _ = tx.send(Outgoing::Control(HostFrame::Error {
        session,
        code,
        message,
    }));
}

fn unknown(tx: &Link, session: SessionId) {
    send_error(
        tx,
        session,
        ErrorCode::UnknownSession,
        format!("no session {session}"),
    );
}

/// Install the ring crypto provider once, for `wss://`.
pub fn ensure_crypto_provider() {
    if rustls::crypto::CryptoProvider::get_default().is_none() {
        let _ = rustls::crypto::ring::default_provider().install_default();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn backoff_doubles_with_jitter_and_caps() {
        let mut b = Backoff::new(Duration::from_millis(100), Duration::from_millis(800));
        let mut bases = Vec::new();
        for _ in 0..6 {
            let d = b.next_delay();
            bases.push(d);
        }
        assert!(bases[0] >= Duration::from_millis(50) && bases[0] <= Duration::from_millis(100));
        assert!(bases[1] >= Duration::from_millis(100) && bases[1] <= Duration::from_millis(200));
        for d in &bases[3..] {
            assert!(*d >= Duration::from_millis(400) && *d <= Duration::from_millis(800));
        }
        b.reset();
        assert!(b.next_delay() <= Duration::from_millis(100));
    }
}
