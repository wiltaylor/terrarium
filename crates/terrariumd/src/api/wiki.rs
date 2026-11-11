//! `/api/projects/{name}/wiki`, `/api/wiki/...` (PLAN §15.1).

use axum::Json;
use axum::extract::{Path, State};
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use serde::Deserialize;

use super::{ApiJson, ApiQuery, Shared, paged};
use crate::actor::Actor;
use crate::error::{ApiResult, Code};
use crate::policy;
use crate::store::Page;
use crate::store::wiki::{
    self, Append, Diff, NewPage, PageSummary, Review, Revision, Save, SectionEdit, WikiFilter,
    WikiPage,
};

pub fn routes() -> super::link_registry::RegisteredRouter {
    super::link_registry::RegisteredRouter::new()
        .route(
            "/projects/{name}/wiki",
            get(list).post(create),
            &[
                (axum::http::Method::GET, super::link_registry::Access::Link),
                (axum::http::Method::POST, super::link_registry::Access::Link),
            ],
        )
        .route(
            "/wiki/{id}",
            get(show).put(save).delete(remove),
            &[
                (axum::http::Method::GET, super::link_registry::Access::Link),
                (axum::http::Method::PUT, super::link_registry::Access::Link),
                (
                    axum::http::Method::DELETE,
                    super::link_registry::Access::Link,
                ),
            ],
        )
        .route(
            "/wiki/{id}/section",
            post(section),
            &[(axum::http::Method::POST, super::link_registry::Access::Link)],
        )
        .route(
            "/wiki/{id}/append",
            post(append),
            &[(axum::http::Method::POST, super::link_registry::Access::Link)],
        )
        .route(
            "/wiki/{id}/revisions",
            get(revisions),
            &[(axum::http::Method::GET, super::link_registry::Access::Link)],
        )
        .route(
            "/wiki/{id}/revisions/{rev}",
            get(revision),
            &[(axum::http::Method::GET, super::link_registry::Access::Link)],
        )
        .route(
            "/wiki/{id}/diff",
            get(diff),
            &[(axum::http::Method::GET, super::link_registry::Access::Link)],
        )
        .route(
            "/wiki/{id}/restore",
            post(restore),
            &[(axum::http::Method::POST, super::link_registry::Access::Link)],
        )
        .route(
            "/wiki/{id}/move",
            post(move_page),
            &[(axum::http::Method::POST, super::link_registry::Access::Link)],
        )
        .route(
            "/wiki/{id}/backlinks",
            get(backlinks),
            &[(axum::http::Method::GET, super::link_registry::Access::Link)],
        )
        .route(
            "/wiki/{id}/review",
            post(review),
            &[(axum::http::Method::POST, super::link_registry::Access::Link)],
        )
}

async fn list(
    State(state): State<Shared>,
    actor: Actor,
    Path(name): Path<String>,
    ApiQuery(page): ApiQuery<Page>,
    ApiQuery(f): ApiQuery<WikiFilter>,
) -> ApiResult<Response> {
    policy::read_project(&actor, &name)?;
    let project = name.clone();
    state
        .db
        .write("system", move |t| wiki::ensure_index(t, &project))
        .await?;
    let rows = state
        .db
        .read(move |c| wiki::list(c, &name, &f, &page))
        .await?;
    Ok(paged(rows))
}

async fn create(
    State(state): State<Shared>,
    actor: Actor,
    Path(name): Path<String>,
    ApiJson(body): ApiJson<NewPage>,
) -> ApiResult<(StatusCode, Json<WikiPage>)> {
    let p = state
        .db
        .write(actor.label(), move |t| wiki::create(t, &actor, &name, body))
        .await?;
    Ok((StatusCode::CREATED, Json(p)))
}

async fn show(
    State(state): State<Shared>,
    actor: Actor,
    Path(id): Path<String>,
) -> ApiResult<Json<WikiPage>> {
    let (a, k) = (actor.clone(), id.clone());
    match state.db.read(move |c| wiki::get(c, &a, &k)).await {
        Err(e) if e.code == Code::NotFound => {
            // Every project has an `index` page from the first access.
            let (a, k) = (actor.clone(), id.clone());
            let target = state
                .db
                .read(move |c| Ok(wiki::key_target(c, &k, a.project())))
                .await?;
            match target {
                Some((project, path)) if path == wiki::INDEX => {
                    policy::read_project(&actor, &project)?;
                    state
                        .db
                        .write("system", move |t| wiki::ensure_index(t, &project))
                        .await?;
                    Ok(Json(
                        state.db.read(move |c| wiki::get(c, &actor, &id)).await?,
                    ))
                }
                _ => Err(e),
            }
        }
        other => Ok(Json(other?)),
    }
}

