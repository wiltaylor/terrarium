//! `/api/hosts`: the host agent registry (PLAN §19, docs/host-protocol.md).
//! The dial-in WebSocket itself is `crate::hosts::connect`.

use axum::extract::{Path, State};
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use axum::routing::get;
use axum::{Json, Router};
use serde::Deserialize;
use serde_json::{Value, json};

use super::{ApiJson, Shared};
use crate::actor::Actor;
use crate::error::ApiResult;
use crate::events::EventDraft;
use crate::policy;
use crate::store::hosts::{self, Host, Issued};

pub fn routes() -> Router<Shared> {
    Router::new()
        .route("/hosts", get(list).post(create))
        .route("/hosts/{name}", get(show).delete(revoke))
        .route("/hosts/{name}/rotate", axum::routing::post(rotate))
}

/// A host row with its live link state.
fn view(state: &Shared, h: Host) -> Value {
    let live = state.executor.hosts.view(&h.name);
    let mut v = serde_json::to_value(&h).unwrap_or(Value::Null);
    v["host_id"] = v["id"].clone();
    v["status"] = if live.online { "online" } else { "offline" }.into();
    v["online"] = live.online.into();
    v["sessions"] = live.sessions.into();
    if let Some(info) = live.info {
        v["capabilities"] = serde_json::to_value(&info.capabilities).unwrap_or(Value::Null);
        v["os"] = info.os.clone().into();
        v["arch"] = info.arch.clone().into();
        v["version"] = info.version.into();
        v["max_sessions"] = info.max_sessions.into();
        if let Some(m) = info.max_running_targets {
            v["max_running_targets"] = m.into();
        }
        v["connected_at"] = info.connected_at.into();
    }
    // The flat summary for the dashboard and for policy checks.
    let caps: terrarium_core::protocol::HostCapabilities =
        serde_json::from_value(v["capabilities"].clone()).unwrap_or_default();
    v["summary"] = serde_json::to_value(caps.summary(
        v["os"].as_str().unwrap_or_default(),
        v["arch"].as_str().unwrap_or_default(),
    ))
    .unwrap_or(Value::Null);
    v
}

/// `GET /api/hosts`: every host agent, with capabilities and link state.
async fn list(State(state): State<Shared>) -> ApiResult<Json<Vec<Value>>> {
    let rows = state.db.read(hosts::list).await?;
    Ok(Json(rows.into_iter().map(|h| view(&state, h)).collect()))
}

async fn show(State(state): State<Shared>, Path(name): Path<String>) -> ApiResult<Json<Value>> {
    let h = state.db.read(move |c| hosts::get(c, &name)).await?;
    Ok(Json(view(&state, h)))
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct NewHost {
    name: String,
    /// Hand out a one-time enrol token (10 minutes) in place of the
    /// long-lived token.
    #[serde(default)]
    enrol: bool,
}

#[derive(Debug, Default, Deserialize)]
#[serde(deny_unknown_fields)]
struct Rotate {
    #[serde(default)]
    enrol: bool,
}

/// The host view with the token that the answer carries once.
fn with_token(state: &Shared, host: Host, issued: Issued) -> Value {
    let mut v = view(state, host);
    v["token"] = issued.token.into();
    v["token_kind"] = issued.kind.into();
    if let Some(e) = issued.expires_at {
        v["token_expires_at"] = e.into();
    }
    v
}

/// `POST /api/hosts`: register a host agent. The answer carries the token
/// once; the daemon keeps only its hash. With `enrol`, the token is a
/// one-time enrol token that the host agent swaps for its long-lived
/// token on the first connect.
async fn create(
    State(state): State<Shared>,
    actor: Actor,
    ApiJson(body): ApiJson<NewHost>,
) -> ApiResult<Response> {
    policy::manage_hosts(&actor, "registering a host agent")?;
    let (host, issued) = state
        .db
        .write(actor.label(), move |t| {
            let (h, issued) = hosts::create(t, &body.name, body.enrol)?;
            t.audit(
                "host.create",
                &h.name,
                None,
                json!({"host_id": h.id, "token_kind": issued.kind}),
            )?;
            Ok((h, issued))
        })
        .await?;
    Ok((StatusCode::CREATED, Json(with_token(&state, host, issued))).into_response())
}

/// `POST /api/hosts/{name}/rotate`: a new token, shown once. The old
/// token stops working for new connections now; a live link stays up
/// until it ends. The body is optional: `{"enrol": true}` gives a one-time
/// enrol token.
async fn rotate(
    State(state): State<Shared>,
    actor: Actor,
    Path(name): Path<String>,
    body: axum::body::Bytes,
) -> ApiResult<Json<Value>> {
    policy::manage_hosts(&actor, "rotating a host agent token")?;
    let body: Rotate = if body.iter().all(u8::is_ascii_whitespace) {
        Rotate::default()
    } else {
        serde_json::from_slice(&body)
            .map_err(|e| crate::error::ApiError::bad_request(e.to_string()))?
    };
    let online = state.executor.hosts.is_online(&name);
    let (host, issued) = state
        .db
        .write(actor.label(), move |t| {
            let (h, issued) = hosts::rotate(t, &name, body.enrol)?;
            t.audit(
                "host.rotate",
                &h.name,
                None,
                json!({"host_id": h.id, "token_kind": issued.kind, "online": online}),
            )?;
            Ok((h, issued))
        })
        .await?;
    Ok(Json(with_token(&state, host, issued)))
}

/// `DELETE /api/hosts/{name}`: revoke the token. A live link gets
/// `goodbye token_revoked`; its sessions end.
async fn revoke(
    State(state): State<Shared>,
    actor: Actor,
    Path(name): Path<String>,
) -> ApiResult<StatusCode> {
    policy::manage_hosts(&actor, "revoking a host agent")?;
    let online = state.executor.hosts.is_online(&name);
    let n = name.clone();
    state
        .db
        .write(actor.label(), move |t| {
            let h = hosts::get(t, &n)?;
            hosts::delete(t, &n)?;
            t.audit("host.revoke", &n, None, json!({"host_id": h.id}))?;
            if online {
                t.emit(EventDraft::new(
                    "host.disconnected",
                    format!("host {n} disconnected: token revoked"),
                    json!({"host": n, "host_id": h.id, "reason": "token revoked"}),
                ))?;
            }
            Ok(())
        })
        .await?;
    state.executor.hosts.revoke(&name);
    Ok(StatusCode::NO_CONTENT)
}
