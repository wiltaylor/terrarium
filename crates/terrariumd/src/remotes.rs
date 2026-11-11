//! Network policy shared by pairing and every upstream HTTP/SSE/WS connection.
use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, AtomicI64};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use axum::http::{HeaderMap, HeaderValue, Method, header};
use reqwest::{Client, Url};
use serde::Deserialize;
use tokio::sync::{OwnedSemaphorePermit, Semaphore, watch};

use crate::error::{ApiError, ApiResult, Code};
use crate::store::remotes::Remote;
use crate::webhooks::net::{self, PrivateMode};

pub const JSON_MAX: usize = 16 * 1024 * 1024;
pub const UPLOAD_MAX: usize = 256 * 1024 * 1024;
pub const CONNECT: Duration = Duration::from_secs(30);
pub const PLAIN: Duration = Duration::from_secs(60);
pub const IDLE: Duration = Duration::from_secs(300);

pub fn unreachable() -> ApiError {
    ApiError::new(Code::RemoteUnreachable, "remote connection failed")
}

pub fn connection_error(e: reqwest::Error) -> ApiError {
    use std::error::Error;
    fn is_pin(e: &(dyn Error + 'static)) -> bool {
        if e.is::<terrarium_host::tls::FingerprintMismatch>() {
            return true;
        }
        if let Some(
            rustls::Error::Other(inner)
            | rustls::Error::InvalidCertificate(rustls::CertificateError::Other(inner)),
        ) = e.downcast_ref::<rustls::Error>()
            && inner.0.is::<terrarium_host::tls::FingerprintMismatch>()
        {
            return true;
        }
        if let Some(io) = e.downcast_ref::<std::io::Error>()
            && let Some(inner) = io.get_ref()
            && is_pin(inner)
        {
            return true;
        }
        e.source().is_some_and(is_pin)
    }
    if is_pin(&e) {
        let mut error = unreachable();
        error.details = Some(serde_json::json!({"status":"pin_mismatch"}));
        return error;
    }
    unreachable()
}

pub fn mismatch() -> ApiError {
    ApiError::new(Code::RemoteMismatch, "remote instance does not match")
}

pub fn check_name(name: &str) -> ApiResult<()> {
    if name.len() > 32
        || !name.as_bytes().first().is_some_and(u8::is_ascii_lowercase)
        || !name
            .bytes()
            .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'-')
    {
        return Err(ApiError::invalid(
            "remote name must match [a-z][a-z0-9-]{0,31}",
        ));
    }
    Ok(())
}

pub fn check_url(raw: &str) -> ApiResult<Url> {
    let u = net::check_url(raw).map_err(|_| ApiError::invalid("invalid remote URL"))?;
    if u.path() != "/" || u.query().is_some() || u.fragment().is_some() {
        return Err(ApiError::invalid(
            "remote URL must be an origin without a path, query or fragment",
        ));
    }
    if u.scheme() == "http"
        && !terrarium_host::tls::is_loopback_host(u.host_str().unwrap_or_default())
    {
        return Err(ApiError::invalid("plaintext remotes must use loopback"));
    }
    Ok(u)
}

/// Reject ambiguous syntax before constructing a URL (URL parsers normalize dots).
pub fn check_path(method: &Method, path: &str, ws: bool, read_only: bool) -> ApiResult<()> {
    let lower = path.to_ascii_lowercase();
    if !path.starts_with("/api/")
        || path.contains(['\\', '?', '#'])
        || path.bytes().any(|b| b <= 32 || b >= 127)
        || ["%2f", "%5c", "%25", "%2e", "%00"]
            .iter()
            .any(|x| lower.contains(x))
        || path
            .trim_start_matches('/')
            .split('/')
            .any(|s| matches!(s, "." | ".." | ""))
    {
        return Err(ApiError::bad_request("ambiguous proxy path"));
    }
    if crate::links::denied(method, path, ws, read_only).is_some() {
        return Err(ApiError::new(
            Code::LinkForbidden,
            "remote path is not allowed for a link",
        ));
    }
    if !matches!(
        *method,
        Method::GET | Method::POST | Method::PUT | Method::PATCH | Method::DELETE | Method::HEAD
    ) {
        return Err(ApiError::bad_request("proxy method is not supported"));
    }
    if ws
        && !(path == "/api/events"
            || (path.starts_with("/api/agents/")
                && (path.ends_with("/pty") || path.ends_with("/events"))
                && path.split('/').count() == 5))
    {
        return Err(ApiError::new(
            Code::LinkForbidden,
            "unsupported remote WebSocket",
        ));
    }
    Ok(())
}

