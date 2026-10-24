//! The HTTP API. The same router serves the Unix socket (no token) and the
//! TCP listener (bearer token on `/api/*`).

use std::os::unix::fs::PermissionsExt;
use std::path::Path;
use std::sync::{Arc, Mutex};
use std::time::Instant;

use anyhow::Context;
use axum::extract::{Query, Request, State};
use axum::http::{StatusCode, header};
use axum::middleware::{self, Next};
use axum::response::{IntoResponse, Response};
use axum::routing::get;
use axum::{Json, Router};
use serde::{Deserialize, Serialize};
use terrarium_core::Agent;
use terrarium_core::config::Definitions;
use tokio::task::JoinHandle;
use tracing::{error, info};

use crate::store::AgentStore;

/// Shared daemon state.
pub struct AppState {
    pub started: Instant,
    pub started_at: chrono::DateTime<chrono::Utc>,
    /// The database. Not read by any route yet.
    #[allow(dead_code)]
    pub db: Mutex<rusqlite::Connection>,
    pub agents: AgentStore,
    pub definitions: Definitions,
}

impl AppState {
    pub fn new(db: rusqlite::Connection, definitions: Definitions) -> Self {
        Self {
            started: Instant::now(),
            started_at: chrono::Utc::now(),
            db: Mutex::new(db),
            agents: AgentStore::default(),
            definitions,
        }
    }
}

type Shared = Arc<AppState>;

/// The body of `GET /api/health`.
#[derive(Debug, Serialize, Deserialize)]
pub struct Health {
    pub status: String,
    pub version: String,
    /// Seconds since the daemon started.
    pub uptime: u64,
    pub started_at: chrono::DateTime<chrono::Utc>,
    pub definitions: DefinitionCounts,
}

#[derive(Debug, Serialize, Deserialize)]
pub struct DefinitionCounts {
    pub projects: usize,
    pub agents: usize,
    pub targets: usize,
    pub subscriptions: usize,
}

#[derive(Debug, Serialize)]
struct ApiError {
    error: String,
}

fn api_error(status: StatusCode, message: &str) -> Response {
    (
        status,
        Json(ApiError {
            error: message.to_owned(),
        }),
    )
        .into_response()
}

async fn health(State(state): State<Shared>) -> Json<Health> {
    let d = &state.definitions;
    Json(Health {
        status: "ok".into(),
        version: terrarium_core::VERSION.into(),
        uptime: state.started.elapsed().as_secs(),
        started_at: state.started_at,
        definitions: DefinitionCounts {
            projects: d.projects.len(),
            agents: d.agents.len(),
            targets: d.targets.len(),
            subscriptions: d.subscriptions.len(),
        },
    })
}

#[derive(Debug, Deserialize)]
struct AgentQuery {
    project: Option<String>,
}

async fn list_agents(State(state): State<Shared>, Query(q): Query<AgentQuery>) -> Json<Vec<Agent>> {
    Json(state.agents.list(q.project.as_deref()))
}

async fn api_not_found() -> Response {
    api_error(StatusCode::NOT_FOUND, "no such API route")
}

/// The routes, without auth.
pub fn router(state: Shared) -> Router {
    let api = Router::new()
        .route("/health", get(health))
        .route("/agents", get(list_agents))
        .fallback(api_not_found);
    Router::new()
        .nest("/api", api)
        .fallback(crate::assets::serve)
        .with_state(state)
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

async fn require_token(State(token): State<Arc<str>>, req: Request, next: Next) -> Response {
    if !req.uri().path().starts_with("/api") {
        return next.run(req).await;
    }
    let given = req
        .headers()
        .get(header::AUTHORIZATION)
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.strip_prefix("Bearer "));
    match given {
        Some(given) if token_matches(given.as_bytes(), token.as_bytes()) => next.run(req).await,
        _ => api_error(StatusCode::UNAUTHORIZED, "missing or wrong bearer token"),
    }
}

/// The routes with bearer-token auth on `/api/*`. The dashboard assets stay
/// public, because a browser cannot send a header on page load.
pub fn tcp_router(state: Shared, token: String) -> Router {
    let token: Arc<str> = token.into();
    router(state).layer(middleware::from_fn_with_state(token, require_token))
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
    let app = router(state);
    Ok(tokio::spawn(async move {
        if let Err(e) = axum::serve(listener, app).await {
            error!("Unix socket server failed: {e}");
        }
    }))
}

/// Bind the TCP listener and serve on it in the background.
pub async fn serve_tcp(state: Shared, addr: &str, token: String) -> anyhow::Result<JoinHandle<()>> {
    let listener = tokio::net::TcpListener::bind(addr)
        .await
        .with_context(|| format!("cannot bind {addr}"))?;
    info!(addr = %listener.local_addr()?, "API and dashboard listening on TCP");
    let app = tcp_router(state, token);
    Ok(tokio::spawn(async move {
        if let Err(e) = axum::serve(listener, app).await {
            error!("TCP server failed: {e}");
        }
    }))
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::*;
    use axum::body::Body;
    use http_body_util::BodyExt;
    use terrarium_core::HarnessId;
    use terrarium_core::config::AgentDef;
    use tower::ServiceExt;

    fn state() -> Shared {
        let mut conn = rusqlite::Connection::open_in_memory().unwrap();
        crate::db::migrate(&mut conn).unwrap();
        Arc::new(AppState::new(conn, Definitions::default()))
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

    #[tokio::test]
    async fn agents_list_filters_by_project() {
        let s = state();
        let (status, body) = get(router(s.clone()), "/api/agents", None).await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(body, b"[]");

        let def: AgentDef = toml_def("a", "p1");
        s.agents.insert(Agent::from_def(&def, Path::new("/state")));
        s.agents
            .insert(Agent::from_def(&toml_def("b", "p2"), Path::new("/state")));
        let (_, body) = get(router(s.clone()), "/api/agents?project=p1", None).await;
        let agents: Vec<Agent> = serde_json::from_slice(&body).unwrap();
        assert_eq!(agents.len(), 1);
        assert_eq!(agents[0].name, "a");
        assert_eq!(agents[0].harness, HarnessId::Claude);
    }

    fn toml_def(name: &str, project: &str) -> AgentDef {
        toml::from_str(&format!(
            "name = \"{name}\"\nproject = \"{project}\"\nharness = \"claude\"\n"
        ))
        .unwrap()
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
    }

    #[tokio::test]
    async fn unknown_api_route_is_json_404() {
        let (status, body) = get(router(state()), "/api/nope", None).await;
        assert_eq!(status, StatusCode::NOT_FOUND);
        assert!(String::from_utf8_lossy(&body).contains("error"));
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
}
