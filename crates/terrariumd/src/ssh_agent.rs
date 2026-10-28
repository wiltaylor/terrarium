//! An ssh-agent protocol server (PLAN §16b.4, draft-miller-ssh-agent).
//!
//! The daemon serves one socket for each agent. The socket lists and signs
//! with the keys of a [`KeySource`] and refuses every other request (add,
//! remove, lock, unlock, extensions) with `SSH_AGENT_FAILURE`.
//!
//! The server holds no private key. For each sign request it finds the
//! identity by its public key blob and calls [`KeySource::sign`]. The source
//! loads the private key, signs (for example with [`sign_with_key`]), and
//! drops (zeroizes) the key. The source also checks the grant and can refuse
//! a sign (no grant, rate limit) with [`SignError::Refused`]. After each sign
//! of a known key, the server calls the [`AgentOptions::on_sign`] hook, so the
//! daemon can write the audit.
//!
//! The wire protocol is small, so this module implements it with the
//! `ssh-key` crate for key blobs and signatures. The `ssh-agent-lib` crate
//! was not used: it brings an async-trait service stack and its own key
//! types for the whole protocol (add, lock, constraints, extensions), and
//! this server answers two requests only.
//!
//! The socket is made with mode 0600: the server binds a temporary name in
//! the same directory, sets the mode, and renames it to the final path, so
//! the final path never has a wider mode. The mode change does not follow a
//! symlink: it opens the bound name with `O_PATH | O_NOFOLLOW`, checks that
//! the file is a socket, and changes the mode through that file descriptor.
//! Messages over [`AgentOptions::max_message_len`] close the connection.
//!
//! [`AgentOptions::sign_limit`] limits the signs of one socket (a token
//! bucket). A sign over the limit answers `SSH_AGENT_FAILURE` with no call
//! to the source, and the hook sees [`SignOutcome::Limited`].

use std::fmt;
use std::io;
use std::os::unix::fs::{FileTypeExt, MetadataExt, PermissionsExt};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;

use signature::Signer;
use ssh_key::encoding::Encode;
use ssh_key::private::KeypairData;
use ssh_key::{HashAlg, PrivateKey, PublicKey, Signature};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{UnixListener, UnixStream};
use tokio::sync::{Semaphore, watch};
use tokio::task::{JoinHandle, JoinSet};

/// `SSH_AGENT_FAILURE`.
pub const SSH_AGENT_FAILURE: u8 = 5;
/// `SSH_AGENTC_REQUEST_IDENTITIES`.
pub const SSH_AGENTC_REQUEST_IDENTITIES: u8 = 11;
/// `SSH_AGENT_IDENTITIES_ANSWER`.
pub const SSH_AGENT_IDENTITIES_ANSWER: u8 = 12;
/// `SSH_AGENTC_SIGN_REQUEST`.
pub const SSH_AGENTC_SIGN_REQUEST: u8 = 13;
/// `SSH_AGENT_SIGN_RESPONSE`.
pub const SSH_AGENT_SIGN_RESPONSE: u8 = 14;
/// Sign flag: an RSA key signs with `rsa-sha2-256`.
pub const SSH_AGENT_RSA_SHA2_256: u32 = 0x02;
/// Sign flag: an RSA key signs with `rsa-sha2-512`.
pub const SSH_AGENT_RSA_SHA2_512: u32 = 0x04;

/// The default limit of one message (the length prefix not included).
pub const DEFAULT_MAX_MESSAGE_LEN: usize = 256 * 1024;
/// The default limit of open connections on one socket.
pub const DEFAULT_MAX_CONNECTIONS: usize = 64;

/// A key that the agent lists. `label` is the name of the secret; clients
/// see it as the key comment.
#[derive(Clone, Debug)]
pub struct Identity {
    pub label: String,
    pub public: PublicKey,
}

/// Why a sign did not give a signature.
#[derive(Debug, thiserror::Error)]
pub enum SignError {
    /// The source refused the sign: no grant, a rate limit, a removed key.
    #[error("sign refused: {0}")]
    Refused(String),
    /// The key or the requested signature algorithm is not supported.
    #[error("unsupported: {0}")]
    Unsupported(String),
    /// The key did not load or the sign failed.
    #[error("sign failed: {0}")]
    Failed(String),
}

/// The keys of one agent socket. The daemon's secret store implements it.
///
/// The server calls both methods on a blocking thread
/// ([`tokio::task::spawn_blocking`]), so they can read the database and
/// decrypt.
pub trait KeySource: Send + Sync + 'static {
    /// The keys to list. The server calls it for each identities request and
    /// for each sign request, so a change applies at once.
    fn list(&self) -> Vec<Identity>;

    /// Signs `data` with the private key of `identity`, an identity from
    /// [`list`](Self::list). `flags` are the request flags
    /// ([`SSH_AGENT_RSA_SHA2_256`], [`SSH_AGENT_RSA_SHA2_512`]).
    ///
    /// Load the private key for this call only and drop it before return;
    /// [`sign_with_key`] does the algorithm choice.
    fn sign(&self, identity: &Identity, data: &[u8], flags: u32) -> Result<Signature, SignError>;
}