/// Never forward credentials carried in the home's WebSocket query.
pub fn query(raw: Option<&str>) -> Option<String> {
    raw.map(|q| {
        url::form_urlencoded::parse(q.as_bytes())
            .filter(|(k, _)| !matches!(k.as_ref(), "token" | "access_token"))
            .fold(
                url::form_urlencoded::Serializer::new(String::new()),
                |mut s, (k, v)| {
                    s.append_pair(&k, &v);
                    s
                },
            )
            .finish()
    })
}

pub fn request_headers(input: &HeaderMap, key: &str, home: &str) -> ApiResult<HeaderMap> {
    let mut out = HeaderMap::new();
    for name in ["accept", "content-type", "last-event-id", "if-none-match"] {
        if let Some(v) = input.get(name) {
            out.insert(name, v.clone());
        }
    }
    let mut auth = HeaderValue::from_str(&format!("Bearer {key}")).map_err(|_| unreachable())?;
    auth.set_sensitive(true);
    out.insert(header::AUTHORIZATION, auth);
    out.insert(
        "x-terrarium-link",
        HeaderValue::from_str(home).map_err(|_| unreachable())?,
    );
    Ok(out)
}

pub fn response_headers(input: &HeaderMap, name: &str, empty: bool) -> ApiResult<HeaderMap> {
    if !empty || input.contains_key(header::CONTENT_TYPE) {
        let ct = input
            .get(header::CONTENT_TYPE)
            .and_then(|v| v.to_str().ok())
            .unwrap_or("");
        if !matches!(
            ct.split(';')
                .next()
                .unwrap_or("")
                .trim()
                .to_ascii_lowercase()
                .as_str(),
            "application/json" | "text/event-stream" | "application/octet-stream"
        ) {
            return Err(unreachable());
        }
    }
    let mut out = HeaderMap::new();
    for name in [
        "content-type",
        "x-next-cursor",
        "retry-after",
        "etag",
        "cache-control",
    ] {
        if let Some(v) = input.get(name) {
            out.insert(name, v.clone());
        }
    }
    if let Some(v) = input.get(header::CONTENT_DISPOSITION)
        && v.to_str().ok().is_some_and(|v| {
            v.split(';')
                .next()
                .unwrap_or("")
                .trim()
                .eq_ignore_ascii_case("attachment")
        })
    {
        out.insert(header::CONTENT_DISPOSITION, v.clone());
    }
    out.insert(
        "x-terrarium-remote",
        HeaderValue::from_str(name).map_err(|_| unreachable())?,
    );
    out.insert(
        header::CONTENT_SECURITY_POLICY,
        HeaderValue::from_static("default-src 'none'; frame-ancestors 'none'"),
    );
    out.insert(
        "x-content-type-options",
        HeaderValue::from_static("nosniff"),
    );
    Ok(out)
}

/// Resolve once, pin the address, disable environment proxies and redirects.
pub async fn client(remote: &Remote, own_ports: &[u16]) -> ApiResult<Client> {
    let u = check_url(&remote.url)?;
    let addr = tokio::time::timeout(
        CONNECT,
        net::resolve(&u, PrivateMode::from_env(), own_ports, None),
    )
    .await
    .map_err(|_| unreachable())?
    .map_err(|_| unreachable())?;
    if u.scheme() == "http" && !addr.ip().is_loopback() {
        return Err(unreachable());
    }
    let ca = remote
        .ca_pem
        .as_ref()
        .map(|pem| {
            use std::io::Write;
            let mut f = tempfile::NamedTempFile::new().map_err(|_| unreachable())?;
            f.write_all(pem.as_bytes()).map_err(|_| unreachable())?;
            Ok::<_, ApiError>(f)
        })
        .transpose()?;
    let tls = terrarium_host::tls::transport(
        &remote.url,
        &terrarium_host::tls::TlsSettings {
            allow_insecure: false,
            ca_file: ca.as_ref().map(|f| f.path()),
            server_fingerprint_sha256: remote.pin_sha256.as_deref(),
        },
    )
    .map_err(|_| ApiError::invalid("invalid remote TLS settings"))?;
    let mut b = Client::builder()
        .no_proxy()
        .redirect(reqwest::redirect::Policy::none())
        .connect_timeout(CONNECT)
        .pool_max_idle_per_host(0)
        .resolve(u.host_str().ok_or_else(unreachable)?, addr);
    if let Some(tls) = tls.tls {
        b = b.use_preconfigured_tls((*tls).clone());
    }
    b.build().map_err(|_| unreachable())
}

