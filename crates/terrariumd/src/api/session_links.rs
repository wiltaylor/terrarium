//! The sessions behind board work (`docs/api.md` §6.6, §7; PLAN §13,
//! §18): `GET /api/sessions/{id}`, `GET /api/sessions/{id}/transcript`,
//! and `GET /api/cards/{id}/sessions`.
//!
//! A session is named by its own ID only, so a link from a card works
//! after `agent rm`: the session row stays (migration 37), and its
//! transcript comes from the archive (`crate::session_archive`).
//!
//! Rights: a member of the session's project reads, the operator reads
//! all. The same as the card and the agent's own transcript.

use axum::extract::{Path, State};
use axum::routing::get;
use axum::{Json, Router};
use serde::Serialize;
use serde_json::Value;
use terrarium_core::{AgentId, AgentStatus, HarnessId};

use super::transcripts::{TranscriptQuery, transcript_page};
use super::{ApiQuery, Shared};
use crate::actor::Actor;
use crate::error::{ApiError, ApiResult};
use crate::policy;
use crate::store::board;
use crate::store::session_refs::{self, SessionRef, SessionRow};
use crate::transcripts::{LocateError, Located, locate_row};

pub fn routes() -> Router<Shared> {
    Router::new()
        .route("/sessions/{id}", get(show))
        .route("/sessions/{id}/transcript", get(transcript))
        .route("/cards/{id}/sessions", get(card_sessions))
}

/// `GET /api/sessions/{id}`.
#[derive(Debug, Serialize)]
struct SessionDetail {
    #[serde(flatten)]
    session: SessionRef,
    #[serde(skip_serializing_if = "Option::is_none")]
    harness_session_id: Option<String>,
    /// The agent's current session, and the agent runs.
    active: bool,
}

/// The session `id`, when `actor` may read its project.
async fn load(state: &Shared, actor: &Actor, id: String) -> ApiResult<SessionRow> {
    let key = id.clone();
    let row = state
        .db
        .read(move |c| session_refs::get(c, &key))
        .await?
        .ok_or_else(|| ApiError::not_found(format!("no session `{id}`")))?;
    policy::read_project(actor, &row.session.project)?;
    Ok(row)
}

async fn active(state: &Shared, row: &SessionRow) -> bool {
    if row.session.removed || !row.current {
        return false;
    }
    let Ok(agent) = row.session.agent_id.parse::<AgentId>() else {
        return false;
    };
    state
        .supervisor
        .status(agent)
        .await
        .is_some_and(AgentStatus::is_live)
}

async fn show(
    State(state): State<Shared>,
    actor: Actor,
    Path(id): Path<String>,
) -> ApiResult<Json<SessionDetail>> {
    let row = load(&state, &actor, id).await?;
    let active = active(&state, &row).await;
    Ok(Json(SessionDetail {
        harness_session_id: row.harness_session_id.clone(),
        session: row.session,
        active,
    }))
}

/// The harness of an archived file: the third part of
/// `archive/agents/<agent>/<harness>/...`.
fn archived_harness(rel: &str) -> Option<HarnessId> {
    rel.split('/').nth(3)?.parse().ok()
}

/// The file to read: the archive for a deleted agent, else the agent's
/// own session file. `None`: the active session has no file yet.
async fn located(state: &Shared, row: &SessionRow, active: bool) -> ApiResult<Option<Located>> {
    let harness: HarnessId = row
        .session
        .harness
        .parse()
        .map_err(|_| ApiError::not_found("the session has no known harness"))?;
    if row.session.removed {
        let rel = row.archive.as_deref().ok_or_else(|| {
            ApiError::not_found("the agent is deleted and the session has no archived file")
        })?;
        let path = crate::session_archive::archived_file(&state.state_dir, rel)
            .ok_or_else(|| ApiError::not_found("the archived session file is gone (pruned)"))?;
        return Ok(Some(Located {
            path,
            harness: archived_harness(rel).unwrap_or(harness),
        }));
    }
    let agent: AgentId = row
        .session
        .agent_id
        .parse()
        .map_err(|_| ApiError::internal("a session row with a bad agent ID"))?;
    match locate_row(
        state,
        agent,
        harness,
        &row.session.id,
        row.harness_session_id.as_deref(),
    )
    .await
    {
        Ok(loc) => Ok(Some(loc)),
        Err(LocateError::Missing(_)) if active => Ok(None),
        Err(LocateError::Missing(m)) => Err(ApiError::not_found(m)),
        Err(LocateError::Refused(m)) => Err(ApiError::forbidden(format!(
            "the session file is outside the agent's own dirs: {m}"
        ))),
    }
}

/// `GET /api/sessions/{id}/transcript`: the same page as
/// `GET /api/agents/{id}/sessions/{s}/transcript`, read-only, for a live
/// or a deleted agent.
async fn transcript(
    State(state): State<Shared>,
    actor: Actor,
    Path(id): Path<String>,
    ApiQuery(q): ApiQuery<TranscriptQuery>,
) -> ApiResult<Json<Value>> {
    let row = load(&state, &actor, id).await?;
    let active = active(&state, &row).await;
    let loc = located(&state, &row, active).await?;
    let harness = row.session.harness.parse().unwrap_or(HarnessId::Claude);
    let body = transcript_page(
        &state,
        &row.session.agent_id,
        &row.session.id,
        harness,
        loc,
        active,
        &q,
        &crate::secrets::redact::view_of(&actor),
    )
    .await?;
    Ok(Json(body))
}

/// `GET /api/cards/{id}/sessions`: the distinct sessions of the card's
/// comments and card events, oldest first.
async fn card_sessions(
    State(state): State<Shared>,
    actor: Actor,
    Path(id): Path<String>,
) -> ApiResult<Json<Vec<SessionRef>>> {
    let (card, sessions) = state
        .db
        .read(move |c| {
            let card = board::get_card(c, &id)?;
            let sessions = session_refs::of_card(c, &card.id)?;
            Ok((card, sessions))
        })
        .await?;
    policy::read_project(&actor, &card.project)?;
    Ok(Json(sessions))
}
