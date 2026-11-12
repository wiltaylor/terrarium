//! The git proxy (issue #11, PLAN §9 "The git proxy",
//! docs/security-review.md §17).
//!
//! A smart HTTP git server in the daemon, on its own loopback listener
//! (`TERRARIUM_GIT_PROXY_LISTEN`, default `127.0.0.1:8084`). The
//! `origin` of an agent checkout of a proxied repo is
//! `http://127.0.0.1:8084/git/<project>/<repo>.git`. The proxy:
//!
//! - authenticates the **agent**, never with the forge token: HTTP Basic
//!   with a proxy token that is an HMAC of the agent ULID under a key of
//!   the daemon (`<state>/git-proxy.key`). The agent's git gets it from
//!   the credential helper (`terrarium repo credential`, through the
//!   runner socket, so only that agent gets it). A deleted agent's token
//!   opens nothing.
//! - checks that the agent is in the repo's project, and that the repo
//!   goes through the proxy ([`push_via`]).
//! - for a push (`git-receive-pack`), reads the ref update commands
//!   ([`pkt`]) and refuses the **whole push** when one ref is a branch
//!   that matches `push_deny`, with a report that git shows for each ref.
//!   `--no-verify` skips only the client hook, not this check.
//! - forwards the rest to the forge with the repo's token from the secret
//!   manager: the request body and the answer stream through, with size
//!   caps and time limits, no redirect, and the address rules of the
//!   webhooks (`TERRARIUM_WEBHOOK_PRIVATE`, one DNS answer, pinned).
//!
//! The forge token never enters the sandbox: the credential helper does
//! not answer it for a proxied repo (the token is "use only" there,
//! security review S3).

pub mod pkt;

use std::collections::{HashMap, HashSet};
use std::net::{IpAddr, Ipv4Addr, Ipv6Addr, SocketAddr};
use std::path::Path;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex, RwLock};
use std::time::Duration;

use anyhow::Context;
use axum::Router;
use axum::body::{Body, Bytes};
use axum::extract::{DefaultBodyLimit, Path as UrlPath, RawQuery, State};
use axum::http::{HeaderMap, HeaderName, HeaderValue, StatusCode, header};
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use base64::Engine as _;
use futures_util::{Stream, StreamExt};
use hmac::{Hmac, KeyInit, Mac};
use serde_json::json;
use sha2::Sha256;
use terrarium_core::config::{PushVia, push_deny_match};
use tokio::task::JoinHandle;
use zeroize::Zeroizing;

use rusqlite::{Connection, OptionalExtension};

use crate::actor::AgentActor;
use crate::api::Shared;
use crate::error::{ApiError, ApiResult};
use crate::secrets::Secret;
use crate::store::repos::{self, Repo};

/// The key file in the state dir: 32 random bytes, hex, mode 0600.
pub const KEY_FILE: &str = "git-proxy.key";
/// The user name in the proxy credential. The proxy reads only the
/// password.
pub const USERNAME: &str = "terrarium";
/// The URL path prefix of the proxied repos.
pub const PREFIX: &str = "/git/";

/// The most bytes of a push body (the pack).
pub const MAX_PUSH: u64 = 2 << 30;
/// The most bytes of a fetch request body (the wants and haves).
pub const MAX_FETCH_REQUEST: u64 = 256 << 20;
/// The most bytes of an answer of the forge (a clone pack).
pub const MAX_ANSWER: u64 = 8 << 30;
/// The most bytes of the command list of a push.
const MAX_COMMANDS: usize = 4 << 20;
/// The connect time limit to the forge.
const CONNECT_TIMEOUT: Duration = Duration::from_secs(30);
/// The longest wait for the next bytes of the forge's answer.
const READ_TIMEOUT: Duration = Duration::from_secs(300);
/// The longest time of one request to the forge, the body included.
const TOTAL_TIMEOUT: Duration = Duration::from_secs(3600);
/// The longest wait for the command list (and the drain of a refused push)
/// from the agent.
const CLIENT_TIMEOUT: Duration = Duration::from_secs(120);
/// The most requests through the proxy at one time.
const MAX_ACTIVE: usize = 32;
/// The most requests of one agent at one time (review P6).
const MAX_PER_AGENT: usize = 4;
/// The longest wait for the next bytes of the agent's request body
/// (review P6).
const UPLOAD_IDLE: Duration = Duration::from_secs(60);

type HmacSha256 = Hmac<Sha256>;

/// The listener of a running proxy.
struct Live {
    /// `http://127.0.0.1:8084`: the base of each proxy URL.
    base: String,
    /// `127.0.0.1:8084`: the host that git sends to the credential helper.
    host: String,
    key: Zeroizing<Vec<u8>>,
}

/// The proxy in the daemon state: off until [`serve`] binds it.
#[derive(Default)]
pub struct Runtime {
    live: RwLock<Option<Live>>,
    active: Arc<AtomicUsize>,
    /// The requests of each agent now.
    per_agent: Arc<Mutex<HashMap<String, usize>>>,
}

impl std::fmt::Debug for Runtime {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("git_proxy::Runtime")
            .field("base", &self.base())
            .finish_non_exhaustive()
    }
}

impl Runtime {
    fn read(&self) -> std::sync::RwLockReadGuard<'_, Option<Live>> {
        match self.live.read() {
            Ok(g) => g,
            Err(p) => p.into_inner(),
        }
    }

    fn set(&self, live: Live) {
        let mut g = match self.live.write() {
            Ok(g) => g,
            Err(p) => p.into_inner(),
        };
        *g = Some(live);
    }

    /// The proxy listens.
    pub fn is_on(&self) -> bool {
        self.read().is_some()
    }

    /// `http://<addr>`, when the proxy listens.
    pub fn base(&self) -> Option<String> {
        self.read().as_ref().map(|l| l.base.clone())
    }

    /// The `origin` URL of a proxied repo.
    pub fn repo_url(&self, project: &str, repo: &str) -> Option<String> {
        self.base()
            .map(|b| format!("{b}{PREFIX}{project}/{repo}.git"))
    }

    /// The prefix of every proxy URL, `http://<addr>/git/`.
    pub fn url_prefix(&self) -> Option<String> {
        self.base().map(|b| format!("{b}{PREFIX}"))
    }

    /// Is `protocol://host` the proxy, as git names it to a credential
    /// helper?
    pub fn is_proxy_host(&self, protocol: &str, host: &str) -> bool {
        protocol == "http" && self.read().as_ref().is_some_and(|l| l.host == host)
    }

    /// The proxy token of an agent. `None` when the proxy is off.
    pub fn token_for(&self, agent: &str) -> Option<Zeroizing<String>> {
        let g = self.read();
        let live = g.as_ref()?;
        Some(Zeroizing::new(format!(
            "tgp.{agent}.{}",
            hex(&mac(&live.key, agent))
        )))
    }

    /// The agent ULID of a good proxy token.
    fn agent_of(&self, token: &str) -> Option<String> {
        let g = self.read();
        let live = g.as_ref()?;
        let rest = token.strip_prefix("tgp.")?;
        let (agent, sig) = rest.rsplit_once('.')?;
        if agent.is_empty() || agent.len() > 64 || !agent.bytes().all(|b| b.is_ascii_alphanumeric())
        {
            return None;
        }
        let sig = unhex(sig)?;
        let mut m = HmacSha256::new_from_slice(&live.key).ok()?;
        m.update(DOMAIN);
        m.update(agent.as_bytes());
        // Constant time.
        m.verify_slice(&sig).ok()?;
        Some(agent.to_owned())
    }

    /// Take a request slot of `agent`, or the reason why not: at most
    /// [`MAX_ACTIVE`] requests run, and at most [`MAX_PER_AGENT`] of one
    /// agent.
    fn slot(&self, agent: &str) -> Result<Slot, String> {
        {
            let mut m = lock(&self.per_agent);
            let n = m.entry(agent.to_owned()).or_default();
            if *n >= MAX_PER_AGENT {
                return Err(format!(
                    "the Terrarium git proxy runs {MAX_PER_AGENT} requests of this agent now; \
                     try again"
                ));
            }
            *n += 1;
        }
        let n = self.active.fetch_add(1, Ordering::SeqCst);
        let s = Slot {
            active: self.active.clone(),
            per_agent: self.per_agent.clone(),
            agent: agent.to_owned(),
        };
        if n >= MAX_ACTIVE {
            return Err(format!(
                "the Terrarium git proxy runs {MAX_ACTIVE} requests now; try again"
            ));
        }
        Ok(s)
    }
}