#[derive(Deserialize)]
pub struct Instance {
    pub id: String,
    pub version: String,
}

pub async fn limited(
    mut r: reqwest::Response,
    cap: usize,
    deadline: Duration,
) -> ApiResult<Vec<u8>> {
    tokio::time::timeout(deadline, async move {
        let mut out = Vec::new();
        while let Some(b) = r.chunk().await.map_err(|_| unreachable())? {
            if out.len().saturating_add(b.len()) > cap {
                return Err(unreachable());
            }
            out.extend_from_slice(&b);
        }
        Ok(out)
    })
    .await
    .map_err(|_| unreachable())?
}

pub async fn instance(c: &Client, r: &Remote, headers: &HeaderMap) -> ApiResult<Instance> {
    let resp = c
        .get(format!("{}api/system/instance", r.url))
        .headers(headers.clone())
        .timeout(PLAIN)
        .send()
        .await
        .map_err(connection_error)?;
    if resp.status() == reqwest::StatusCode::UNAUTHORIZED {
        return Err(ApiError::new(
            Code::Unauthorized,
            "remote link authentication failed",
        ));
    }
    if resp.status() != reqwest::StatusCode::OK {
        return Err(unreachable());
    }
    let id: Instance =
        serde_json::from_slice(&limited(resp, 16384, PLAIN).await?).map_err(|_| unreachable())?;
    if id.id != r.instance_id {
        return Err(mismatch());
    }
    Ok(id)
}

pub struct Limits {
    requests: Arc<Semaphore>,
    streams: Arc<Semaphore>,
    pub closed: watch::Sender<bool>,
    pub events: tokio::sync::Mutex<Option<Arc<EventFeed>>>,
}

pub struct EventFeed {
    pub frames: tokio::sync::broadcast::Sender<Arc<String>>,
    pub alive: AtomicBool,
    pub last_seq: AtomicI64,
}

impl Default for EventFeed {
    fn default() -> Self {
        Self {
            frames: tokio::sync::broadcast::channel(32).0,
            alive: AtomicBool::new(true),
            last_seq: AtomicI64::new(0),
        }
    }
}

impl Default for Limits {
    fn default() -> Self {
        Self {
            requests: Arc::new(Semaphore::new(64)),
            streams: Arc::new(Semaphore::new(8)),
            closed: watch::channel(false).0,
            events: tokio::sync::Mutex::default(),
        }
    }
}

impl Limits {
    pub fn acquire(&self, stream: bool) -> ApiResult<OwnedSemaphorePermit> {
        if *self.closed.borrow() {
            return Err(unreachable());
        }
        (if stream {
            &self.streams
        } else {
            &self.requests
        })
        .clone()
        .try_acquire_owned()
        .map_err(|_| ApiError::new(Code::RateLimited, "remote concurrency limit reached"))
    }
}

#[derive(Default)]
pub struct Runtime(Mutex<HashMap<String, Arc<Limits>>>);
impl Runtime {
    pub fn get(&self, id: &str) -> Arc<Limits> {
        self.0
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .entry(id.to_owned())
            .or_default()
            .clone()
    }
    pub fn close(&self, id: &str) {
        self.get(id).closed.send_replace(true);
    }
    pub fn reset(&self, id: &str) {
        let mut map = self.0.lock().unwrap_or_else(|e| e.into_inner());
        if let Some(l) = map.insert(id.to_owned(), Arc::default()) {
            l.closed.send_replace(true);
        }
    }
}
