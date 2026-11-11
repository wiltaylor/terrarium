//! The ingest listener (PLAN §12c, docs/api.md §20): a TCP listener of its
//! own, on `TERRARIUM_INGEST_ADDR`, for the webhooks of the event sources.
//! It serves only `POST /hooks/{project}/{source}` (`_` is the global
//! scope) and `GET /healthz`. The API and the dashboard are not on it, so
//! the operator can open this port to a forge without the API.
//!
//! Each request names its source in the path, and proves it with the
//! source's secret: an HMAC-SHA256 of the body (GitHub, Forgejo, generic)
//! or a bearer token (generic). An unknown or disabled source and a bad or
//! missing signature get the same `401` with no detail; the source keeps
//! the reason in `last_error` for its admin. A good request becomes one
//! event in the log, with the actor `source:<project>/<name>`.

use axum::body::Bytes;
use axum::extract::{DefaultBodyLimit, Path, State};
use axum::http::{HeaderMap, StatusCode, header};
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use axum::{Json, Router};
use hmac::{Hmac, KeyInit, Mac};
use serde::Deserialize;
use serde_json::{Value, json};
use sha2::Sha256;
use terrarium_core::ci::{self, Provider, Webhook};
use tokio::task::JoinHandle;
use tracing::{info, warn};

use crate::api::Shared;
use crate::error::{ApiError, Code};
use crate::events::EventDraft;
use crate::store::event_sources::{self, EventSource, SourceKind};

/// `TERRARIUM_INGEST_ADDR`: the address of the listener. Unset or empty:
/// no listener.
pub const ENV_ADDR: &str = "TERRARIUM_INGEST_ADDR";
/// `TERRARIUM_INGEST_PUBLIC_URL`: the listener as a forge reaches it, for
/// the `hook_url` of a source.
pub const ENV_PUBLIC_URL: &str = "TERRARIUM_INGEST_PUBLIC_URL";
/// The largest webhook body.
pub const MAX_BODY: usize = 1024 * 1024;

type HmacSha256 = Hmac<Sha256>;

/// The path of a source's hook: `/hooks/{project}/{name}`.
pub fn hook_path(s: &EventSource) -> String {
    format!("/hooks/{}", s.key())
}

/// The URL of a hook path, and a note when the daemon is not sure of it.
pub fn hook_url(path: &str) -> (Option<String>, Option<String>) {
    hook_url_from(path, |k| std::env::var(k).ok())
}

/// [`hook_url`] with a lookup, for tests.
pub fn hook_url_from(
    path: &str,
    var: impl Fn(&str) -> Option<String>,
) -> (Option<String>, Option<String>) {
    let set = |k: &str| {
        var(k)
            .map(|v| v.trim().to_owned())
            .filter(|v| !v.is_empty())
    };
    if let Some(base) = set(ENV_PUBLIC_URL) {
        return (Some(format!("{}{path}", base.trim_end_matches('/'))), None);
    }
    let Some(addr) = set(ENV_ADDR) else {
        return (
            None,
            Some(format!(
                "the ingest listener is off: set {ENV_ADDR} (and {ENV_PUBLIC_URL})"
            )),
        );
    };
    let (host, port) = addr.rsplit_once(':').unwrap_or((addr.as_str(), "80"));
    let host = match host {
        "0.0.0.0" | "[::]" | "" => "localhost",
        h => h,
    };
    (
        Some(format!("http://{host}:{port}{path}")),
        Some(format!(
            "made from {ENV_ADDR}; set {ENV_PUBLIC_URL} to the URL that the forge can reach"
        )),
    )
}

/// The routes of the listener.
pub fn router(state: Shared) -> Router {
    Router::new()
        .route("/hooks/{project}/{source}", post(hook))
        .route("/healthz", get(healthz))
        .fallback(not_found)
        .layer(DefaultBodyLimit::max(MAX_BODY))
        .with_state(state)
}