/// Signs `data` with `key` as an ssh-agent does: an RSA key uses
/// `rsa-sha2-512` or `rsa-sha2-256` from `flags` (`ssh-rsa` with SHA-1 is not
/// supported); Ed25519 and ECDSA keys ignore `flags`.
pub fn sign_with_key(key: &PrivateKey, data: &[u8], flags: u32) -> Result<Signature, SignError> {
    let signed = match key.key_data() {
        KeypairData::Encrypted(_) => {
            return Err(SignError::Failed("the private key is encrypted".into()));
        }
        KeypairData::Rsa(keypair) => {
            let hash = if flags & SSH_AGENT_RSA_SHA2_512 != 0 {
                HashAlg::Sha512
            } else if flags & SSH_AGENT_RSA_SHA2_256 != 0 {
                HashAlg::Sha256
            } else {
                return Err(SignError::Unsupported(
                    "ssh-rsa (SHA-1) signatures; the client must ask for rsa-sha2-256 or rsa-sha2-512"
                        .into(),
                ));
            };
            (keypair, Some(hash)).try_sign(data)
        }
        other => other.try_sign(data),
    };
    signed.map_err(|e| SignError::Failed(e.to_string()))
}

/// The result of one sign, for the [`AgentOptions::on_sign`] hook.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SignOutcome {
    Signed,
    Refused,
    Failed,
    /// Over [`AgentOptions::sign_limit`]: the source was not called.
    Limited,
}

/// One sign of a listed key. A sign request for a key that is not listed
/// fails without a call to the source or the hook.
#[derive(Debug, Clone)]
pub struct SignEvent {
    pub label: String,
    pub outcome: SignOutcome,
}

impl SignEvent {
    /// True when the sign gave a signature.
    pub fn ok(&self) -> bool {
        self.outcome == SignOutcome::Signed
    }
}

/// The hook that the server calls after each sign of a listed key.
pub type SignHook = Arc<dyn Fn(&SignEvent) + Send + Sync>;

/// A token bucket for the signs of one socket: `burst` signs at once,
/// then `per_second` signs each second.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SignLimit {
    pub per_second: u32,
    pub burst: u32,
}

impl Default for SignLimit {
    /// 10 signs a second, 30 at once. An ssh login signs one time; a git
    /// fetch over ssh signs one time for each connection.
    fn default() -> Self {
        Self {
            per_second: 10,
            burst: 30,
        }
    }
}

/// The state of a [`SignLimit`].
struct Bucket {
    limit: SignLimit,
    tokens: f64,
    last: std::time::Instant,
}

impl Bucket {
    fn new(limit: SignLimit) -> Self {
        Self {
            limit,
            tokens: f64::from(limit.burst),
            last: std::time::Instant::now(),
        }
    }

    /// Take one token. False: the bucket is empty.
    fn take(&mut self) -> bool {
        let now = std::time::Instant::now();
        let add = now.duration_since(self.last).as_secs_f64() * f64::from(self.limit.per_second);
        self.tokens = (self.tokens + add).min(f64::from(self.limit.burst));
        self.last = now;
        if self.tokens >= 1.0 {
            self.tokens -= 1.0;
            true
        } else {
            false
        }
    }
}

/// The options of one agent socket.
#[derive(Clone)]
pub struct AgentOptions {
    /// The limit of one message. A larger message closes the connection.
    pub max_message_len: usize,
    /// The limit of open connections. More clients wait for a free slot.
    pub max_connections: usize,
    /// Called after each sign of a listed key (audit, counters).
    pub on_sign: Option<SignHook>,
    /// The sign rate limit of the socket. `None`: no limit.
    pub sign_limit: Option<SignLimit>,
}

impl Default for AgentOptions {
    fn default() -> Self {
        Self {
            max_message_len: DEFAULT_MAX_MESSAGE_LEN,
            max_connections: DEFAULT_MAX_CONNECTIONS,
            on_sign: None,
            sign_limit: None,
        }
    }
}

impl fmt::Debug for AgentOptions {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("AgentOptions")
            .field("max_message_len", &self.max_message_len)
            .field("max_connections", &self.max_connections)
            .field("on_sign", &self.on_sign.is_some())
            .field("sign_limit", &self.sign_limit)
            .finish()
    }
}

/// A running agent socket. [`shutdown`](Self::shutdown) stops it and waits;
/// a drop stops it too. Both close the open connections and remove the
/// socket file (when it is still the file that this server made).
pub struct AgentHandle {
    path: PathBuf,
    /// The device and inode of the socket file that this server made.
    file_id: (u64, u64),
    stop: watch::Sender<bool>,
    task: Option<JoinHandle<()>>,
}

impl AgentHandle {
    /// The socket path.
    pub fn path(&self) -> &Path {
        &self.path
    }

    /// Stops the server, closes the connections, and removes the socket.
    pub async fn shutdown(mut self) {
        let _ = self.stop.send(true);
        if let Some(task) = self.task.take() {
            let _ = task.await;
        }
        self.remove_socket();
    }

