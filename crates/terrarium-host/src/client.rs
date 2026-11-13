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

use crate::config::{HostPaths, PathPolicy};
use crate::outbox::{Link, Outbox, Outgoing};
use crate::pending_logon::{Event as PendingEvent, Pending, Reservations, Routed};
use crate::protocol::{
    CpPull, CpPush, DaemonFrame, DataFrame, ErrorCode, ExitReason, Hello, HostCapabilities,
    HostFrame, METRICS_INTERVAL_SECS, PROTOCOL_VERSION, Resume, ResumeOffer, SessionId, StreamTag,
    Welcome, decode_control, encode_control, parse_signal,
};
use crate::session::{self, SessionInput};
use crate::transfer::{self, PushFile};
use crate::verb::{BuildError, VerbPlan};
use crate::{docker, vmlab};

/// How many heartbeat intervals may pass with nothing received.
const MISSED_HEARTBEATS: u32 = 3;
/// Queued push chunks before the reader waits for the disk.
const PUSH_QUEUE: usize = 64;

#[cfg(windows)]
type PreparedIdentity = Arc<terrarium_winapi::Identity>;
#[cfg(not(windows))]
type PreparedIdentity = ();

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
    /// NVIDIA inventory/compute sampler. Tests may substitute a fixture binary.
    pub nvidia_smi_bin: String,
    pub vmlab_bin: String,
    /// The `git` binary for `vmlab.prepare` and `vmlab.release`.
    pub git_bin: String,
    /// `work_dir`, `instances_dir`, and `repos_dir` of the config.
    pub paths: HostPaths,
    /// The private dir (mode 0700) of the `docker --env-file` files:
    /// [`crate::config::default_env_dir`].
    pub env_dir: PathBuf,
    /// Sent in `hello`; the daemon enforces it.
    pub max_running_targets: u32,
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
    /// `[display]` of the config: the opt-in for `desktop` targets.
    pub display: crate::config::DisplayConfig,
    /// The time between two `metrics` frames: [`METRICS_INTERVAL_SECS`].
    /// Only the tests change it.
    pub metrics_interval: Duration,
    /// The version in `hello`: the build version
    /// ([`terrarium_version::FULL`]). Only the tests change it.
    pub version: String,
    /// Self-update ([`crate::selfupdate`]). `None`: off.
    pub self_update: Option<SelfUpdateOptions>,
    /// Native GPU enforcement through the root helper (`[native_gpu]`,
    /// TER-345). `None`: off.
    pub native: Option<crate::native::NativeOptions>,
    pub windows_run_as: crate::windows_run_as::Policy,
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
            nvidia_smi_bin: "nvidia-smi".into(),
            vmlab_bin: "vmlab".into(),
            git_bin: "git".into(),
            paths: HostPaths::default(),
            env_dir: crate::config::default_env_dir(),
            max_running_targets: crate::config::DEFAULT_MAX_RUNNING_TARGETS,
            tls: None,
            allow_insecure: false,
            token_file: None,
            display: crate::config::DisplayConfig::default(),
            metrics_interval: Duration::from_secs(METRICS_INTERVAL_SECS),
            version: terrarium_version::FULL.to_owned(),
            self_update: None,
            native: None,
            windows_run_as: Default::default(),
        }
    }
}

