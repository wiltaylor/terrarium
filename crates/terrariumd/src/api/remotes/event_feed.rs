//! One live upstream firehose per remote, independent browser cursors and filters.
//! Historical pages remain on the remote. No remote frame enters the home log.
use std::sync::{Arc, atomic::Ordering};
use std::time::Duration;

use axum::extract::{
    FromRequestParts, Request, WebSocketUpgrade,
    ws::{Message, WebSocket},
};
use axum::http::{HeaderMap, StatusCode};
use axum::response::Response;
use futures_util::{SinkExt, StreamExt};
use serde_json::Value;
use tokio::sync::{OwnedSemaphorePermit, broadcast};
use tokio_tungstenite::{
    WebSocketStream,
    tungstenite::{
        self,
        protocol::{Role, WebSocketConfig},
    },
};

use super::{Remote, Shared, key, mark, net, own_ports};
use crate::error::{ApiError, ApiResult};
use crate::remotes::{EventFeed, Limits};

type Upstream = WebSocketStream<reqwest::Upgraded>;
type Connection = (Upstream, OwnedSemaphorePermit, i64);

async fn open(s: &Shared, r: &Remote, since: &str) -> ApiResult<Connection> {
    let limits = s.remotes.get(&r.id);
    let stream = limits.acquire(true)?;
    let _request = limits.acquire(false)?;
    let c = net::client(r, &own_ports(s)).await?;
    let h = net::request_headers(
        &HeaderMap::new(),
        &key(s, r).await?,
        &s.links.instance_id(&s.state_dir)?,
    )?;
    let id = net::instance(&c, r, &h).await?;
    let since = if since == "latest" {
        let resp = c
            .get(format!("{}api/events/latest", r.url))
            .headers(h.clone())
            .timeout(net::PLAIN)
            .send()
            .await
            .map_err(net::connection_error)?;
        if resp.status() != StatusCode::OK {
            return Err(net::unreachable());
        }
        let value: Value = serde_json::from_slice(&net::limited(resp, 16384, net::PLAIN).await?)
            .map_err(|_| net::unreachable())?;
        value["seq"]
            .as_i64()
            .ok_or_else(net::unreachable)?
            .to_string()
    } else {
        since.to_owned()
    };
    let mut u = url::Url::parse(&format!("{}api/events", r.url)).map_err(|_| net::unreachable())?;
    u.query_pairs_mut()
        .append_pair("since", &since)
        .append_pair("audit", "1");
    let k = tungstenite::handshake::client::generate_key();
    let response = c
        .get(u)
        .headers(h)
        .header("connection", "Upgrade")
        .header("upgrade", "websocket")
        .header("sec-websocket-version", "13")
        .header("sec-websocket-key", &k)
        .timeout(net::PLAIN)
        .send()
        .await
        .map_err(net::connection_error)?;
    if response.status() != StatusCode::SWITCHING_PROTOCOLS
        || response
            .headers()
            .get("sec-websocket-accept")
            .and_then(|h| h.to_str().ok())
            != Some(tungstenite::handshake::derive_accept_key(k.as_bytes()).as_str())
    {
        return Err(net::unreachable());
    }
    let upstream = response.upgrade().await.map_err(|_| net::unreachable())?;
    let cfg = WebSocketConfig::default()
        .max_message_size(Some(net::JSON_MAX))
        .max_frame_size(Some(net::JSON_MAX));
    mark(
        s,
        r,
        if id.version != terrarium_version::VERSION {
            "version_skew"
        } else {
            "online"
        },
    )
    .await?;
    Ok((
        WebSocketStream::from_raw_socket(upstream, Role::Client, Some(cfg)).await,
        stream,
        since.parse().map_err(|_| net::unreachable())?,
    ))
}