/// Serve the listener when `TERRARIUM_INGEST_ADDR` is set.
pub async fn serve_from_env(
    state: Shared,
) -> anyhow::Result<Option<(JoinHandle<()>, std::net::SocketAddr)>> {
    match std::env::var(ENV_ADDR).map(|a| a.trim().to_owned()) {
        Ok(addr) if !addr.is_empty() => Ok(Some(serve(state, &addr).await?)),
        _ => {
            info!("{ENV_ADDR} is not set: the ingest listener (webhooks) stays off");
            Ok(None)
        }
    }
}

/// Bind `addr` and serve the listener.
pub async fn serve(
    state: Shared,
    addr: &str,
) -> anyhow::Result<(JoinHandle<()>, std::net::SocketAddr)> {
    use anyhow::Context;
    let listener = tokio::net::TcpListener::bind(addr)
        .await
        .with_context(|| format!("cannot bind the ingest listener {addr}"))?;
    let local = listener.local_addr()?;
    info!(addr = %local, "ingest listener (webhooks) listening on TCP");
    state.webhooks.add_own_port(local.port());
    let app = router(state);
    Ok((
        tokio::spawn(async move {
            if let Err(e) = axum::serve(listener, app).await {
                tracing::error!("ingest listener failed: {e}");
            }
        }),
        local,
    ))
}

async fn healthz() -> &'static str {
    "ok"
}

async fn not_found() -> Response {
    (StatusCode::NOT_FOUND, "not found").into_response()
}

/// The answer to every auth failure: no detail.
fn unauthorized() -> Response {
    ApiError::new(Code::Unauthorized, "unauthorized").into_response()
}

fn answer(status: &str, extra: Value) -> Response {
    let mut v = json!({"status": status});
    if let (Value::Object(m), Value::Object(e)) = (&mut v, extra) {
        m.extend(e);
    }
    (StatusCode::OK, Json(v)).into_response()
}

fn header_str<'a>(h: &'a HeaderMap, names: &[&str]) -> Option<&'a str> {
    names
        .iter()
        .find_map(|n| h.get(*n).and_then(|v| v.to_str().ok()))
        .map(str::trim)
        .filter(|v| !v.is_empty())
}

fn unhex(s: &str) -> Option<Vec<u8>> {
    if !s.len().is_multiple_of(2) {
        return None;
    }
    (0..s.len())
        .step_by(2)
        .map(|i| u8::from_str_radix(s.get(i..i + 2)?, 16).ok())
        .collect()
}

/// Is `sig_hex` the HMAC-SHA256 of `body` with `secret`? The compare is
/// constant-time (`Mac::verify_slice`).
pub fn hmac_matches(secret: &str, body: &[u8], sig_hex: &str) -> bool {
    let Some(sig) = unhex(sig_hex.trim()) else {
        return false;
    };
    let Ok(mut mac) = HmacSha256::new_from_slice(secret.as_bytes()) else {
        return false;
    };
    mac.update(body);
    mac.verify_slice(&sig).is_ok()
}

/// The HMAC-SHA256 of `body` with `secret`, in hex: what a sender puts in
/// its signature header.
pub fn sign(secret: &str, body: &[u8]) -> String {
    match HmacSha256::new_from_slice(secret.as_bytes()) {
        Ok(mut mac) => {
            mac.update(body);
            terrarium_host::protocol::hex(&mac.finalize().into_bytes())
        }
        Err(_) => String::new(),
    }
}

/// Does the request carry a good signature for its source kind?
pub fn verify(kind: SourceKind, secret: &str, h: &HeaderMap, body: &[u8]) -> bool {
    let prefixed = |name: &str| {
        header_str(h, &[name])
            .and_then(|v| v.strip_prefix("sha256="))
            .is_some_and(|sig| hmac_matches(secret, body, sig))
    };
    match kind {
        SourceKind::Github => prefixed("x-hub-signature-256"),
        SourceKind::Forgejo => match header_str(h, &["x-forgejo-signature", "x-gitea-signature"]) {
            Some(sig) => hmac_matches(secret, body, sig),
            None => prefixed("x-hub-signature-256"),
        },
        SourceKind::Generic => {
            if header_str(h, &["x-terrarium-signature"]).is_some() {
                return prefixed("x-terrarium-signature");
            }
            header_str(h, &[header::AUTHORIZATION.as_str()])
                .and_then(|v| v.strip_prefix("Bearer "))
                .is_some_and(|t| crate::api::token_matches(t.trim().as_bytes(), secret.as_bytes()))
        }
    }
}