fn lock<T>(m: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    m.lock().unwrap_or_else(std::sync::PoisonError::into_inner)
}

/// One request through the proxy; the counts go down on drop.
struct Slot {
    active: Arc<AtomicUsize>,
    per_agent: Arc<Mutex<HashMap<String, usize>>>,
    agent: String,
}

impl Drop for Slot {
    fn drop(&mut self) {
        self.active.fetch_sub(1, Ordering::SeqCst);
        let mut m = lock(&self.per_agent);
        if let Some(n) = m.get_mut(&self.agent) {
            *n = n.saturating_sub(1);
            if *n == 0 {
                m.remove(&self.agent);
            }
        }
    }
}

/// The domain of the token MAC, so the key signs nothing else.
const DOMAIN: &[u8] = b"terrarium git proxy token v1\0";

fn mac(key: &[u8], agent: &str) -> Vec<u8> {
    match HmacSha256::new_from_slice(key) {
        Ok(mut m) => {
            m.update(DOMAIN);
            m.update(agent.as_bytes());
            m.finalize().into_bytes().to_vec()
        }
        Err(_) => Vec::new(),
    }
}

fn hex(b: &[u8]) -> String {
    b.iter().map(|x| format!("{x:02x}")).collect()
}

fn unhex(s: &str) -> Option<Vec<u8>> {
    if !s.len().is_multiple_of(2) || s.len() > 128 {
        return None;
    }
    (0..s.len())
        .step_by(2)
        .map(|i| u8::from_str_radix(s.get(i..i + 2)?, 16).ok())
        .collect()
}

/// Read the key file, or make it (32 random bytes, mode 0600).
fn load_or_create_key(state_dir: &Path) -> anyhow::Result<Zeroizing<Vec<u8>>> {
    use std::io::{Read as _, Write as _};
    use std::os::unix::fs::OpenOptionsExt;
    let path = state_dir.join(KEY_FILE);
    let read = |p: &Path| -> anyhow::Result<Zeroizing<Vec<u8>>> {
        let text = Zeroizing::new(std::fs::read_to_string(p)?);
        let key = unhex(text.trim())
            .filter(|k| k.len() == 32)
            .with_context(|| {
                format!(
                    "{} is not 64 hex digits; remove it to make a new key",
                    p.display()
                )
            })?;
        Ok(Zeroizing::new(key))
    };
    if path.exists() {
        return read(&path);
    }
    std::fs::create_dir_all(state_dir)?;
    let mut key = Zeroizing::new(vec![0u8; 32]);
    std::fs::File::open("/dev/urandom")?.read_exact(&mut key)?;
    match std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .open(&path)
    {
        Ok(mut f) => {
            f.write_all(Zeroizing::new(hex(&key)).as_bytes())?;
            f.sync_all()?;
            Ok(key)
        }
        // Another start made it first.
        Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => read(&path),
        Err(e) => Err(e.into()),
    }
}

/// Is `url` a URL that the proxy can forward to: `http` or `https`, a
/// host, no user or password, no query or fragment.
pub fn check_proxy_url(url: &str) -> Result<(), String> {
    let u = reqwest::Url::parse(url.trim())
        .map_err(|e| format!("`push_via = proxy` needs an http or https URL: {e}"))?;
    if !matches!(u.scheme(), "http" | "https") {
        return Err(format!(
            "`push_via = proxy` needs an http or https URL, not `{}`; the proxy does not \
             cover ssh or local repos",
            u.scheme()
        ));
    }
    if !u.username().is_empty() || u.password().is_some() {
        return Err(
            "`push_via = proxy`: the URL has a user or a password; put the token in \
                    the repo's credential"
                .into(),
        );
    }
    if u.query().is_some() || u.fragment().is_some() {
        return Err("`push_via = proxy`: the URL has a query or a fragment".into());
    }
    if u.host_str().is_none_or(str::is_empty) {
        return Err("`push_via = proxy`: the URL has no host".into());
    }
    Ok(())
}

/// How the agent checkouts of `repo` reach the forge. `proxy_on`: the
/// proxy listens.
///
/// - `push_via` set on the repo: that value. `proxy` with the proxy off
///   stays `proxy`: the checkout and the push fail closed.
/// - Not set: `proxy` for an `https` repo with a credential and a
///   `push_deny` list, when the proxy is on. Else `direct`. So a repo
///   with no `push_deny` keeps its behaviour from before the proxy.
pub fn push_via(repo: &Repo, proxy_on: bool) -> PushVia {
    match repo.push_via {
        Some(v) => v,
        None => {
            let https = repo
                .url
                .trim()
                .get(..8)
                .is_some_and(|s| s.eq_ignore_ascii_case("https://"));
            if proxy_on
                && https
                && repo.credential.is_some()
                && !repo.push_deny.is_empty()
                && check_proxy_url(&repo.url).is_ok()
            {
                PushVia::Proxy
            } else {
                PushVia::Direct
            }
        }
    }
}

/// Why a repo with `push_deny` is `direct`, so its `push_deny` is only
/// the client hook (review P8). `None`: the repo has no `push_deny`, or it
/// goes through the proxy.
pub fn direct_reason(repo: &Repo, proxy_on: bool) -> Option<String> {
    if repo.push_deny.is_empty() || push_via(repo, proxy_on) == PushVia::Proxy {
        return None;
    }
    let url = repo.url.trim();
    let why = if repo.push_via == Some(PushVia::Direct) {
        "push_via = direct"
    } else if !proxy_on {
        "the git proxy is off (TERRARIUM_GIT_PROXY_LISTEN)"
    } else if !url
        .get(..8)
        .is_some_and(|s| s.eq_ignore_ascii_case("https://"))
    {
        "the URL is not https (ssh, local, and http repos are not proxied by default)"
    } else if repo.credential.is_none() {
        "the repo has no credential"
    } else {
        "the URL is not one that the proxy takes (a user, a query, or a fragment)"
    };
    Some(format!(
        "push_deny is only the pre-push hook here, not a hard rule: {why}"
    ))
}

/// The view of a repo for the API: `via` is `proxy`, `direct`, or
/// `direct` with the reason of [`direct_reason`].
pub fn via_label(repo: &Repo, proxy_on: bool) -> String {
    match (push_via(repo, proxy_on), direct_reason(repo, proxy_on)) {
        (PushVia::Proxy, _) => "proxy".to_owned(),
        (PushVia::Direct, None) => "direct".to_owned(),
        (PushVia::Direct, Some(r)) => format!("direct ({r})"),
    }
}