    fn remove_socket(&self) {
        if let Ok(meta) = std::fs::symlink_metadata(&self.path)
            && (meta.dev(), meta.ino()) == self.file_id
        {
            let _ = std::fs::remove_file(&self.path);
        }
    }
}

impl Drop for AgentHandle {
    fn drop(&mut self) {
        let _ = self.stop.send(true);
        if let Some(task) = self.task.take() {
            // Aborting the accept loop drops its JoinSet, which aborts the
            // connection tasks.
            task.abort();
        }
        self.remove_socket();
    }
}

impl fmt::Debug for AgentHandle {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("AgentHandle")
            .field("path", &self.path)
            .finish_non_exhaustive()
    }
}

/// Binds `path` (mode 0600) and serves the ssh-agent protocol for the keys
/// of `source`. Call it in a Tokio runtime.
///
/// The parent directory must exist. A stale socket at `path` is replaced;
/// any other file at `path` is an error.
pub fn serve(
    path: impl AsRef<Path>,
    source: Arc<dyn KeySource>,
    options: AgentOptions,
) -> io::Result<AgentHandle> {
    let path = path.as_ref().to_path_buf();
    match std::fs::symlink_metadata(&path) {
        Ok(meta) if !meta.file_type().is_socket() => {
            return Err(io::Error::new(
                io::ErrorKind::AlreadyExists,
                format!("{} exists and is not a socket", path.display()),
            ));
        }
        _ => {}
    }

    let std_listener = bind_private(&path)?;
    let meta = std::fs::symlink_metadata(&path)?;
    std_listener.set_nonblocking(true)?;
    let listener = UnixListener::from_std(std_listener)?;

    let (stop, stop_rx) = watch::channel(false);
    let task = tokio::spawn(accept_loop(listener, source, options, stop_rx));
    Ok(AgentHandle {
        path,
        file_id: (meta.dev(), meta.ino()),
        stop,
        task: Some(task),
    })
}

/// Binds a temporary name next to `path`, sets mode 0600, and renames it to
/// `path`.
fn bind_private(path: &Path) -> io::Result<std::os::unix::net::UnixListener> {
    static COUNTER: AtomicU64 = AtomicU64::new(0);
    let dir = path.parent().filter(|d| !d.as_os_str().is_empty());
    let tmp_name = format!(
        ".sa{}-{}",
        std::process::id(),
        COUNTER.fetch_add(1, Ordering::Relaxed)
    );
    let tmp = match dir {
        Some(d) => d.join(tmp_name),
        None => PathBuf::from(tmp_name),
    };
    let _ = std::fs::remove_file(&tmp);
    let listener = std::os::unix::net::UnixListener::bind(&tmp)?;
    let result = chmod_socket_nofollow(&tmp).and_then(|()| std::fs::rename(&tmp, path));
    if let Err(e) = result {
        let _ = std::fs::remove_file(&tmp);
        return Err(e);
    }
    Ok(listener)
}

/// Set mode 0600 on the socket at `path`. A symlink at `path` is not
/// followed, and a file that is not a socket is an error: the mode change
/// goes through an `O_PATH | O_NOFOLLOW` descriptor of the checked file.
fn chmod_socket_nofollow(path: &Path) -> io::Result<()> {
    use nix::libc;
    use std::os::fd::AsRawFd;
    use std::os::unix::fs::OpenOptionsExt;
    let file = std::fs::OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_PATH | libc::O_NOFOLLOW | libc::O_CLOEXEC)
        .open(path)?;
    if !file.metadata()?.file_type().is_socket() {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            format!("{} is not a socket", path.display()),
        ));
    }
    // A chmod of the magic link changes the inode of the descriptor.
    std::fs::set_permissions(
        format!("/proc/self/fd/{}", file.as_raw_fd()),
        std::fs::Permissions::from_mode(0o600),
    )
}

async fn accept_loop(
    listener: UnixListener,
    source: Arc<dyn KeySource>,
    options: AgentOptions,
    mut stop: watch::Receiver<bool>,
) {
    let slots = Arc::new(Semaphore::new(options.max_connections.max(1)));
    let bucket = Arc::new(std::sync::Mutex::new(options.sign_limit.map(Bucket::new)));
    let options = Arc::new(options);
    let mut connections = JoinSet::new();
    loop {
        let permit = tokio::select! {
            _ = stop.changed() => break,
            permit = Arc::clone(&slots).acquire_owned() => match permit {
                Ok(p) => p,
                Err(_) => break,
            },
        };
        let stream = tokio::select! {
            _ = stop.changed() => break,
            accepted = listener.accept() => match accepted {
                Ok((stream, _)) => stream,
                Err(e) => {
                    tracing::warn!(error = %e, "ssh-agent accept failed");
                    // EMFILE and friends: do not spin.
                    tokio::time::sleep(Duration::from_millis(100)).await;
                    continue;
                }
            },
        };
        while connections.try_join_next().is_some() {}
        let source = Arc::clone(&source);
        let options = Arc::clone(&options);
        let bucket = Arc::clone(&bucket);
        connections.spawn(async move {
            let _permit = permit;
            if let Err(e) = serve_connection(stream, source, options, bucket).await {
                tracing::debug!(error = %e, "ssh-agent connection ended");
            }
        });
    }
    connections.shutdown().await;
}