/// The body of a generic source.
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct GenericBody {
    kind: String,
    #[serde(default)]
    summary: Option<String>,
    #[serde(default)]
    data: Option<Value>,
}

/// Keep why a request failed, for the source's admin. In the background,
/// so a failure answers as fast as an unknown source.
fn note_error(state: &Shared, s: &EventSource, error: &str) {
    let (state, id, error) = (state.clone(), s.id.clone(), error.to_owned());
    tokio::spawn(async move {
        let r = state
            .db
            .write("system", move |t| event_sources::mark_error(t, &id, &error))
            .await;
        if let Err(e) = r {
            warn!("cannot note an event source error: {e}");
        }
    });
}

/// A request that is authentic but not usable: `400`, with the reason.
fn rejected(state: &Shared, s: &EventSource, status: StatusCode, error: &str) -> Response {
    note_error(state, s, error);
    (status, Json(json!({"status": "rejected", "error": error}))).into_response()
}

async fn hook(
    State(state): State<Shared>,
    Path((project, name)): Path<(String, String)>,
    headers: HeaderMap,
    body: Bytes,
) -> Response {
    let found = state
        .db
        .read(move |c| event_sources::for_ingest(c, &project, &name))
        .await;
    let (source, secret) = match found {
        Ok(Some(found)) => found,
        Ok(None) => return unauthorized(),
        Err(e) => {
            warn!("ingest: cannot read the event sources: {e}");
            return StatusCode::INTERNAL_SERVER_ERROR.into_response();
        }
    };
    if !source.enabled {
        return unauthorized();
    }
    if header_str(&headers, &[header::CONTENT_TYPE.as_str()])
        .is_some_and(|ct| ct.contains("x-www-form-urlencoded"))
    {
        return rejected(
            &state,
            &source,
            StatusCode::UNSUPPORTED_MEDIA_TYPE,
            "the sender posts application/x-www-form-urlencoded; set its content type to \
             application/json",
        );
    }
    if !verify(source.kind, &secret, &headers, &body) {
        note_error(&state, &source, "bad or missing signature");
        return unauthorized();
    }
    let payload: Value = match serde_json::from_slice(&body) {
        Ok(v) => v,
        Err(e) => {
            return rejected(
                &state,
                &source,
                StatusCode::BAD_REQUEST,
                &format!("the body is not JSON: {e}"),
            );
        }
    };
    let (event_header, delivery) = match source.kind {
        SourceKind::Github => (
            header_str(&headers, &["x-github-event"]),
            header_str(&headers, &["x-github-delivery"]),
        ),
        SourceKind::Forgejo => (
            header_str(&headers, &["x-forgejo-event", "x-gitea-event"]),
            header_str(&headers, &["x-forgejo-delivery", "x-gitea-delivery"]),
        ),
        SourceKind::Generic => (None, header_str(&headers, &["x-terrarium-delivery"])),
    };
    let delivery = delivery.map(|d| ci::clean(d, 200));
    if source.kind == SourceKind::Github
        && event_header == Some("issues")
        && matches!(payload["action"].as_str(), Some("opened" | "reopened"))
    {
        let Some(project) = source.project.clone() else {
            return rejected(&state, &source, StatusCode::BAD_REQUEST, "issue imports need a project-scoped source");
        };
        let id = source.id.clone();
        let result = state.db.write(source.actor(), move |t| {
            if let Some(d) = &delivery
                && !event_sources::record_delivery(t, &id, d)?
            {
                event_sources::mark_seen(t, &id, false)?;
                return Ok(None);
            }
            let card = crate::github_issues::import_webhook(t, &project, payload)?;
            event_sources::mark_seen(t, &id, true)?;
            Ok(Some(card))
        }).await;
        return match result {
            Ok(Some(card)) => answer("accepted", json!({"card": card.id})),
            Ok(None) => answer("duplicate", json!({})),
            Err(e) => rejected(&state, &source, StatusCode::BAD_REQUEST, &e.message),
        };
    }
    let event = match source.kind {
        SourceKind::Generic => match generic_event(payload) {
            Ok(e) => e,
            Err(msg) => return rejected(&state, &source, StatusCode::BAD_REQUEST, &msg),
        },
        kind => {
            let provider = if kind == SourceKind::Github {
                Provider::Github
            } else {
                Provider::Forgejo
            };
            let Some(event_header) = event_header else {
                return rejected(
                    &state,
                    &source,
                    StatusCode::BAD_REQUEST,
                    "no event header (X-GitHub-Event, X-Forgejo-Event)",
                );
            };
            match ci::normalize(provider, event_header, &payload) {
                Webhook::Event(e) => e,
                Webhook::Ping => {
                    seen(&state, &source).await;
                    return answer("pong", json!({}));
                }
                Webhook::Ignored(reason) => {
                    seen(&state, &source).await;
                    return answer("ignored", json!({"reason": reason}));
                }
            }
        }
    };
    let actor = source.actor();
    let id = source.id.clone();
    let key = source.key();
    let project = source.project.clone();
    let result = state
        .db
        .write(actor, move |t| {
            if let Some(d) = &delivery
                && !event_sources::record_delivery(t, &id, d)?
            {
                event_sources::mark_seen(t, &id, false)?;
                return Ok(None);
            }
            let mut data = event.data;
            if let Value::Object(m) = &mut data {
                m.insert("source".into(), json!(key));
                m.insert("delivery".into(), json!(delivery));
            }
            let mut draft = EventDraft::new(event.kind, event.summary, data);
            if let Some(p) = project {
                draft = draft.project(p);
            }
            let stored = t.emit(draft)?;
            event_sources::mark_seen(t, &id, true)?;
            Ok(Some(stored))
        })
        .await;
    match result {
        Ok(Some(e)) => answer("accepted", json!({"seq": e.seq, "kind": e.kind})),
        Ok(None) => answer("duplicate", json!({})),
        Err(e) => {
            warn!(source = %source.key(), "ingest: cannot add the event: {e}");
            StatusCode::INTERNAL_SERVER_ERROR.into_response()
        }
    }
}