/// The secret (`scope`, `name`) that a repo's credential names: the
/// project's secret of that name, else the global one (as
/// `crate::secrets::resolve`). `None`: no credential, or no such secret.
pub fn secret_of_repo(conn: &Connection, repo: &Repo) -> ApiResult<Option<(String, String)>> {
    let Some(name) = repo.credential.as_deref() else {
        return Ok(None);
    };
    let scope: Option<String> = conn
        .query_row(
            "SELECT scope FROM secrets WHERE name = ?1 AND scope IN (?2, 'global')
             ORDER BY scope = 'global' LIMIT 1",
            rusqlite::params![name, repo.project],
            |r| r.get(0),
        )
        .optional()?;
    Ok(scope.map(|sc| (sc, name.to_owned())))
}

/// The secrets that are "use only" (review P1): each secret that a repo
/// which goes through the proxy names, in any project. The credential
/// helper, the grants, `secret get`, and the launch env never give such a
/// secret to an agent, also for another repo or a grant: else a direct
/// repo with the same token opens the proxied repo past the proxy.
pub fn use_only_secrets(conn: &Connection, proxy_on: bool) -> ApiResult<HashSet<(String, String)>> {
    let mut out = HashSet::new();
    for r in repos::all(conn)? {
        if push_via(&r, proxy_on) == PushVia::Proxy
            && let Some(id) = secret_of_repo(conn, &r)?
        {
            out.insert(id);
        }
    }
    Ok(out)
}

/// Fill the API fields of repos (review P1, P8): `via`, and a warning
/// when `push_deny` is only the hook, or when the repo's secret is shared
/// between a proxied repo and a direct one (the direct repo then gets no
/// token from the credential helper).
pub fn annotate(conn: &Connection, repos: &mut [Repo], proxy_on: bool) -> ApiResult<()> {
    if repos.is_empty() {
        return Ok(());
    }
    let all = repos::all(conn)?;
    let mut users: HashMap<(String, String), Vec<(String, PushVia)>> = HashMap::new();
    for r in &all {
        if let Some(id) = secret_of_repo(conn, r)? {
            users
                .entry(id)
                .or_default()
                .push((format!("{}/{}", r.project, r.name), push_via(r, proxy_on)));
        }
    }
    for r in repos.iter_mut() {
        let via = push_via(r, proxy_on);
        r.via = Some(via_label(r, proxy_on));
        if let Some(why) = direct_reason(r, proxy_on) {
            r.warnings.push(why);
        }
        let Some(id) = secret_of_repo(conn, r)? else {
            continue;
        };
        let me = format!("{}/{}", r.project, r.name);
        let others = users.get(&id).map(Vec::as_slice).unwrap_or_default();
        let (scope, name) = &id;
        match via {
            PushVia::Proxy => {
                let direct: Vec<&str> = others
                    .iter()
                    .filter(|(n, v)| *n != me && *v == PushVia::Direct)
                    .map(|(n, _)| n.as_str())
                    .collect();
                if !direct.is_empty() {
                    r.warnings.push(format!(
                        "secret `{scope}/{name}` is shared with direct repos ({}): it is use only \
                         now, so their checkouts get no token from the credential helper; give \
                         them their own token, or let them go through the proxy",
                        direct.join(", ")
                    ));
                }
            }
            PushVia::Direct => {
                if others.iter().any(|(_, v)| *v == PushVia::Proxy) {
                    r.warnings.push(format!(
                        "secret `{scope}/{name}` is the token of a repo that goes through the git \
                         proxy: it is use only, so the credential helper gives no token for this \
                         repo"
                    ));
                }
            }
        }
    }
    Ok(())
}

/// The refusal of a use-only secret.
pub fn use_only_error(scope: &str, name: &str) -> ApiError {
    ApiError::conflict(format!(
        "secret `{scope}/{name}` is the token of a repo that goes through the Terrarium git \
         proxy, so it stays in the daemon (use only); give the other repos their own token, or \
         let them go through the proxy too"
    ))
}

/// The base URL for a bound address: an unspecified address is reached
/// on loopback.
fn base_of(addr: SocketAddr) -> (String, String) {
    let ip = match addr.ip() {
        IpAddr::V4(v) if v.is_unspecified() => IpAddr::V4(Ipv4Addr::LOCALHOST),
        IpAddr::V6(v) if v.is_unspecified() => IpAddr::V6(Ipv6Addr::LOCALHOST),
        ip => ip,
    };
    let host = SocketAddr::new(ip, addr.port()).to_string();
    (format!("http://{host}"), host)
}

/// Bind the proxy on `addr` and serve it.
pub async fn serve(state: Shared, addr: &str) -> anyhow::Result<(JoinHandle<()>, SocketAddr)> {
    let key = load_or_create_key(&state.state_dir)
        .with_context(|| format!("git proxy key {}", state.state_dir.join(KEY_FILE).display()))?;
    let listener = tokio::net::TcpListener::bind(addr)
        .await
        .with_context(|| format!("cannot bind the git proxy {addr}"))?;
    let local = listener.local_addr()?;
    if !local.ip().is_loopback() {
        tracing::warn!(
            addr = %local,
            "INSECURE: the git proxy listens on a non-loopback address over plain HTTP; the \
             agent proxy tokens can be read on the network. The sandboxes share the daemon's \
             network, so 127.0.0.1 is enough"
        );
    }
    let (base, host) = base_of(local);
    state.git_proxy.set(Live { base, host, key });
    state.webhooks.add_own_port(local.port());
    tracing::info!(addr = %local, "git proxy listening on TCP");
    let app = router(state);
    Ok((
        tokio::spawn(async move {
            if let Err(e) = axum::serve(listener, app).await {
                tracing::error!("git proxy failed: {e}");
            }
        }),
        local,
    ))
}

/// The routes of the proxy listener. Nothing else is served there.
pub fn router(state: Shared) -> Router {
    Router::new()
        .route("/git/{project}/{repo}/info/refs", get(info_refs))
        .route("/git/{project}/{repo}/git-upload-pack", post(upload_pack))
        .route("/git/{project}/{repo}/git-receive-pack", post(receive_pack))
        .fallback(|| async {
            text(
                StatusCode::NOT_FOUND,
                "not a path of the Terrarium git proxy (smart HTTP only)",
            )
        })
        // The caps of this module apply, not the API's body limit.
        .layer(DefaultBodyLimit::disable())
        .with_state(state)
}

/// A `text/plain` answer. git shows the text of an error answer as
/// `remote: ...` where it can.
fn text(status: StatusCode, msg: impl Into<String>) -> Response {
    let mut m: String = msg.into();
    m.push('\n');
    (
        status,
        [
            (header::CONTENT_TYPE, "text/plain; charset=utf-8"),
            (header::CACHE_CONTROL, "no-cache"),
        ],
        m,
    )
        .into_response()
}

fn unauthorized(msg: &str) -> Response {
    let mut r = text(StatusCode::UNAUTHORIZED, msg);
    r.headers_mut().insert(
        header::WWW_AUTHENTICATE,
        HeaderValue::from_static("Basic realm=\"Terrarium git proxy\""),
    );
    r
}

