//! Ephemeral agents (PLAN §18, "Ephemeral agents", docs/api.md §6.10).
//!
//! - `POST /api/agents/{owner}/spawn`: spawn an ephemeral agent for
//!   `owner` (an agent, or `operator`).
//! - `GET /api/agents/{owner}/spawned`: the ephemeral agents of `owner`,
//!   running and finished, newest first.
//! - `POST /api/agents/{id}/done`: the ephemeral agent ends its work.
//! - `POST /api/agents/{id}/cancel`: cancel an ephemeral agent.
//! - `GET /api/spawns/{id}`: one spawn.
//!
//! The rules and the run time are in [`crate::spawn`].

use axum::extract::{Path, State};
use axum::http::StatusCode;
use axum::response::Response;
use axum::routing::{get, post};
use axum::{Json, Router};
use serde::Deserialize;

use super::{ApiJson, ApiQuery, Shared, paged};
use crate::actor::Actor;
use crate::error::{ApiError, ApiResult};
use crate::policy;
use crate::spawn::{SpawnBody, Spawned};
use crate::store::Page;
use crate::store::spawns::{self, OPERATOR, Spawn};

pub fn routes() -> Router<Shared> {
    Router::new()
        .route("/agents/{id}/spawn", post(spawn))
        .route("/agents/{id}/spawned", get(spawned))
        .route("/agents/{id}/done", post(done))
        .route("/agents/{id}/cancel", post(cancel))
        .route("/spawns/{id}", get(show))
}

async fn spawn(
    State(state): State<Shared>,
    actor: Actor,
    Path(owner): Path<String>,
    ApiJson(body): ApiJson<SpawnBody>,
) -> ApiResult<(StatusCode, Json<Spawned>)> {
    let max = state.waits.tuning().max_timeout;
    let timeout = body
        .timeout
        .as_deref()
        .map(|t| super::waits::parse_span("timeout", t, max))
        .transpose()?;
    let lib = super::skills::library_opt(&state);
    let s = crate::spawn::spawn(&state, &actor, owner, body, lib, timeout).await?;
    Ok((StatusCode::CREATED, Json(s)))
}

#[derive(Debug, Default, Deserialize)]
struct SpawnedQuery {
    /// For `operator`: one project only.
    project: Option<String>,
}

/// The ephemeral agents of an agent: whoever may read the agent. Those
/// of `operator`: the operator only.
async fn spawned(
    State(state): State<Shared>,
    actor: Actor,
    Path(owner): Path<String>,
    ApiQuery(page): ApiQuery<Page>,
    ApiQuery(q): ApiQuery<SpawnedQuery>,
) -> ApiResult<Response> {
    let (owner, project) = if owner == OPERATOR {
        policy::operator_only(&actor, "reading the ephemeral agents of the operator")?;
        (OPERATOR.to_owned(), q.project)
    } else {
        let v = super::agents::load(&state, &actor, owner).await?;
        (v.agent.id.to_string(), None)
    };
    let rows = state
        .db
        .read(move |c| spawns::list_of(c, &owner, project.as_deref(), &page))
        .await?;
    Ok(paged(rows))
}

/// The body of `POST /api/agents/{id}/done`.
#[derive(Debug, Default, Deserialize)]
#[serde(deny_unknown_fields)]
struct DoneBody {
    #[serde(default)]
    result: Option<String>,
}

/// Only the ephemeral agent itself.
async fn done(
    State(state): State<Shared>,
    actor: Actor,
    Path(id): Path<String>,
    ApiJson(body): ApiJson<DoneBody>,
) -> ApiResult<(StatusCode, Json<Spawn>)> {
    let v = super::agents::load(&state, &actor, id).await?;
    let me = v.agent.id.to_string();
    match actor.agent() {
        Some(a) if a.id == me => {}
        _ => {
            return Err(ApiError::forbidden(
                "`agent done` is for the ephemeral agent itself; cancel it with \
                 POST /api/agents/{id}/cancel",
            ));
        }
    }
    let sp = crate::spawn::done(&state, &me, body.result).await?;
    Ok((StatusCode::ACCEPTED, Json(sp)))
}

/// The owner, a project_admin of the project, or the operator.
async fn cancel(
    State(state): State<Shared>,
    actor: Actor,
    Path(id): Path<String>,
) -> ApiResult<Json<Spawn>> {
    let v = super::agents::load(&state, &actor, id).await?;
    if !v.ephemeral {
        return Err(ApiError::invalid(format!(
            "`{}` is not an ephemeral agent; stop it with `terrarium agent stop`",
            v.agent.name
        )));
    }
    Ok(Json(
        crate::spawn::cancel(&state, &actor, &v.agent.id.to_string()).await?,
    ))
}

/// One spawn: whoever reads its project; one of the operator: the
/// operator only.
async fn show(
    State(state): State<Shared>,
    actor: Actor,
    Path(id): Path<String>,
) -> ApiResult<Json<Spawn>> {
    let sp = state.db.read(move |c| spawns::get(c, &id)).await?;
    policy::read_project(&actor, &sp.project)?;
    Ok(Json(sp))
}
