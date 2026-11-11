//! `/api/agents/{id}/sessions` and the unified session transcript
//! (`docs/api.md` §6.3 and §6.6).
//!
//! Rights: the same as the agent's events. A member of the agent's
//! project reads, the operator reads all.

use std::collections::VecDeque;
use std::convert::Infallible;
use std::sync::Arc;
use std::time::Duration;

use axum::Json;
use axum::extract::{Path, State};
use axum::http::HeaderMap;
use axum::response::sse::{Event as SseEvent, KeepAlive, Sse};
use axum::response::{IntoResponse, Response};
use axum::routing::get;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use terrarium_core::redact::View;
use terrarium_core::transcript::{Block, Origin};
use terrarium_core::{Agent, AgentStatus, HarnessId};
use tokio::sync::broadcast::error::RecvError;

use super::agents::load;
use super::{ApiQuery, Shared, paged};
use crate::actor::Actor;
use crate::error::{ApiError, ApiResult};
use crate::store::agents::{self, AgentView, Session};
use crate::store::transcripts as tstore;
use crate::store::{Page, Paged};
use crate::transcripts::{LocateError, Located, Summary, locate_session, lock_entry};

/// The default and the largest `limit` of a transcript page.
pub const DEFAULT_LIMIT: usize = 500;
pub const MAX_LIMIT: usize = 5000;

/// The largest transcript page, in bytes of block JSON. A page always has
/// at least one block when one is left.
pub const MAX_PAGE_BYTES: usize = 2 * 1024 * 1024;

/// How often a transcript stream reads the file when no event of the agent
/// came.
const STREAM_TICK: Duration = Duration::from_secs(1);

pub fn routes() -> super::link_registry::RegisteredRouter {
    super::link_registry::RegisteredRouter::new()
        .route(
            "/agents/{id}/sessions",
            get(sessions),
            &[(axum::http::Method::GET, super::link_registry::Access::Link)],
        )
        .route(
            "/agents/{id}/sessions/{session}/transcript",
            get(transcript),
            &[(axum::http::Method::GET, super::link_registry::Access::Link)],
        )
        .route(
            "/agents/{id}/sessions/{session}/transcript/stream",
            get(stream),
            &[(axum::http::Method::GET, super::link_registry::Access::Link)],
        )
}

/// A row of `GET /api/agents/{id}/sessions`.
#[derive(Debug, Serialize)]
struct SessionView {
    #[serde(flatten)]
    session: Session,
    /// A session file was found and passed the path check.
    transcript: bool,
    /// The number of transcript blocks.
    blocks: usize,
    /// The first user text, at most 80 characters. `null` when there is
    /// none.
    title: Option<String>,
    /// The agent's current session, and the agent runs.
    active: bool,
}

/// True when the supervisor runs the agent.
async fn live(state: &Shared, agent: &Agent) -> bool {
    state
        .supervisor
        .status(agent.id)
        .await
        .is_some_and(AgentStatus::is_live)
}

async fn summary(state: &Shared, loc: &Located) -> Summary {
    let cache = state.transcripts.clone();
    let loc = loc.clone();
    tokio::task::spawn_blocking(move || cache.summary(&loc))
        .await
        .ok()
        .and_then(Result::ok)
        .unwrap_or_default()
}

async fn sessions(
    State(state): State<Shared>,
    actor: Actor,
    Path(id): Path<String>,
    ApiQuery(page): ApiQuery<Page>,
) -> ApiResult<Response> {
    let a = load(&state, &actor, id).await?;
    let id = a.agent.id.to_string();
    let rows = state
        .db
        .read(move |c| agents::sessions(c, &id, &page))
        .await?;
    let running = live(&state, &a.agent).await;
    let redactor = state.db.redactor().get();
    let view = crate::secrets::redact::view_of(&actor);
    let mut items = Vec::with_capacity(rows.items.len());
    for s in rows.items {
        let (transcript, sum) = match locate_session(&state, &a.agent, &s).await {
            Ok(loc) if loc.path.is_file() => (true, summary(&state, &loc).await),
            _ => (false, Summary::default()),
        };
        let active = s.current && running;
        items.push(SessionView {
            session: s,
            transcript,
            blocks: sum.blocks,
            title: sum.title.map(|t| redactor.redact_string_as(t, &view).0),
            active,
        });
    }
    Ok(paged(Paged {
        items,
        next: rows.next,
    }))
}