/// The password of a Basic `Authorization`.
fn basic_password(h: &HeaderMap) -> Option<Zeroizing<String>> {
    let v = h.get(header::AUTHORIZATION)?.to_str().ok()?;
    let (scheme, b64) = v.split_once(' ')?;
    if !scheme.eq_ignore_ascii_case("basic") {
        return None;
    }
    let raw = Zeroizing::new(
        base64::engine::general_purpose::STANDARD
            .decode(b64.trim())
            .ok()?,
    );
    let s = std::str::from_utf8(&raw).ok()?;
    let (_, pass) = s.split_once(':')?;
    Some(Zeroizing::new(pass.to_owned()))
}

/// One request: the agent, the repo, the forge credential, and the
/// checked forge address.
struct Ctx {
    agent: AgentActor,
    repo: Repo,
    /// `(secret name, user name, token)`.
    cred: Option<(String, String, Zeroizing<String>)>,
    /// The scope of the secret that `cred` came from (`global` or the
    /// project), for the message of a refusal (TER-39).
    cred_scope: Option<String>,
    base: reqwest::Url,
    addr: SocketAddr,
    _slot: Slot,
}

impl Ctx {
    /// Replace the token in a text for the agent or the log.
    fn scrub(&self, s: &str) -> String {
        match &self.cred {
            Some((name, _, t)) if !t.is_empty() => {
                s.replace(t.as_str(), &format!("[secret:{name}]"))
            }
            _ => s.to_owned(),
        }
    }

    fn label(&self) -> String {
        format!("{}/{}", self.repo.project, self.repo.name)
    }
}

/// Authenticate the agent and find the repo. An error is the answer.
async fn open(
    state: &Shared,
    headers: &HeaderMap,
    project: &str,
    repo: &str,
) -> Result<Ctx, Response> {
    let Some(pass) = basic_password(headers) else {
        return Err(unauthorized(
            "the Terrarium git proxy needs the agent's proxy credential \
             (credential.helper = !terrarium repo credential)",
        ));
    };
    let Some(agent_id) = state.git_proxy.agent_of(&pass) else {
        return Err(unauthorized("the proxy credential is not good"));
    };
    let key = agent_id.clone();
    let agent = match state
        .db
        .read(move |c| crate::store::agents::actor_for(c, &key))
        .await
    {
        Ok(Some(a)) if a.id == agent_id => a,
        Ok(_) => {
            return Err(unauthorized(
                "the agent of this proxy credential no longer exists",
            ));
        }
        Err(e) => return Err(text(StatusCode::INTERNAL_SERVER_ERROR, e.message)),
    };
    // The slot after the authentication: a stranger takes none.
    let slot = state
        .git_proxy
        .slot(&agent.id)
        .map_err(|m| text(StatusCode::SERVICE_UNAVAILABLE, m))?;
    if agent.project != project {
        tracing::warn!(agent = %agent.id, project, repo, "git proxy: an agent asked for a repo of another project");
        return Err(text(
            StatusCode::FORBIDDEN,
            format!(
                "agent `{}` is in project `{}`; the git proxy serves only the repos of its project",
                agent.name, agent.project
            ),
        ));
    }
    let name = repo.strip_suffix(".git").unwrap_or(repo).to_owned();
    let p = project.to_owned();
    let n = name.clone();
    let found = state
        .db
        .read(move |c| repos::find_by_name(c, &p, &n))
        .await
        .map_err(|e| text(StatusCode::INTERNAL_SERVER_ERROR, e.message))?;
    let Some(repo) = found else {
        return Err(text(
            StatusCode::NOT_FOUND,
            format!("project `{project}` has no repo `{name}`"),
        ));
    };
    if push_via(&repo, true) != PushVia::Proxy {
        return Err(text(
            StatusCode::NOT_FOUND,
            format!(
                "repo `{name}` does not go through the git proxy (push_via is direct); use its URL"
            ),
        ));
    }
    let base = check_proxy_url(&repo.url)
        .and_then(|()| reqwest::Url::parse(repo.url.trim()).map_err(|e| e.to_string()))
        .map_err(|e| text(StatusCode::BAD_GATEWAY, e))?;
    let cred = match crate::secrets::secret_of(state, repo.owner(), repo.credential.as_deref())
        .await
    {
        Ok(None) => None,
        Ok(Some((name, Secret::Token { username, token }))) => Some((name, username, token)),
        Ok(Some((name, other))) => {
            return Err(text(
                StatusCode::BAD_GATEWAY,
                format!(
                    "the credential `{name}` of repo `{}` is a {}; the git proxy needs a token or a \
                     github_token",
                    repo.name,
                    other.kind_label()
                ),
            ));
        }
        Err(e) => {
            return Err(text(
                StatusCode::BAD_GATEWAY,
                format!("the credential of repo `{}`: {}", repo.name, e.message),
            ));
        }
    };
    // Which secret the credential is: the project's of that name, else the
    // global one (the order of `crate::secrets::load`).
    let cred_scope = match &cred {
        Some(_) => {
            let r = repo.clone();
            state
                .db
                .read(move |c| secret_of_repo(c, &r))
                .await
                .ok()
                .flatten()
                .map(|(scope, _)| scope)
        }
        None => None,
    };
    let (mode, ports, resolver) = state.webhooks.net_rules();
    let addr = crate::webhooks::net::resolve(&base, mode, &ports, resolver)
        .await
        .map_err(|e| {
            text(
                StatusCode::BAD_GATEWAY,
                format!("the forge of repo `{}`: {e}", repo.name),
            )
        })?;
    Ok(Ctx {
        agent,
        repo,
        cred,
        cred_scope,
        base,
        addr,
        _slot: slot,
    })
}

/// The URL of `suffix` under the repo URL.
fn forge_url(base: &reqwest::Url, suffix: &str, query: Option<&str>) -> reqwest::Url {
    let mut u = base.clone();
    let path = format!("{}/{suffix}", u.path().trim_end_matches('/'));
    u.set_path(&path);
    u.set_query(query);
    u
}

/// The request headers that go to the forge as they are.
const FORWARD_REQUEST: &[HeaderName] = &[
    header::CONTENT_TYPE,
    header::ACCEPT,
    header::ACCEPT_ENCODING,
    header::CONTENT_LENGTH,
    header::USER_AGENT,
];

/// The answer headers that go back to the agent.
const FORWARD_ANSWER: &[HeaderName] = &[
    header::CONTENT_TYPE,
    header::CACHE_CONTROL,
    header::EXPIRES,
    header::PRAGMA,
    header::CONTENT_ENCODING,
];

/// A stream that fails past `cap` bytes.
fn capped<S, E>(
    s: S,
    cap: u64,
    what: &'static str,
) -> impl Stream<Item = Result<Bytes, std::io::Error>> + Send
where
    S: Stream<Item = Result<Bytes, E>> + Send,
    E: std::fmt::Display,
{
    let mut seen = 0u64;
    s.map(move |r| match r {
        Ok(b) => {
            seen += b.len() as u64;
            if seen > cap {
                Err(std::io::Error::other(format!(
                    "{what} is larger than {cap} bytes"
                )))
            } else {
                Ok(b)
            }
        }
        Err(e) => Err(std::io::Error::other(e.to_string())),
    })
}

/// A hint of a token for the log: its length and its last 4 characters
/// (none for a token of 12 characters or fewer). Never the token.
fn token_hint(token: &str) -> String {
    let n = token.chars().count();
    if n <= 12 {
        return format!("{n} chars");
    }
    let tail: String = token.chars().skip(n - 4).collect();
    format!("{n} chars, ends with {tail}")
}