/// A ping or an ignored webhook: the source was seen, no event.
async fn seen(state: &Shared, source: &EventSource) {
    let id = source.id.clone();
    if let Err(e) = state
        .db
        .write("system", move |t| event_sources::mark_seen(t, &id, false))
        .await
    {
        warn!("cannot note an event source delivery: {e}");
    }
}

/// The event of a generic source: `{kind, summary?, data?}`. The kind is
/// one of [`ci::EXTERNAL_PREFIXES`], so a script cannot pose as the daemon.
fn generic_event(payload: Value) -> Result<ci::CiEvent, String> {
    let b: GenericBody = serde_json::from_value(payload)
        .map_err(|e| format!("bad body: {e}; send {{kind, summary?, data?}}"))?;
    let kind = b.kind.trim().to_owned();
    if !ci::valid_kind(&kind) || !ci::external_kind(&kind) {
        return Err(format!(
            "bad kind `{}`: use a kind that starts with {}",
            ci::clean(&kind, 100),
            ci::EXTERNAL_PREFIXES.join(", ")
        ));
    }
    let mut data = b.data.unwrap_or_else(|| json!({}));
    let Value::Object(m) = &mut data else {
        return Err("data: use a JSON object".into());
    };
    m.insert("provider".into(), json!("generic"));
    let summary = ci::clean(b.summary.as_deref().unwrap_or(&kind), ci::MAX_SUMMARY);
    Ok(ci::CiEvent {
        kind,
        summary,
        data,
    })
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::*;

    const SECRET: &str = "0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef";

    fn headers(pairs: &[(&str, &str)]) -> HeaderMap {
        let mut h = HeaderMap::new();
        for (k, v) in pairs {
            h.insert(
                axum::http::HeaderName::from_bytes(k.as_bytes()).unwrap(),
                v.parse().unwrap(),
            );
        }
        h
    }

    #[test]
    fn signatures_per_provider() {
        let body = br#"{"zen":"Design for failure."}"#;
        let sig = sign(SECRET, body);
        // The GitHub test vector of "validating webhook deliveries".
        assert_eq!(
            sign("It's a Secret to Everybody", b"Hello, World!"),
            "757107ea0eb2509fc211221cce984b8a37570b6d7586c22c46f4379c8b043e17"
        );
        let good = format!("sha256={sig}");
        let bad = format!("sha256={}", sign("other", body));

        let gh = |h: &HeaderMap| verify(SourceKind::Github, SECRET, h, body);
        assert!(gh(&headers(&[("x-hub-signature-256", &good)])));
        assert!(!gh(&headers(&[("x-hub-signature-256", &bad)])));
        assert!(
            !gh(&headers(&[("x-hub-signature-256", &sig)])),
            "the prefix is needed"
        );
        assert!(!gh(&headers(&[])));
        assert!(!verify(
            SourceKind::Github,
            SECRET,
            &headers(&[("x-hub-signature-256", &good)]),
            b"{}"
        ));

        let fj = |h: &HeaderMap| verify(SourceKind::Forgejo, SECRET, h, body);
        assert!(fj(&headers(&[("x-forgejo-signature", &sig)])));
        assert!(fj(&headers(&[("x-gitea-signature", &sig)])));
        assert!(fj(&headers(&[("x-hub-signature-256", &good)])));
        assert!(!fj(&headers(&[("x-forgejo-signature", &bad[7..])])));
        assert!(!fj(&headers(&[("x-forgejo-signature", "zz")])));
        assert!(!fj(&headers(&[])));

        let generic = |h: &HeaderMap| verify(SourceKind::Generic, SECRET, h, body);
        assert!(generic(&headers(&[("x-terrarium-signature", &good)])));
        assert!(!generic(&headers(&[("x-terrarium-signature", &bad)])));
        let bearer = format!("Bearer {SECRET}");
        assert!(generic(&headers(&[("authorization", &bearer)])));
        assert!(!generic(&headers(&[("authorization", "Bearer nope")])));
        assert!(!generic(&headers(&[("authorization", SECRET)])));
        // A bad signature header is not saved by a good bearer token.
        assert!(!generic(&headers(&[
            ("x-terrarium-signature", &bad),
            ("authorization", &bearer)
        ])));
        assert!(!generic(&headers(&[])));
    }

    #[test]
    fn hook_urls() {
        let none = |_: &str| None;
        let (url, note) = hook_url_from("/hooks/p/s", none);
        assert!(url.is_none() && note.unwrap().contains(ENV_ADDR));
        let (url, note) = hook_url_from("/hooks/p/s", |k| {
            (k == ENV_ADDR).then(|| "0.0.0.0:8083".to_owned())
        });
        assert_eq!(url.as_deref(), Some("http://localhost:8083/hooks/p/s"));
        assert!(note.unwrap().contains(ENV_PUBLIC_URL));
        let (url, note) = hook_url_from("/hooks/p/s", |k| {
            (k == ENV_PUBLIC_URL).then(|| "https://hooks.example.com/".to_owned())
        });
        assert_eq!(url.as_deref(), Some("https://hooks.example.com/hooks/p/s"));
        assert!(note.is_none());
    }

    #[test]
    fn generic_bodies() {
        let e = generic_event(
            json!({"kind": "ci.run.failed", "summary": "nightly\nfailed",
                                     "data": {"branch": "main"}}),
        )
        .unwrap();
        assert_eq!(e.kind, "ci.run.failed");
        assert_eq!(e.summary, "nightly failed");
        assert_eq!(e.data["provider"], "generic");
        assert!(generic_event(json!({"kind": "status"})).is_err());
        assert!(generic_event(json!({"kind": "mail"})).is_err());
        assert!(generic_event(json!({"kind": "ci.run", "data": [1]})).is_err());
        assert!(generic_event(json!({"kind": "ci.run", "extra": 1})).is_err());
    }
}