pub(super) async fn connect(s: &Shared, r: &Remote, req: Request) -> ApiResult<Response> {
    let limits = s.remotes.get(&r.id);
    let _request = limits.acquire(false)?;
    let query = net::query(req.uri().query());
    let pairs: Vec<(String, String)> =
        url::form_urlencoded::parse(query.as_deref().unwrap_or("").as_bytes())
            .map(|(k, v)| (k.into_owned(), v.into_owned()))
            .collect();
    for (k, _) in &pairs {
        if !matches!(
            k.as_str(),
            "since" | "live" | "kind" | "project" | "agent" | "audit"
        ) {
            return Err(ApiError::bad_request("unsupported event stream query"));
        }
    }
    let (mut parts, _) = req.into_parts();
    let ws = WebSocketUpgrade::from_request_parts(&mut parts, &())
        .await
        .map_err(|_| ApiError::bad_request("invalid WebSocket upgrade"))?;
    let mut held = limits.events.lock().await;
    let mut opened = None;
    let feed = if let Some(f) = held.as_ref().filter(|f| f.alive.load(Ordering::Acquire)) {
        f.clone()
    } else {
        let upstream = open(s, r, "latest").await?;
        let f = Arc::new(EventFeed::default());
        f.last_seq.store(upstream.2, Ordering::Release);
        *held = Some(f.clone());
        opened = Some(upstream);
        f
    };
    let rx = feed.frames.subscribe();
    if let Some(upstream) = opened {
        let (s, r, l, f) = (s.clone(), r.clone(), limits.clone(), feed.clone());
        tokio::spawn(async move {
            relay(s, r, l, f, upstream).await;
        });
    }
    drop(held);
    let s = s.clone();
    let remote = r.clone();
    let mut response = ws
        .max_message_size(net::JSON_MAX)
        .max_frame_size(net::JSON_MAX)
        .on_upgrade(move |socket| browser(socket, s, remote, limits, rx, pairs));
    response.headers_mut().insert(
        "x-terrarium-remote",
        r.name.parse().map_err(|_| net::unreachable())?,
    );
    Ok(response)
}

async fn relay(
    s: Shared,
    r: Remote,
    limits: Arc<Limits>,
    feed: Arc<EventFeed>,
    mut upstream: Connection,
) {
    let mut closed = limits.closed.subscribe();
    let mut ping = tokio::time::interval(Duration::from_secs(30));
    let mut backoff = 1;
    loop {
        if *closed.borrow() {
            break;
        }
        let incoming = tokio::select! {
            _ = closed.changed() => break,
            _ = ping.tick() => {
                if feed.frames.receiver_count() == 0 { break; }
                if !matches!(tokio::time::timeout(net::CONNECT,upstream.0.send(tungstenite::Message::Ping(Vec::new().into()))).await,Ok(Ok(()))) { None }
                else { continue; }
            }
            message = tokio::time::timeout(net::IDLE,upstream.0.next()) => {
                match message { Ok(Some(Ok(m))) => Some(m), _ => None }
            }
        };
        match incoming {
            Some(tungstenite::Message::Text(text)) => {
                let Ok(value) = serde_json::from_str::<Value>(text.as_str()) else {
                    break;
                };
                if let Some(seq) = value["seq"].as_i64() {
                    feed.last_seq.fetch_max(seq, Ordering::AcqRel);
                }
                if feed.frames.send(Arc::new(text.to_string())).is_err() {
                    break;
                }
                backoff = 1;
            }
            Some(tungstenite::Message::Ping(_)) => {
                if !matches!(
                    tokio::time::timeout(net::CONNECT, upstream.0.flush()).await,
                    Ok(Ok(()))
                ) {
                    break;
                }
            }
            Some(tungstenite::Message::Pong(_)) => {}
            Some(tungstenite::Message::Binary(_) | tungstenite::Message::Frame(_)) => break,
            Some(tungstenite::Message::Close(_)) | None => {
                let _ = mark(&s, &r, "offline").await;
                loop {
                    if *closed.borrow() || feed.frames.receiver_count() == 0 {
                        feed.alive.store(false, Ordering::Release);
                        return;
                    }
                    tokio::select! { _ = closed.changed() => { feed.alive.store(false,Ordering::Release); return; }, _ = tokio::time::sleep(Duration::from_secs(backoff)) => {} }
                    backoff = (backoff * 2).min(30);
                    let seq = feed.last_seq.load(Ordering::Acquire).to_string();
                    match open(&s, &r, &seq).await {
                        Ok(next) => {
                            upstream = next;
                            break;
                        }
                        Err(e) => {
                            let status = super::failure_status(&e);
                            let _ = mark(&s, &r, status).await;
                            if matches!(status, "auth_failed" | "pin_mismatch" | "mismatch") {
                                limits.closed.send_replace(true);
                                feed.alive.store(false, Ordering::Release);
                                return;
                            }
                        }
                    }
                }
            }
        }
    }
    feed.alive.store(false, Ordering::Release);
}

