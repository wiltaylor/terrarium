//! Operator-only home proxy. The browser never receives a link key.
use std::sync::Arc;
use std::time::Instant;

use axum::body::{Body, to_bytes};
use axum::extract::{FromRequestParts, Path, Request, State, WebSocketUpgrade};
use axum::http::{HeaderMap, StatusCode, header};
use axum::response::{IntoResponse, Response};
use axum::routing::{any, get, post};
use axum::{Json, Router};
use futures_util::{SinkExt, StreamExt};
use serde::Deserialize;
use serde_json::json;
use tokio_tungstenite::tungstenite::{self, protocol::Role};

use super::{ApiJson, Shared};
use crate::actor::Actor;
use crate::error::{ApiError, ApiResult, Code};
use crate::remotes::{self as net, Limits};
use crate::store::remotes::{self as store, Remote};

pub fn routes() -> Router<Shared> {
    Router::new()
        .route("/remotes", get(list).post(add))
        .route("/remotes/{name}", get(show).delete(remove))
        .route("/remotes/{name}/repin", post(repin))
        .route("/remotes/{name}/pair", post(repair))
        .route("/remotes/{name}/proxy/{*rest}", any(proxy))
}

fn operator(actor: &Actor) -> ApiResult<()> {
    crate::links::refuse_for_link("use remotes")?;
    crate::policy::operator_only(actor, "using remotes")
}

async fn load(s: &Shared, name: String) -> ApiResult<Remote> {
    s.db.read(move |c| store::get(c, &name)).await
}

async fn list(State(s): State<Shared>, actor: Actor) -> ApiResult<Json<Vec<Remote>>> {
    operator(&actor)?;
    Ok(Json(s.db.read(store::list).await?))
}

