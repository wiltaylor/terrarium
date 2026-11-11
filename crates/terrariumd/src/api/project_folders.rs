//! `/api/project-folders` (docs/api.md §6.12, TER-48): the folders that
//! the dashboard shows under Projects in its sidebar, and the folder of
//! each project. They are on the daemon, so they are the same in each
//! browser. Only the operator and a sysadmin read or write them: folders
//! order the sidebar and change no right, scope, or name.

use axum::Json;
use axum::extract::{Path, State};
use axum::http::StatusCode;
use axum::routing::{get, patch, put};

use super::{ApiJson, Shared};
use crate::actor::Actor;
use crate::error::ApiResult;
use crate::policy;
use crate::store::project_folders::{self, Folder, FolderPatch, NewFolder, Place, Placement, Tree};

const WHAT: &str = "project folders";

pub fn routes() -> super::link_registry::RegisteredRouter {
    super::link_registry::RegisteredRouter::new()
        .route(
            "/project-folders",
            get(list).post(create),
            &[
                (axum::http::Method::GET, super::link_registry::Access::Link),
                (axum::http::Method::POST, super::link_registry::Access::Link),
            ],
        )
        .route(
            "/project-folders/{id}",
            patch(update).delete(remove),
            &[
                (
                    axum::http::Method::PATCH,
                    super::link_registry::Access::Link,
                ),
                (
                    axum::http::Method::DELETE,
                    super::link_registry::Access::Link,
                ),
            ],
        )
        .route(
            "/projects/{name}/folder",
            put(place),
            &[(axum::http::Method::PUT, super::link_registry::Access::Link)],
        )
}

/// `GET /api/project-folders`: every folder and every placed project.
async fn list(State(state): State<Shared>, actor: Actor) -> ApiResult<Json<Tree>> {
    policy::sysadmin(&actor, WHAT)?;
    Ok(Json(state.db.read(project_folders::list).await?))
}

/// `POST /api/project-folders`: a new folder at the end of its parent.
async fn create(
    State(state): State<Shared>,
    actor: Actor,
    ApiJson(body): ApiJson<NewFolder>,
) -> ApiResult<(StatusCode, Json<Folder>)> {
    policy::sysadmin(&actor, WHAT)?;
    let f = state
        .db
        .write(actor.label(), move |t| project_folders::create(t, &body))
        .await?;
    Ok((StatusCode::CREATED, Json(f)))
}

/// `PATCH /api/project-folders/{id}`: rename, move, or reorder.
async fn update(
    State(state): State<Shared>,
    actor: Actor,
    Path(id): Path<String>,
    ApiJson(body): ApiJson<FolderPatch>,
) -> ApiResult<Json<Folder>> {
    policy::sysadmin(&actor, WHAT)?;
    let f = state
        .db
        .write(actor.label(), move |t| {
            project_folders::update(t, &id, &body)
        })
        .await?;
    Ok(Json(f))
}

/// `DELETE /api/project-folders/{id}`: its content moves up one level.
async fn remove(
    State(state): State<Shared>,
    actor: Actor,
    Path(id): Path<String>,
) -> ApiResult<StatusCode> {
    policy::sysadmin(&actor, WHAT)?;
    state
        .db
        .write(actor.label(), move |t| project_folders::delete(t, &id))
        .await?;
    Ok(StatusCode::NO_CONTENT)
}

/// `PUT /api/projects/{name}/folder`: put the project in a folder, or at
/// the root with `folder_id: null`.
async fn place(
    State(state): State<Shared>,
    actor: Actor,
    Path(name): Path<String>,
    ApiJson(body): ApiJson<Place>,
) -> ApiResult<Json<Placement>> {
    policy::sysadmin(&actor, WHAT)?;
    let p = state
        .db
        .write(actor.label(), move |t| {
            project_folders::place(t, &name, &body)
        })
        .await?;
    Ok(Json(p))
}