/// The sign bucket of one socket, shared by its connections.
type SharedBucket = Arc<std::sync::Mutex<Option<Bucket>>>;

async fn serve_connection(
    mut stream: UnixStream,
    source: Arc<dyn KeySource>,
    options: Arc<AgentOptions>,
    bucket: SharedBucket,
) -> io::Result<()> {
    loop {
        let mut len = [0u8; 4];
        match stream.read_exact(&mut len).await {
            Ok(_) => {}
            Err(e) if e.kind() == io::ErrorKind::UnexpectedEof => return Ok(()),
            Err(e) => return Err(e),
        }
        let len = u32::from_be_bytes(len) as usize;
        if len == 0 || len > options.max_message_len {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                format!("message length {len} out of range"),
            ));
        }
        let mut body = vec![0u8; len];
        stream.read_exact(&mut body).await?;
        let reply = handle_message(&body, &source, &options, &bucket).await;
        let mut framed = Vec::with_capacity(reply.len() + 4);
        framed.extend_from_slice(&(reply.len() as u32).to_be_bytes());
        framed.extend_from_slice(&reply);
        stream.write_all(&framed).await?;
    }
}

/// Answers one message (type byte and payload) with one reply.
async fn handle_message(
    body: &[u8],
    source: &Arc<dyn KeySource>,
    options: &AgentOptions,
    bucket: &SharedBucket,
) -> Vec<u8> {
    let failure = vec![SSH_AGENT_FAILURE];
    let (&kind, payload) = match body.split_first() {
        Some(split) => split,
        None => return failure,
    };
    match kind {
        SSH_AGENTC_REQUEST_IDENTITIES if payload.is_empty() => {
            let Some(identities) = list(source).await else {
                return failure;
            };
            identities_answer(&identities).unwrap_or(failure)
        }
        SSH_AGENTC_SIGN_REQUEST => sign_request(payload, source, options, bucket)
            .await
            .unwrap_or(failure),
        _ => failure,
    }
}

async fn list(source: &Arc<dyn KeySource>) -> Option<Vec<Identity>> {
    let source = Arc::clone(source);
    tokio::task::spawn_blocking(move || source.list())
        .await
        .ok()
}

fn identities_answer(identities: &[Identity]) -> Option<Vec<u8>> {
    let mut out = vec![SSH_AGENT_IDENTITIES_ANSWER];
    let mut count: u32 = 0;
    let mut entries = Vec::new();
    for identity in identities {
        // A key that does not encode is left out, not fatal.
        let Ok(blob) = identity.public.to_bytes() else {
            continue;
        };
        put_string(&mut entries, &blob);
        put_string(&mut entries, identity.label.as_bytes());
        count += 1;
    }
    out.extend_from_slice(&count.to_be_bytes());
    out.extend_from_slice(&entries);
    Some(out)
}

async fn sign_request(
    payload: &[u8],
    source: &Arc<dyn KeySource>,
    options: &AgentOptions,
    bucket: &SharedBucket,
) -> Option<Vec<u8>> {
    let mut reader = payload;
    let key_blob = get_string(&mut reader)?.to_vec();
    let data = get_string(&mut reader)?.to_vec();
    let flags = get_u32(&mut reader)?;
    if !reader.is_empty() {
        return None;
    }

    let identities = list(source).await?;
    let identity = identities
        .into_iter()
        .find(|i| i.public.to_bytes().is_ok_and(|b| b == key_blob))?;

    let label = identity.label.clone();
    let allowed = bucket
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .as_mut()
        .is_none_or(Bucket::take);
    if !allowed {
        tracing::info!(label = %label, "ssh-agent sign over the rate limit");
        if let Some(hook) = &options.on_sign {
            hook(&SignEvent {
                label,
                outcome: SignOutcome::Limited,
            });
        }
        return None;
    }
    let source = Arc::clone(source);
    let result = tokio::task::spawn_blocking(move || source.sign(&identity, &data, flags))
        .await
        .unwrap_or_else(|e| Err(SignError::Failed(format!("sign task: {e}"))));

    let outcome = match &result {
        Ok(_) => SignOutcome::Signed,
        Err(SignError::Refused(_)) => SignOutcome::Refused,
        Err(_) => SignOutcome::Failed,
    };
    if let Err(e) = &result {
        tracing::info!(label = %label, error = %e, "ssh-agent sign did not complete");
    }
    if let Some(hook) = &options.on_sign {
        hook(&SignEvent { label, outcome });
    }

    let blob = result.ok()?.encode_vec().ok()?;
    let mut out = vec![SSH_AGENT_SIGN_RESPONSE];
    put_string(&mut out, &blob);
    Some(out)
}

fn put_string(out: &mut Vec<u8>, bytes: &[u8]) {
    out.extend_from_slice(&(bytes.len() as u32).to_be_bytes());
    out.extend_from_slice(bytes);
}

fn get_u32(reader: &mut &[u8]) -> Option<u32> {
    let (head, rest) = reader.split_first_chunk::<4>()?;
    *reader = rest;
    Some(u32::from_be_bytes(*head))
}