/// The session row named by the path: a session ID or a harness session
/// ID.
async fn session_row(state: &Shared, a: &AgentView, key: &str) -> ApiResult<Session> {
    let (id, k) = (a.agent.id.to_string(), key.to_owned());
    state
        .db
        .read(move |c| agents::session(c, &id, &k))
        .await?
        .ok_or_else(|| {
            ApiError::not_found(format!("agent `{}` has no session `{key}`", a.agent.name))
        })
}

/// Set the origin of each user text that the daemon typed, from its
/// delivery log.
async fn annotate(state: &Shared, agent: &str, blocks: &mut [Block]) {
    let wanted = |b: &Block| b.is_user_text() && matches!(b.origin, None | Some(Origin::User));
    let hashes: Vec<String> = blocks
        .iter()
        .filter(|b| wanted(b))
        .map(|b| tstore::text_hash(b.text.as_deref().unwrap_or("")))
        .collect();
    if hashes.is_empty() {
        return;
    }
    let (agent, hs) = (agent.to_owned(), hashes);
    let Ok(found) = state
        .db
        .read(move |c| tstore::delivery_origins(c, &agent, &hs))
        .await
    else {
        return;
    };
    for b in blocks.iter_mut().filter(|b| wanted(b)) {
        let h = tstore::text_hash(b.text.as_deref().unwrap_or(""));
        if let Some(o) = found.get(&h) {
            b.origin = Some(*o);
        }
    }
}

/// Redact the blocks of a page or a stream frame for the reader's view
/// (PLAN §16b.6). The harness writes the session file, so the daemon
/// redacts at read time, after [`annotate`] (it matches the text that the
/// daemon typed).
fn redact_blocks(state: &Shared, view: &View, blocks: &mut [Block]) {
    let r = state.db.redactor().get();
    if r.is_empty() {
        return;
    }
    for b in blocks {
        crate::secrets::redact::redact_block(&r, view, b);
    }
}

fn locate_error(e: LocateError) -> ApiError {
    match e {
        LocateError::Missing(m) => ApiError::not_found(m),
        LocateError::Refused(m) => ApiError::forbidden(format!(
            "the session file is outside the agent's own dirs: {m}"
        )),
    }
}

#[derive(Debug, Default, Deserialize)]
pub(super) struct TranscriptQuery {
    /// Only blocks with a larger `seq` (exclusive).
    after: Option<u64>,
    limit: Option<usize>,
}

/// One page of a cached log, after `after`: at most `limit` blocks and
/// [`MAX_PAGE_BYTES`], and the number of blocks in the log.
fn page(
    state: &Shared,
    loc: &Located,
    after: u64,
    limit: usize,
) -> std::io::Result<(Vec<Block>, u64)> {
    let entry = state.transcripts.refreshed(loc)?;
    let g = lock_entry(&entry);
    let mut out = Vec::new();
    let mut bytes = 0;
    for b in g.log.after(after).iter().take(limit) {
        let size = serde_json::to_vec(b).map(|v| v.len()).unwrap_or(0);
        if !out.is_empty() && bytes + size > MAX_PAGE_BYTES {
            break;
        }
        bytes += size;
        out.push(b.clone());
    }
    Ok((out, g.log.len() as u64))
}

/// The session file of a session, or `None` for the active session whose
/// file is not known yet.
async fn located(
    state: &Shared,
    agent: &Agent,
    row: &Session,
    active: bool,
) -> ApiResult<Option<Located>> {
    match locate_session(state, agent, row).await {
        Ok(loc) => Ok(Some(loc)),
        Err(LocateError::Missing(_)) if active => Ok(None),
        Err(e) => Err(locate_error(e)),
    }
}

