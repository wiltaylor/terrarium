//! The HTTP API. The same router serves the Unix socket (no token) and the
//! TCP listener (bearer token on `/api/*`). `docs/api.md` is the contract.

mod agents;
mod artifacts;
mod board;
mod fleet;
mod hosts;
mod longpoll;
mod mail;
mod memory;
mod projects;
mod repos;
pub mod session;
mod streams;
mod subscriptions;
mod system;
mod targets;
mod usage;
mod versions;
mod wiki;

use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::sync::{Arc, RwLock};
use std::time::Instant;

use anyhow::Context;
use axum::extract::{FromRequest, FromRequestParts, Request, State};
use axum::http::request::Parts;
use axum::http::{HeaderValue, header};
use axum::middleware::{self, Next};
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use axum::{Extension, Json, Router};
use serde::Serialize;
use serde::de::DeserializeOwned;
use terrarium_core::config::Definitions;
use tokio::task::JoinHandle;
use tracing::{error, info};

pub use artifacts::{artifact_router, run_prune};
pub use system::{DefinitionCounts, Health};

use crate::actor::{Transport, resolve_actor};
use crate::db::Db;
use crate::error::{ApiError, Code};
use crate::store::Paged;
use crate::supervisor::Supervisor;

/// The header that carries the cursor of the next page of a list.
pub const NEXT_CURSOR: &str = "x-next-cursor";

/// Shared daemon state.
pub struct AppState {
    pub started: Instant,
    pub started_at: chrono::DateTime<chrono::Utc>,
    pub db: Db,
    /// The definitions from the config dir. `POST /api/reload` replaces them.
    pub definitions: RwLock<Definitions>,
    pub supervisor: Arc<dyn Supervisor>,
    pub state_dir: PathBuf,
    pub config_dir: PathBuf,
    /// Trust `X-Terrarium-Agent` on the Unix socket. Tests and development
    /// only: any local caller could act as any agent.
    pub insecure_agent_header: bool,
    /// Usage accounting, quota probes, and login sessions (PLAN §16).
    pub usage: Arc<crate::usage_collector::UsageRuntime>,
    /// Host agents, exec sessions, and target routing (PLAN §19).
    pub executor: Arc<crate::executor::Executor>,
    /// The origins, WebSocket auth, and sessions (`DaemonConfig::web`).
    pub web: crate::settings::WebConfig,
    /// The artifact listener address. Empty: no listener of its own, and
    /// the dashboard port serves `/a/...`.
    pub artifact_listen: String,
}

impl AppState {
    pub fn definitions(&self) -> Definitions {
        match self.definitions.read() {
            Ok(d) => d.clone(),
            Err(poisoned) => poisoned.into_inner().clone(),
        }
    }

    pub fn set_definitions(&self, defs: Definitions) {
        match self.definitions.write() {
            Ok(mut d) => *d = defs,
            Err(poisoned) => *poisoned.into_inner() = defs,
        }
    }
}

pub type Shared = Arc<AppState>;

/// A JSON body. A bad body is a `bad_request` in the API error shape.
pub struct ApiJson<T>(pub T);

impl<S, T> FromRequest<S> for ApiJson<T>
where
    T: DeserializeOwned,
    S: Send + Sync,
{
    type Rejection = ApiError;

    async fn from_request(req: Request, state: &S) -> Result<Self, Self::Rejection> {
        match Json::<T>::from_request(req, state).await {
            Ok(Json(v)) => Ok(Self(v)),
            Err(e) => Err(ApiError::bad_request(e.body_text())),
        }
    }
}

/// A query string. A bad query is a `bad_request` in the API error shape.
pub struct ApiQuery<T>(pub T);

impl<S, T> FromRequestParts<S> for ApiQuery<T>
where
    T: DeserializeOwned,
    S: Send + Sync,
{
    type Rejection = ApiError;

    async fn from_request_parts(parts: &mut Parts, state: &S) -> Result<Self, Self::Rejection> {
        match axum::extract::Query::<T>::from_request_parts(parts, state).await {
            Ok(axum::extract::Query(v)) => Ok(Self(v)),
            Err(e) => Err(ApiError::bad_request(e.body_text())),
        }
    }
}

/// A list answer: a JSON array, and `X-Next-Cursor` when there is more.
pub fn paged<T: Serialize>(p: Paged<T>) -> Response {
    let mut resp = Json(p.items).into_response();
    if let Some(next) = p.next
        && let Ok(v) = HeaderValue::from_str(&next)
    {
        resp.headers_mut().insert(NEXT_CURSOR, v);
    }
    resp
}

async fn api_not_found() -> ApiError {
    ApiError::not_found("no such API route")
}

async fn api_method_not_allowed() -> ApiError {
    ApiError::new(
        Code::MethodNotAllowed,
        "this route does not take this method",
    )
}

/// The routes, without auth or transport, with the artifact serving routes
/// (`/a/...`) and `/hosts/connect`. The Unix socket and the agent RPC use
/// it.
pub fn router(state: Shared) -> Router {
    routes(state, true, true)
}

/// Does the dashboard port serve `/a/...`? Only when the artifact origin
/// has no listener of its own (`--artifact-listen ""`). With a listener
/// (the default), artifacts load from that origin only, so an uploaded
/// page can never be same-origin with the dashboard
/// (`docs/security-review.md`, F5).
pub fn artifacts_on_dashboard(state: &AppState) -> bool {
    state.artifact_listen.is_empty()
}

/// The routes, without auth or transport, and without `/hosts/connect`.
/// `serve_artifacts`: mount the artifact serving routes (`/a/...`).
pub fn router_with(state: Shared, serve_artifacts: bool) -> Router {
    routes(state, serve_artifacts, false)
}

/// The routes. `host_connect`: mount `/hosts/connect`. The TCP dashboard
/// port does not: it is plaintext, and a host agent that dials it would
/// send its token in clear. Host agents dial the host listener
/// (`TERRARIUM_HOST_LISTEN`, `crate::hosts::router`) only
/// (`docs/security-review.md`, F2).
fn routes(state: Shared, serve_artifacts: bool, host_connect: bool) -> Router {
    let api = Router::new()
        .route("/health", get(system::health))
        .merge(hosts::routes())
        .merge(targets::routes())
        .route("/reload", post(system::reload))
        .route("/audit", get(system::audit))
        .route("/events", get(streams::firehose))
        .route("/events/latest", get(streams::latest))
        .merge(projects::routes())
        .merge(repos::routes())
        .merge(agents::routes())
        .merge(mail::routes())
        .merge(board::routes())
        .merge(memory::routes())
        .merge(wiki::routes())
        .merge(artifacts::routes())
        .merge(versions::routes())
        .merge(subscriptions::routes())
        .merge(usage::routes())
        .merge(fleet::routes())
        .route("/session", post(session::create).delete(session::delete))
        .fallback(api_not_found)
        .method_not_allowed_fallback(api_method_not_allowed)
        .layer(middleware::from_fn_with_state(state.clone(), resolve_actor));
    let mut app = Router::new().nest("/api", api);
    if serve_artifacts {
        app = app.merge(artifacts::serving_routes(&state));
    }
    if host_connect {
        app = app.route(
            terrarium_host::protocol::CONNECT_PATH,
            get(crate::hosts::connect),
        );
    }
    app.fallback(crate::assets::serve).with_state(state)
}