async fn show(
    State(s): State<Shared>,
    actor: Actor,
    Path(name): Path<String>,
) -> ApiResult<Json<Remote>> {
    operator(&actor)?;
    Ok(Json(load(&s, name).await?))
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct NewRemote {
    name: String,
    url: String,
    instance_id: String,
    code: String,
    pin_sha256: Option<String>,
    ca_pem: Option<String>,
    label: Option<String>,
}

fn own_ports(s: &Shared) -> Vec<u16> {
    // Include configured public endpoints as well as auxiliary listeners.
    [
        s.web.public_url.as_deref(),
        s.web.artifact_public_url.as_deref(),
        s.web.git_proxy_listen.as_deref(),
        Some(s.artifact_listen.as_str()),
    ]
    .into_iter()
    .flatten()
    .filter_map(|raw| {
        raw.parse::<std::net::SocketAddr>()
            .ok()
            .map(|a| a.port())
            .or_else(|| {
                url::Url::parse(raw)
                    .ok()
                    .and_then(|u| u.port_or_known_default())
            })
    })
    .collect()
}

async fn key(s: &Shared, r: &Remote) -> ApiResult<zeroize::Zeroizing<String>> {
    let id = r.id.clone();
    let sealed = s.db.read(move |c| store::sealed(c, &id)).await?;
    let plain = crate::secrets::key(&s.state_dir)?
        .open(&sealed, &store::aad(&r.id))
        .ok_or_else(net::unreachable)?;
    Ok(zeroize::Zeroizing::new(
        String::from_utf8(plain.to_vec()).map_err(|_| net::unreachable())?,
    ))
}

#[derive(Deserialize)]
struct Paired {
    key: String,
    instance: net::Instance,
    read_only: bool,
}

async fn pair(s: &Shared, r: &mut Remote, code: String) -> ApiResult<Vec<u8>> {
    if !code.starts_with("tlp_") || code.len() != 68 {
        return Err(ApiError::invalid("invalid pairing code"));
    }
    let c = net::client(r, &own_ports(s)).await?;
    let body = zeroize::Zeroizing::new(serde_json::to_vec(&json!({
        "code": code, "home_instance_id": s.links.instance_id(&s.state_dir)?,
    }))?);
    let resp = c
        .post(format!("{}api/links/pair", r.url))
        .header(header::CONTENT_TYPE, "application/json")
        .body(body.to_vec())
        .timeout(net::PLAIN)
        .send()
        .await
        .map_err(net::connection_error)?;
    if resp.status() != StatusCode::OK {
        return Err(net::unreachable());
    }
    let bytes = zeroize::Zeroizing::new(net::limited(resp, 16384, net::PLAIN).await?);
    let mut p: Paired = serde_json::from_slice(&bytes).map_err(|_| net::unreachable())?;
    let secret = zeroize::Zeroizing::new(std::mem::take(&mut p.key));
    if p.instance.id != r.instance_id {
        return Err(net::mismatch());
    }
    if !secret.starts_with("tlk_") || secret.len() != 68 {
        return Err(net::unreachable());
    }
    r.read_only = p.read_only;
    crate::secrets::key(&s.state_dir)?.seal(secret.as_bytes(), &store::aad(&r.id))
}

async fn add(
    State(s): State<Shared>,
    actor: Actor,
    ApiJson(b): ApiJson<NewRemote>,
) -> ApiResult<Response> {
    operator(&actor)?;
    net::check_name(&b.name)?;
    b.instance_id
        .parse::<ulid::Ulid>()
        .map_err(|_| ApiError::invalid("instance_id must be a ULID"))?;
    if b.instance_id == s.links.instance_id(&s.state_dir)? {
        return Err(ApiError::invalid("cannot link to this instance"));
    }
    let url = net::check_url(&b.url)?.to_string();
    let name = b.name.clone();
    if s.db
        .read(move |c| Ok(store::list(c)?.iter().any(|r| r.name == name)))
        .await?
    {
        return Err(ApiError::conflict("remote name is already used"));
    }
    let label = b.label.unwrap_or_else(|| b.name.clone());
    if label.len() > 64 || label.chars().any(char::is_control) {
        return Err(ApiError::invalid("invalid remote label"));
    }
    let mut r = Remote {
        id: ulid::Ulid::new().to_string(),
        name: b.name,
        url,
        instance_id: b.instance_id,
        pin_sha256: b.pin_sha256,
        ca_pem: b.ca_pem,
        label,
        read_only: false,
        created_at: crate::db::now(),
        last_ok_at: None,
        last_error: None,
        status: "offline".into(),
    };
    let sealed = pair(&s, &mut r, b.code).await?;
    let result = r.clone();
    s.db.write(actor.label(), move |t| {
        store::insert(t, &r, &sealed)?;
        t.audit(
            "remote.add",
            &r.name,
            None,
            json!({"instance_id": r.instance_id}),
        )?;
        Ok(())
    })
    .await?;
    Ok((StatusCode::CREATED, Json(result)).into_response())
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Repin {
    pin_sha256: Option<String>,
    ca_pem: Option<String>,
}

async fn repin(
    State(s): State<Shared>,
    actor: Actor,
    Path(name): Path<String>,
    ApiJson(b): ApiJson<Repin>,
) -> ApiResult<StatusCode> {
    operator(&actor)?;
    let mut r = load(&s, name).await?;
    r.pin_sha256 = b.pin_sha256;
    r.ca_pem = b.ca_pem;
    let c = net::client(&r, &own_ports(&s)).await?;
    let h = net::request_headers(
        &HeaderMap::new(),
        &key(&s, &r).await?,
        &s.links.instance_id(&s.state_dir)?,
    )?;
    net::instance(&c, &r, &h).await?;
    s.remotes.close(&r.id);
    let id = r.id.clone();
    s.db.write(actor.label(), move |t| {
        t.execute(
            "UPDATE remotes SET pin_sha256=?2,ca_pem=?3 WHERE id=?1",
            rusqlite::params![r.id, r.pin_sha256, r.ca_pem],
        )?;
        t.audit("remote.repin", &r.name, None, json!({}))
    })
    .await?;
    s.remotes.reset(&id);
    Ok(StatusCode::NO_CONTENT)
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Repair {
    code: String,
}

async fn repair(
    State(s): State<Shared>,
    actor: Actor,
    Path(name): Path<String>,
    ApiJson(b): ApiJson<Repair>,
) -> ApiResult<StatusCode> {
    operator(&actor)?;
    let mut r = load(&s, name).await?;
    let sealed = pair(&s, &mut r, b.code).await?;
    s.remotes.close(&r.id);
    let id = r.id.clone();
    s.db.write(actor.label(), move |t| {
        t.execute(
            "UPDATE remotes SET sealed=?2,read_only=?3 WHERE id=?1",
            rusqlite::params![r.id, sealed, r.read_only],
        )?;
        t.audit("remote.pair", &r.name, None, json!({}))
    })
    .await?;
    s.remotes.reset(&id);
    Ok(StatusCode::NO_CONTENT)
}

async fn remove(
    State(s): State<Shared>,
    actor: Actor,
    Path(name): Path<String>,
) -> ApiResult<Json<serde_json::Value>> {
    operator(&actor)?;
    let r = load(&s, name).await?;
    s.remotes.close(&r.id);
    let revoked = async {
        let c = net::client(&r, &own_ports(&s)).await?;
        let h = net::request_headers(
            &HeaderMap::new(),
            &key(&s, &r).await?,
            &s.links.instance_id(&s.state_dir)?,
        )?;
        net::instance(&c, &r, &h).await?;
        let resp = c
            .delete(format!("{}api/links/self", r.url))
            .headers(h)
            .timeout(net::PLAIN)
            .send()
            .await
            .map_err(|_| net::unreachable())?;
        Ok::<_, ApiError>(resp.status().is_success())
    }
    .await
    .unwrap_or(false);
    s.db.write(actor.label(), move |t| {
        t.execute("DELETE FROM remotes WHERE id=?1", [&r.id])?;
        t.audit("remote.remove", &r.name, None, json!({"revoked": revoked}))
    })
    .await?;
    Ok(Json(
        json!({"revoked": revoked, "revoke_on_remote": !revoked}),
    ))
}

async fn mark(s: &Shared, r: &Remote, status: &str) -> ApiResult<()> {
    let r = r.clone();
    let status = status.to_owned();
    s.db.write("operator", move |t| {
        let old = store::get(t, &r.name)?;
        store::mark(t, &r.id, &status)?;
        if old.status != status {
            t.emit(crate::events::EventDraft::new(
                "remote",
                "remote status changed",
                json!({"name": r.name, "status": status}),
            ))?;
        }
        Ok(())
    })
    .await
}

async fn proxy(
    State(s): State<Shared>,
    actor: Actor,
    Path((name, _rest)): Path<(String, String)>,
    req: Request,
) -> ApiResult<Response> {
    operator(&actor)?;
    let r = load(&s, name).await?;
    let prefix = format!("/api/remotes/{}/proxy", r.name);
    let path = req
        .uri()
        .path()
        .strip_prefix(&prefix)
        .ok_or_else(|| ApiError::bad_request("bad proxy path"))?
        .to_owned();
    let ws = req
        .headers()
        .get(header::UPGRADE)
        .is_some_and(|v| v.as_bytes().eq_ignore_ascii_case(b"websocket"));
    net::check_path(req.method(), &path, ws, r.read_only)?;
    let limits = s.remotes.get(&r.id);
    let mut closed = limits.closed.subscribe();
    if *closed.borrow() {
        return Err(net::unreachable());
    }
    let result = tokio::select! {
        _ = closed.changed() => Err(net::unreachable()),
        result = forward(&s, &r, actor, path, ws, req) => result,
    };
    if let Err(e) = &result {
        let status = if e
            .details
            .as_ref()
            .is_some_and(|d| d["status"] == "pin_mismatch")
        {
            "pin_mismatch"
        } else {
            match e.code {
                Code::RemoteMismatch => "mismatch",
                Code::Unauthorized => "auth_failed",
                _ => "offline",
            }
        };
        mark(&s, &r, status).await?;
    }
    result
}

async fn forward(
    s: &Shared,
    r: &Remote,
    actor: Actor,
    path: String,
    ws: bool,
    req: Request,
) -> ApiResult<Response> {
    let start = Instant::now();
    let method = req.method().clone();
    let limits = s.remotes.get(&r.id);
    let permit = limits.acquire(false)?;
    let c = net::client(r, &own_ports(s)).await?;
    let headers = net::request_headers(
        req.headers(),
        &key(s, r).await?,
        &s.links.instance_id(&s.state_dir)?,
    )?;
    let id = net::instance(&c, r, &headers).await?;
    let mut url = url::Url::parse(&r.url).map_err(|_| net::unreachable())?;
    url.set_path(&path);
    url.set_query(net::query(req.uri().query()).as_deref());
    let mut outgoing = c.request(method.clone(), url).headers(headers);
    let mut ws_key = None;
    if ws {
        let k = tungstenite::handshake::client::generate_key();
        outgoing = outgoing
            .header(header::CONNECTION, "Upgrade")
            .header(header::UPGRADE, "websocket")
            .header("sec-websocket-version", "13")
            .header("sec-websocket-key", &k);
        ws_key = Some(k);
    }
    let (mut parts, body) = req.into_parts();
    let body = tokio::time::timeout(net::PLAIN, to_bytes(body, net::UPLOAD_MAX))
        .await
        .map_err(|_| ApiError::bad_request("upload timeout"))?
        .map_err(|_| ApiError::new(Code::TooLarge, "remote upload exceeds 256 MiB"))?;
    let resp = tokio::time::timeout(net::PLAIN, outgoing.body(body).send())
        .await
        .map_err(|_| net::unreachable())?
        .map_err(net::connection_error)?;
    let status = resp.status();
    if status.is_redirection() && status != StatusCode::SWITCHING_PROTOCOLS {
        return Err(net::unreachable());
    }
    let state = if status == StatusCode::UNAUTHORIZED {
        "auth_failed"
    } else if id.version != terrarium_version::VERSION {
        "version_skew"
    } else {
        "online"
    };
    mark(s, r, state).await?;
    if method != axum::http::Method::GET && method != axum::http::Method::HEAD
        || ws && path != "/api/events"
    {
        let name = r.name.clone();
        s.db.write(actor.label(), move |t| t.audit("remote.call", &name, None,
            json!({"method": method.as_str(), "path": path, "status": status.as_u16(), "duration_ms": start.elapsed().as_millis()}))).await?;
    }
    if ws && status == StatusCode::SWITCHING_PROTOCOLS {
        let want = tungstenite::handshake::derive_accept_key(
            ws_key.as_ref().ok_or_else(net::unreachable)?.as_bytes(),
        );
        if resp
            .headers()
            .get("sec-websocket-accept")
            .and_then(|v| v.to_str().ok())
            != Some(want.as_str())
        {
            return Err(net::unreachable());
        }
        let stream_permit = limits.acquire(true)?;
        let upstream = resp.upgrade().await.map_err(|_| net::unreachable())?;
        let ws = WebSocketUpgrade::from_request_parts(&mut parts, &())
            .await
            .map_err(|_| ApiError::bad_request("invalid WebSocket upgrade"))?;
        let mut response = ws
            .max_message_size(net::JSON_MAX)
            .max_frame_size(net::JSON_MAX)
            .on_upgrade(move |socket| async move {
                let _permits = (permit, stream_permit);
                let mut cfg = tungstenite::protocol::WebSocketConfig::default();
                cfg.max_message_size = Some(net::JSON_MAX);
                cfg.max_frame_size = Some(net::JSON_MAX);
                let upstream = tokio_tungstenite::WebSocketStream::from_raw_socket(
                    upstream,
                    Role::Client,
                    Some(cfg),
                )
                .await;
                bridge(socket, upstream, limits).await;
            });
        response.headers_mut().insert(
            "x-terrarium-remote",
            r.name.parse().map_err(|_| net::unreachable())?,
        );
        return Ok(response);
    }
    if ws {
        return Err(net::unreachable());
    }
    let empty = status == StatusCode::NO_CONTENT
        || status == StatusCode::NOT_MODIFIED
        || parts.method == axum::http::Method::HEAD;
    let headers = net::response_headers(resp.headers(), &r.name, empty)?;
    let sse = resp
        .headers()
        .get(header::CONTENT_TYPE)
        .and_then(|v| v.to_str().ok())
        .is_some_and(|v| v.split(';').next() == Some("text/event-stream"));
    let binary = resp
        .headers()
        .get(header::CONTENT_TYPE)
        .and_then(|v| v.to_str().ok())
        .is_some_and(|v| v.split(';').next() == Some("application/octet-stream"));
    let body = if sse || binary {
        let stream_permit = limits.acquire(true)?;
        let rx = limits.closed.subscribe();
        let stream = futures_util::stream::unfold(
            (resp, permit, stream_permit, rx, 0usize),
            move |(mut resp, p, sp, mut rx, total)| async move {
                if *rx.borrow() || total > net::UPLOAD_MAX && !sse {
                    return None;
                }
                let next = tokio::select! {
                    _ = rx.changed() => return None,
                    b = tokio::time::timeout(net::IDLE, resp.chunk()) => b,
                };
                match next {
                    Ok(Ok(Some(b))) if sse || total.saturating_add(b.len()) <= net::UPLOAD_MAX => {
                        let n = total.saturating_add(b.len());
                        Some((Ok::<_, std::io::Error>(b), (resp, p, sp, rx, n)))
                    }
                    Ok(Ok(None)) => None,
                    _ => None,
                }
            },
        );
        Body::from_stream(stream)
    } else {
        Body::from(net::limited(resp, net::JSON_MAX, net::PLAIN).await?)
    };
    let mut response = Response::new(body);
    *response.status_mut() = status;
    *response.headers_mut() = headers;
    Ok(response)
}

async fn bridge<T>(
    mut home: axum::extract::ws::WebSocket,
    mut remote: tokio_tungstenite::WebSocketStream<T>,
    limits: Arc<Limits>,
) where
    T: tokio::io::AsyncRead + tokio::io::AsyncWrite + Unpin,
{
    use axum::extract::ws::Message as H;
    use tungstenite::Message as R;
    let mut closed = limits.closed.subscribe();
    loop {
        if *closed.borrow() {
            break;
        }
        tokio::select! {
            _ = closed.changed() => break,
            _ = tokio::time::sleep(net::IDLE) => break,
            msg = home.next() => {
                let m = match msg {
                    Some(Ok(H::Text(t))) => R::Text(t.as_str().into()),
                    Some(Ok(H::Binary(b))) => R::Binary(b),
                    Some(Ok(H::Ping(b))) => R::Ping(b),
                    Some(Ok(H::Pong(b))) => R::Pong(b),
                    _ => break,
                };
                if !matches!(tokio::time::timeout(net::IDLE, remote.send(m)).await, Ok(Ok(()))) { break; }
            }
            msg = remote.next() => {
                let m = match msg {
                    Some(Ok(R::Text(t))) => H::Text(t.as_str().into()),
                    Some(Ok(R::Binary(b))) => H::Binary(b),
                    Some(Ok(R::Ping(b))) => H::Ping(b),
                    Some(Ok(R::Pong(b))) => H::Pong(b),
                    _ => break,
                };
                if !matches!(tokio::time::timeout(net::IDLE, home.send(m)).await, Ok(Ok(()))) { break; }
            }
        }
    }
}