/// The longest part of a forge's error body in the message.
const REASON_MAX: usize = 300;

/// The step of a forge request, from the content type that it wants.
fn step_of(want_type: &str) -> &'static str {
    match want_type {
        RECEIVE_PACK_AD => "on the push advertisement (info/refs?service=git-receive-pack)",
        "application/x-git-receive-pack-result" => "on the push (git-receive-pack)",
        "application/x-git-upload-pack-result" => "on the fetch (git-upload-pack)",
        _ => "on the fetch advertisement (info/refs)",
    }
}

/// `; the forge said: <text>` with the start of the error body of `resp`:
/// at most [`REASON_MAX`] characters of one line each, control characters
/// out, the token scrubbed. Empty when the body is empty or does not come
/// within a short time.
async fn forge_reason(ctx: &Ctx, resp: reqwest::Response) -> String {
    let mut raw = Vec::new();
    let mut stream = resp.bytes_stream();
    let read = async {
        while let Some(Ok(b)) = stream.next().await {
            raw.extend_from_slice(&b);
            if raw.len() >= 4096 {
                break;
            }
        }
    };
    let _ = tokio::time::timeout(Duration::from_secs(2), read).await;
    let text = String::from_utf8_lossy(&raw);
    let one: String = text
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ")
        .chars()
        .filter(|c| !c.is_control())
        .take(REASON_MAX)
        .collect();
    let one = ctx.scrub(one.trim());
    if one.is_empty() {
        String::new()
    } else {
        format!("; the forge said: {one}")
    }
}

/// Send one request to the forge and stream its answer back. `service`
/// names the content type that a good answer must have.
async fn forward(
    ctx: Ctx,
    method: reqwest::Method,
    url: reqwest::Url,
    headers: &HeaderMap,
    body: Option<(reqwest::Body, bool)>,
    want_type: &str,
) -> Response {
    let label = ctx.label();
    let mut b = reqwest::Client::builder()
        .connect_timeout(CONNECT_TIMEOUT)
        .read_timeout(READ_TIMEOUT)
        .timeout(TOTAL_TIMEOUT)
        .redirect(reqwest::redirect::Policy::none())
        .no_proxy();
    if let Some(url::Host::Domain(d)) = url.host() {
        b = b.resolve(d, SocketAddr::new(ctx.addr.ip(), 0));
    }
    let client = match b.build() {
        Ok(c) => c,
        Err(e) => {
            return text(
                StatusCode::INTERNAL_SERVER_ERROR,
                format!("cannot make the HTTP client: {e}"),
            );
        }
    };
    // The push advertisement is rewritten (no `push-options`), so it
    // must come plain, not compressed.
    let push_ad = want_type == RECEIVE_PACK_AD;
    let mut req = client.request(method, url.clone());
    for name in FORWARD_REQUEST {
        if push_ad && *name == header::ACCEPT_ENCODING {
            continue;
        }
        if let Some(v) = headers.get(name) {
            req = req.header(name, v);
        }
    }
    // A fetch may use protocol v2. A push stays v0: the proxy reads its
    // command list in that form, so the forge must not answer in another.
    if !want_type.contains("receive-pack")
        && let Some(v) = headers.get("git-protocol")
    {
        req = req.header("git-protocol", v);
    }
    if let Some((_, user, token)) = &ctx.cred {
        let raw = Zeroizing::new(format!("{user}:{}", token.as_str()));
        let enc = Zeroizing::new(format!(
            "Basic {}",
            base64::engine::general_purpose::STANDARD.encode(raw.as_bytes())
        ));
        match HeaderValue::from_str(&enc) {
            Ok(mut v) => {
                v.set_sensitive(true);
                req = req.header(header::AUTHORIZATION, v);
            }
            Err(_) => {
                return text(
                    StatusCode::BAD_GATEWAY,
                    "the repo token is not a valid header value",
                );
            }
        }
    }
    if let Some((body, pass_encoding)) = body {
        if pass_encoding && let Some(v) = headers.get(header::CONTENT_ENCODING) {
            req = req.header(header::CONTENT_ENCODING, v);
        }
        req = req.body(body);
    }
    let resp = match req.send().await {
        Ok(r) => r,
        Err(e) => {
            let why = if e.is_timeout() {
                "timed out".to_owned()
            } else {
                ctx.scrub(&source_text(&e))
            };
            tracing::warn!(repo = %label, agent = %ctx.agent.id, "git proxy: the forge request failed: {why}");
            return text(
                StatusCode::BAD_GATEWAY,
                format!("Terrarium git proxy: the forge of `{label}` did not answer: {why}"),
            );
        }
    };
    let status = resp.status();
    let ctype = resp
        .headers()
        .get(header::CONTENT_TYPE)
        .and_then(|v| v.to_str().ok())
        .unwrap_or("")
        .to_owned();
    if !status.is_success() {
        let what = if status.is_redirection() {
            format!(
                "a redirect ({}); the proxy follows no redirect, so give the repo its final URL",
                status.as_u16()
            )
        } else if status.as_u16() == 401 {
            "HTTP 401: the forge did not accept the repo's credential (a wrong or expired \
             token, or none)"
                .to_owned()
        } else if status.as_u16() == 403 {
            "HTTP 403: the forge accepted the repo's credential but refused this step (the \
             token has no right for it, for example no write access to the repo)"
                .to_owned()
        } else {
            format!("HTTP {}", status.as_u16())
        };
        // The forge says why in its body (GitHub: `Permission to o/r.git
        // denied to u.`, `Write access to repository not granted.`): a
        // short, scrubbed part of it goes along (TER-39).
        let said = forge_reason(&ctx, resp).await;
        let mut what = format!("{what} {}{said}", step_of(want_type));
        if matches!(status.as_u16(), 401 | 403) {
            // What went out, so that the next refusal can be checked
            // against the token that works: the secret and the user for
            // the agent; the URL and a hint of the token (its length and
            // last 4 characters, never the token) only in the log.
            let (sent, hint) = match &ctx.cred {
                Some((name, user, token)) => (
                    format!(
                        "the credential {}/{name} as user `{user}`",
                        ctx.cred_scope.as_deref().unwrap_or("?")
                    ),
                    token_hint(token),
                ),
                None => ("no credential".to_owned(), String::new()),
            };
            what.push_str(&format!(" (the proxy sent {sent})"));
            tracing::warn!(repo = %label, agent = %ctx.agent.id,
                url = %crate::webhooks::net::log_url(&url), token = %hint,
                "git proxy: the forge refused a request: {what}");
        } else {
            tracing::warn!(repo = %label, agent = %ctx.agent.id, url = %crate::webhooks::net::log_url(&url), "git proxy: the forge answered {what}");
        }
        return text(
            StatusCode::BAD_GATEWAY,
            format!("Terrarium git proxy: the forge of `{label}` answered {what}"),
        );
    }
    if !ctype.eq_ignore_ascii_case(want_type) {
        return text(
            StatusCode::BAD_GATEWAY,
            format!(
                "Terrarium git proxy: the forge of `{label}` does not speak smart HTTP (content \
                 type `{}`)",
                ctype.escape_default()
            ),
        );
    }
    if push_ad && resp.headers().contains_key(header::CONTENT_ENCODING) {
        return text(
            StatusCode::BAD_GATEWAY,
            format!("Terrarium git proxy: the forge of `{label}` sent a compressed advertisement"),
        );
    }
    let mut out = Response::builder().status(StatusCode::OK);
    for name in FORWARD_ANSWER {
        if let Some(v) = resp.headers().get(name) {
            out = out.header(name, v);
        }
    }
    // The slot lives until the answer is sent.
    let slot = ctx._slot;
    let stream = capped(resp.bytes_stream(), MAX_ANSWER, "the answer of the forge");
    let stream: std::pin::Pin<Box<dyn Stream<Item = Result<Bytes, std::io::Error>> + Send>> =
        if push_ad {
            Box::pin(no_push_options(stream))
        } else {
            Box::pin(stream)
        };
    let stream = stream.map(move |r| {
        let _ = &slot;
        r
    });
    out.body(Body::from_stream(stream))
        .unwrap_or_else(|e| text(StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))
}