/// The routes for the Unix socket.
pub fn unix_router(state: Shared) -> Router {
    router(state).layer(Extension(Transport::Unix))
}

/// Compare two tokens in constant time, the length too: both sides are
/// hashed to 32 bytes first, and the compare of the hashes has no early
/// exit. So the timing leaks neither the bytes nor the length of the
/// token (`docs/security-review.md`, F7).
pub(crate) fn token_matches(given: &[u8], expected: &[u8]) -> bool {
    use sha2::Digest;
    let a = sha2::Sha256::digest(given);
    let b = sha2::Sha256::digest(expected);
    a.iter()
        .zip(b.iter())
        .fold(0u8, |acc, (x, y)| acc | (x ^ y))
        == 0
}

fn is_websocket(req: &Request) -> bool {
    req.headers()
        .get(header::UPGRADE)
        .and_then(|v| v.to_str().ok())
        .is_some_and(|v| v.eq_ignore_ascii_case("websocket"))
}

/// The prefix of the WebSocket subprotocol that carries the bearer token:
/// `Sec-WebSocket-Protocol: bearer.<token>`.
pub const WS_BEARER_PREFIX: &str = "bearer.";

/// The `bearer.<token>` entry of `Sec-WebSocket-Protocol`, if any. A
/// browser cannot set `Authorization` on a WebSocket, but it can offer
/// subprotocols. Returns the whole entry (to echo back) and the token.
fn protocol_token(req: &Request) -> Option<(String, String)> {
    req.headers()
        .get_all(header::SEC_WEBSOCKET_PROTOCOL)
        .iter()
        .filter_map(|v| v.to_str().ok())
        .flat_map(|v| v.split(','))
        .map(str::trim)
        .find_map(|p| {
            let t = p.strip_prefix(WS_BEARER_PREFIX)?;
            (!t.is_empty()).then(|| (p.to_owned(), t.to_owned()))
        })
}

/// The `token` query parameter, for WebSocket upgrades from an old client.
fn query_token(req: &Request) -> Option<String> {
    req.uri().query()?.split('&').find_map(|pair| {
        let (k, v) = pair.split_once('=')?;
        (k == "token").then(|| percent_decode(v))
    })
}

fn percent_decode(s: &str) -> String {
    let bytes = s.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        match bytes[i] {
            b'%' if i + 2 < bytes.len() => {
                let hex = std::str::from_utf8(&bytes[i + 1..i + 3]).unwrap_or("");
                match u8::from_str_radix(hex, 16) {
                    Ok(b) => {
                        out.push(b);
                        i += 3;
                    }
                    Err(_) => {
                        out.push(b'%');
                        i += 1;
                    }
                }
            }
            b'+' => {
                out.push(b' ');
                i += 1;
            }
            b => {
                out.push(b);
                i += 1;
            }
        }
    }
    String::from_utf8_lossy(&out).into_owned()
}

/// The most client addresses that get their own `?token=` warning; then
/// the warnings stop, so the set does not grow without a bound.
const MAX_WARNED_CLIENTS: usize = 4096;

/// The auth of the TCP listener: the bearer token, and (with
/// [`TokenAuth::with_sessions`]) the session cookie.
pub struct TokenAuth {
    token: String,
    /// Take `?token=` on a WebSocket upgrade (deprecated). On with
    /// `TERRARIUM_WS_QUERY_TOKEN=1`.
    allow_query: bool,
    /// The cookie sessions. `None`: the bearer token only.
    sessions: Option<Arc<session::Sessions>>,
    /// The dashboard origin from `TERRARIUM_PUBLIC_URL`, for the `Origin`
    /// check of a cookie WebSocket upgrade.
    public_origin: Option<String>,
    /// The client addresses that got the `?token=` warning.
    warned: std::sync::Mutex<std::collections::HashSet<String>>,
}

impl TokenAuth {
    pub fn new(token: String, allow_query: bool) -> Self {
        Self {
            token,
            allow_query,
            sessions: None,
            public_origin: None,
            warned: std::sync::Mutex::default(),
        }
    }

    /// Also take the session cookie. `public_origin`: the dashboard origin
    /// that a cookie WebSocket upgrade may come from, besides its own host.
    #[must_use]
    pub fn with_sessions(
        mut self,
        sessions: session::Sessions,
        public_origin: Option<String>,
    ) -> Self {
        self.sessions = Some(Arc::new(sessions));
        self.public_origin = public_origin;
        self
    }

    /// Log the `?token=` deprecation once per client address.
    fn warn_query(&self, client: &str) {
        let mut w = self
            .warned
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if w.len() < MAX_WARNED_CLIENTS && w.insert(client.to_owned()) {
            tracing::warn!(
                client,
                "a WebSocket upgrade sent the bearer token as `?token=`; the query string can \
                 reach proxy logs and browser history. Send it as the subprotocol \
                 `bearer.<token>` (Sec-WebSocket-Protocol) instead. TERRARIUM_WS_QUERY_TOKEN=0 \
                 (the default) refuses `?token=`"
            );
        }
    }
}

fn client_addr(req: &Request) -> String {
    req.extensions()
        .get::<axum::extract::ConnectInfo<std::net::SocketAddr>>()
        .map_or_else(|| "unknown".to_owned(), |c| c.0.ip().to_string())
}

fn unauthorized(message: &str) -> Response {
    ApiError::new(Code::Unauthorized, message).into_response()
}

fn forbidden(message: &str) -> Response {
    ApiError::new(Code::Forbidden, message).into_response()
}

/// Does the method change state? Those need `X-CSRF-Token` with a cookie.
fn is_mutating(method: &axum::http::Method) -> bool {
    use axum::http::Method;
    [Method::POST, Method::PUT, Method::PATCH, Method::DELETE].contains(method)
}