/// Send a `metrics` frame every `interval` until the link ends. The sampler reads `/proc` on a blocking thread; while one sample
/// runs, the next tick waits, so a slow `statvfs` holds back only the
/// metrics.
async fn run_metrics(
    out: mpsc::Sender<HostFrame>,
    interval: Duration,
    docker: String,
    nvidia: String,
) {
    let interval = interval.max(Duration::from_millis(10));
    let Ok(mut sampler) = tokio::task::spawn_blocking(|| {
        let mut s = crate::metrics::Sampler::default();
        s.prime();
        s
    })
    .await
    else {
        return;
    };
    let mut ticker = tokio::time::interval_at(tokio::time::Instant::now() + interval, interval);
    ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
    let mut gpu = crate::gpu::Cache::default();
    loop {
        ticker.tick().await;
        let Ok((s, sample)) = tokio::task::spawn_blocking(move || {
            let m = sampler.sample();
            (sampler, m)
        })
        .await
        else {
            return;
        };
        sampler = s;
        let Some(mut m) = sample else { continue };
        gpu.refresh(&nvidia, &docker).await;
        m.gpus = gpu.devices.clone();
        m.gpu_usage = gpu.usage.clone();
        match out.try_send(HostFrame::Metrics(Box::new(m))) {
            Ok(()) => {}
            Err(mpsc::error::TrySendError::Full(_)) => {
                tracing::debug!("the link is busy; dropped a metrics sample");
            }
            Err(mpsc::error::TrySendError::Closed(_)) => return,
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
    input: Option<crate::session::InputSender>,
    push: Option<mpsc::Sender<PushMsg>>,
    detached_since: Option<Instant>,
}

struct Shared {
    pending_logons: Reservations,
    #[cfg(windows)]
    windows_jobs: Mutex<HashMap<(String, String), Arc<terrarium_winapi::Job>>>,
    sessions: Mutex<HashMap<SessionId, Entry>>,
    resume_token: Mutex<Option<String>>,
    /// The bearer token for the next dial. It starts as the configured
    /// token and changes when the daemon sends `welcome.token`.
    bearer: Mutex<String>,
    /// The kept VNC and RDP connections of `display` frames.
    remotes: Arc<crate::display::remote::Remotes>,
    /// A self-update runs (only one at a time).
    updating: std::sync::atomic::AtomicBool,
    /// A verified update is staged. Refuse new sessions until the swap,
    /// so a busy host can eventually reach idle without killing work.
    draining: std::sync::atomic::AtomicBool,
    /// Serialize staging the drain with admission and session insertion.
    admission: tokio::sync::Mutex<()>,
    /// The helper answered with enforcement at the last `hello`
    /// (`capabilities.native_gpu.enforced`). Native routing follows it,
    /// not the config.
    native_enforced: std::sync::atomic::AtomicBool,
    /// The last offered version that this process refused or failed to
    /// install: it does not try that version again.
    update_refused: Mutex<Option<String>>,
    /// A self-update is in place: [`Client::run`] returns, so the process
    /// exits and systemd starts the new binary.
    restart: tokio::sync::Notify,
}

/// Self-update settings ([`crate::selfupdate`]). `main` sets them; a
/// client without them never replaces a binary.
#[derive(Debug, Clone)]
pub struct SelfUpdateOptions {
    /// The running binary (`current_exe`).
    pub binary: PathBuf,
    /// `auto_update` of the config.
    pub enabled: bool,
    /// systemd runs this process (`INVOCATION_ID` is set), so it starts
    /// the new binary after the exit.
    pub supervised: bool,
    /// How often to look for a moment with no session.
    pub idle_poll: Duration,
    /// Maximum admission drain per attempt. Expiry keeps the staged binary
    /// and reopens admission before retrying after `idle_poll`.
    pub drain_timeout: Duration,
}

/// The host agent's client. Cheap to clone.
#[derive(Clone)]
pub struct Client {
    opts: Arc<ClientOptions>,
    shared: Arc<Shared>,
}

#[cfg(windows)]
struct LogonPreparation {
    client: Client,
    lease: crate::pending_logon::Lease,
}

#[cfg(windows)]
impl Drop for LogonPreparation {
    fn drop(&mut self) {
        // A failed/panicked preparation must not orphan a partially published
        // session. A successful empty-queue handoff has removed this ownership.
        if self
            .client
            .shared
            .pending_logons
            .owns(self.lease.id, Some(&self.lease.pending))
        {
            self.client.cancel_pending(self.lease.id);
        }
    }
}

impl Client {
    pub fn new(opts: ClientOptions) -> Self {
        let shared = Shared {
            pending_logons: Reservations::default(),
            #[cfg(windows)]
            windows_jobs: Mutex::new(HashMap::new()),
            sessions: Mutex::new(HashMap::new()),
            resume_token: Mutex::new(None),
            bearer: Mutex::new(opts.token.clone()),
            remotes: crate::display::remote::Remotes::new(opts.display.rdp_known_hosts_path()),
            updating: std::sync::atomic::AtomicBool::new(false),
            draining: std::sync::atomic::AtomicBool::new(false),
            admission: tokio::sync::Mutex::new(()),
            native_enforced: std::sync::atomic::AtomicBool::new(
                opts.capabilities
                    .native_gpu
                    .as_ref()
                    .is_some_and(|n| n.enforced),
            ),
            update_refused: Mutex::new(None),
            restart: tokio::sync::Notify::new(),
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
        let sessions = self.sessions();
        sessions.len()
            + self
                .shared
                .pending_logons
                .unpublished(|id| sessions.contains_key(&id))
    }

    /// Dial, serve, and redial for ever. Returns when the transport is
    /// refused, or when a self-update put a new binary in place (then the
    /// caller exits, and systemd starts the new binary).
    pub async fn run(self) {
        ensure_crypto_provider();
        let gc = tokio::spawn(self.clone().reap_detached());
        let shared = self.shared.clone();
        tokio::select! {
            () = self.dial_loop() => {}
            () = shared.restart.notified() => {
                tracing::info!("self-update: the new binary is in place; exiting so that systemd starts it");
            }
        }
        gc.abort();
    }

    async fn dial_loop(self) {
        let mut backoff = Backoff::new(self.opts.backoff_min, self.opts.backoff_max);
        loop {
            match self.connect_once().await {
                Err(ClientError::Insecure(e)) => {
                    tracing::error!(error = %e, "not connecting");
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
        }
    }

    async fn hello(&self) -> Hello {
        let mut capabilities = self.opts.capabilities.clone();
        if capabilities.gpu_reservations {
            // Never reuse startup inventory after a reconnect. Missing probes
            // clear inventory so the daemon keeps affected admission closed.
            capabilities.gpus = crate::gpu::sample_with(&self.opts.nvidia_smi_bin)
                .await
                .unwrap_or_default();
        }
        if let Some(native) = &self.opts.native {
            // A helper that went away or changed its version since the
            // start closes native GPU admission on the daemon.
            let cap = crate::native::capability(&native.socket).await;
            self.shared
                .native_enforced
                .store(cap.enforced, std::sync::atomic::Ordering::SeqCst);
            capabilities.native_gpu = Some(cap);
        }
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
            path_kind: if cfg!(windows) {
                crate::protocol::PathKind::Windows
            } else {
                crate::protocol::PathKind::Posix
            },
            protocol: PROTOCOL_VERSION,
            host: self.opts.name.clone(),
            version: self.opts.version.clone(),
            os: std::env::consts::OS.to_owned(),
            arch: std::env::consts::ARCH.to_owned(),
            capabilities,
            max_sessions: self.opts.max_sessions,
            max_running_targets: Some(self.opts.max_running_targets),
            resume,
            update_failed: self
                .opts
                .self_update
                .as_ref()
                .and_then(|s| crate::selfupdate::failed_version(&s.binary)),
        }
    }

    /// After a `welcome`: confirm a pending self-update, and take a new
    /// offer.
    fn after_welcome(&self, welcome: &Welcome) {
        let Some(su) = &self.opts.self_update else {
            return;
        };
        if let Some(v) = crate::selfupdate::confirm(&su.binary) {
            tracing::info!(
                version = %v,
                binary = %su.binary.display(),
                "self-update: the new binary reached the daemon; the previous one stays as .prev"
            );
        }
        let Some(offer) = welcome.update.clone() else {
            return;
        };
        let refused = self
            .shared
            .update_refused
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .clone();
        if refused.as_deref() == Some(offer.version.as_str()) {
            return;
        }
        let failed = crate::selfupdate::failed_version(&su.binary);
        if let Err(why) = crate::selfupdate::decide(
            &offer,
            &self.opts.version,
            su.enabled,
            su.supervised,
            failed.as_deref(),
        ) {
            tracing::warn!(offered = %offer.version, asset = %offer.path, own = %self.opts.version, "self-update: not updating: {why}");
            self.refuse_update(&offer.version);
            return;
        }
        if self
            .shared
            .updating
            .swap(true, std::sync::atomic::Ordering::SeqCst)
        {
            return;
        }
        let client = self.clone();
        let su = su.clone();
        tokio::spawn(async move {
            let version = offer.version.clone();
            match client.self_update(&su, &offer).await {
                Ok(()) => client.shared.restart.notify_one(),
                Err(e) => {
                    client
                        .shared
                        .draining
                        .store(false, std::sync::atomic::Ordering::SeqCst);
                    tracing::error!(offered = %version, error = %e, "self-update failed; this binary keeps running");
                    client.refuse_update(&version);
                    client
                        .shared
                        .updating
                        .store(false, std::sync::atomic::Ordering::SeqCst);
                }
            }
        });
    }

    fn refuse_update(&self, version: &str) {
        *self
            .shared
            .update_refused
            .lock()
            .unwrap_or_else(|e| e.into_inner()) = Some(version.to_owned());
    }

    /// Download, check, and stage the offered binary; wait for a moment
    /// with no session; then swap it in.
    async fn self_update(
        &self,
        su: &SelfUpdateOptions,
        offer: &crate::protocol::UpdateOffer,
    ) -> Result<(), crate::selfupdate::UpdateError> {
        use crate::selfupdate as u;
        let key = crate::update_signature::RELEASE_KEY.ok_or_else(|| {
            u::UpdateError::Offer(
                "this dev build has no release verification key; install manually".into(),
            )
        })?;
        u::check_writable(&su.binary)?;
        let url = u::http_url(&self.opts.url, &offer.path)?;
        tracing::info!(offered = %offer.version, own = %self.opts.version, %url, "self-update: downloading the daemon's build");
        let data = tokio::time::timeout(
            u::DOWNLOAD_TIMEOUT,
            u::download(&url, self.opts.tls.clone(), u::MAX_BINARY_BYTES),
        )
        .await
        .map_err(|_| u::UpdateError::Download {
            url: url.clone(),
            message: format!("timed out after {:?}", u::DOWNLOAD_TIMEOUT),
        })??;
        let sha = u::verify(&data, offer)?;
        let (path, query) = offer.path.split_once('?').unwrap_or((&offer.path, ""));
        let bundle_path = format!("{path}.sigstore.json?{query}");
        let bundle_url = u::http_url(&self.opts.url, &bundle_path)?;
        crate::update_signature::download_and_verify(
            &data,
            &bundle_url,
            self.opts.tls.clone(),
            Some(key),
        )
        .await?;
        u::stage(&su.binary, &data, &offer.version).await?;
        tracing::info!(sha256 = %sha, bytes = data.len(), "self-update: the new binary checks out");
        self.drain_for_update(su).await;
        u::swap(&su.binary, &offer.version)?;
        tracing::info!(version = %offer.version, binary = %su.binary.display(), "self-update: installed");
        Ok(())
    }

    async fn drain_for_update(&self, su: &SelfUpdateOptions) {
        loop {
            let _admission = self.shared.admission.lock().await;
            self.shared
                .draining
                .store(true, std::sync::atomic::Ordering::SeqCst);
            drop(_admission);
            let drained = tokio::time::timeout(su.drain_timeout, async {
                while self.session_count() > 0 {
                    tokio::time::sleep(su.idle_poll).await;
                }
            })
            .await;
            if drained.is_ok() {
                return;
            }
            let _admission = self.shared.admission.lock().await;
            self.shared
                .draining
                .store(false, std::sync::atomic::Ordering::SeqCst);
            drop(_admission);
            tracing::info!(
                sessions = self.session_count(),
                "self-update: drain expired; admission reopened, staged update will retry"
            );
            tokio::time::sleep(su.idle_poll).await;
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
        self.serve_connection(ws).await
    }

    async fn serve_connection<W>(&self, ws: W) -> Result<(), ClientError>
    where
        W: futures_util::Sink<Message, Error = tungstenite::Error>
            + futures_util::Stream<Item = Result<Message, tungstenite::Error>>
            + Unpin
            + Send
            + 'static,
    {
        let (mut sink, mut stream) = ws.split();

        let hello = HostFrame::Hello(Box::new(self.hello().await));
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
        self.after_welcome(&welcome);
        // Metrics go on their own queue of one. The writer takes them only
        // when the other queue is empty, so a sample never delays a frame
        // of a session; a sample that finds the queue full is dropped.
        let (mtx, mut mrx) = mpsc::channel::<HostFrame>(1);
        let sampler = (welcome.metrics && self.opts.capabilities.metrics).then(|| {
            tokio::spawn(run_metrics(
                mtx,
                self.opts.metrics_interval,
                self.opts.docker_bin.clone(),
                self.opts.nvidia_smi_bin.clone(),
            ))
        });

        let mut writer = tokio::spawn(async move {
            loop {
                let item = tokio::select! {
                    biased;
                    item = rx.recv() => match item {
                        Some(i) => i,
                        None => break,
                    },
                    Some(f) = mrx.recv() => Outgoing::Control(f),
                };
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
        let mut writer_joined = false;
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
                _ = &mut writer => {
                    writer_joined = true;
                    break Ok(());
                },
            }
        };
        if let Some(t) = sampler {
            t.abort();
        }
        self.detach_all();
        drop(tx);
        // A JoinHandle consumed by select! must not be polled again.
        // Otherwise give the writer a moment to flush a close, then stop it.
        if !writer_joined
            && tokio::time::timeout(Duration::from_millis(200), &mut writer)
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
        self.shared.pending_logons.cancel_all();
        let now = Instant::now();
        for (id, e) in self.sessions().iter_mut() {
            if self.shared.pending_logons.contains(*id)
                && let Some(input) = &e.input
            {
                let _ = input.send(SessionInput::Kill(ExitReason::DetachedTimeout));
            }
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
        if let Ok(frame) = DataFrame::decode(bytes)
            && matches!(frame.stream, StreamTag::Stdin | StreamTag::File)
            && self.shared.pending_logons.contains(frame.session)
        {
            match self.shared.pending_logons.route(
                frame.session,
                PendingEvent::Binary(bytes.to_vec()),
                bytes.len(),
            ) {
                Routed::Absent(_) => {}
                Routed::Overflow => {
                    self.cancel_pending(frame.session);
                    send_error(
                        tx,
                        frame.session,
                        ErrorCode::Busy,
                        "pending logon input queue is full".into(),
                    );
                    return;
                }
                Routed::Queued | Routed::Cancelled => return,
            }
        }
        self.on_binary_ready(bytes, tx).await;
    }

    async fn on_binary_ready(&self, bytes: &[u8], tx: &Link) {
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

    fn cancel_pending(&self, id: SessionId) -> bool {
        let committed = self.shared.pending_logons.cancel(id).unwrap_or(false);
        if let Some(entry) = self.sessions().get_mut(&id) {
            if let Some(input) = &entry.input {
                let _ = input.send(SessionInput::Kill(ExitReason::Signaled));
            }
            if let Some(push) = entry.push.take() {
                let _ = push.try_send(PushMsg::Abort);
            }
        }
        committed
    }

    fn route_pending_control(
        &self,
        frame: DaemonFrame,
        tx: &Link,
        bytes: usize,
    ) -> Option<DaemonFrame> {
        let (id, cancel) = match &frame {
            DaemonFrame::Release { session } | DaemonFrame::Signal { session, .. } => {
                (*session, true)
            }
            DaemonFrame::Eof { session }
            | DaemonFrame::Resize { session, .. }
            | DaemonFrame::CpEnd { session, .. } => (*session, false),
            _ => return Some(frame),
        };
        if !self.shared.pending_logons.contains(id) {
            return Some(frame);
        }
        if let DaemonFrame::Signal { signal, .. } = &frame
            && parse_signal(signal).is_none()
        {
            send_error(
                tx,
                id,
                ErrorCode::BadRequest,
                format!("unknown signal {signal:?}"),
            );
            return None;
        }
        if cancel {
            let committed = self.cancel_pending(id);
            if !committed && !matches!(frame, DaemonFrame::Release { .. }) {
                send_error(
                    tx,
                    id,
                    ErrorCode::Interrupted,
                    "pending logon was cancelled".into(),
                );
            }
            if matches!(frame, DaemonFrame::Release { .. }) {
                self.sessions().remove(&id);
            }
            return None;
        }
        match self
            .shared
            .pending_logons
            .route(id, PendingEvent::Control(Box::new(frame)), bytes)
        {
            Routed::Absent(PendingEvent::Control(frame)) => Some(*frame),
            Routed::Absent(PendingEvent::Binary(_)) => {
                unreachable!("control routing preserves type")
            }
            Routed::Overflow => {
                self.cancel_pending(id);
                send_error(
                    tx,
                    id,
                    ErrorCode::Busy,
                    "pending logon input queue is full".into(),
                );
                None
            }
            Routed::Queued | Routed::Cancelled => None,
        }
    }

    #[cfg(windows)]
    fn prepare_logon(
        &self,
        id: SessionId,
        frame: DaemonFrame,
        credentials: crate::protocol::WindowsRunAs,
        tx: Link,
    ) {
        let lease = self.shared.pending_logons.reserve(id);
        let client = self.clone();
        tokio::spawn(async move {
            let preparation = LogonPreparation { client, lease };
            let client = &preparation.client;
            let lease = &preparation.lease;
            // The reservation remains charged during an uncancellable native
            // API call, including after connection-close/revoke cancellation.
            let result =
                crate::windows_run_as::logon(Some(credentials), &client.opts.windows_run_as).await;
            if lease.pending.cancelled() {
                return;
            }
            let identity = match result {
                Ok(Some(identity)) => identity,
                Ok(None) => {
                    send_error(
                        &tx,
                        id,
                        ErrorCode::BadRequest,
                        "Windows logon did not provide an identity".into(),
                    );
                    return;
                }
                Err(error) => {
                    send_error(&tx, id, error.code, error.message);
                    return;
                }
            };
            {
                // Exec's final synchronous spawn is the admission commit.
                // Copy opens may block on a remote server, so their builders
                // take the fence only for checks/publication, never disk IO.
                let _admission = if matches!(&frame, DaemonFrame::Exec(_)) {
                    Some(client.shared.admission.lock().await)
                } else {
                    None
                };
                if lease.pending.cancelled() {
                    return;
                }
                if client
                    .on_frame(frame, &tx, Some(identity), Some(&lease.pending))
                    .await
                    .is_err()
                {
                    return;
                }
            }
            if !client.sessions().contains_key(&id) {
                return;
            }
            // Preparation/start has published an entry. Route its early frames
            // in order until an atomic empty-queue handoff to normal routing.
            while let Some(event) = lease.pop_or_handoff() {
                if lease.pending.cancelled() {
                    break;
                }
                match event {
                    PendingEvent::Control(frame) => {
                        let _ = client.on_frame(*frame, &tx, None, None).await;
                    }
                    PendingEvent::Binary(bytes) => client.on_binary_ready(&bytes, &tx).await,
                }
            }
            if lease.pending.cancelled() {
                client.cancel_pending(id);
            }
        });
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
        #[cfg(windows)]
        let mut frame = frame;
        #[cfg(windows)]
        {
            let credentials = match &mut frame {
                DaemonFrame::Exec(req) => req.windows_run_as.take(),
                DaemonFrame::CpPush(req) => req.windows_run_as.take(),
                DaemonFrame::CpPull(req) => req.windows_run_as.take(),
                _ => None,
            };
            if let Some(credentials) = credentials {
                let id = match &frame {
                    DaemonFrame::Exec(req) => req.session,
                    DaemonFrame::CpPush(req) => req.session,
                    DaemonFrame::CpPull(req) => req.session,
                    _ => unreachable!("credentials only on native admission frames"),
                };
                let Ok(_admission) = self.shared.admission.try_lock() else {
                    send_error(
                        tx,
                        id,
                        ErrorCode::Busy,
                        "host admission is committing another session".into(),
                    );
                    return Ok(());
                };
                if self.admit(id, tx) {
                    self.prepare_logon(id, frame, credentials, tx.clone());
                }
                return Ok(());
            }
        }
        let Some(frame) = self.route_pending_control(frame, tx, text.len()) else {
            return Ok(());
        };
        // Only new work participates in the admission/drain fence. Heartbeats,
        // kill/release and input must not wait for another session's preparation.
        let admission = !matches!(
            &frame,
            DaemonFrame::Welcome(_)
                | DaemonFrame::Ping { .. }
                | DaemonFrame::Pong { .. }
                | DaemonFrame::Goodbye { .. }
                | DaemonFrame::Ack { .. }
                | DaemonFrame::Release { .. }
                | DaemonFrame::Eof { .. }
                | DaemonFrame::Resize { .. }
                | DaemonFrame::Signal { .. }
                | DaemonFrame::CpEnd { .. }
        );
        let _admission = if admission && self.shared.pending_logons.count() > 0 {
            match self.shared.admission.try_lock() {
                Ok(guard) => Some(guard),
                Err(_) => {
                    send_error(
                        tx,
                        frame.session().unwrap_or(0),
                        ErrorCode::Busy,
                        "host admission is committing another session".into(),
                    );
                    return Ok(());
                }
            }
        } else if admission {
            Some(self.shared.admission.lock().await)
        } else {
            None
        };
        self.on_frame(frame, tx, None, None).await
    }

    async fn on_frame(
        &self,
        frame: DaemonFrame,
        tx: &Link,
        identity: Option<PreparedIdentity>,
        owner: Option<&Arc<Pending>>,
    ) -> Result<(), ClientError> {
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
            F::CpPush(req) => self.start_push(req, tx, identity, owner).await,
            F::CpPull(req) => self.start_pull(req, tx, identity, owner).await,
            F::Exec(req) => {
                let id = req.session;
                #[cfg(not(windows))]
                let plan = crate::verb::plan_exec(&req, p);
                #[cfg(windows)]
                let plan = match identity.as_deref() {
                    Some(identity) => identity
                        .with_impersonation(|| Ok(crate::verb::plan_exec(&req, p)))
                        .map_err(|_| BuildError::bad("Windows path check impersonation failed"))
                        .and_then(|plan| plan),
                    None => crate::verb::plan_exec(&req, p),
                };
                #[cfg(windows)]
                let plan = plan.map(|mut plan| {
                    plan.exec.windows_identity = identity;
                    plan
                });
                let plan = match (plan, &req.limits) {
                    (Ok(plan), Some(l)) if l.native_gpu.is_some() => self.native(plan, l, id),
                    (Ok(plan), Some(l)) => self.limited(plan, l, id).await,
                    (plan, _) => plan,
                };
                self.start_reserved(id, plan, tx, owner);
            }
            F::DockerRun(r) => self.start(
                r.session,
                need(docker_ok, "docker")
                    .and_then(|_| docker::plan_run(&r, db, p, &o.paths.work_dir, &o.env_dir)),
                tx,
            ),
            F::DockerExec(r) => self.start(
                r.session,
                need(docker_ok, "docker").and_then(|_| docker::plan_exec(&r, db, &o.env_dir)),
                tx,
            ),
            F::ResourceApply(r) => {
                let plan = if let Some(container) = &r.container {
                    need(docker_ok, "docker")
                        .and_then(|_| docker::plan_update(container, &r.limits, db))
                } else {
                    self.slice_apply_plan(&r)
                };
                self.start(r.session, plan, tx);
            }
            F::ResourceStop(r) => self.start(r.session, self.slice_stop_plan(&r), tx),
            F::DockerStart(r) => self.start(
                r.session,
                need(docker_ok, "docker").and_then(|_| docker::plan_start(&r, db)),
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
                need(docker_ok, "docker")
                    .and_then(|_| docker::plan_inspect(&r, db, &o.paths.work_dir)),
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
            F::VmlabPrepare(r) => {
                let id = r.session;
                let ctx = self.prepare_ctx();
                if vmlab_ok {
                    self.start_task(id, tx, async move {
                        let mut log = Vec::new();
                        let res = crate::prepare::prepare(&ctx, &r, &mut log).await;
                        (log, res)
                    });
                } else {
                    send_error(
                        tx,
                        id,
                        ErrorCode::Unsupported,
                        "vmlab is not on this host".into(),
                    );
                }
            }
            F::VmlabRelease(r) => {
                let id = r.session;
                let ctx = self.prepare_ctx();
                self.start_task(id, tx, async move {
                    let mut log = Vec::new();
                    let res = crate::prepare::release(&ctx, &r, &mut log).await;
                    (log, res)
                });
            }
            F::VmlabSnapshotList(r) => self.start(
                r.session,
                need(vmlab_ok, "vmlab").and_then(|_| vmlab::plan_snapshot_list(&r, vb, p)),
                tx,
            ),
            F::VmlabSnapshotDelete(r) => self.start(
                r.session,
                need(vmlab_ok, "vmlab").and_then(|_| vmlab::plan_snapshot_delete(&r, vb, p)),
                tx,
            ),
            F::Display(r) => self.start_display(*r, tx),
            F::Probe(r) => {
                let timeout = crate::verb::secs(r.timeout_secs).unwrap_or(Duration::from_secs(5));
                self.start_task(r.session, tx, async move {
                    let res = crate::display::remote::tcp_probe(&r.address, timeout)
                        .await
                        .map_err(|e| (e.code(), e.to_string()));
                    (Vec::new(), res)
                });
            }
        }
        Ok(())
    }

    fn display_ctx(&self, timeout: Option<u64>) -> crate::display::DisplayCtx {
        let o = &self.opts;
        crate::display::DisplayCtx {
            runner: Arc::new(crate::display::runner::SystemRunner),
            docker_bin: o.docker_bin.clone(),
            docker_ok: o.capabilities.docker.present,
            vmlab_bin: o.vmlab_bin.clone(),
            vmlab_ok: o.capabilities.vmlab.present,
            policy: o.policy.clone(),
            config: o.display.clone(),
            tesseract: o.capabilities.display.tesseract,
            timeout: crate::verb::secs(timeout).unwrap_or(crate::display::DEFAULT_TIMEOUT),
            remotes: self.shared.remotes.clone(),
        }
    }

    /// A `display` frame: one operation as a task. A screenshot's PNG goes
    /// out as `stdout` data, then `result` and `exit` 0; a failure is one
    /// `error` frame.
    fn start_display(&self, req: crate::protocol::DisplayRequest, tx: &Link) {
        let session = req.session;
        if !self.admit(session, tx) {
            return;
        }
        let ctx = self.display_ctx(req.timeout_secs);
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
        tracing::info!(
            session,
            backend = req.target.backend(),
            op = req.request.name(),
            "display"
        );
        tokio::spawn(async move {
            let limit = ctx.timeout + Duration::from_secs(5);
            let res = match tokio::time::timeout(
                limit,
                crate::display::dispatch(&ctx, &req.target, &req.request),
            )
            .await
            {
                Ok(r) => r,
                Err(_) => Err(crate::display::DisplayError::Failed(format!(
                    "the display operation did not end in {} s",
                    limit.as_secs()
                ))),
            };
            let frames = match res {
                Ok(out) => {
                    let mut overflow = false;
                    if let Some(png) = out.png {
                        for chunk in png.chunks(crate::protocol::CHUNK_SIZE) {
                            if outbox
                                .push(StreamTag::Stdout, chunk.to_vec())
                                .await
                                .is_err()
                            {
                                overflow = true;
                                break;
                            }
                        }
                    }
                    if overflow {
                        vec![HostFrame::Error {
                            session,
                            code: ErrorCode::Io,
                            message: "the link was down and the screenshot filled the buffer"
                                .into(),
                        }]
                    } else {
                        vec![
                            HostFrame::Result {
                                session,
                                value: out.value,
                            },
                            HostFrame::Exit {
                                session,
                                code: Some(0),
                                signal: None,
                                reason: ExitReason::Exited,
                            },
                        ]
                    }
                }
                Err(e) => {
                    tracing::info!(session, error = %e, "display operation failed");
                    vec![HostFrame::Error {
                        session,
                        code: e.code(),
                        message: e.to_string(),
                    }]
                }
            };
            outbox.finish(frames);
        });
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
        self.admit_reserved(session, tx, None)
    }

    fn admit_reserved(&self, session: SessionId, tx: &Link, owner: Option<&Arc<Pending>>) -> bool {
        if session == 0 {
            send_error(tx, 0, ErrorCode::BadRequest, "session 0 is reserved".into());
            return false;
        }
        if self
            .shared
            .draining
            .load(std::sync::atomic::Ordering::SeqCst)
        {
            send_error(
                tx,
                session,
                ErrorCode::Busy,
                "host is draining sessions for a staged self-update".into(),
            );
            return false;
        }
        let sessions = self.sessions();
        let reserved = self.shared.pending_logons.contains(session);
        let owned = self.shared.pending_logons.owns(session, owner);
        if owner.is_some() && (!owned || owner.is_some_and(|p| p.cancelled())) {
            send_error(
                tx,
                session,
                ErrorCode::Interrupted,
                "pending logon was cancelled".into(),
            );
            return false;
        }
        if sessions.contains_key(&session) || (reserved && !owned) {
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
            .count()
            + self
                .shared
                .pending_logons
                .unpublished(|id| sessions.contains_key(&id))
            - usize::from(owned);
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
        self.start_reserved(session, plan, tx, None);
    }

    fn start_reserved(
        &self,
        session: SessionId,
        plan: Result<VerbPlan, BuildError>,
        tx: &Link,
        owner: Option<&Arc<Pending>>,
    ) {
        if !self.admit_reserved(session, tx, owner) {
            return;
        }
        let plan = match plan {
            Ok(p) => p,
            Err(e) => return send_error(tx, session, e.code, e.message),
        };
        let outbox = self.new_outbox(session, tx);
        let program = plan.exec.program.clone();
        let commitment = match owner {
            Some(owner) => match owner.commit() {
                Some(commitment) => Some(commitment),
                None => {
                    return send_error(
                        tx,
                        session,
                        ErrorCode::Interrupted,
                        "pending logon was cancelled".into(),
                    );
                }
            },
            None => None,
        };
        let running = match session::spawn(plan.exec, outbox.clone()) {
            Ok(r) => r,
            Err(e) => {
                outbox.detach();
                return send_error(tx, session, ErrorCode::SpawnFailed, e.to_string());
            }
        };
        drop(commitment);
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
        if owner.is_some_and(|owner| owner.cancelled()) {
            self.cancel_pending(session);
        }
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

    /// Put an `exec` plan in a scope in the holder's slice, after the
    /// limits are on the slice (docs/host-protocol.md, "Resource limits").
    /// A host agent that cannot apply them refuses the session: it never
    /// runs it without its limits. The frame loop waits for the
    /// `systemctl` call, so the session's input frames come after it.
    async fn limited(
        &self,
        mut plan: VerbPlan,
        l: &crate::protocol::ExecLimits,
        session: SessionId,
    ) -> Result<VerbPlan, BuildError> {
        let refuse = |reason: String| BuildError {
            code: ErrorCode::Unsupported,
            message: terrarium_core::limits::refusal(&reason),
        };
        l.limits
            .host_support(self.opts.capabilities.limits.as_ref())
            .map_err(refuse)?;
        #[cfg(windows)]
        {
            let _ = session;
            if l.reservation.is_some() {
                return Err(refuse(
                    "Windows resource reservations are unsupported".into(),
                ));
            }
            let limits = crate::session_windows::budget(&l.limits).map_err(refuse)?;
            let mut jobs = self
                .shared
                .windows_jobs
                .lock()
                .unwrap_or_else(|e| e.into_inner());
            let key = (l.target.clone(), l.holder.clone());
            let job = if let Some(job) = jobs.get(&key) {
                job.set_limits(limits).map_err(|e| refuse(e.to_string()))?;
                job.clone()
            } else {
                let job = Arc::new(
                    terrarium_winapi::Job::new(limits).map_err(|e| refuse(e.to_string()))?,
                );
                jobs.insert(key, job.clone());
                job
            };
            plan.exec.windows_holder = Some(job);
            Ok(plan)
        }
        #[cfg(unix)]
        {
            let slice = crate::limits::slice_name(&l.target, &l.holder);
            let applied = if let Some(reservation) = &l.reservation {
                reservation
                    .host_support(self.opts.capabilities.limits.as_ref())
                    .map_err(refuse)?;
                crate::limits::apply_reservation(&l.target, &l.holder, reservation).await
            } else {
                crate::limits::apply_slice(&slice, &l.limits).await
            };
            applied.map_err(|e| BuildError {
                code: ErrorCode::SpawnFailed,
                message: format!("cannot set the resource limits on {slice}: {e}"),
            })?;
            #[allow(clippy::cast_possible_truncation)]
            let scope = crate::limits::scope_name(&self.opts.name, session, random_u64() as u32);
            tracing::info!(session, %slice, %scope, limits = %l.limits.summary(), "limited session");
            crate::limits::wrap(&mut plan.exec, &slice, &scope);
            Ok(plan)
        }
    }

    /// The helper, when `[native_gpu]` is on and the helper answered with
    /// enforcement at the last `hello`.
    #[cfg(unix)]
    fn native_live(&self) -> Option<&crate::native::NativeOptions> {
        self.opts.native.as_ref().filter(|_| {
            self.shared
                .native_enforced
                .load(std::sync::atomic::Ordering::SeqCst)
        })
    }

    /// `resource.apply` of a holder slice: to the helper's system slice
    /// when the helper enforces (native sessions are there), else to the
    /// user slice.
    pub(crate) fn slice_apply_plan(
        &self,
        r: &crate::protocol::ResourceApply,
    ) -> Result<VerbPlan, BuildError> {
        #[cfg(unix)]
        if let Some(native) = self.native_live() {
            r.limits.validate().map_err(BuildError::bad)?;
            return crate::native::verb_plan(
                native,
                &crate::native::Request::Apply(crate::native::ApplyRequest {
                    target: r.target.clone(),
                    holder: r.holder.clone(),
                    limits: r.limits.clone(),
                }),
            );
        }
        r.limits
            .host_support(self.opts.capabilities.limits.as_ref())
            .map_err(BuildError::bad)?;
        crate::limits::reservation_plan(&r.target, &r.holder, &r.limits)
    }

    /// `resource.stop` of a holder slice. With `[native_gpu]` in the
    /// config, the holder can have native units in the helper's system
    /// slice (also from before a restart of this process) and sessions in
    /// the user slice (from a time the helper did not answer): the stop
    /// always goes through the shim, which stops both and fails when the
    /// helper cannot be reached. The helper's answer at `hello` does not
    /// matter here: a stop never reports success while native units can
    /// still run. Without `[native_gpu]`, only the user slice.
    pub(crate) fn slice_stop_plan(
        &self,
        r: &crate::protocol::ResourceStop,
    ) -> Result<VerbPlan, BuildError> {
        #[cfg(windows)]
        if self.opts.native.is_some() {
            return Err(BuildError::bad(
                "native GPU isolation is unavailable on Windows",
            ));
        }
        #[cfg(windows)]
        return crate::limits::reservation_stop_plan(&r.target, &r.holder);
        #[cfg(unix)]
        match self.opts.native.as_ref() {
            Some(native) => crate::native::stop_plan(
                native,
                &crate::native::SliceRequest {
                    target: r.target.clone(),
                    holder: r.holder.clone(),
                },
            ),
            None => crate::limits::reservation_stop_plan(&r.target, &r.holder),
        }
    }

    /// Run an `exec` through the native GPU helper (TER-345): the daemon
    /// marked it with `native_gpu`. A host agent without `[native_gpu]`, or
    /// whose helper did not answer with enforcement at the last `hello`,
    /// refuses it: the session never runs without its device policy.
    #[cfg(unix)]
    pub(crate) fn native(
        &self,
        mut plan: VerbPlan,
        l: &crate::protocol::ExecLimits,
        session: SessionId,
    ) -> Result<VerbPlan, BuildError> {
        let refuse = |message: &str| BuildError {
            code: ErrorCode::Unsupported,
            message: message.to_owned(),
        };
        if self.opts.native.is_none() {
            return Err(refuse(
                "native GPU enforcement is off on this host agent ([native_gpu] in its config)",
            ));
        }
        let Some(native) = self.native_live() else {
            return Err(refuse(
                "the native GPU helper did not answer with enforcement at the last hello",
            ));
        };
        let req =
            crate::native::start_request(&plan.exec, session, Some(l), &self.opts.paths.work_dir);
        tracing::info!(session, holder = %req.holder, gpus = ?req.gpus, lease = ?req.lease, "native session");
        plan.exec =
            crate::native::shim_plan(native, &crate::native::Request::Start(req), plan.exec)
                .map_err(BuildError::bad)?;
        Ok(plan)
    }

    #[cfg(windows)]
    pub(crate) fn native(
        &self,
        _plan: VerbPlan,
        _limits: &crate::protocol::ExecLimits,
        _session: SessionId,
    ) -> Result<VerbPlan, BuildError> {
        Err(BuildError {
            code: ErrorCode::Unsupported,
            message: "native GPU isolation is unavailable on Windows".into(),
        })
    }

    fn prepare_ctx(&self) -> crate::prepare::Ctx {
        crate::prepare::Ctx {
            paths: self.opts.paths.clone(),
            git_bin: self.opts.git_bin.clone(),
            vmlab_bin: self.opts.vmlab_bin.clone(),
        }
    }

    /// A session that runs in the host agent, not as one process
    /// (`vmlab.prepare`, `vmlab.release`). The log lines go to stderr; the
    /// end is `result` and `exit` 0, or an `error` frame.
    fn start_task<F>(&self, session: SessionId, tx: &Link, task: F)
    where
        F: std::future::Future<
                Output = (
                    Vec<String>,
                    Result<serde_json::Value, crate::prepare::TaskError>,
                ),
            > + Send
            + 'static,
    {
        if !self.admit(session, tx) {
            return;
        }
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
        tracing::info!(session, "task started");
        tokio::spawn(async move {
            let (log, res) = task.await;
            let mut text = log.join("\n");
            if !text.is_empty() {
                text.push('\n');
            }
            if let Err((_, m)) = &res {
                text.push_str(m);
                text.push('\n');
            }
            let _ = outbox.push(StreamTag::Stderr, text.into_bytes()).await;
            let frames = match res {
                Ok(value) => vec![
                    HostFrame::Result { session, value },
                    HostFrame::Exit {
                        session,
                        code: Some(0),
                        signal: None,
                        reason: ExitReason::Exited,
                    },
                ],
                Err((code, message)) => {
                    tracing::warn!(session, ?code, %message, "task failed");
                    vec![HostFrame::Error {
                        session,
                        code,
                        message,
                    }]
                }
            };
            outbox.finish(frames);
        });
    }

    async fn start_push(
        &self,
        req: CpPush,
        tx: &Link,
        identity: Option<PreparedIdentity>,
        owner: Option<&Arc<Pending>>,
    ) {
        let prepare_admission = if owner.is_some() {
            Some(self.shared.admission.lock().await)
        } else {
            None
        };
        #[cfg(not(windows))]
        let _ = identity;
        if !self.admit_reserved(req.session, tx, owner) {
            return;
        }
        #[cfg(not(windows))]
        if req.windows_run_as.is_some() {
            send_error(
                tx,
                req.session,
                ErrorCode::BadRequest,
                "Windows run-as logon is not supported by this host".into(),
            );
            return;
        }
        let session = req.session;
        if !self.admit_reserved(session, tx, owner) {
            return;
        }
        drop(prepare_admission);
        #[cfg(windows)]
        let checked_path =
            crate::windows_run_as::check_path(identity.as_deref(), &self.opts.policy, &req.path);
        #[cfg(not(windows))]
        let checked_path = self.opts.policy.check(&req.path);
        let path = match checked_path {
            Ok(p) => p,
            Err(m) => return send_error(tx, session, ErrorCode::PathDenied, m),
        };
        #[cfg(windows)]
        let opened = PushFile::open_as(&path, req.mode, req.mkdirs, session, identity).await;
        #[cfg(not(windows))]
        let opened = PushFile::open(&path, req.mode, req.mkdirs, session).await;
        let file = match opened {
            Ok(f) => f,
            Err(e) => return send_error(tx, session, e.code(), e.to_string()),
        };
        let commit_admission = if owner.is_some() {
            Some(self.shared.admission.lock().await)
        } else {
            None
        };
        if !self.admit_reserved(session, tx, owner) {
            drop(commit_admission);
            file.abort().await;
            return;
        }
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

    async fn start_pull(
        &self,
        req: CpPull,
        tx: &Link,
        identity: Option<PreparedIdentity>,
        owner: Option<&Arc<Pending>>,
    ) {
        let prepare_admission = if owner.is_some() {
            Some(self.shared.admission.lock().await)
        } else {
            None
        };
        #[cfg(not(windows))]
        let _ = identity;
        if !self.admit_reserved(req.session, tx, owner) {
            return;
        }
        #[cfg(not(windows))]
        if req.windows_run_as.is_some() {
            send_error(
                tx,
                req.session,
                ErrorCode::BadRequest,
                "Windows run-as logon is not supported by this host".into(),
            );
            return;
        }
        let session = req.session;
        if !self.admit_reserved(session, tx, owner) {
            return;
        }
        drop(prepare_admission);
        #[cfg(windows)]
        let checked_path =
            crate::windows_run_as::check_path(identity.as_deref(), &self.opts.policy, &req.path);
        #[cfg(not(windows))]
        let checked_path = self.opts.policy.check(&req.path);
        let path = match checked_path {
            Ok(p) => p,
            Err(m) => return send_error(tx, session, ErrorCode::PathDenied, m),
        };
        let _commit_admission = if owner.is_some() {
            Some(self.shared.admission.lock().await)
        } else {
            None
        };
        if !self.admit_reserved(session, tx, owner) {
            return;
        }
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
            #[cfg(windows)]
            let pulled = transfer::pull_as(&path, &outbox, identity).await;
            #[cfg(not(windows))]
            let pulled = transfer::pull(&path, &outbox).await;
            let frame = match pulled {
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
pub(crate) mod tests {
    use super::*;

    // Keep the read side open forever after Welcome while the first
    // heartbeat write fails. A real socket close races the reader instead.
    struct WriterFirstDaemon {
        welcome: Option<Message>,
        writes: usize,
    }

    impl futures_util::Stream for WriterFirstDaemon {
        type Item = Result<Message, tungstenite::Error>;

        fn poll_next(
            mut self: std::pin::Pin<&mut Self>,
            _: &mut std::task::Context<'_>,
        ) -> std::task::Poll<Option<Self::Item>> {
            match self.welcome.take() {
                Some(welcome) => std::task::Poll::Ready(Some(Ok(welcome))),
                None => std::task::Poll::Pending,
            }
        }
    }

    impl futures_util::Sink<Message> for WriterFirstDaemon {
        type Error = tungstenite::Error;

        fn poll_ready(
            self: std::pin::Pin<&mut Self>,
            _: &mut std::task::Context<'_>,
        ) -> std::task::Poll<Result<(), Self::Error>> {
            std::task::Poll::Ready(Ok(()))
        }

        fn start_send(
            mut self: std::pin::Pin<&mut Self>,
            item: Message,
        ) -> Result<(), Self::Error> {
            self.writes += 1;
            let Message::Text(text) = item else {
                panic!("expected control frame");
            };
            match decode_control::<HostFrame>(&text).unwrap() {
                HostFrame::Hello(_) if self.writes == 1 => Ok(()),
                HostFrame::Ping { .. } => Err(tungstenite::Error::ConnectionClosed),
                other => panic!("unexpected frame: {other:?}"),
            }
        }

        fn poll_flush(
            self: std::pin::Pin<&mut Self>,
            _: &mut std::task::Context<'_>,
        ) -> std::task::Poll<Result<(), Self::Error>> {
            std::task::Poll::Ready(Ok(()))
        }

        fn poll_close(
            self: std::pin::Pin<&mut Self>,
            _: &mut std::task::Context<'_>,
        ) -> std::task::Poll<Result<(), Self::Error>> {
            std::task::Poll::Ready(Ok(()))
        }
    }

    #[tokio::test]
    async fn writer_finishes_first_connection_returns_ok_and_can_reconnect() {
        let client = Client::new(ClientOptions::new("ws://localhost", "test", "test"));
        for _ in 0..2 {
            let welcome = DaemonFrame::Welcome(Welcome {
                host_id: "test".into(),
                heartbeat_secs: 1,
                resume_token: "resume".into(),
                resume: vec![],
                token: None,
                metrics: false,
                update: None,
            });
            let daemon = WriterFirstDaemon {
                welcome: Some(Message::Text(encode_control(&welcome).unwrap().into())),
                writes: 0,
            };
            tokio::time::timeout(Duration::from_secs(5), client.serve_connection(daemon))
                .await
                .expect("writer completion must end the connection")
                .expect("writer-first disconnect is reconnectable");
        }
    }

    #[tokio::test]
    async fn staged_update_drain_expires_reopens_and_retries_without_discarding_stage() {
        let client = Client::new(ClientOptions::new("ws://localhost", "test", "test"));
        let dir = tempfile::tempdir().unwrap();
        let binary = dir.path().join("host");
        let staged = crate::selfupdate::stage(
            &binary,
            b"#!/bin/sh\necho 'terrarium-host 0.3.0+bbbbbbb'\n",
            "0.3.0+bbbbbbb",
        )
        .await
        .unwrap();
        let (tx, _rx) = mpsc::unbounded_channel();
        client.sessions().insert(
            1,
            Entry {
                outbox: client.new_outbox(1, &tx),
                input: None,
                push: None,
                detached_since: None,
            },
        );
        let su = SelfUpdateOptions {
            binary,
            enabled: true,
            supervised: true,
            idle_poll: Duration::from_millis(50),
            drain_timeout: Duration::from_millis(20),
        };
        let worker = client.clone();
        let task = tokio::spawn(async move { worker.drain_for_update(&su).await });
        tokio::time::timeout(Duration::from_secs(2), async {
            while !client
                .shared
                .draining
                .load(std::sync::atomic::Ordering::SeqCst)
            {
                tokio::task::yield_now().await;
            }
            while client
                .shared
                .draining
                .load(std::sync::atomic::Ordering::SeqCst)
            {
                tokio::task::yield_now().await;
            }
        })
        .await
        .unwrap();
        assert!(client.admit(2, &tx));
        assert_eq!(
            client.session_count(),
            1,
            "drain expiry must not kill sessions"
        );
        assert!(!task.is_finished());
        assert!(staged.exists(), "staged binary retained");
        client.sessions().remove(&1);
        tokio::time::timeout(Duration::from_secs(2), task)
            .await
            .unwrap()
            .unwrap();
        assert!(
            client
                .shared
                .draining
                .load(std::sync::atomic::Ordering::SeqCst)
        );
    }

    #[tokio::test]
    async fn staged_update_refuses_new_sessions_but_keeps_control_frames() {
        let client = Client::new(ClientOptions::new("ws://localhost", "test", "test"));
        let (tx, mut rx) = mpsc::unbounded_channel();
        assert!(client.admit(1, &tx));
        client
            .shared
            .draining
            .store(true, std::sync::atomic::Ordering::SeqCst);
        assert!(!client.admit(2, &tx));
        assert!(matches!(
            rx.recv().await,
            Some(Outgoing::Control(HostFrame::Error {
                session: 2,
                code: ErrorCode::Busy,
                ..
            }))
        ));
        client
            .on_text(r#"{"type":"ping","nonce":7}"#, &tx)
            .await
            .unwrap();
        assert!(matches!(
            rx.recv().await,
            Some(Outgoing::Control(HostFrame::Pong { .. }))
        ));
        client
            .shared
            .draining
            .store(false, std::sync::atomic::Ordering::SeqCst);
        assert!(client.admit(3, &tx));
    }

    #[tokio::test]
    async fn pending_logon_capacity_stays_reserved_after_release_until_auth_returns() {
        let mut options = ClientOptions::new("ws://localhost", "fixture", "fixture");
        options.max_sessions = 1;
        let client = Client::new(options);
        let (tx, mut rx) = mpsc::unbounded_channel();
        assert!(client.admit(1, &tx));
        let lease = client.shared.pending_logons.reserve(1);
        assert_eq!(client.session_count(), 1);
        assert!(client.admit_reserved(1, &tx, Some(&lease.pending)));
        assert!(!client.admit(2, &tx));
        assert!(matches!(
            rx.recv().await,
            Some(Outgoing::Control(HostFrame::Error {
                code: ErrorCode::Busy,
                ..
            }))
        ));
        client
            .on_text(r#"{"type":"release","session":1}"#, &tx)
            .await
            .unwrap();
        assert!(lease.pending.cancelled());
        assert!(lease.pending.commit().is_none());
        assert!(!client.admit(2, &tx));
        assert!(matches!(
            rx.recv().await,
            Some(Outgoing::Control(HostFrame::Error {
                code: ErrorCode::Busy,
                ..
            }))
        ));
        drop(lease); // The delayed native operation has finally returned.
        assert_eq!(client.session_count(), 0);
        assert!(client.admit(2, &tx));
    }

    #[tokio::test]
    async fn pending_logon_close_cancels_delayed_auth_before_process_commit() {
        let client = Client::new(ClientOptions::new("ws://localhost", "fixture", "fixture"));
        let lease = client.shared.pending_logons.reserve(1);
        let (complete, waiting) = tokio::sync::oneshot::channel();
        let actor = tokio::spawn(async move {
            waiting.await.unwrap();
            lease.pending.commit().map(|_| ()).is_some()
        });
        client.detach_all();
        assert_eq!(client.session_count(), 1);
        complete.send(()).unwrap();
        assert!(!actor.await.unwrap());
        assert_eq!(client.session_count(), 0);
    }

    #[tokio::test]
    async fn pending_logon_early_file_chunks_and_end_keep_wire_order() {
        let client = Client::new(ClientOptions::new("ws://localhost", "fixture", "fixture"));
        let (tx, mut rx) = mpsc::unbounded_channel();
        let lease = client.shared.pending_logons.reserve(1);
        for payload in [b"one".to_vec(), b"two".to_vec()] {
            client
                .on_binary(&DataFrame::new(1, StreamTag::File, payload).encode(), &tx)
                .await;
        }
        client
            .on_text(
                r#"{"type":"cp_end","session":1,"bytes":6,"sha256":"fixture"}"#,
                &tx,
            )
            .await
            .unwrap();
        for expected in [b"one", b"two"] {
            let Some(PendingEvent::Binary(bytes)) = lease.pop_or_handoff() else {
                panic!("missing queued chunk");
            };
            assert_eq!(
                DataFrame::decode(&bytes).unwrap().payload,
                expected.as_slice()
            );
        }
        assert!(matches!(
            lease.pop_or_handoff(),
            Some(PendingEvent::Control(frame)) if matches!(*frame, DaemonFrame::CpEnd { bytes: 6, .. })
        ));
        assert!(lease.pop_or_handoff().is_none());
        assert!(rx.try_recv().is_err());
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn pending_logon_heartbeat_and_other_session_signal_bypass_admission() {
        let client = Client::new(ClientOptions::new("ws://localhost", "fixture", "fixture"));
        let (tx, mut rx) = mpsc::unbounded_channel();
        let (input, mut inputs) = mpsc::unbounded_channel();
        let lease = client.shared.pending_logons.reserve(1);
        client.sessions().insert(
            2,
            Entry {
                outbox: client.new_outbox(2, &tx),
                input: Some(input),
                push: None,
                detached_since: None,
            },
        );
        // Simulate final admission being busy while a separate logon is still
        // pending. Control processing must never acquire this lock.
        let _admission = client.shared.admission.lock().await;
        tokio::time::timeout(
            Duration::from_secs(1),
            client.on_text(r#"{"type":"ping","nonce":19}"#, &tx),
        )
        .await
        .unwrap()
        .unwrap();
        assert!(matches!(
            rx.recv().await,
            Some(Outgoing::Control(HostFrame::Pong { nonce: 19 }))
        ));
        tokio::time::timeout(
            Duration::from_secs(1),
            client.on_text(r#"{"type":"signal","session":2,"signal":"KILL"}"#, &tx),
        )
        .await
        .unwrap()
        .unwrap();
        assert!(matches!(inputs.recv().await, Some(SessionInput::Signal(_))));
        tokio::time::timeout(
            Duration::from_secs(1),
            client.on_text(r#"{"type":"signal","session":1,"signal":"KILL"}"#, &tx),
        )
        .await
        .unwrap()
        .unwrap();
        assert!(lease.pending.cancelled());
        assert!(matches!(
            rx.recv().await,
            Some(Outgoing::Control(HostFrame::Error {
                code: ErrorCode::Interrupted,
                ..
            }))
        ));
    }

    #[tokio::test]
    async fn windows_run_as_checks_capacity_before_logon_or_profile_creation() {
        let mut options = ClientOptions::new("ws://localhost", "fixture", "fixture");
        options.max_sessions = 0;
        options.windows_run_as.enabled = true;
        let client = Client::new(options);
        let (tx, mut rx) = mpsc::unbounded_channel();
        for verb in ["exec", "cp_push", "cp_pull"] {
            let request = serde_json::json!({
                "type": verb, "session": 1, "cmd": "unused", "path": "C:\\unused",
                "windows_run_as": { "account": "unqualified", "password": "non-secret-fixture" }
            });
            client.on_text(&request.to_string(), &tx).await.unwrap();
            assert!(matches!(
                rx.recv().await,
                Some(Outgoing::Control(HostFrame::Error {
                    code: ErrorCode::Busy,
                    ..
                }))
            ));
        }
        assert_eq!(client.session_count(), 0);
    }

    /// Write the 0700 fake `nvidia-smi` script that the product execs.
    /// `gpu::sample_with` runs `nvidia_smi_bin` directly (`Command::new`)
    /// and must keep doing so for the real binary, so the fake cannot be
    /// run as `sh <path>` (with the `--query-gpu` args `sh` answers
    /// `Illegal option`). This process must then never hold a write fd on
    /// the script: a fork of the test binary while such an fd is open
    /// copies the fd table, and the exec of the script in that window
    /// fails with ETXTBSY under load (TER-415, TER-423). A child shell
    /// does the write and drops the fd by exiting.
    pub(crate) fn write_execd_script(
        dir: &std::path::Path,
        name: &str,
        text: &str,
    ) -> std::path::PathBuf {
        let path = dir.join(name);
        let status = std::process::Command::new("sh")
            .arg("-c")
            .arg("printf %s \"$1\" > \"$2\" && chmod 700 \"$2\"")
            .arg("sh")
            .arg(text)
            .arg(&path)
            .status()
            .unwrap();
        assert!(status.success(), "write the {name} fixture");
        path
    }

    #[tokio::test]
    async fn each_connection_hello_refreshes_gpu_inventory_and_clears_failed_probes() {
        let dir = tempfile::tempdir().unwrap();
        let inventory = dir.path().join("inventory");
        let bin = write_execd_script(
            dir.path(),
            "smi",
            &format!("#!/bin/sh\ncat '{}'\n", inventory.display()),
        );
        let mut opts = ClientOptions::new("ws://localhost", "test", "test");
        opts.capabilities.gpu_reservations = true;
        opts.nvidia_smi_bin = bin.to_string_lossy().into_owned();
        let client = Client::new(opts);
        std::fs::write(&inventory, "GPU-abcd, test, 8192, 0\n").unwrap();
        assert_eq!(client.hello().await.capabilities.gpus[0].uuid, "GPU-abcd");
        std::fs::write(&inventory, "GPU-1234, replacement, 16384, 0\n").unwrap();
        let hello = client.hello().await;
        assert_eq!(hello.capabilities.gpus[0].uuid, "GPU-1234");
        assert_eq!(hello.capabilities.gpus[0].memory_bytes, 16 << 30);
        std::fs::write(&inventory, "unavailable\n").unwrap();
        assert!(client.hello().await.capabilities.gpus.is_empty());
    }

    /// A helper that answers `hello` with the `session` right.
    async fn fake_helper(socket: &std::path::Path) -> tokio::task::JoinHandle<()> {
        let listener = tokio::net::UnixListener::bind(socket).unwrap();
        tokio::spawn(async move {
            while let Ok((mut s, _)) = listener.accept().await {
                let _ = crate::native::read_frame(&mut s).await;
                let reply = crate::native::Reply::Hello {
                    version: crate::native::PROTOCOL_VERSION,
                    rights: vec![crate::native::Right::Session],
                };
                let _ = crate::native::write_frame(
                    &mut s,
                    crate::native::Kind::Reply,
                    &serde_json::to_vec(&reply).unwrap(),
                )
                .await;
            }
        })
    }

    fn native_client(socket: &std::path::Path) -> Client {
        let mut opts = ClientOptions::new("ws://localhost", "test", "test");
        opts.native = Some(crate::native::NativeOptions {
            socket: socket.to_path_buf(),
            shim: "/opt/terrarium-host".into(),
        });
        opts.capabilities.limits = Some(crate::protocol::LimitsCapability {
            systemd: true,
            controllers: vec!["cpu".into(), "memory".into(), "cpuset".into()],
            ..Default::default()
        });
        Client::new(opts)
    }

    fn native_exec() -> (VerbPlan, crate::protocol::ExecLimits) {
        let plan = VerbPlan::plain(crate::session::ExecPlan {
            program: "true".into(),
            ..Default::default()
        });
        let l = crate::protocol::ExecLimits {
            target: "box".into(),
            holder: "p/a".into(),
            limits: Default::default(),
            reservation: None,
            native_gpu: Some(Default::default()),
        };
        (plan, l)
    }

    #[tokio::test]
    async fn native_routing_follows_the_helper_answer_at_hello() {
        let dir = tempfile::tempdir().unwrap();
        let socket = dir.path().join("helper.sock");
        let apply = crate::protocol::ResourceApply {
            session: 1,
            target: "box".into(),
            holder: "p/a".into(),
            container: None,
            limits: terrarium_core::resources::RuntimeLimits {
                cpus: Some(vec![1]),
                ..Default::default()
            },
        };
        let stop = crate::protocol::ResourceStop {
            session: 2,
            target: "box".into(),
            holder: "p/a".into(),
        };
        // [native_gpu] on, but the helper does not answer: apply goes to
        // the user slice, a stop still goes to the helper (see
        // `a_restarted_client_with_the_helper_down_stops_through_the_helper`),
        // and a native exec is refused rather than run without device policy.
        let client = native_client(&socket);
        let hello = client.hello().await;
        assert!(!hello.capabilities.native_gpu.unwrap().enforced);
        assert_eq!(
            client.slice_apply_plan(&apply).unwrap().exec.program,
            "systemctl"
        );
        assert_eq!(
            client.slice_stop_plan(&stop).unwrap().exec.program,
            "/opt/terrarium-host"
        );
        let (plan, l) = native_exec();
        let e = client.native(plan, &l, 3).err().unwrap();
        assert!(e.message.contains("did not answer"), "{}", e.message);

        // The helper answers: apply, stop, and exec go through the shim.
        let helper = fake_helper(&socket).await;
        assert!(
            client
                .hello()
                .await
                .capabilities
                .native_gpu
                .unwrap()
                .enforced
        );
        for plan in [
            client.slice_apply_plan(&apply).unwrap(),
            client.slice_stop_plan(&stop).unwrap(),
        ] {
            assert_eq!(plan.exec.program, "/opt/terrarium-host");
            assert_eq!(plan.exec.args[0], "native");
            assert!(plan.exec.env.contains_key(crate::native::REQUEST_ENV));
        }
        let (plan, l) = native_exec();
        let plan = client.native(plan, &l, 3).unwrap();
        assert_eq!(plan.exec.program, "/opt/terrarium-host");

        // The helper goes away: the next hello turns apply and exec back,
        // but a stop still goes to the helper (its native units may still
        // run) and to the user slice.
        helper.abort();
        let _ = helper.await;
        std::fs::remove_file(&socket).unwrap();
        assert!(
            !client
                .hello()
                .await
                .capabilities
                .native_gpu
                .unwrap()
                .enforced
        );
        assert_eq!(
            client.slice_apply_plan(&apply).unwrap().exec.program,
            "systemctl"
        );
        let plan = client.slice_stop_plan(&stop).unwrap();
        assert_eq!(plan.exec.program, "/opt/terrarium-host");
        assert_eq!(plan.exec.args.last().unwrap(), "--user-slice");
        let (plan, l) = native_exec();
        assert!(client.native(plan, &l, 3).is_err());
    }

    /// TER-503: a host agent that restarts while the helper is down has no
    /// memory of an earlier enforcement, but native units of the holder
    /// may still run in the helper's system slice. The stop goes to the
    /// shim with `--user-slice`, which fails when the helper cannot be
    /// reached (`a_shim_stop_also_stops_the_user_slice` in
    /// `tests/native_helper.rs`), so the stop never reports success.
    #[tokio::test]
    async fn a_restarted_client_with_the_helper_down_stops_through_the_helper() {
        let dir = tempfile::tempdir().unwrap();
        let socket = dir.path().join("helper.sock");
        let stop = crate::protocol::ResourceStop {
            session: 1,
            target: "box".into(),
            holder: "p/a".into(),
        };
        // A new process: the helper never answered with enforcement here.
        let client = native_client(&socket);
        assert!(
            !client
                .hello()
                .await
                .capabilities
                .native_gpu
                .unwrap()
                .enforced
        );
        let plan = client.slice_stop_plan(&stop).unwrap();
        assert_eq!(plan.exec.program, "/opt/terrarium-host");
        assert_eq!(plan.exec.args[0], "native");
        assert_eq!(plan.exec.args.last().unwrap(), "--user-slice");
        let req: crate::native::Request =
            serde_json::from_str(&plan.exec.env[crate::native::REQUEST_ENV]).unwrap();
        assert_eq!(
            req,
            crate::native::Request::Stop(crate::native::SliceRequest {
                target: "box".into(),
                holder: "p/a".into(),
            })
        );

        // Without [native_gpu], only the user slice.
        let plain = Client::new(ClientOptions::new("ws://localhost", "test", "test"));
        assert_eq!(
            plain.slice_stop_plan(&stop).unwrap().exec.program,
            "systemctl"
        );
    }

    #[test]
    fn a_native_exec_without_native_gpu_config_is_refused() {
        let client = Client::new(ClientOptions::new("ws://localhost", "test", "test"));
        let (plan, l) = native_exec();
        let e = client.native(plan, &l, 1).err().unwrap();
        assert_eq!(e.code, ErrorCode::Unsupported);
        assert!(e.message.contains("[native_gpu]"), "{}", e.message);
    }

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