/// The content type of the push advertisement.
const RECEIVE_PACK_AD: &str = "application/x-git-receive-pack-advertisement";
/// The most bytes before the first ref line of an advertisement.
const MAX_AD_HEAD: usize = 1 << 20;

/// The push advertisement with no `push-options` capability (review P3):
/// the first bytes are held until the first ref line is complete, then
/// rewritten ([`pkt::strip_push_options`]); the rest streams on.
fn no_push_options<S>(s: S) -> impl Stream<Item = Result<Bytes, std::io::Error>> + Send
where
    S: Stream<Item = Result<Bytes, std::io::Error>> + Send + 'static,
{
    // (stream, held bytes, the head is done)
    let s = Box::pin(s);
    futures_util::stream::unfold(Some((s, Vec::new(), false)), |state| async move {
        let (mut s, mut held, done) = state?;
        if done {
            return s.next().await.map(|item| (item, Some((s, held, true))));
        }
        loop {
            match pkt::strip_push_options(&held) {
                Some(Ok((head, used))) => {
                    let mut out = head;
                    out.extend_from_slice(&held[used..]);
                    held.clear();
                    return Some((Ok(Bytes::from(out)), Some((s, held, true))));
                }
                Some(Err(m)) => return Some((Err(std::io::Error::other(m)), None)),
                None => {}
            }
            if held.len() > MAX_AD_HEAD {
                let m = "the forge's ref advertisement has no ref line in its first MiB";
                return Some((Err(std::io::Error::other(m)), None));
            }
            match s.next().await {
                Some(Ok(b)) => held.extend_from_slice(&b),
                Some(Err(e)) => return Some((Err(e), None)),
                // The end: what is held goes as it is (a short answer).
                None => {
                    if held.is_empty() {
                        return None;
                    }
                    let out = std::mem::take(&mut held);
                    return Some((Ok(Bytes::from(out)), Some((s, held, true))));
                }
            }
        }
    })
}

/// The deepest cause of a reqwest error, with no URL.
fn source_text(e: &reqwest::Error) -> String {
    let mut s: &dyn std::error::Error = e;
    while let Some(next) = s.source() {
        s = next;
    }
    s.to_string()
}

/// The `service` of an `info/refs` query.
fn service_of(query: Option<&str>) -> Option<&'static str> {
    let q = query?;
    q.split('&').find_map(|kv| match kv {
        "service=git-upload-pack" => Some("git-upload-pack"),
        "service=git-receive-pack" => Some("git-receive-pack"),
        _ => None,
    })
}

async fn info_refs(
    State(state): State<Shared>,
    UrlPath((project, repo)): UrlPath<(String, String)>,
    RawQuery(query): RawQuery,
    headers: HeaderMap,
) -> Response {
    let Some(service) = service_of(query.as_deref()) else {
        return text(
            StatusCode::FORBIDDEN,
            "the Terrarium git proxy speaks only smart HTTP: ?service=git-upload-pack or \
             git-receive-pack",
        );
    };
    let ctx = match open(&state, &headers, &project, &repo).await {
        Ok(c) => c,
        Err(r) => return r,
    };
    let url = forge_url(&ctx.base, "info/refs", Some(&format!("service={service}")));
    let want = format!("application/x-{service}-advertisement");
    forward(ctx, reqwest::Method::GET, url, &headers, None, &want).await
}

async fn upload_pack(
    State(state): State<Shared>,
    UrlPath((project, repo)): UrlPath<(String, String)>,
    headers: HeaderMap,
    body: Body,
) -> Response {
    let ctx = match open(&state, &headers, &project, &repo).await {
        Ok(c) => c,
        Err(r) => return r,
    };
    let url = forge_url(&ctx.base, "git-upload-pack", None);
    let stream = capped(
        idle_limited(body.into_data_stream()),
        MAX_FETCH_REQUEST,
        "the fetch request",
    );
    let body = reqwest::Body::wrap_stream(stream);
    // A fetch request may be gzip: it goes on as it is, with its
    // Content-Encoding.
    forward(
        ctx,
        reqwest::Method::POST,
        url,
        &headers,
        Some((body, true)),
        "application/x-git-upload-pack-result",
    )
    .await
}

async fn provenance_warning(state: &Shared, ctx: &Ctx, commands: &[pkt::Command]) {
    let repo = ctx.repo.id.clone();
    let project = ctx.repo.project.clone();
    let agent = ctx.agent.id.clone();
    let refs: Vec<_> = commands.iter().map(|c| c.refname.clone()).collect();
    if let Err(e) = state.db.write("system", move |t| {
        use crate::store::{agents, board, mail};
        t.audit("git.provenance_warning", &repo, Some(&project), json!({"repo": repo, "agent": agent, "refs": refs}))?;
        let body = format!("> Git provenance warning: agent {agent} pushed refs {} to repo {repo} with provenance off and no managed hook check. The proxy did not inspect commit contents. Check the outgoing identity and trailers.", refs.join(", "));
        let view = agents::get(t, &agent)?;
        if let Some(card) = view.current_card {
            board::system_comment(t, &card, &body, None)?;
        } else {
            mail::send(t, mail::Outgoing {
                from: &mail::Address::System,
                to: &mail::Address::Project(project),
                subject: "Git provenance warning",
                body: &body,
                thread: None,
                in_reply_to: None,
            })?;
        }
        Ok(())
    }).await {
        tracing::warn!("cannot record git provenance warning: {e}");
    }
}

/// Read the body until the command list is complete.
async fn read_commands(
    stream: &mut (impl Stream<Item = Result<Bytes, axum::Error>> + Unpin),
) -> Result<(Vec<u8>, pkt::Commands), Response> {
    let mut buf: Vec<u8> = Vec::new();
    // One parser for all the chunks: each chunk costs its own bytes only
    // (review P2).
    let mut parser = pkt::Parser::new();
    let read = async {
        loop {
            match parser.feed(&buf) {
                pkt::Parse::Done(c) => return Ok(c),
                pkt::Parse::Bad(m) => {
                    return Err(text(
                        StatusCode::BAD_REQUEST,
                        format!("Terrarium git proxy: {m}"),
                    ));
                }
                pkt::Parse::More => {}
            }
            if buf.len() > MAX_COMMANDS {
                return Err(text(
                    StatusCode::PAYLOAD_TOO_LARGE,
                    format!(
                        "Terrarium git proxy: the command list of the push is larger than {MAX_COMMANDS} bytes"
                    ),
                ));
            }
            match stream.next().await {
                Some(Ok(b)) => buf.extend_from_slice(&b),
                Some(Err(e)) => {
                    return Err(text(
                        StatusCode::BAD_REQUEST,
                        format!("Terrarium git proxy: reading the push failed: {e}"),
                    ));
                }
                None => {
                    return Err(text(
                        StatusCode::BAD_REQUEST,
                        "Terrarium git proxy: the push request ended early",
                    ));
                }
            }
        }
    };
    match tokio::time::timeout(CLIENT_TIMEOUT, read).await {
        Ok(Ok(c)) => Ok((buf, c)),
        Ok(Err(r)) => Err(r),
        Err(_) => Err(text(
            StatusCode::REQUEST_TIMEOUT,
            "Terrarium git proxy: the push request was too slow",
        )),
    }
}