async fn transcript(
    State(state): State<Shared>,
    actor: Actor,
    Path((id, key)): Path<(String, String)>,
    ApiQuery(q): ApiQuery<TranscriptQuery>,
) -> ApiResult<Json<Value>> {
    let a = load(&state, &actor, id).await?;
    let row = session_row(&state, &a, &key).await?;
    let active = row.current && live(&state, &a.agent).await;
    let loc = located(&state, &a.agent, &row, active).await?;
    let body = transcript_page(
        &state,
        &a.agent.id.to_string(),
        &row.id,
        a.agent.harness,
        loc,
        active,
        &q,
        &crate::secrets::redact::view_of(&actor),
    )
    .await?;
    Ok(Json(body))
}

/// The body of a transcript page: the one place that reads blocks for
/// `/api/agents/{id}/sessions/{s}/transcript` and
/// `/api/sessions/{id}/transcript`, so a filter of the blocks (such as a
/// redaction) covers both.
#[allow(clippy::too_many_arguments)]
pub(super) async fn transcript_page(
    state: &Shared,
    agent: &str,
    session: &str,
    default_harness: HarnessId,
    loc: Option<Located>,
    active: bool,
    q: &TranscriptQuery,
    view: &View,
) -> ApiResult<Value> {
    let after = q.after.unwrap_or(0);
    let limit = q.limit.unwrap_or(DEFAULT_LIMIT).clamp(1, MAX_LIMIT);
    let (mut blocks, total) = match &loc {
        Some(loc) => {
            let (st, l) = (Arc::clone(state), loc.clone());
            tokio::task::spawn_blocking(move || page(&st, &l, after, limit))
                .await
                .map_err(|e| ApiError::internal(e.to_string()))?
                .map_err(|e| ApiError::internal(format!("cannot read the session file: {e}")))?
        }
        None => (Vec::new(), 0),
    };
    for b in &mut blocks {
        b.session = session.to_owned();
    }
    annotate(state, agent, &mut blocks).await;
    redact_blocks(state, view, &mut blocks);
    let next_after = blocks.last().map_or(after, |b| b.seq);
    let harness: HarnessId = loc.as_ref().map_or(default_harness, |l| l.harness);
    Ok(json!({
        "session": session,
        "harness": harness,
        "active": active,
        "blocks": blocks,
        "next_after": next_after,
        "complete": next_after >= total,
    }))
}

#[derive(Debug, Default, Deserialize)]
struct StreamQuery {
    after: Option<u64>,
}

/// The state of one transcript stream.
struct Streamer {
    state: Shared,
    /// The labels that the reader sees (PLAN §16b.6).
    view: View,
    agent: Agent,
    row: Session,
    loc: Option<Located>,
    /// The highest `seq` sent.
    last_seq: u64,
    /// The log revision sent; `None` before the first read.
    last_rev: Option<u64>,
    queue: VecDeque<SseEvent>,
    hub: tokio::sync::broadcast::Receiver<crate::events::StoredEvent>,
    tick: tokio::time::Interval,
    last_status: Option<String>,
    /// The stream sent its `error` frame: nothing more comes.
    failed: bool,
}

fn frame(event: &str, data: &impl Serialize) -> SseEvent {
    SseEvent::default()
        .event(event)
        .json_data(data)
        .unwrap_or_else(|_| SseEvent::default().comment("bad frame"))
}