fn option<'a>(q: &'a [(String, String)], name: &str) -> Option<&'a str> {
    q.iter().find(|(k, _)| k == name).map(|(_, v)| v.as_str())
}

fn matches(q: &[(String, String)], v: &Value) -> bool {
    if v["kind"] == "audit" {
        return matches!(option(q, "audit"), Some("1" | "true"))
            && option(q, "project").is_none_or(|p| v["audit"]["project"] == p);
    }
    [
        ("project", "project"),
        ("kind", "kind"),
        ("agent", "agent_id"),
    ]
    .iter()
    .all(|(qkey, vkey)| option(q, qkey).is_none_or(|want| v[*vkey] == want))
}

async fn browser(
    mut socket: WebSocket,
    s: Shared,
    r: Remote,
    limits: Arc<Limits>,
    mut rx: broadcast::Receiver<Arc<String>>,
    pairs: Vec<(String, String)>,
) {
    let mut closed = limits.closed.subscribe();
    let mut last = 0i64;
    // Subscribe to live first, then page historical rows. On lag, close the
    // browser so its normal reconnect resumes from the last delivered seq.
    if !matches!(option(&pairs, "live"), Some("1" | "true"))
        && option(&pairs, "since") != Some("latest")
    {
        let backlog = async {
            let c = net::client(&r, &own_ports(&s)).await?;
            let h = net::request_headers(
                &HeaderMap::new(),
                &key(&s, &r).await?,
                &s.links.instance_id(&s.state_dir)?,
            )?;
            net::instance(&c, &r, &h).await?;
            let mut since = option(&pairs, "since").unwrap_or("0").to_owned();
            loop {
                let mut u = url::Url::parse(&format!("{}api/events", r.url))
                    .map_err(|_| net::unreachable())?;
                for (k, v) in &pairs {
                    if matches!(k.as_str(), "kind" | "project" | "agent") {
                        u.query_pairs_mut().append_pair(k, v);
                    }
                }
                u.query_pairs_mut()
                    .append_pair("since", &since)
                    .append_pair("limit", "100");
                let response = c
                    .get(u)
                    .headers(h.clone())
                    .timeout(net::PLAIN)
                    .send()
                    .await
                    .map_err(net::connection_error)?;
                if response.status() != StatusCode::OK {
                    return Err(net::unreachable());
                }
                let next = response
                    .headers()
                    .get("x-next-cursor")
                    .and_then(|h| h.to_str().ok())
                    .map(str::to_owned);
                let page: Vec<Value> = serde_json::from_slice(
                    &net::limited(response, net::JSON_MAX, net::PLAIN).await?,
                )
                .map_err(|_| net::unreachable())?;
                for frame in page {
                    last = last.max(frame["seq"].as_i64().unwrap_or(0));
                    let text = serde_json::to_string(&frame)?;
                    tokio::time::timeout(net::IDLE, socket.send(Message::Text(text.into())))
                        .await
                        .map_err(|_| net::unreachable())?
                        .map_err(|_| net::unreachable())?;
                }
                match next {
                    Some(n) if n != since => since = n,
                    _ => break,
                }
            }
            Ok::<_, ApiError>(())
        };
        if *closed.borrow() {
            return;
        }
        let done = tokio::select! { _ = closed.changed() => return, result = backlog => result };
        if done.is_err() {
            return;
        }
    }
    loop {
        if *closed.borrow() {
            return;
        }
        tokio::select! {
            _ = closed.changed() => return,
            _ = tokio::time::sleep(net::IDLE) => return,
            message = socket.next() => if !matches!(message,Some(Ok(Message::Ping(_) | Message::Pong(_)))) { return; },
            frame = rx.recv() => {
                let Ok(text) = frame else { return; };
                let Ok(v) = serde_json::from_str::<Value>(&text) else { return; };
                if !matches(&pairs,&v) { continue; }
                if let Some(seq) = v["seq"].as_i64() { if seq <= last { continue; } last = seq; }
                if !matches!(tokio::time::timeout(net::IDLE,socket.send(Message::Text(text.as_str().into()))).await,Ok(Ok(()))) { return; }
            }
        }
    }
}
