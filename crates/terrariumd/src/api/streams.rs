//! Event streams and the PTY socket.
//!
//! `GET /api/events` and `GET /api/agents/{id}/events` answer in three
//! ways, by request:
//!
//! - a WebSocket upgrade: one JSON event per text frame, the backlog after
//!   `?since=` first, then live events;
//! - `Accept: text/event-stream`: the same as Server-Sent Events;
//! - anything else: one JSON page of the log after `?since=`.

use std::collections::VecDeque;
use std::convert::Infallible;
use std::time::Duration;

use axum::extract::ws::{Message as WsMessage, WebSocket, WebSocketUpgrade};
use axum::extract::{FromRequestParts, Path, Request, State};
use axum::http::header;
use axum::response::sse::{Event as SseEvent, KeepAlive, Sse};
use axum::response::{IntoResponse, Response};
use futures_util::{SinkExt, StreamExt};
use serde::Deserialize;
use serde_json::json;
use tokio::sync::broadcast::error::RecvError;
use tracing::debug;

use super::agents::{load, supervisor_error};
use super::{ApiQuery, Shared, is_websocket, paged};
use crate::actor::Actor;
use crate::error::{ApiError, ApiResult};
use crate::events::{self, EventFilter, StoredEvent};
use crate::policy;
use crate::store::{Page, Paged};
use crate::supervisor::{PtyAttachment, PtyInput};

const BATCH: usize = 500;

#[derive(Debug, Default, Deserialize)]
pub struct StreamQuery {
    /// A `seq`, or an event ID. Only later events come.
    since: Option<String>,
    kind: Option<String>,
    project: Option<String>,
    agent: Option<String>,
}

enum Mode {
    WebSocket(WebSocketUpgrade),
    Sse,
    Json,
}

async fn mode(req: Request) -> ApiResult<(Mode, Option<String>)> {
    let last_event_id = req
        .headers()
        .get("last-event-id")
        .and_then(|v| v.to_str().ok())
        .map(str::to_owned);
    if is_websocket(&req) {
        let (mut parts, _) = req.into_parts();
        let ws = WebSocketUpgrade::from_request_parts(&mut parts, &())
            .await
            .map_err(|e| ApiError::bad_request(e.body_text()))?;
        return Ok((Mode::WebSocket(ws), last_event_id));
    }
    let sse = req
        .headers()
        .get(header::ACCEPT)
        .and_then(|v| v.to_str().ok())
        .is_some_and(|v| v.contains("text/event-stream"));
    Ok((if sse { Mode::Sse } else { Mode::Json }, last_event_id))
}

async fn respond(
    state: Shared,
    filter: EventFilter,
    page: Page,
    since: Option<String>,
    req: Request,
) -> ApiResult<Response> {
    let (mode, last_event_id) = mode(req).await?;
    let since_key = page.cursor().map(str::to_owned).or(since).or(last_event_id);
    let since = state
        .db
        .read(move |c| events::resolve_since(c, since_key.as_deref()))
        .await?;
    Ok(match mode {
        Mode::Json => {
            let limit = page.limit();
            let f = filter.clone();
            let rows = state
                .db
                .read(move |c| events::list(c, &f, since, limit + 1))
                .await?;
            paged(Paged::from_rows(rows, limit, |e: &StoredEvent| {
                e.seq.to_string()
            }))
        }
        Mode::WebSocket(ws) => ws.on_upgrade(move |socket| ws_events(socket, state, filter, since)),
        Mode::Sse => sse_events(state, filter, since).into_response(),
    })
}

