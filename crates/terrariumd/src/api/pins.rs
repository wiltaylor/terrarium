//! `/api/pins` (docs/api.md §6.11): the pinned conversations of the
//! operator. The dashboard shows them as tabs in its right-hand side bar.
//! The list is on the daemon, so it is the same in each browser. Only the
//! operator has pins.

use axum::Json;
use axum::extract::{Path, State};
use axum::routing::get;

use super::Shared;
use crate::actor::Actor;
use crate::error::ApiResult;
use crate::policy;
use crate::store::agents;
use crate::store::pins::{self, Pin};

pub fn routes() -> super::link_registry::RegisteredRouter {
    super::link_registry::RegisteredRouter::new()
        .route(
            "/pins",
            get(list),
            &[(
                axum::http::Method::GET,
                super::link_registry::Access::OperatorRead,
            )],
        )
        .route(
            "/pins/{agent}",
            axum::routing::put(pin).delete(unpin),
            &[
                (
                    axum::http::Method::PUT,
                    super::link_registry::Access::OperatorOnly,
                ),
                (
                    axum::http::Method::DELETE,
                    super::link_registry::Access::OperatorOnly,
                ),
            ],
        )
}

/// `GET /api/pins`: the pins, oldest first.
async fn list(State(state): State<Shared>, actor: Actor) -> ApiResult<Json<Vec<Pin>>> {
    policy::operator_only(&actor, "pinned conversations")?;
    let owner = actor.address();
    Ok(Json(state.db.read(move |c| pins::list(c, &owner)).await?))
}

/// `PUT /api/pins/{agent}`: pin the agent (ULID or `project/name`) at the
/// end. Answers with the new list.
async fn pin(
    State(state): State<Shared>,
    actor: Actor,
    Path(agent): Path<String>,
) -> ApiResult<Json<Vec<Pin>>> {
    policy::operator_only(&actor, "pinned conversations")?;
    let owner = actor.address();
    let rows = state
        .db
        .write(actor.label(), move |t| {
            let a = agents::find(t, &agent, None)?;
            let id = a.agent.id.to_string();
            pins::pin(t, &owner, &id, &a.agent.project)?;
            pins::list(t, &owner)
        })
        .await?;
    Ok(Json(rows))
}

/// `DELETE /api/pins/{agent}`: unpin the agent. An agent that is gone, or
/// is not pinned, is no error. Answers with the new list.
async fn unpin(
    State(state): State<Shared>,
    actor: Actor,
    Path(agent): Path<String>,
) -> ApiResult<Json<Vec<Pin>>> {
    policy::operator_only(&actor, "pinned conversations")?;
    let owner = actor.address();
    let rows = state
        .db
        .write(actor.label(), move |t| {
            let (id, project) = match agents::find(t, &agent, None) {
                Ok(a) => (a.agent.id.to_string(), Some(a.agent.project)),
                Err(_) => (agent, None),
            };
            pins::unpin(t, &owner, &id, project.as_deref())?;
            pins::list(t, &owner)
        })
        .await?;
    Ok(Json(rows))
}