impl Streamer {
    /// Read the file and the status, and queue the frames.
    async fn poll(&mut self) {
        let current = {
            let (id, me) = (self.agent.id.to_string(), self.row.id.clone());
            self.state
                .db
                .read(move |c| agents::current_session(c, &id))
                .await
                .ok()
                .flatten()
                .is_some_and(|s| s == me)
        };
        let status = self.state.supervisor.status(self.agent.id).await;
        let active = current && status.is_some_and(AgentStatus::is_live);
        if self.loc.is_none() {
            match located(&self.state, &self.agent, &self.row, active).await {
                Ok(l) => self.loc = l,
                Err(e) => {
                    self.queue.push_back(frame(
                        "error",
                        &json!({"code": e.code, "message": e.message}),
                    ));
                    self.failed = true;
                    return;
                }
            }
        }
        if let Some(loc) = self.loc.clone() {
            let (st, after, rev) = (self.state.clone(), self.last_seq, self.last_rev);
            let read = tokio::task::spawn_blocking(move || {
                let entry = st.transcripts.refreshed(&loc)?;
                let g = lock_entry(&entry);
                let blocks: Vec<(bool, Block)> = match rev {
                    None => g
                        .log
                        .after(after)
                        .iter()
                        .map(|b| (true, b.clone()))
                        .collect(),
                    Some(r) => g
                        .log
                        .changed_since(r)
                        .into_iter()
                        .map(|b| (b.seq > after, b.clone()))
                        .collect(),
                };
                Ok::<_, std::io::Error>((blocks, g.log.rev()))
            })
            .await;
            if let Ok(Ok((sent, rev))) = read {
                self.last_rev = Some(rev);
                let (flags, mut blocks): (Vec<bool>, Vec<Block>) = sent.into_iter().unzip();
                for b in &mut blocks {
                    b.session.clone_from(&self.row.id);
                }
                annotate(&self.state, &self.agent.id.to_string(), &mut blocks).await;
                redact_blocks(&self.state, &self.view, &mut blocks);
                for (new, b) in flags.into_iter().zip(blocks) {
                    if new {
                        self.last_seq = self.last_seq.max(b.seq);
                        self.queue
                            .push_back(frame("block", &b).id(b.seq.to_string()));
                    } else {
                        self.queue.push_back(frame("update", &b));
                    }
                }
            }
        }
        let word = if active {
            status.map_or_else(|| "stopped".to_owned(), |s| s.to_string())
        } else {
            "ended".to_owned()
        };
        if self.last_status.as_deref() != Some(word.as_str()) {
            self.queue.push_back(frame(
                "status",
                &json!({
                    "status": word,
                    "active": active,
                    "agent_status": status.map(|s| s.to_string()),
                }),
            ));
            self.last_status = Some(word);
        }
    }

    /// Wait for a tick or an event of the agent.
    async fn wait(&mut self) -> bool {
        let me = self.agent.id.to_string();
        loop {
            tokio::select! {
                _ = self.tick.tick() => return true,
                ev = self.hub.recv() => match ev {
                    Ok(e) if e.agent_id.as_deref() == Some(me.as_str()) => return true,
                    Ok(_) => {}
                    Err(RecvError::Lagged(_)) => return true,
                    Err(RecvError::Closed) => return false,
                },
            }
        }
    }
}

async fn stream(
    State(state): State<Shared>,
    actor: Actor,
    Path((id, key)): Path<(String, String)>,
    ApiQuery(q): ApiQuery<StreamQuery>,
    headers: HeaderMap,
) -> ApiResult<Response> {
    let a = load(&state, &actor, id).await?;
    let row = session_row(&state, &a, &key).await?;
    let active = row.current && live(&state, &a.agent).await;
    let loc = located(&state, &a.agent, &row, active).await?;
    let after = q
        .after
        .or_else(|| {
            headers
                .get("last-event-id")
                .and_then(|v| v.to_str().ok())
                .and_then(|v| v.parse().ok())
        })
        .unwrap_or(0);
    let mut tick = tokio::time::interval(STREAM_TICK);
    tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
    let init = Streamer {
        hub: state.db.hub().subscribe(),
        state: Arc::clone(&state),
        view: crate::secrets::redact::view_of(&actor),
        agent: a.agent.clone(),
        row,
        loc,
        last_seq: after,
        last_rev: None,
        queue: VecDeque::new(),
        tick,
        last_status: None,
        failed: false,
    };
    let s = futures_util::stream::unfold(init, |mut s| async move {
        loop {
            if let Some(f) = s.queue.pop_front() {
                return Some((Ok::<_, Infallible>(f), s));
            }
            if s.failed || !s.wait().await {
                return None;
            }
            s.poll().await;
        }
    });
    Ok(Sse::new(s)
        .keep_alive(KeepAlive::new().interval(Duration::from_secs(15)))
        .into_response())
}