/// Read and drop the rest of a refused push, so git reads the answer and
/// not a broken connection. At most [`MAX_PUSH`] bytes and
/// [`CLIENT_TIMEOUT`].
async fn drain(
    stream: &mut (impl Stream<Item = Result<Bytes, axum::Error>> + Unpin),
    mut seen: u64,
) {
    let _ = tokio::time::timeout(CLIENT_TIMEOUT, async {
        while let Some(Ok(b)) = stream.next().await {
            seen += b.len() as u64;
            if seen > MAX_PUSH {
                break;
            }
        }
    })
    .await;
}

async fn receive_pack(
    State(state): State<Shared>,
    UrlPath((project, repo)): UrlPath<(String, String)>,
    headers: HeaderMap,
    body: Body,
) -> Response {
    let ctx = match open(&state, &headers, &project, &repo).await {
        Ok(c) => c,
        Err(r) => return r,
    };
    if headers
        .get(header::CONTENT_ENCODING)
        .and_then(|v| v.to_str().ok())
        .is_some_and(|v| !v.eq_ignore_ascii_case("identity"))
    {
        return text(
            StatusCode::UNSUPPORTED_MEDIA_TYPE,
            "Terrarium git proxy: a compressed push body is not supported",
        );
    }
    let mut stream = body.into_data_stream();
    let (prefix, cmds) = match read_commands(&mut stream).await {
        Ok(v) => v,
        Err(r) => return r,
    };
    let label = ctx.label();
    let denied: Vec<(usize, String)> = cmds
        .commands
        .iter()
        .enumerate()
        .filter_map(|(i, c)| deny_reason(&ctx.repo, &c.refname).map(|why| (i, why)))
        .collect();
    let refs: Vec<serde_json::Value> = cmds
        .commands
        .iter()
        .map(|c| json!({"ref": c.refname, "old": c.old, "new": c.new}))
        .collect();
    if !denied.is_empty() {
        drain(&mut stream, prefix.len() as u64).await;
        let reasons: Vec<String> = cmds
            .commands
            .iter()
            .enumerate()
            .map(|(i, _)| match denied.iter().find(|(j, _)| *j == i) {
                Some((_, why)) => why.clone(),
                None => "not pushed: another ref of this push is denied by push_deny".to_owned(),
            })
            .collect();
        let names: Vec<&str> = denied
            .iter()
            .map(|(i, _)| cmds.commands[*i].refname.as_str())
            .collect();
        let message = format!(
            "Terrarium: push_deny of repo {} refuses {}; the push changed no ref",
            ctx.repo.name,
            names.join(", ")
        );
        tracing::info!(repo = %label, agent = %ctx.agent.id, refs = ?names, "git proxy: refused a push (push_deny)");
        audit(
            &state,
            &ctx,
            json!({
                "agent": ctx.agent.id, "refs": refs, "result": "refused",
                "denied": names, "via": "proxy",
            }),
        )
        .await;
        return match pkt::refusal(&cmds, &reasons, &message) {
            Some(bytes) => (
                StatusCode::OK,
                [
                    (
                        header::CONTENT_TYPE,
                        "application/x-git-receive-pack-result",
                    ),
                    (header::CACHE_CONTROL, "no-cache"),
                ],
                bytes,
            )
                .into_response(),
            None => text(StatusCode::FORBIDDEN, message),
        };
    }
    if !cmds.commands.is_empty() {
        tracing::info!(repo = %label, agent = %ctx.agent.id, refs = cmds.commands.len(), "git proxy: push");
        audit(
            &state,
            &ctx,
            json!({
                "agent": ctx.agent.id, "refs": refs, "result": "forwarded", "via": "proxy",
            }),
        )
        .await;
    }
    let first =
        futures_util::stream::once(async move { Ok::<Bytes, axum::Error>(Bytes::from(prefix)) });
    let all = capped(first.chain(idle_limited(stream)), MAX_PUSH, "the push");
    let url = forge_url(&ctx.base, "git-receive-pack", None);
    let repo_for_settings = ctx.repo.clone();
    let enabled = match state.db.read(move |c| Ok(crate::git_identity::effective(c, &repo_for_settings)?.0)).await {
        Ok(enabled) => enabled,
        Err(_) => return text(StatusCode::INTERNAL_SERVER_ERROR, "cannot read git provenance settings"),
    };
    let checked = crate::git_identity::take_push_marker(&ctx.repo.id, &ctx.agent.id,
        cmds.commands.iter().map(|c| format!("{} {} {}", c.refname, c.old, c.new)).collect());
    let warn = !enabled && !checked && !cmds.commands.is_empty();
    let sideband = cmds.has_cap("side-band-64k") || cmds.has_cap("side-band");
    if warn {
        provenance_warning(&state, &ctx, &cmds.commands).await;
    }
    let mut response = forward(
        ctx,
        reqwest::Method::POST,
        url,
        &headers,
        Some((reqwest::Body::wrap_stream(all), false)),
        "application/x-git-receive-pack-result",
    )
    .await;
    if warn && sideband && response.status().is_success() && !response.headers().contains_key(header::CONTENT_ENCODING) {
        let line = "\x02Terrarium: provenance is off and no managed hook check was recorded; check outgoing identity and trailers.\n";
        let prefix = Bytes::from(format!("{:04x}{line}", line.len() + 4));
        response.headers_mut().remove(header::CONTENT_LENGTH);
        let (parts, body) = response.into_parts();
        let first = futures_util::stream::once(async move { Ok::<Bytes, axum::Error>(prefix) });
        return Response::from_parts(parts, Body::from_stream(first.chain(body.into_data_stream())));
    }
    response
}

/// Why the push of `refname` is refused by the `push_deny` of `repo`, or
/// `None`. A repo with no `push_deny` refuses nothing. With it: a branch
/// that matches a glob; a review ref `refs/for/<b>[%...]` whose `<b>`
/// matches (Gerrit `%submit` lands on `<b>`); and any ref outside
/// `refs/heads/`, `refs/tags/`, and `refs/for/` (review P3).
fn deny_reason(repo: &Repo, refname: &str) -> Option<String> {
    if repo.push_deny.is_empty() {
        return None;
    }
    let name = &repo.name;
    match pkt::target(refname) {
        pkt::Target::Branch(b) => push_deny_match(&repo.push_deny, b)
            .map(|g| format!("denied by push_deny `{g}` of repo {name}")),
        pkt::Target::ReviewOf(b) => push_deny_match(&repo.push_deny, b).map(|g| {
            format!("denied by push_deny `{g}` of repo {name} (a review ref for branch {b})")
        }),
        pkt::Target::Tag => None,
        pkt::Target::Other => Some(format!(
            "denied: repo {name} has push_deny, and the proxy takes only refs/heads/, \
             refs/tags/, and refs/for/ refs"
        )),
    }
}