async fn save(
    State(state): State<Shared>,
    actor: Actor,
    Path(id): Path<String>,
    ApiJson(body): ApiJson<Save>,
) -> ApiResult<Json<WikiPage>> {
    let p = state
        .db
        .write(actor.label(), move |t| wiki::save(t, &actor, &id, body))
        .await?;
    Ok(Json(p))
}

async fn remove(
    State(state): State<Shared>,
    actor: Actor,
    Path(id): Path<String>,
) -> ApiResult<Response> {
    state
        .db
        .write(actor.label(), move |t| wiki::delete(t, &actor, &id))
        .await?;
    Ok(StatusCode::NO_CONTENT.into_response())
}

async fn section(
    State(state): State<Shared>,
    actor: Actor,
    Path(id): Path<String>,
    ApiJson(body): ApiJson<SectionEdit>,
) -> ApiResult<Json<WikiPage>> {
    let p = state
        .db
        .write(actor.label(), move |t| wiki::section(t, &actor, &id, body))
        .await?;
    Ok(Json(p))
}

async fn append(
    State(state): State<Shared>,
    actor: Actor,
    Path(id): Path<String>,
    ApiJson(body): ApiJson<Append>,
) -> ApiResult<Json<WikiPage>> {
    let p = state
        .db
        .write(actor.label(), move |t| wiki::append(t, &actor, &id, body))
        .await?;
    Ok(Json(p))
}

async fn revisions(
    State(state): State<Shared>,
    actor: Actor,
    Path(id): Path<String>,
    ApiQuery(page): ApiQuery<Page>,
) -> ApiResult<Response> {
    let rows = state
        .db
        .read(move |c| wiki::revisions(c, &actor, &id, &page))
        .await?;
    Ok(paged(rows))
}

async fn revision(
    State(state): State<Shared>,
    actor: Actor,
    Path((id, rev)): Path<(String, i64)>,
) -> ApiResult<Json<Revision>> {
    Ok(Json(
        state
            .db
            .read(move |c| wiki::revision(c, &actor, &id, rev))
            .await?,
    ))
}

#[derive(Debug, Default, Deserialize)]
struct DiffQuery {
    a: Option<i64>,
    b: Option<i64>,
}

async fn diff(
    State(state): State<Shared>,
    actor: Actor,
    Path(id): Path<String>,
    ApiQuery(q): ApiQuery<DiffQuery>,
) -> ApiResult<Json<Diff>> {
    Ok(Json(
        state
            .db
            .read(move |c| wiki::diff(c, &actor, &id, q.a, q.b))
            .await?,
    ))
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct RestoreBody {
    rev: i64,
}

async fn restore(
    State(state): State<Shared>,
    actor: Actor,
    Path(id): Path<String>,
    ApiJson(b): ApiJson<RestoreBody>,
) -> ApiResult<Json<WikiPage>> {
    let p = state
        .db
        .write(actor.label(), move |t| wiki::restore(t, &actor, &id, b.rev))
        .await?;
    Ok(Json(p))
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct MoveBody {
    path: String,
}

async fn move_page(
    State(state): State<Shared>,
    actor: Actor,
    Path(id): Path<String>,
    ApiJson(b): ApiJson<MoveBody>,
) -> ApiResult<Json<WikiPage>> {
    let p = state
        .db
        .write(actor.label(), move |t| {
            wiki::move_page(t, &actor, &id, &b.path)
        })
        .await?;
    Ok(Json(p))
}

async fn backlinks(
    State(state): State<Shared>,
    actor: Actor,
    Path(id): Path<String>,
) -> ApiResult<Json<Vec<PageSummary>>> {
    Ok(Json(
        state
            .db
            .read(move |c| wiki::backlinks(c, &actor, &id))
            .await?,
    ))
}

async fn review(
    State(state): State<Shared>,
    actor: Actor,
    Path(id): Path<String>,
    ApiJson(b): ApiJson<Review>,
) -> ApiResult<Json<WikiPage>> {
    let p = state
        .db
        .write(actor.label(), move |t| wiki::review(t, &actor, &id, b))
        .await?;
    Ok(Json(p))
}