/// `GET /api/events`: every event the actor may see.
pub async fn firehose(
    State(state): State<Shared>,
    actor: Actor,
    ApiQuery(page): ApiQuery<Page>,
    ApiQuery(q): ApiQuery<StreamQuery>,
    req: Request,
) -> ApiResult<Response> {
    let mut filter = EventFilter {
        kind: q.kind,
        project: q.project,
        ..EventFilter::default()
    };
    if let Some(p) = &filter.project {
        policy::read_project(&actor, p)?;
    }
    if let Some(agent) = q.agent {
        // A ULID filters as is, so the log of a deleted agent stays readable.
        filter.agent = Some(if agent.parse::<terrarium_core::AgentId>().is_ok() {
            agent
        } else {
            load(&state, &actor, agent).await?.agent.id.to_string()
        });
    }
    if let Actor::Agent(a) = &actor
        && !a.is_sysadmin()
    {
        filter.visible_to = Some((a.project.clone(), a.id.clone()));
    }
    respond(state, filter, page, q.since, req).await
}

/// `GET /api/agents/{id}/events`: the events of one agent.
pub async fn agent_events(
    State(state): State<Shared>,
    actor: Actor,
    Path(id): Path<String>,
    ApiQuery(page): ApiQuery<Page>,
    ApiQuery(q): ApiQuery<StreamQuery>,
    req: Request,
) -> ApiResult<Response> {
    let a = load(&state, &actor, id).await?;
    let filter = EventFilter {
        agent: Some(a.agent.id.to_string()),
        kind: q.kind,
        ..EventFilter::default()
    };
    respond(state, filter, page, q.since, req).await
}

async fn backlog(state: &Shared, filter: &EventFilter, after: i64) -> ApiResult<Vec<StoredEvent>> {
    let f = filter.clone();
    state
        .db
        .read(move |c| events::list(c, &f, after, BATCH))
        .await
}

async fn ws_events(socket: WebSocket, state: Shared, filter: EventFilter, since: i64) {
    let (mut tx, mut rx) = socket.split();
    let mut live = state.db.hub().subscribe();
    let mut last = since;
    let send = |e: &StoredEvent| {
        serde_json::to_string(e)
            .map(|s| WsMessage::Text(s.into()))
            .ok()
    };
    // The backlog, in batches, then the live feed.
    loop {
        let Ok(batch) = backlog(&state, &filter, last).await else {
            return;
        };
        for e in &batch {
            if let Some(m) = send(e)
                && tx.send(m).await.is_err()
            {
                return;
            }
            last = e.seq;
        }
        if batch.len() < BATCH {
            break;
        }
    }
    loop {
        tokio::select! {
            msg = rx.next() => match msg {
                None | Some(Err(_)) | Some(Ok(WsMessage::Close(_))) => return,
                Some(Ok(_)) => {}
            },
            ev = live.recv() => match ev {
                Ok(e) => {
                    if e.seq > last && filter.matches(&e) {
                        if let Some(m) = send(&e)
                            && tx.send(m).await.is_err()
                        {
                            return;
                        }
                        last = e.seq;
                    }
                }
                Err(RecvError::Lagged(_)) => {
                    // Catch up from the log.
                    let Ok(batch) = backlog(&state, &filter, last).await else {
                        return;
                    };
                    for e in &batch {
                        if let Some(m) = send(e)
                            && tx.send(m).await.is_err()
                        {
                            return;
                        }
                        last = e.seq;
                    }
                }
                Err(RecvError::Closed) => return,
            },
        }
    }
}

struct SseState {
    state: Shared,
    filter: EventFilter,
    last: i64,
    queue: VecDeque<StoredEvent>,
    live: tokio::sync::broadcast::Receiver<StoredEvent>,
    caught_up: bool,
}