/// The host that the request names: `X-Forwarded-Host`, else `Host`.
fn request_host(headers: &axum::http::HeaderMap) -> Option<&str> {
    headers
        .get("x-forwarded-host")
        .or_else(|| headers.get(header::HOST))
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.split(',').next())
        .map(str::trim)
        .filter(|h| !h.is_empty())
}

/// May a cookie WebSocket upgrade come from this `Origin`? Yes when it is
/// the `TERRARIUM_PUBLIC_URL` origin, or an `http`/`https` origin whose
/// host and port are the request's own `Host` (the dashboard's own page).
/// No `Origin`, `null`, or another site: no. This stops cross-site
/// WebSocket hijacking with the operator's cookie.
pub fn ws_origin_allowed(headers: &axum::http::HeaderMap, public_origin: Option<&str>) -> bool {
    let Some(origin) = headers
        .get(header::ORIGIN)
        .and_then(|v| v.to_str().ok())
        .map(|o| o.trim().trim_end_matches('/'))
    else {
        return false;
    };
    if public_origin.is_some_and(|p| p.eq_ignore_ascii_case(origin)) {
        return true;
    }
    let Some((scheme, authority)) = origin.split_once("://") else {
        return false;
    };
    (scheme.eq_ignore_ascii_case("http") || scheme.eq_ignore_ascii_case("https"))
        && request_host(headers).is_some_and(|h| h.eq_ignore_ascii_case(authority))
}

async fn require_token(
    State(auth): State<Arc<TokenAuth>>,
    mut req: Request,
    next: Next,
) -> Response {
    if !req.uri().path().starts_with("/api") {
        return next.run(req).await;
    }
    // Clearing the cookie needs no proof (session::delete).
    if req.uri().path() == "/api/session" && req.method() == axum::http::Method::DELETE {
        return next.run(req).await;
    }
    let header_token = req
        .headers()
        .get(header::AUTHORIZATION)
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.strip_prefix("Bearer "))
        .map(str::to_owned);
    // The subprotocol that carried the token: the answer must name it, or
    // the browser drops the connection.
    let mut echo = None;
    let websocket = is_websocket(&req);
    let mut query = None;
    let given = match header_token {
        Some(t) => Some(t),
        None if websocket => match protocol_token(&req) {
            Some((proto, t)) => {
                echo = Some(proto);
                Some(t)
            }
            None => {
                query = query_token(&req);
                None
            }
        },
        None => None,
    };
    if let Some(given) = given {
        if !token_matches(given.as_bytes(), auth.token.as_bytes()) {
            return unauthorized("missing or wrong bearer token");
        }
        req.extensions_mut().insert(session::AuthVia::Bearer);
        req.extensions_mut().insert(Transport::Tcp);
        let mut resp = next.run(req).await;
        if let Some(proto) = echo
            && resp.status() == axum::http::StatusCode::SWITCHING_PROTOCOLS
            && !resp.headers().contains_key(header::SEC_WEBSOCKET_PROTOCOL)
            && let Ok(v) = HeaderValue::from_str(&proto)
        {
            resp.headers_mut().insert(header::SEC_WEBSOCKET_PROTOCOL, v);
        }
        return resp;
    }
    // The session cookie.
    if let Some(sessions) = &auth.sessions
        && let Some(cookie) = session::cookie_value(req.headers(), session::COOKIE)
    {
        let Some(s) = sessions.key.verify(cookie, chrono::Utc::now().timestamp()) else {
            return unauthorized(
                "the session cookie is wrong or has expired; POST /api/session again",
            );
        };
        if websocket {
            if !ws_origin_allowed(req.headers(), auth.public_origin.as_deref()) {
                return forbidden(
                    "a WebSocket upgrade with the session cookie must come from the dashboard \
                     origin (the Origin header)",
                );
            }
        } else if is_mutating(req.method()) {
            let ok = req
                .headers()
                .get(session::CSRF_HEADER)
                .and_then(|v| v.to_str().ok())
                .is_some_and(|t| s.csrf_matches(t));
            if !ok {
                return forbidden(
                    "a request with the session cookie must send X-CSRF-Token with the csrf \
                     value of POST /api/session",
                );
            }
        }
        req.extensions_mut().insert(session::AuthVia::Cookie);
        req.extensions_mut().insert(Transport::Tcp);
        return next.run(req).await;
    }
    // The deprecated `?token=` on a WebSocket upgrade.
    match query {
        Some(_) if !auth.allow_query => unauthorized(
            "`?token=` is off (TERRARIUM_WS_QUERY_TOKEN=0); send the token as the WebSocket \
             subprotocol `bearer.<token>`",
        ),
        Some(t) if token_matches(t.as_bytes(), auth.token.as_bytes()) => {
            auth.warn_query(&client_addr(&req));
            req.extensions_mut().insert(session::AuthVia::Bearer);
            req.extensions_mut().insert(Transport::Tcp);
            next.run(req).await
        }
        _ => unauthorized("missing or wrong bearer token"),
    }
}

/// The CSP of an API answer: it loads nothing and is never framed.
pub const API_CSP: &str = "default-src 'none'; frame-ancestors 'none'";

/// The dashboard's Content-Security-Policy: the directives of its
/// `<meta>` tag (`dashboard/vite.config.ts`, `dashboardCsp`) and
/// `frame-ancestors 'none'`, which a `<meta>` tag cannot set.
/// `artifact_origin`: the origin that signed artifact URLs name; empty when
/// the dashboard port serves `/a/...` itself.
pub fn dashboard_csp(artifact_origin: &str) -> String {
    let art: Vec<&str> = Some(artifact_origin)
        .filter(|a| !a.is_empty())
        .into_iter()
        .collect();
    let join = |base: &[&str]| {
        base.iter()
            .chain(art.iter())
            .copied()
            .collect::<Vec<_>>()
            .join(" ")
    };
    let frame = if art.is_empty() {
        "'self'".to_owned()
    } else {
        art.join(" ")
    };
    [
        "default-src 'none'".to_owned(),
        "script-src 'self'".to_owned(),
        "style-src 'self' 'unsafe-inline'".to_owned(),
        format!("img-src {}", join(&["'self'", "data:"])),
        "font-src 'self'".to_owned(),
        format!("connect-src {}", join(&["'self'"])),
        format!("frame-src {frame}"),
        "worker-src 'none'".to_owned(),
        "manifest-src 'self'".to_owned(),
        "media-src 'none'".to_owned(),
        "object-src 'none'".to_owned(),
        "base-uri 'none'".to_owned(),
        "form-action 'none'".to_owned(),
        "frame-ancestors 'none'".to_owned(),
    ]
    .join("; ")
}

