//! The HTTP API. The same router serves the Unix socket (no token) and the
//! TCP listener (bearer token on `/api/*`). `docs/api.md` is the contract.

mod agents;
mod artifacts;
mod board;
mod longpoll;
mod mail;
mod memory;
mod projects;
mod streams;
mod system;
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

/// The routes, without auth or transport.
pub fn router(state: Shared) -> Router {
    let api = Router::new()
        .route("/health", get(system::health))
        .route("/hosts", get(system::hosts))
        .route("/targets", get(system::targets))
        .route("/reload", post(system::reload))
        .route("/audit", get(system::audit))
        .route("/events", get(streams::firehose))
        .merge(projects::routes())
        .merge(agents::routes())
        .merge(mail::routes())
        .merge(board::routes())
        .merge(memory::routes())
        .merge(wiki::routes())
        .merge(artifacts::routes())
        .merge(versions::routes())
        .fallback(api_not_found)
        .method_not_allowed_fallback(api_method_not_allowed)
        .layer(middleware::from_fn_with_state(state.clone(), resolve_actor));
    Router::new()
        .nest("/api", api)
        .merge(artifacts::serving_routes())
        .fallback(crate::assets::serve)
        .with_state(state)
}

/// The routes for the Unix socket.
pub fn unix_router(state: Shared) -> Router {
    router(state).layer(Extension(Transport::Unix))
}

/// Compare without an early exit, so timing does not leak the token.
fn token_matches(given: &[u8], expected: &[u8]) -> bool {
    if given.len() != expected.len() {
        return false;
    }
    given
        .iter()
        .zip(expected)
        .fold(0u8, |acc, (a, b)| acc | (a ^ b))
        == 0
}

fn is_websocket(req: &Request) -> bool {
    req.headers()
        .get(header::UPGRADE)
        .and_then(|v| v.to_str().ok())
        .is_some_and(|v| v.eq_ignore_ascii_case("websocket"))
}

/// The `token` query parameter, for WebSocket upgrades from a browser.
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

async fn require_token(State(token): State<Arc<str>>, mut req: Request, next: Next) -> Response {
    if !req.uri().path().starts_with("/api") {
        return next.run(req).await;
    }
    let header_token = req
        .headers()
        .get(header::AUTHORIZATION)
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.strip_prefix("Bearer "))
        .map(str::to_owned);
    let given = header_token.or_else(|| is_websocket(&req).then(|| query_token(&req)).flatten());
    match given {
        Some(given) if token_matches(given.as_bytes(), token.as_bytes()) => {
            req.extensions_mut().insert(Transport::Tcp);
            next.run(req).await
        }
        _ => ApiError::new(Code::Unauthorized, "missing or wrong bearer token").into_response(),
    }
}

/// The routes with bearer-token auth on `/api/*`. The dashboard assets stay
/// public, because a browser cannot send a header on page load. A WebSocket
/// upgrade may carry the token as `?token=`.
pub fn tcp_router(state: Shared, token: String) -> Router {
    let token: Arc<str> = token.into();
    router(state)
        .layer(Extension(Transport::Tcp))
        .layer(middleware::from_fn_with_state(token, require_token))
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
    let app = tcp_router(state, token);
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
        let app = || tcp_router(s.clone(), "secret".into());
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
        assert!(!token_matches(b"ab", b"abc"));
    }

    #[test]
    fn percent_decoding() {
        assert_eq!(percent_decode("a%2Fb+c"), "a/b c");
        assert_eq!(percent_decode("100%"), "100%");
        assert_eq!(percent_decode("%zz"), "%zz");
    }
}