fn get_string<'a>(reader: &mut &'a [u8]) -> Option<&'a [u8]> {
    let len = get_u32(reader)? as usize;
    if reader.len() < len {
        return None;
    }
    let (value, rest) = reader.split_at(len);
    *reader = rest;
    Some(value)
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::*;
    use signature::Verifier;
    use ssh_key::{Algorithm, EcdsaCurve, SshSig};
    use std::sync::Mutex;
    use std::sync::atomic::AtomicUsize;

    /// An in-memory key source. It keeps the private keys only because it
    /// is a test; the real source loads each key per sign.
    struct MemorySource {
        keys: Vec<(String, PrivateKey)>,
        refuse: Option<String>,
        signs: AtomicUsize,
    }

    impl KeySource for MemorySource {
        fn list(&self) -> Vec<Identity> {
            self.keys
                .iter()
                .map(|(label, key)| Identity {
                    label: label.clone(),
                    public: key.public_key().clone(),
                })
                .collect()
        }

        fn sign(
            &self,
            identity: &Identity,
            data: &[u8],
            flags: u32,
        ) -> Result<Signature, SignError> {
            self.signs.fetch_add(1, Ordering::SeqCst);
            if self.refuse.as_deref() == Some(identity.label.as_str()) {
                return Err(SignError::Refused("rate limit".into()));
            }
            let (_, key) = self
                .keys
                .iter()
                .find(|(l, _)| *l == identity.label)
                .ok_or_else(|| SignError::Refused("gone".into()))?;
            sign_with_key(key, data, flags)
        }
    }

    /// Ed25519, ECDSA P-256, and RSA keys. The RSA key comes from
    /// `ssh-keygen` when it exists (fast), else from the `rsa` crate.
    fn test_keys(dir: &Path) -> Vec<(String, PrivateKey)> {
        let mut rng = rand::rng();
        let ed = PrivateKey::random(&mut rng, Algorithm::Ed25519).unwrap();
        let ec = PrivateKey::random(
            &mut rng,
            Algorithm::Ecdsa {
                curve: EcdsaCurve::NistP256,
            },
        )
        .unwrap();
        let rsa = rsa_key(dir).unwrap_or_else(|| {
            PrivateKey::random(&mut rng, Algorithm::Rsa { hash: None }).unwrap()
        });
        vec![
            ("deploy-ed".to_string(), ed),
            ("deploy-ec".to_string(), ec),
            ("deploy-rsa".to_string(), rsa),
        ]
    }

    fn rsa_key(dir: &Path) -> Option<PrivateKey> {
        let file = dir.join("rsa_gen");
        let status = std::process::Command::new("ssh-keygen")
            .args(["-q", "-t", "rsa", "-b", "2048", "-N", "", "-C", "", "-f"])
            .arg(&file)
            .status()
            .ok()?;
        if !status.success() {
            return None;
        }
        let key = PrivateKey::read_openssh_file(&file).ok()?;
        let _ = std::fs::remove_file(&file);
        let _ = std::fs::remove_file(file.with_extension("pub"));
        Some(key)
    }

    struct Fixture {
        _dir: tempfile::TempDir,
        path: PathBuf,
        handle: AgentHandle,
        source: Arc<MemorySource>,
        events: Arc<Mutex<Vec<SignEvent>>>,
    }

    fn fixture(refuse: Option<&str>) -> Fixture {
        fixture_with(refuse, None)
    }

    fn fixture_with(refuse: Option<&str>, sign_limit: Option<SignLimit>) -> Fixture {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("agent.sock");
        let source = Arc::new(MemorySource {
            keys: test_keys(dir.path()),
            refuse: refuse.map(str::to_string),
            signs: AtomicUsize::new(0),
        });
        let events = Arc::new(Mutex::new(Vec::new()));
        let sink = Arc::clone(&events);
        let options = AgentOptions {
            on_sign: Some(Arc::new(move |e: &SignEvent| {
                sink.lock().unwrap().push(e.clone())
            })),
            sign_limit,
            ..AgentOptions::default()
        };
        let handle = serve(&path, Arc::clone(&source) as Arc<dyn KeySource>, options).unwrap();
        Fixture {
            _dir: dir,
            path,
            handle,
            source,
            events,
        }
    }

    /// A pure-Rust ssh-agent client.
    struct Client(UnixStream);

    impl Client {
        async fn connect(path: &Path) -> Self {
            Self(UnixStream::connect(path).await.unwrap())
        }

        async fn call(&mut self, body: &[u8]) -> Vec<u8> {
            let mut framed = (body.len() as u32).to_be_bytes().to_vec();
            framed.extend_from_slice(body);
            self.0.write_all(&framed).await.unwrap();
            let mut len = [0u8; 4];
            self.0.read_exact(&mut len).await.unwrap();
            let mut reply = vec![0u8; u32::from_be_bytes(len) as usize];
            self.0.read_exact(&mut reply).await.unwrap();
            reply
        }

        async fn identities(&mut self) -> Vec<(PublicKey, String)> {
            let reply = self.call(&[SSH_AGENTC_REQUEST_IDENTITIES]).await;
            assert_eq!(reply[0], SSH_AGENT_IDENTITIES_ANSWER);
            let mut r = &reply[1..];
            let n = get_u32(&mut r).unwrap();
            let mut out = Vec::new();
            for _ in 0..n {
                let blob = get_string(&mut r).unwrap();
                let comment = get_string(&mut r).unwrap();
                out.push((
                    PublicKey::from_bytes(blob).unwrap(),
                    String::from_utf8(comment.to_vec()).unwrap(),
                ));
            }
            assert!(r.is_empty());
            out
        }

        async fn sign(&mut self, key: &PublicKey, data: &[u8], flags: u32) -> Option<Signature> {
            let mut body = vec![SSH_AGENTC_SIGN_REQUEST];
            put_string(&mut body, &key.to_bytes().unwrap());
            put_string(&mut body, data);
            body.extend_from_slice(&flags.to_be_bytes());
            let reply = self.call(&body).await;
            if reply == [SSH_AGENT_FAILURE] {
                return None;
            }
            assert_eq!(reply[0], SSH_AGENT_SIGN_RESPONSE);
            let mut r = &reply[1..];
            let blob = get_string(&mut r).unwrap();
            assert!(r.is_empty());
            Some(Signature::try_from(blob).unwrap())
        }
    }

    fn public(f: &Fixture, label: &str) -> PublicKey {
        f.source
            .keys
            .iter()
            .find(|(l, _)| l == label)
            .unwrap()
            .1
            .public_key()
            .clone()
    }

    #[tokio::test]
    async fn socket_has_mode_0600() {
        let f = fixture(None);
        let meta = std::fs::metadata(&f.path).unwrap();
        assert!(meta.file_type().is_socket());
        assert_eq!(meta.permissions().mode() & 0o777, 0o600);
        assert_eq!(f.handle.path(), f.path);
    }

    #[tokio::test]
    async fn lists_identities_with_labels() {
        let f = fixture(None);
        let mut c = Client::connect(&f.path).await;
        let ids = c.identities().await;
        let labels: Vec<_> = ids.iter().map(|(_, l)| l.as_str()).collect();
        assert_eq!(labels, ["deploy-ed", "deploy-ec", "deploy-rsa"]);
        for (key, label) in &ids {
            assert_eq!(key.key_data(), public(&f, label).key_data());
        }
    }

    #[tokio::test]
    async fn signs_and_signatures_verify() {
        let f = fixture(None);
        let mut c = Client::connect(&f.path).await;
        let data = b"session id and userauth request";

        let ed = public(&f, "deploy-ed");
        let sig = c.sign(&ed, data, 0).await.unwrap();
        assert_eq!(sig.algorithm(), Algorithm::Ed25519);
        ed.key_data().verify(data, &sig).unwrap();
        assert!(ed.key_data().verify(b"other data", &sig).is_err());

        let ec = public(&f, "deploy-ec");
        let sig = c.sign(&ec, data, 0).await.unwrap();
        assert_eq!(sig.algorithm(), ec.algorithm());
        ec.key_data().verify(data, &sig).unwrap();

        let rsa = public(&f, "deploy-rsa");
        let sig = c.sign(&rsa, data, SSH_AGENT_RSA_SHA2_512).await.unwrap();
        assert_eq!(
            sig.algorithm(),
            Algorithm::Rsa {
                hash: Some(HashAlg::Sha512)
            }
        );
        rsa.key_data().verify(data, &sig).unwrap();
        let sig = c.sign(&rsa, data, SSH_AGENT_RSA_SHA2_256).await.unwrap();
        assert_eq!(
            sig.algorithm(),
            Algorithm::Rsa {
                hash: Some(HashAlg::Sha256)
            }
        );
        rsa.key_data().verify(data, &sig).unwrap();
        // ssh-rsa (SHA-1) is refused.
        assert!(c.sign(&rsa, data, 0).await.is_none());

        let events = f.events.lock().unwrap().clone();
        let summary: Vec<_> = events.iter().map(|e| (e.label.as_str(), e.ok())).collect();
        assert_eq!(
            summary,
            [
                ("deploy-ed", true),
                ("deploy-ec", true),
                ("deploy-rsa", true),
                ("deploy-rsa", true),
                ("deploy-rsa", false),
            ]
        );
        assert_eq!(events[4].outcome, SignOutcome::Failed);
    }

    #[tokio::test]
    async fn unknown_key_fails_without_calling_the_source() {
        let f = fixture(None);
        let mut c = Client::connect(&f.path).await;
        let other = PrivateKey::random(&mut rand::rng(), Algorithm::Ed25519).unwrap();
        assert!(c.sign(other.public_key(), b"x", 0).await.is_none());
        assert_eq!(f.source.signs.load(Ordering::SeqCst), 0);
        assert!(f.events.lock().unwrap().is_empty());
    }

    #[tokio::test]
    async fn source_refusal_is_a_failure_and_is_reported() {
        let f = fixture(Some("deploy-ed"));
        let mut c = Client::connect(&f.path).await;
        assert!(c.sign(&public(&f, "deploy-ed"), b"x", 0).await.is_none());
        let events = f.events.lock().unwrap().clone();
        assert_eq!(events.len(), 1);
        assert_eq!(events[0].label, "deploy-ed");
        assert_eq!(events[0].outcome, SignOutcome::Refused);
        assert!(!events[0].ok());
    }

    /// S10 (secret manager review, 2026-10-29): a sign over the rate limit
    /// answers SSH_AGENT_FAILURE without a call to the source, and the
    /// hook sees it as `Limited`.
    #[tokio::test]
    async fn signs_over_the_rate_limit_fail_without_the_source() {
        let limit = SignLimit {
            per_second: 1,
            burst: 3,
        };
        let f = fixture_with(None, Some(limit));
        let mut c = Client::connect(&f.path).await;
        let ed = public(&f, "deploy-ed");
        let mut good = 0;
        for i in 0..6u32 {
            if c.sign(&ed, &i.to_be_bytes(), 0).await.is_some() {
                good += 1;
            }
        }
        assert_eq!(good, 3);
        assert_eq!(f.source.signs.load(Ordering::SeqCst), 3);
        let events = f.events.lock().unwrap().clone();
        let limited = events
            .iter()
            .filter(|e| e.outcome == SignOutcome::Limited)
            .count();
        assert_eq!(limited, 3, "{events:?}");
        // The bucket fills again: one more token after a second.
        tokio::time::sleep(Duration::from_millis(1100)).await;
        assert!(c.sign(&ed, b"later", 0).await.is_some());
    }

    /// S13: the socket mode is set on the socket that this server bound,
    /// never through a symlink.
    #[test]
    fn chmod_private_does_not_follow_a_symlink() {
        let dir = tempfile::tempdir().unwrap();
        let target = dir.path().join("target");
        std::fs::write(&target, "x").unwrap();
        std::fs::set_permissions(&target, std::fs::Permissions::from_mode(0o644)).unwrap();
        let link = dir.path().join("link");
        std::os::unix::fs::symlink(&target, &link).unwrap();
        assert!(chmod_socket_nofollow(&link).is_err());
        let mode = std::fs::metadata(&target).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode, 0o644);
        // A regular file is not a socket either.
        assert!(chmod_socket_nofollow(&target).is_err());
        let sock = dir.path().join("s");
        let _l = std::os::unix::net::UnixListener::bind(&sock).unwrap();
        chmod_socket_nofollow(&sock).unwrap();
        let mode = std::fs::symlink_metadata(&sock)
            .unwrap()
            .permissions()
            .mode()
            & 0o777;
        assert_eq!(mode, 0o600);
    }

    #[tokio::test]
    async fn other_requests_answer_failure_and_keep_the_connection() {
        let f = fixture(None);
        let mut c = Client::connect(&f.path).await;
        let mut add = vec![17u8];
        put_string(&mut add, b"ssh-ed25519");
        let mut extension = vec![27u8];
        put_string(&mut extension, b"session-bind@openssh.com");
        let mut lock = vec![22u8];
        put_string(&mut lock, b"passphrase");
        for request in [
            add,                  // SSH_AGENTC_ADD_IDENTITY
            vec![18, 0, 0, 0, 0], // SSH_AGENTC_REMOVE_IDENTITY
            vec![19],             // SSH_AGENTC_REMOVE_ALL_IDENTITIES
            vec![20],             // SSH_AGENTC_ADD_SMARTCARD_KEY
            lock,                 // SSH_AGENTC_LOCK
            vec![23, 0, 0, 0, 0], // SSH_AGENTC_UNLOCK
            vec![25],             // SSH_AGENTC_ADD_ID_CONSTRAINED
            extension,            // SSH_AGENTC_EXTENSION
            vec![1],              // SSH1 request
            vec![200],            // unknown
            vec![11, 0],          // identities with trailing bytes
            vec![13, 0, 0, 0, 9], // truncated sign request
        ] {
            assert_eq!(c.call(&request).await, [SSH_AGENT_FAILURE], "{request:?}");
        }
        // The connection still works, and nothing changed.
        assert_eq!(c.identities().await.len(), 3);
    }

    #[tokio::test]
    async fn oversized_and_empty_messages_close_the_connection() {
        let f = fixture(None);
        for len in [(DEFAULT_MAX_MESSAGE_LEN + 1) as u32, 0] {
            let mut s = UnixStream::connect(&f.path).await.unwrap();
            s.write_all(&len.to_be_bytes()).await.unwrap();
            let mut buf = [0u8; 1];
            let n = s.read(&mut buf).await.unwrap_or(0);
            assert_eq!(n, 0, "len {len}: connection not closed");
        }
        // A message at the limit is read (and refused as an unknown type).
        let mut c = Client::connect(&f.path).await;
        let mut big = vec![0u8; DEFAULT_MAX_MESSAGE_LEN];
        big[0] = 99;
        assert_eq!(c.call(&big).await, [SSH_AGENT_FAILURE]);
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn many_connections_at_once() {
        let f = fixture(None);
        let ed = public(&f, "deploy-ed");
        let mut tasks = JoinSet::new();
        for i in 0..100u32 {
            let path = f.path.clone();
            let ed = ed.clone();
            tasks.spawn(async move {
                let mut c = Client::connect(&path).await;
                let data = i.to_be_bytes();
                let sig = c.sign(&ed, &data, 0).await.unwrap();
                ed.key_data().verify(&data, &sig).unwrap();
                assert_eq!(c.identities().await.len(), 3);
            });
        }
        while let Some(r) = tasks.join_next().await {
            r.unwrap();
        }
        assert_eq!(f.events.lock().unwrap().len(), 100);
    }

    #[tokio::test]
    async fn shutdown_closes_connections_and_removes_the_socket() {
        let f = fixture(None);
        let mut open = UnixStream::connect(&f.path).await.unwrap();
        let path = f.path.clone();
        f.handle.shutdown().await;
        assert!(!path.exists());
        assert!(UnixStream::connect(&path).await.is_err());
        let mut buf = [0u8; 1];
        assert_eq!(open.read(&mut buf).await.unwrap_or(0), 0);
    }

    #[tokio::test]
    async fn drop_removes_the_socket() {
        let f = fixture(None);
        let path = f.path.clone();
        drop(f.handle);
        assert!(!path.exists());
    }

    #[tokio::test]
    async fn replaces_a_stale_socket_but_not_a_file() {
        let dir = tempfile::tempdir().unwrap();
        let source: Arc<dyn KeySource> = Arc::new(MemorySource {
            keys: Vec::new(),
            refuse: None,
            signs: AtomicUsize::new(0),
        });
        let path = dir.path().join("s.sock");
        let stale = std::os::unix::net::UnixListener::bind(&path).unwrap();
        drop(stale);
        let handle = serve(&path, Arc::clone(&source), AgentOptions::default()).unwrap();
        let mut c = Client::connect(&path).await;
        assert!(c.identities().await.is_empty());
        drop(handle);

        let file = dir.path().join("plain");
        std::fs::write(&file, b"x").unwrap();
        let err = serve(&file, source, AgentOptions::default()).unwrap_err();
        assert_eq!(err.kind(), io::ErrorKind::AlreadyExists);
        assert_eq!(std::fs::read(&file).unwrap(), b"x");
    }

    fn have(bin: &str) -> bool {
        std::process::Command::new(bin)
            .arg("-?")
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .status()
            .is_ok()
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn openssh_ssh_add_lists_the_keys() {
        if !have("ssh-add") {
            eprintln!("skipped: no ssh-add");
            return;
        }
        let f = fixture(None);
        let out = tokio::process::Command::new("ssh-add")
            .arg("-L")
            .env("SSH_AUTH_SOCK", &f.path)
            .output()
            .await
            .unwrap();
        assert!(
            out.status.success(),
            "{}",
            String::from_utf8_lossy(&out.stderr)
        );
        let listed = String::from_utf8(out.stdout).unwrap();
        for (label, key) in &f.source.keys {
            let mut public = key.public_key().clone();
            public.set_comment(label.as_str());
            let line = public.to_openssh().unwrap();
            assert!(listed.lines().any(|l| l == line), "{line} not in {listed}");
        }

        // ssh-add refuses to add, remove, and lock: the agent says no.
        for args in [&["-D"][..]] {
            let out = tokio::process::Command::new("ssh-add")
                .args(args)
                .env("SSH_AUTH_SOCK", &f.path)
                .stdin(std::process::Stdio::null())
                .output()
                .await
                .unwrap();
            assert!(!out.status.success(), "ssh-add {args:?} succeeded");
        }
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn openssh_ssh_keygen_signs_through_the_agent() {
        if !have("ssh-keygen") {
            eprintln!("skipped: no ssh-keygen");
            return;
        }
        let f = fixture(None);
        let work = tempfile::tempdir().unwrap();
        let message = b"commit tree 0123456789abcdef\n";
        let data = work.path().join("data");
        std::fs::write(&data, message).unwrap();
        for label in ["deploy-ed", "deploy-ec", "deploy-rsa"] {
            let public = public(&f, label);
            let pubfile = work.path().join(format!("{label}.pub"));
            std::fs::write(&pubfile, public.to_openssh().unwrap()).unwrap();
            let out = tokio::process::Command::new("ssh-keygen")
                .args(["-Y", "sign", "-n", "git", "-f"])
                .arg(&pubfile)
                .arg(&data)
                .env("SSH_AUTH_SOCK", &f.path)
                .stdin(std::process::Stdio::null())
                .output()
                .await
                .unwrap();
            assert!(
                out.status.success(),
                "{label}: {}",
                String::from_utf8_lossy(&out.stderr)
            );
            let pem = std::fs::read_to_string(work.path().join("data.sig")).unwrap();
            let sig = SshSig::from_pem(&pem).unwrap();
            public.verify("git", message, &sig).unwrap();
            assert!(public.verify("git", b"tampered", &sig).is_err());
            std::fs::remove_file(work.path().join("data.sig")).unwrap();
        }
        let ok = f.events.lock().unwrap().iter().filter(|e| e.ok()).count();
        assert_eq!(ok, 3);
    }
}