/// The security headers of the dashboard port: a CSP (the dashboard
/// policy on pages and assets, [`API_CSP`] on `/api`), `nosniff`,
/// `Referrer-Policy: no-referrer`, and `X-Frame-Options: DENY`. An answer
/// that sets its own CSP (a served artifact, with its `sandbox`) keeps its
/// own headers.
async fn security_headers(State(state): State<Shared>, req: Request, next: Next) -> Response {
    let csp = if req.uri().path().starts_with("/api") {
        API_CSP.to_owned()
    } else {
        dashboard_csp(&artifacts::artifact_origin(&state, req.headers()))
    };
    let mut resp = next.run(req).await;
    let h = resp.headers_mut();
    if h.contains_key(header::CONTENT_SECURITY_POLICY) {
        return resp;
    }
    if let Ok(v) = HeaderValue::from_str(&csp) {
        h.insert(header::CONTENT_SECURITY_POLICY, v);
    }
    h.entry(header::X_CONTENT_TYPE_OPTIONS)
        .or_insert(HeaderValue::from_static("nosniff"));
    h.entry(header::REFERRER_POLICY)
        .or_insert(HeaderValue::from_static("no-referrer"));
    h.entry(header::X_FRAME_OPTIONS)
        .or_insert(HeaderValue::from_static("DENY"));
    resp
}

/// The routes of the TCP listener. `/api/*` needs the bearer token
/// (`Authorization`; on a WebSocket upgrade also the subprotocol
/// `bearer.<token>`, and `?token=` when `TERRARIUM_WS_QUERY_TOKEN=1`) or
/// the session cookie of `POST /api/session`. The dashboard assets stay
/// public, because a browser cannot send a header on page load. `/a/...` is
/// here only when the artifact origin has no listener of its own
/// ([`artifacts_on_dashboard`]). The settings come from `state.web`; the
/// session key is `<state>/session.key`.
pub fn tcp_router(state: Shared, token: String) -> anyhow::Result<Router> {
    let key = session::SessionKey::load_or_create(&state.state_dir).with_context(|| {
        format!(
            "cannot read or make the session key {}",
            state.state_dir.join(session::KEY_FILE).display()
        )
    })?;
    let auth = TokenAuth::new(token, state.web.ws_query_token).with_sessions(
        session::Sessions {
            key,
            ttl: state.web.session_ttl,
        },
        state.web.public_origin(),
    );
    let on_dashboard = artifacts_on_dashboard(&state);
    Ok(tcp_router_with(state, auth, on_dashboard))
}

/// [`tcp_router`] with the auth given, for tests.
pub fn tcp_router_with(state: Shared, auth: TokenAuth, serve_artifacts: bool) -> Router {
    let mut app = router_with(state.clone(), serve_artifacts).layer(Extension(Transport::Tcp));
    if let Some(s) = &auth.sessions {
        app = app.layer(Extension(s.clone()));
    }
    app.layer(middleware::from_fn_with_state(
        Arc::new(auth),
        require_token,
    ))
    .layer(middleware::from_fn_with_state(state, security_headers))
}

/// Bind the Unix socket and serve on it in the background.
pub async fn serve_unix(state: Shared, path: &Path) -> anyhow::Result<JoinHandle<()>> {
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir)
            .with_context(|| format!("cannot create socket dir {}", dir.display()))?;
    }
    if path.exists() {
        if tokio::net::UnixStream::connect(path).await.is_ok() {
            anyhow::bail!("another daemon listens on {}", path.display());
        }
        std::fs::remove_file(path)
            .with_context(|| format!("cannot remove stale socket {}", path.display()))?;
    }
    let listener = tokio::net::UnixListener::bind(path)
        .with_context(|| format!("cannot bind {}", path.display()))?;
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o660))
        .with_context(|| format!("cannot set mode on {}", path.display()))?;
    info!(socket = %path.display(), "API listening on Unix socket");
    let app = unix_router(state);
    Ok(tokio::spawn(async move {
        if let Err(e) = axum::serve(listener, app).await {
            error!("Unix socket server failed: {e}");
        }
    }))
}