fn sse_events(
    state: Shared,
    filter: EventFilter,
    since: i64,
) -> Sse<impl futures_util::Stream<Item = Result<SseEvent, Infallible>>> {
    let live = state.db.hub().subscribe();
    let init = SseState {
        state,
        filter,
        last: since,
        queue: VecDeque::new(),
        live,
        caught_up: false,
    };
    let stream = futures_util::stream::unfold(init, |mut s| async move {
        loop {
            if let Some(e) = s.queue.pop_front() {
                s.last = e.seq;
                let ev = SseEvent::default()
                    .id(e.seq.to_string())
                    .event(e.kind.clone())
                    .json_data(&e)
                    .unwrap_or_else(|_| SseEvent::default().comment("bad event"));
                return Some((Ok(ev), s));
            }
            if !s.caught_up {
                let batch = backlog(&s.state, &s.filter, s.last).await.ok()?;
                s.caught_up = batch.len() < BATCH;
                s.queue.extend(batch);
                continue;
            }
            match s.live.recv().await {
                Ok(e) if e.seq > s.last && s.filter.matches(&e) => s.queue.push_back(e),
                Ok(_) => {}
                Err(RecvError::Lagged(_)) => s.caught_up = false,
                Err(RecvError::Closed) => return None,
            }
        }
    });
    Sse::new(stream).keep_alive(KeepAlive::new().interval(Duration::from_secs(15)))
}

/// A control frame from a PTY client.
#[derive(Debug, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
enum PtyControl {
    Input { data: String },
    Resize { rows: u16, cols: u16 },
}

/// `GET /api/agents/{id}/pty`: the agent's terminal over a WebSocket.
pub async fn pty(
    State(state): State<Shared>,
    actor: Actor,
    Path(id): Path<String>,
    req: Request,
) -> ApiResult<Response> {
    let a = load(&state, &actor, id).await?;
    policy::admin_project(&actor, &a.agent.project, "attaching to an agent terminal")?;
    if !is_websocket(&req) {
        return Err(ApiError::bad_request(
            "the PTY is a WebSocket; send an upgrade request",
        ));
    }
    let (mut parts, _) = req.into_parts();
    let ws = WebSocketUpgrade::from_request_parts(&mut parts, &())
        .await
        .map_err(|e| ApiError::bad_request(e.body_text()))?;
    let attachment = state
        .supervisor
        .attach_pty(a.agent.id)
        .await
        .map_err(supervisor_error)?;
    let id = a.agent.id.to_string();
    let project = a.agent.project.clone();
    state
        .db
        .write(actor.label(), move |t| {
            t.audit("agent.attach", &id, Some(&project), json!({}))
        })
        .await?;
    Ok(ws.on_upgrade(move |socket| pty_session(socket, attachment)))
}

async fn pty_session(socket: WebSocket, mut att: PtyAttachment) {
    let (mut tx, mut rx) = socket.split();
    if !att.snapshot.is_empty()
        && tx
            .send(WsMessage::Binary(std::mem::take(&mut att.snapshot).into()))
            .await
            .is_err()
    {
        return;
    }
    loop {
        tokio::select! {
            msg = rx.next() => {
                let input = match msg {
                    None | Some(Err(_)) | Some(Ok(WsMessage::Close(_))) => return,
                    Some(Ok(WsMessage::Text(text))) => {
                        match serde_json::from_str::<PtyControl>(text.as_str()) {
                            Ok(PtyControl::Input { data }) => PtyInput::Bytes(data.into_bytes()),
                            Ok(PtyControl::Resize { rows, cols }) => PtyInput::Resize { rows, cols },
                            Err(e) => {
                                debug!("ignored PTY frame: {e}");
                                continue;
                            }
                        }
                    }
                    Some(Ok(WsMessage::Binary(bytes))) => PtyInput::Bytes(bytes.to_vec()),
                    Some(Ok(_)) => continue,
                };
                if att.input.send(input).await.is_err() {
                    let _ = tx.send(WsMessage::Close(None)).await;
                    return;
                }
            }
            out = att.output.recv() => match out {
                Some(bytes) => {
                    if tx.send(WsMessage::Binary(bytes.into())).await.is_err() {
                        return;
                    }
                }
                None => {
                    let _ = tx.send(WsMessage::Close(None)).await;
                    return;
                }
            },
        }
    }
}