/// The agent's request body with an idle limit: no bytes for
/// [`UPLOAD_IDLE`] ends it with an error (review P6).
fn idle_limited<S>(s: S) -> impl Stream<Item = Result<Bytes, axum::Error>> + Send
where
    S: Stream<Item = Result<Bytes, axum::Error>> + Send + Unpin,
{
    futures_util::stream::unfold(Some(s), |state| async move {
        let mut s = state?;
        match tokio::time::timeout(UPLOAD_IDLE, s.next()).await {
            Ok(Some(item)) => Some((item, Some(s))),
            Ok(None) => None,
            Err(_) => Some((
                Err(axum::Error::new(std::io::Error::other(format!(
                    "no bytes from the agent for {} s",
                    UPLOAD_IDLE.as_secs()
                )))),
                None,
            )),
        }
    })
}

/// A `repo.push` audit row. Best effort.
async fn audit(state: &Shared, ctx: &Ctx, detail: serde_json::Value) {
    let target = ctx.label();
    let project = ctx.repo.project.clone();
    let actor = format!("agent:{}", ctx.agent.id);
    if let Err(e) = state
        .db
        .write(actor, move |t| {
            t.audit("repo.push", &target, Some(&project), detail)
        })
        .await
    {
        tracing::warn!("cannot write the repo.push audit row: {e}");
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::*;

    /// TER-39: the log hint of a token never holds more than its last 4
    /// characters.
    #[test]
    fn a_token_hint_shows_only_the_length_and_the_end() {
        let t = "ghp_abcdefghijklmnopqrstuvwxyz0123456789";
        let h = token_hint(t);
        assert_eq!(h, "40 chars, ends with 6789");
        assert!(!h.contains("ghp_"));
        assert_eq!(token_hint("short-token"), "11 chars");
        assert_eq!(token_hint(""), "0 chars");
    }

    fn repo(url: &str, cred: bool, deny: &[&str], via: Option<PushVia>) -> Repo {
        Repo {
            id: "r".into(),
            project: "p".into(),
            name: "app".into(),
            url: url.into(),
            default_branch: None,
            credential: cred.then(|| "tok".to_owned()),
            credential_admin: true,
            path: "app".into(),
            hooks_dir: None,
            push_deny: deny.iter().map(|s| (*s).to_owned()).collect(),
            push_via: via,
            via: None,
            warnings: Vec::new(),
            source: "api".into(),
            created_at: String::new(),
        }
    }

    #[test]
    fn the_default_rule() {
        let https = "https://git.example.com/a/app.git";
        assert_eq!(
            push_via(&repo(https, true, &["op/*"], None), true),
            PushVia::Proxy
        );
        // The proxy off, no token, no push_deny, or not https: direct.
        assert_eq!(
            push_via(&repo(https, true, &["op/*"], None), false),
            PushVia::Direct
        );
        assert_eq!(
            push_via(&repo(https, false, &["op/*"], None), true),
            PushVia::Direct
        );
        assert_eq!(
            push_via(&repo(https, true, &[], None), true),
            PushVia::Direct
        );
        for url in [
            "git@github.com:a/app.git",
            "ssh://git@x/a.git",
            "file:///repos/a.git",
            "http://x/a.git",
        ] {
            assert_eq!(
                push_via(&repo(url, true, &["op/*"], None), true),
                PushVia::Direct,
                "{url}"
            );
        }
        // An explicit value wins, also with the proxy off.
        assert_eq!(
            push_via(&repo(https, true, &["op/*"], Some(PushVia::Direct)), true),
            PushVia::Direct
        );
        assert_eq!(
            push_via(&repo(https, false, &[], Some(PushVia::Proxy)), false),
            PushVia::Proxy
        );
    }

    #[test]
    fn proxy_urls() {
        assert!(check_proxy_url("https://github.com/a/b.git").is_ok());
        assert!(check_proxy_url("http://10.0.0.5:3000/a/b").is_ok());
        for bad in [
            "git@github.com:a/b.git",
            "ssh://x/y",
            "file:///repos/a.git",
            "/repos/a.git",
            "https://u:p@x/a.git",
            "https://x/a.git?x=1",
            "https://x/a.git#f",
        ] {
            assert!(check_proxy_url(bad).is_err(), "{bad}");
        }
        let base = reqwest::Url::parse("https://x/a/b.git/").unwrap();
        assert_eq!(
            forge_url(&base, "info/refs", Some("service=git-upload-pack")).as_str(),
            "https://x/a/b.git/info/refs?service=git-upload-pack"
        );
        assert_eq!(
            base_of("0.0.0.0:8084".parse().unwrap()).0,
            "http://127.0.0.1:8084"
        );
        assert_eq!(base_of("[::]:1".parse().unwrap()).1, "[::1]:1");
        assert_eq!(
            service_of(Some("service=git-receive-pack")),
            Some("git-receive-pack")
        );
        assert_eq!(service_of(Some("service=git-upload-archive")), None);
        assert_eq!(service_of(None), None);
    }

    #[test]
    fn tokens_bind_the_agent_and_the_key() {
        let rt = Runtime::default();
        assert!(rt.token_for("01AGENT").is_none());
        rt.set(Live {
            base: "http://127.0.0.1:1".into(),
            host: "127.0.0.1:1".into(),
            key: Zeroizing::new(vec![7; 32]),
        });
        let t = rt.token_for("01AGENT").unwrap();
        assert_eq!(rt.agent_of(&t), Some("01AGENT".to_owned()));
        // Another agent's name with this MAC, a cut MAC, junk: refused.
        let forged = t.replace("01AGENT", "01OTHER");
        assert_eq!(rt.agent_of(&forged), None);
        assert_eq!(rt.agent_of(&t[..t.len() - 2]), None);
        assert_eq!(rt.agent_of("tgp..00"), None);
        assert_eq!(rt.agent_of("nonsense"), None);
        // A new key: the old token opens nothing.
        rt.set(Live {
            base: "http://127.0.0.1:1".into(),
            host: "127.0.0.1:1".into(),
            key: Zeroizing::new(vec![8; 32]),
        });
        assert_eq!(rt.agent_of(&t), None);
        assert!(rt.is_proxy_host("http", "127.0.0.1:1"));
        assert!(!rt.is_proxy_host("https", "127.0.0.1:1"));
        assert!(!rt.is_proxy_host("http", "127.0.0.1:2"));
    }

    #[test]
    fn the_key_file_is_private_and_stable() {
        use std::os::unix::fs::PermissionsExt;
        let d = tempfile::tempdir().unwrap();
        let a = load_or_create_key(d.path()).unwrap();
        let b = load_or_create_key(d.path()).unwrap();
        assert_eq!(*a, *b);
        let mode = std::fs::metadata(d.path().join(KEY_FILE))
            .unwrap()
            .permissions()
            .mode();
        assert_eq!(mode & 0o777, 0o600);
        std::fs::write(d.path().join(KEY_FILE), "short").unwrap();
        assert!(load_or_create_key(d.path()).is_err());
    }

    #[test]
    fn basic_auth_reads_the_password() {
        let mut h = HeaderMap::new();
        let v = base64::engine::general_purpose::STANDARD.encode("terrarium:tgp.X.ab");
        h.insert(header::AUTHORIZATION, format!("Basic {v}").parse().unwrap());
        assert_eq!(basic_password(&h).unwrap().as_str(), "tgp.X.ab");
        h.insert(header::AUTHORIZATION, "Bearer x".parse().unwrap());
        assert!(basic_password(&h).is_none());
    }
}