/// Bind the TCP listener and serve on it in the background.
pub async fn serve_tcp(
    state: Shared,
    addr: &str,
    token: String,
) -> anyhow::Result<(JoinHandle<()>, std::net::SocketAddr)> {
    let listener = tokio::net::TcpListener::bind(addr)
        .await
        .with_context(|| format!("cannot bind {addr}"))?;
    let local = listener.local_addr()?;
    info!(addr = %local, "API and dashboard listening on TCP");
    if !artifacts_on_dashboard(&state) {
        info!("the dashboard port does not serve /a/...: artifacts load from their own origin");
    }
    let app =
        tcp_router(state, token)?.into_make_service_with_connect_info::<std::net::SocketAddr>();
    Ok((
        tokio::spawn(async move {
            if let Err(e) = axum::serve(listener, app).await {
                error!("TCP server failed: {e}");
            }
        }),
        local,
    ))
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::*;
    use axum::body::Body;
    use axum::http::StatusCode;
    use http_body_util::BodyExt;
    use terrarium_core::config::AgentDef;
    use terrarium_core::{Agent, HarnessId};
    use tower::ServiceExt;

    fn state() -> Shared {
        let mut conn = rusqlite::Connection::open_in_memory().unwrap();
        crate::db::migrate(&mut conn).unwrap();
        let hub = crate::events::EventHub::default();
        Arc::new(AppState {
            started: Instant::now(),
            started_at: chrono::Utc::now(),
            db: Db::new(conn, hub),
            definitions: RwLock::new(Definitions::default()),
            supervisor: Arc::new(crate::supervisor::InMemorySupervisor::new(
                std::time::Duration::from_millis(5),
            )),
            state_dir: PathBuf::from("/state"),
            config_dir: PathBuf::from("/nonexistent"),
            insecure_agent_header: false,
            usage: Arc::default(),
            executor: Arc::default(),
            web: crate::settings::WebConfig::default(),
            artifact_listen: String::new(),
        })
    }

    async fn get(app: Router, uri: &str, token: Option<&str>) -> (StatusCode, Vec<u8>) {
        let mut req = Request::builder().uri(uri);
        if let Some(t) = token {
            req = req.header(header::AUTHORIZATION, format!("Bearer {t}"));
        }
        let resp = app.oneshot(req.body(Body::empty()).unwrap()).await.unwrap();
        let status = resp.status();
        let body = resp
            .into_body()
            .collect()
            .await
            .unwrap()
            .to_bytes()
            .to_vec();
        (status, body)
    }

    #[tokio::test]
    async fn health_reports_version() {
        let (status, body) = get(router(state()), "/api/health", None).await;
        assert_eq!(status, StatusCode::OK);
        let h: Health = serde_json::from_slice(&body).unwrap();
        assert_eq!(h.version, terrarium_core::VERSION);
        assert_eq!(h.status, "ok");
    }

    fn toml_def(name: &str, project: &str) -> AgentDef {
        toml::from_str(&format!(
            "name = \"{name}\"\nproject = \"{project}\"\nharness = \"claude\"\n"
        ))
        .unwrap()
    }

    #[tokio::test]
    async fn agents_list_filters_by_project() {
        let s = state();
        let (status, body) = get(router(s.clone()), "/api/agents", None).await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(body, b"[]");

        s.db.write("operator", |t| {
            for p in ["p1", "p2"] {
                crate::store::projects::create(
                    t,
                    &crate::store::projects::NewProject {
                        name: p.into(),
                        ..Default::default()
                    },
                    "api",
                )?;
            }
            crate::store::agents::create(t, &toml_def("a", "p1"), Path::new("/state"), "api")?;
            crate::store::agents::create(t, &toml_def("b", "p2"), Path::new("/state"), "api")?;
            Ok(())
        })
        .await
        .unwrap();
        let (_, body) = get(router(s.clone()), "/api/agents?project=p1", None).await;
        // The CLI decodes the list as core agents.
        let agents: Vec<Agent> = serde_json::from_slice(&body).unwrap();
        assert_eq!(agents.len(), 1);
        assert_eq!(agents[0].name, "a");
        assert_eq!(agents[0].harness, HarnessId::Claude);
    }

    #[tokio::test]
    async fn tcp_router_needs_the_token_on_api_only() {
        let s = state();
        let app = || tcp_router_with(s.clone(), TokenAuth::new("secret".into(), false), false);
        assert_eq!(
            get(app(), "/api/health", None).await.0,
            StatusCode::UNAUTHORIZED
        );
        assert_eq!(
            get(app(), "/api/health", Some("wrong")).await.0,
            StatusCode::UNAUTHORIZED
        );
        assert_eq!(
            get(app(), "/api/health", Some("secret")).await.0,
            StatusCode::OK
        );
        assert_eq!(get(app(), "/", None).await.0, StatusCode::OK);
        // `?token=` counts on a WebSocket upgrade only.
        assert_eq!(
            get(app(), "/api/health?token=secret", None).await.0,
            StatusCode::UNAUTHORIZED
        );
    }

    #[tokio::test]
    async fn unknown_api_route_is_json_404() {
        let (status, body) = get(router(state()), "/api/nope", None).await;
        assert_eq!(status, StatusCode::NOT_FOUND);
        let v: serde_json::Value = serde_json::from_slice(&body).unwrap();
        assert_eq!(v["error"]["code"], "not_found");
        let resp = router(state())
            .oneshot(
                Request::builder()
                    .method("DELETE")
                    .uri("/api/health")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::METHOD_NOT_ALLOWED);
        let body = resp.into_body().collect().await.unwrap().to_bytes();
        let v: serde_json::Value = serde_json::from_slice(&body).unwrap();
        assert_eq!(v["error"]["code"], "method_not_allowed");
    }

    #[tokio::test]
    async fn spa_routes_get_a_page() {
        let (status, body) = get(router(state()), "/agents/123", None).await;
        assert_eq!(status, StatusCode::OK);
        assert!(String::from_utf8_lossy(&body).contains("<html"));
        let (status, _) = get(router(state()), "/missing.js", None).await;
        assert_eq!(status, StatusCode::NOT_FOUND);
    }

    #[test]
    fn token_compare() {
        assert!(token_matches(b"abc", b"abc"));
        assert!(!token_matches(b"abd", b"abc"));
        // A length mismatch takes the same path as a byte mismatch: both
        // sides become 32-byte hashes.
        assert!(!token_matches(b"ab", b"abc"));
        assert!(!token_matches(b"", b"abc"));
        assert!(!token_matches(
            b"abcabcabcabcabcabcabcabcabcabcabcabc",
            b"abc"
        ));
        assert!(token_matches(b"", b""));
    }

    /// A WebSocket upgrade request with `extra` headers.
    async fn ws(app: Router, uri: &str, extra: &[(&str, &str)]) -> Response {
        let mut req = Request::builder()
            .uri(uri)
            .header(header::HOST, "localhost")
            .header(header::CONNECTION, "upgrade")
            .header(header::UPGRADE, "websocket")
            .header(header::SEC_WEBSOCKET_VERSION, "13")
            .header(header::SEC_WEBSOCKET_KEY, "dGhlIHNhbXBsZSBub25jZQ==");
        for (k, v) in extra {
            req = req.header(*k, *v);
        }
        app.oneshot(req.body(Body::empty()).unwrap()).await.unwrap()
    }

    /// F4: the token rides in `Sec-WebSocket-Protocol`; the answer echoes
    /// the chosen subprotocol. `?token=` still works, unless it is off.
    #[tokio::test]
    async fn websocket_token_in_the_subprotocol() {
        let s = state();
        let app = |q: bool| tcp_router_with(s.clone(), TokenAuth::new("secret".into(), q), false);
        // An upgrade with no hyper connection behind it cannot switch, so
        // the handler answers an error; the point is that auth passed (not
        // 401). The echo is checked below on a 101 from a real server.
        let r = ws(
            app(true),
            "/api/events",
            &[("sec-websocket-protocol", "bearer.secret")],
        )
        .await;
        assert_ne!(r.status(), StatusCode::UNAUTHORIZED);
        let r = ws(
            app(true),
            "/api/events",
            &[("sec-websocket-protocol", "terrarium, bearer.wrong")],
        )
        .await;
        assert_eq!(r.status(), StatusCode::UNAUTHORIZED);
        // The deprecated query form.
        let r = ws(app(true), "/api/events?token=secret", &[]).await;
        assert_ne!(r.status(), StatusCode::UNAUTHORIZED);
        // TERRARIUM_WS_QUERY_TOKEN=0.
        let r = ws(app(false), "/api/events?token=secret", &[]).await;
        assert_eq!(r.status(), StatusCode::UNAUTHORIZED);
        let body = r.into_body().collect().await.unwrap().to_bytes();
        assert!(String::from_utf8_lossy(&body).contains("bearer.<token>"));
        // The subprotocol counts on an upgrade only.
        let resp = app(true)
            .oneshot(
                Request::builder()
                    .uri("/api/health")
                    .header("sec-websocket-protocol", "bearer.secret")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::UNAUTHORIZED);
    }

    #[test]
    fn query_token_warns_once_per_client() {
        let auth = TokenAuth::new("t".into(), true);
        for ip in ["10.0.0.1", "10.0.0.1", "10.0.0.2", "10.0.0.1"] {
            auth.warn_query(ip);
        }
        assert_eq!(auth.warned.lock().unwrap().len(), 2);
    }

    /// The echo needs a real upgrade: serve the TCP router on a port and
    /// connect a WebSocket client that offers `bearer.<token>`.
    #[tokio::test]
    async fn websocket_upgrade_echoes_the_bearer_subprotocol() {
        use tokio_tungstenite::tungstenite::client::IntoClientRequest;
        let s = state();
        let app = tcp_router_with(s, TokenAuth::new("secret".into(), false), false)
            .into_make_service_with_connect_info::<std::net::SocketAddr>();
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let server = tokio::spawn(async move { axum::serve(listener, app).await });
        let mut req = format!("ws://{addr}/api/events")
            .into_client_request()
            .unwrap();
        req.headers_mut().insert(
            "sec-websocket-protocol",
            HeaderValue::from_static("bearer.secret"),
        );
        let (_ws, resp) = tokio_tungstenite::connect_async(req).await.unwrap();
        assert_eq!(
            resp.headers()
                .get("sec-websocket-protocol")
                .and_then(|v| v.to_str().ok()),
            Some("bearer.secret")
        );
        // A wrong token never upgrades.
        let mut bad = format!("ws://{addr}/api/events")
            .into_client_request()
            .unwrap();
        bad.headers_mut().insert(
            "sec-websocket-protocol",
            HeaderValue::from_static("bearer.nope"),
        );
        assert!(tokio_tungstenite::connect_async(bad).await.is_err());
        server.abort();
    }

    /// F5: with an artifact origin of its own, the dashboard port does not
    /// serve `/a/...`; without one, it does. The Unix router always does.
    #[tokio::test]
    async fn artifacts_mount_on_the_dashboard_only_without_their_own_origin() {
        let s = state();
        let auth = || TokenAuth::new("secret".into(), true);
        // The SPA fallback answers an unknown path with the page, so look
        // at the body: a served route answers JSON or a file, not HTML.
        let page = |b: &[u8]| String::from_utf8_lossy(b).contains("<html");
        let (_, body) = get(tcp_router_with(s.clone(), auth(), false), "/a/ART-1/", None).await;
        assert!(page(&body), "the dashboard must not serve /a/ here");
        let (status, body) = get(tcp_router_with(s.clone(), auth(), true), "/a/ART-1/", None).await;
        assert!(!page(&body), "{status}");
        let (_, body) = get(router(s), "/a/ART-1/", None).await;
        assert!(!page(&body));
    }

    #[test]
    fn percent_decoding() {
        assert_eq!(percent_decode("a%2Fb+c"), "a/b c");
        assert_eq!(percent_decode("100%"), "100%");
        assert_eq!(percent_decode("%zz"), "%zz");
    }

    // ---- cookie sessions ----

    const KEY: [u8; 32] = [3; 32];

    fn session_app(s: &Shared, key: [u8; 32], public: Option<&str>) -> Router {
        let auth = TokenAuth::new("secret".into(), false).with_sessions(
            session::Sessions {
                key: session::SessionKey::from_bytes(key.to_vec()),
                ttl: std::time::Duration::from_secs(3600),
            },
            public.map(str::to_owned),
        );
        tcp_router_with(s.clone(), auth, false)
    }

    async fn send(app: Router, method: &str, uri: &str, headers: &[(&str, &str)]) -> Response {
        let mut req = Request::builder().method(method).uri(uri);
        for (k, v) in headers {
            req = req.header(*k, *v);
        }
        app.oneshot(req.body(Body::empty()).unwrap()).await.unwrap()
    }

    fn hdr(r: &Response, name: &str) -> String {
        r.headers()
            .get(name)
            .and_then(|v| v.to_str().ok())
            .unwrap_or_default()
            .to_owned()
    }

    /// Make a session; answer the `Cookie` header value and the csrf token.
    async fn login(app: Router) -> (String, String) {
        let r = send(
            app,
            "POST",
            "/api/session",
            &[("authorization", "Bearer secret")],
        )
        .await;
        assert_eq!(r.status(), StatusCode::CREATED);
        let set = hdr(&r, "set-cookie");
        let cookie = set.split(';').next().unwrap().to_owned();
        let body = r.into_body().collect().await.unwrap().to_bytes();
        let v: serde_json::Value = serde_json::from_slice(&body).unwrap();
        (cookie, v["csrf"].as_str().unwrap().to_owned())
    }

    #[tokio::test]
    async fn session_cookie_create_use_and_clear() {
        let s = state();
        let app = || session_app(&s, KEY, None);
        // The bearer token buys the cookie.
        let r = send(
            app(),
            "POST",
            "/api/session",
            &[("authorization", "Bearer secret")],
        )
        .await;
        assert_eq!(r.status(), StatusCode::CREATED);
        let set = hdr(&r, "set-cookie");
        assert!(set.starts_with("terrarium_session=v1."), "{set}");
        for part in ["Path=/", "Max-Age=3600", "HttpOnly", "SameSite=Strict"] {
            assert!(set.contains(part), "{set}");
        }
        assert!(!set.contains("Secure"), "plain http: {set}");
        assert_eq!(hdr(&r, "cache-control"), "no-store");
        let body = r.into_body().collect().await.unwrap().to_bytes();
        let v: serde_json::Value = serde_json::from_slice(&body).unwrap();
        assert!(v["csrf"].as_str().unwrap().len() >= 32);
        assert!(v["exp"].is_i64() && v["expires_at"].is_string());
        // Behind a TLS proxy the cookie is Secure.
        let r = send(
            app(),
            "POST",
            "/api/session",
            &[
                ("authorization", "Bearer secret"),
                ("x-forwarded-proto", "https"),
            ],
        )
        .await;
        assert!(hdr(&r, "set-cookie").ends_with("; Secure"));
        // No token, a wrong token: no cookie.
        let r = send(app(), "POST", "/api/session", &[]).await;
        assert_eq!(r.status(), StatusCode::UNAUTHORIZED);
        let r = send(
            app(),
            "POST",
            "/api/session",
            &[("authorization", "Bearer nope")],
        )
        .await;
        assert_eq!(r.status(), StatusCode::UNAUTHORIZED);

        let (cookie, csrf) = login(app()).await;
        // The cookie alone reads the API.
        let r = send(app(), "GET", "/api/health", &[("cookie", &cookie)]).await;
        assert_eq!(r.status(), StatusCode::OK);
        // A cookie cannot buy another cookie, even with the csrf token.
        let r = send(
            app(),
            "POST",
            "/api/session",
            &[("cookie", &cookie), ("x-csrf-token", &csrf)],
        )
        .await;
        assert_eq!(r.status(), StatusCode::UNAUTHORIZED);
        // DELETE clears it, with no auth.
        let r = send(app(), "DELETE", "/api/session", &[]).await;
        assert_eq!(r.status(), StatusCode::NO_CONTENT);
        let set = hdr(&r, "set-cookie");
        assert!(
            set.starts_with("terrarium_session=;") && set.contains("Max-Age=0"),
            "{set}"
        );
        // The Unix socket has no sessions.
        let r = send(unix_router(s.clone()), "POST", "/api/session", &[]).await;
        assert_eq!(r.status(), StatusCode::NOT_FOUND);
    }

    #[tokio::test]
    async fn session_cookie_needs_csrf_to_change_state() {
        let s = state();
        let app = || session_app(&s, KEY, None);
        let (cookie, csrf) = login(app()).await;
        // `DELETE /api/health` passes auth, then gets 405: the probe of a
        // mutating method that changes nothing.
        let r = send(app(), "DELETE", "/api/health", &[("cookie", &cookie)]).await;
        assert_eq!(r.status(), StatusCode::FORBIDDEN);
        let body = r.into_body().collect().await.unwrap().to_bytes();
        assert!(String::from_utf8_lossy(&body).contains("X-CSRF-Token"));
        let r = send(
            app(),
            "DELETE",
            "/api/health",
            &[("cookie", &cookie), ("x-csrf-token", "wrong")],
        )
        .await;
        assert_eq!(r.status(), StatusCode::FORBIDDEN);
        let r = send(
            app(),
            "DELETE",
            "/api/health",
            &[("cookie", &cookie), ("x-csrf-token", &csrf)],
        )
        .await;
        assert_eq!(r.status(), StatusCode::METHOD_NOT_ALLOWED);
        // The csrf token of another session does not count.
        let (_, other) = login(app()).await;
        let r = send(
            app(),
            "DELETE",
            "/api/health",
            &[("cookie", &cookie), ("x-csrf-token", &other)],
        )
        .await;
        assert_eq!(r.status(), StatusCode::FORBIDDEN);
        // The bearer token needs no csrf token, as before.
        let r = send(
            app(),
            "DELETE",
            "/api/health",
            &[("authorization", "Bearer secret")],
        )
        .await;
        assert_eq!(r.status(), StatusCode::METHOD_NOT_ALLOWED);
    }

    #[tokio::test]
    async fn session_cookie_expires_and_dies_with_the_key() {
        let s = state();
        let key = session::SessionKey::from_bytes(KEY.to_vec());
        let old = key.mint(chrono::Utc::now().timestamp() - 1).unwrap();
        let r = send(
            session_app(&s, KEY, None),
            "GET",
            "/api/health",
            &[("cookie", &format!("terrarium_session={}", old.cookie))],
        )
        .await;
        assert_eq!(r.status(), StatusCode::UNAUTHORIZED);
        // A good cookie, then a new key (session.key replaced): refused.
        let (cookie, _) = login(session_app(&s, KEY, None)).await;
        let r = send(
            session_app(&s, KEY, None),
            "GET",
            "/api/health",
            &[("cookie", &cookie)],
        )
        .await;
        assert_eq!(r.status(), StatusCode::OK);
        let r = send(
            session_app(&s, [9; 32], None),
            "GET",
            "/api/health",
            &[("cookie", &cookie)],
        )
        .await;
        assert_eq!(r.status(), StatusCode::UNAUTHORIZED);
        // Without sessions (the bearer-only auth) the cookie is nothing.
        let r = send(
            tcp_router_with(s.clone(), TokenAuth::new("secret".into(), false), false),
            "GET",
            "/api/health",
            &[("cookie", &cookie)],
        )
        .await;
        assert_eq!(r.status(), StatusCode::UNAUTHORIZED);
    }

    /// The key file: made once, mode 0600, and a new file ends every session.
    #[tokio::test]
    async fn session_key_file_rotation() {
        let dir = tempfile::tempdir().unwrap();
        let mut st = Arc::try_unwrap(state()).ok().unwrap();
        st.state_dir = dir.path().to_owned();
        let s = Arc::new(st);
        let app = || tcp_router(s.clone(), "secret".into()).unwrap();
        let (cookie, _) = login(app()).await;
        let r = send(app(), "GET", "/api/health", &[("cookie", &cookie)]).await;
        assert_eq!(r.status(), StatusCode::OK, "the key is kept");
        std::fs::remove_file(dir.path().join(session::KEY_FILE)).unwrap();
        let r = send(app(), "GET", "/api/health", &[("cookie", &cookie)]).await;
        assert_eq!(r.status(), StatusCode::UNAUTHORIZED, "a new key");
    }

    #[test]
    fn websocket_origin_check() {
        let h = |pairs: &[(&'static str, &'static str)]| {
            let mut m = axum::http::HeaderMap::new();
            for (k, v) in pairs {
                m.insert(*k, HeaderValue::from_static(v));
            }
            m
        };
        let same = h(&[("host", "t.lan:8080"), ("origin", "http://t.lan:8080")]);
        assert!(ws_origin_allowed(&same, None));
        assert!(!ws_origin_allowed(&h(&[("host", "t.lan:8080")]), None));
        let evil = h(&[("host", "t.lan:8080"), ("origin", "https://evil.example")]);
        assert!(!ws_origin_allowed(&evil, None));
        let null = h(&[("host", "t.lan:8080"), ("origin", "null")]);
        assert!(!ws_origin_allowed(&null, None));
        // Behind a proxy that rewrites Host, TERRARIUM_PUBLIC_URL names it.
        let proxied = h(&[("host", "inner:8080"), ("origin", "https://t.example.com")]);
        assert!(!ws_origin_allowed(&proxied, None));
        assert!(ws_origin_allowed(&proxied, Some("https://t.example.com")));
        // Another port is another origin.
        let port = h(&[("host", "t.lan:8080"), ("origin", "http://t.lan:8081")]);
        assert!(!ws_origin_allowed(&port, None));
    }

    #[tokio::test]
    async fn websocket_upgrade_with_the_cookie_checks_the_origin() {
        let s = state();
        let app = || session_app(&s, KEY, Some("https://t.example.com"));
        let (cookie, _) = login(app()).await;
        let c = ("cookie", cookie.as_str());
        // `ws()` sends `host: localhost`. Auth passed: not 401 or 403.
        let r = ws(app(), "/api/events", &[c, ("origin", "http://localhost")]).await;
        assert!(
            ![StatusCode::UNAUTHORIZED, StatusCode::FORBIDDEN].contains(&r.status()),
            "{}",
            r.status()
        );
        let r = ws(
            app(),
            "/api/events",
            &[c, ("origin", "https://t.example.com")],
        )
        .await;
        assert!(![StatusCode::UNAUTHORIZED, StatusCode::FORBIDDEN].contains(&r.status()));
        let r = ws(app(), "/api/events", &[c]).await;
        assert_eq!(r.status(), StatusCode::FORBIDDEN);
        let r = ws(
            app(),
            "/api/events",
            &[c, ("origin", "https://evil.example")],
        )
        .await;
        assert_eq!(r.status(), StatusCode::FORBIDDEN);
        // The bearer subprotocol needs no Origin (the CLI sends none).
        let r = ws(
            app(),
            "/api/events",
            &[("sec-websocket-protocol", "bearer.secret")],
        )
        .await;
        assert_ne!(r.status(), StatusCode::UNAUTHORIZED);
        assert_ne!(r.status(), StatusCode::FORBIDDEN);
    }

    /// A real upgrade with the cookie and the page's Origin switches.
    #[tokio::test]
    async fn websocket_upgrade_with_the_cookie_on_a_real_server() {
        use tokio_tungstenite::tungstenite::client::IntoClientRequest;
        let s = state();
        let (cookie, _) = login(session_app(&s, KEY, None)).await;
        let app = session_app(&s, KEY, None)
            .into_make_service_with_connect_info::<std::net::SocketAddr>();
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let server = tokio::spawn(async move { axum::serve(listener, app).await });
        let request = |origin: Option<String>| {
            let mut req = format!("ws://{addr}/api/events")
                .into_client_request()
                .unwrap();
            req.headers_mut()
                .insert("cookie", HeaderValue::from_str(&cookie).unwrap());
            if let Some(o) = origin {
                req.headers_mut()
                    .insert("origin", HeaderValue::from_str(&o).unwrap());
            }
            req
        };
        assert!(
            tokio_tungstenite::connect_async(request(Some(format!("http://{addr}"))))
                .await
                .is_ok()
        );
        assert!(
            tokio_tungstenite::connect_async(request(None))
                .await
                .is_err()
        );
        assert!(
            tokio_tungstenite::connect_async(request(Some("http://evil.example".into())))
                .await
                .is_err()
        );
        server.abort();
    }

    // ---- security headers ----

    #[tokio::test]
    async fn dashboard_and_api_answers_carry_security_headers() {
        let s = state();
        let app = || tcp_router_with(s.clone(), TokenAuth::new("secret".into(), false), true);
        let r = send(app(), "GET", "/", &[("host", "t.lan:8080")]).await;
        assert_eq!(r.status(), StatusCode::OK);
        let csp = hdr(&r, "content-security-policy");
        assert!(csp.contains("frame-ancestors 'none'"), "{csp}");
        assert!(csp.contains("default-src 'none'") && csp.contains("script-src 'self'"));
        // No artifact listener: the artifacts are on 'self'.
        assert!(csp.contains("frame-src 'self'"), "{csp}");
        assert_eq!(hdr(&r, "x-content-type-options"), "nosniff");
        assert_eq!(hdr(&r, "referrer-policy"), "no-referrer");
        assert_eq!(hdr(&r, "x-frame-options"), "DENY");
        // The API, also a 401.
        for auth in [Some("Bearer secret"), None] {
            let mut h = vec![];
            if let Some(a) = auth {
                h.push(("authorization", a));
            }
            let r = send(app(), "GET", "/api/health", &h).await;
            assert_eq!(hdr(&r, "content-security-policy"), API_CSP);
            assert_eq!(hdr(&r, "x-frame-options"), "DENY");
            assert_eq!(hdr(&r, "x-content-type-options"), "nosniff");
        }
        // tests/artifacts.rs: a served artifact keeps its own sandbox CSP
        // and no X-Frame-Options on the dashboard port.
    }

    /// F2: the plaintext dashboard port does not take host agents; the
    /// Unix socket router (tests) still does.
    #[tokio::test]
    async fn hosts_connect_is_not_on_the_dashboard_port() {
        let s = state();
        let tcp = tcp_router_with(s.clone(), TokenAuth::new("secret".into(), false), false);
        let r = ws(
            tcp,
            terrarium_host::protocol::CONNECT_PATH,
            &[("authorization", "Bearer host-token")],
        )
        .await;
        // The SPA fallback: a page, not the host handler.
        let body = r.into_body().collect().await.unwrap().to_bytes();
        assert!(String::from_utf8_lossy(&body).contains("<html"));
        let r = ws(
            router(s),
            terrarium_host::protocol::CONNECT_PATH,
            &[("authorization", "Bearer host-token")],
        )
        .await;
        assert_eq!(r.status(), StatusCode::UNAUTHORIZED, "the host handler");
    }

    #[tokio::test]
    async fn dashboard_csp_names_the_artifact_origin() {
        let mut st = Arc::try_unwrap(state()).ok().unwrap();
        st.artifact_listen = "0.0.0.0:8081".into();
        let s = Arc::new(st);
        let app = tcp_router_with(s.clone(), TokenAuth::new("t".into(), false), false);
        let r = send(app, "GET", "/", &[("host", "t.lan:8080")]).await;
        let csp = hdr(&r, "content-security-policy");
        assert!(csp.contains("frame-src http://t.lan:8081;"), "{csp}");
        assert!(
            csp.contains("img-src 'self' data: http://t.lan:8081;"),
            "{csp}"
        );
        assert!(
            csp.contains("connect-src 'self' http://t.lan:8081;"),
            "{csp}"
        );
        let mut st = Arc::try_unwrap(s).ok().unwrap();
        st.web.artifact_public_url = Some("https://art.example.com".into());
        let app = tcp_router_with(Arc::new(st), TokenAuth::new("t".into(), false), false);
        let r = send(app, "GET", "/agents", &[("host", "t.lan:8080")]).await;
        let csp = hdr(&r, "content-security-policy");
        assert!(csp.contains("frame-src https://art.example.com;"), "{csp}");
    }

    #[test]
    fn dashboard_csp_matches_the_meta_policy() {
        assert_eq!(
            dashboard_csp(""),
            "default-src 'none'; script-src 'self'; style-src 'self' 'unsafe-inline'; \
             img-src 'self' data:; font-src 'self'; connect-src 'self'; frame-src 'self'; \
             worker-src 'none'; manifest-src 'self'; media-src 'none'; object-src 'none'; \
             base-uri 'none'; form-action 'none'; frame-ancestors 'none'"
        );
    }
}
