//! `/api/projects/{name}/versions`, `/api/versions/...` (PLAN §15.3).

use axum::body::Bytes;
use axum::extract::{Path, State};
use axum::http::StatusCode;
use axum::response::Response;
use axum::routing::{get, post};
use axum::{Json, Router};
use serde::Deserialize;

use super::longpoll::{WaitQuery, hold};
use super::{ApiJson, ApiQuery, Shared, paged};
use crate::actor::Actor;
use crate::error::{ApiError, ApiResult};
use crate::policy;
use crate::store::Page;
use crate::store::versions::{self, Compare, NewVersion, Version, VersionFilter, VersionPatch};

pub fn routes() -> Router<Shared> {
    Router::new()
        .route("/projects/{name}/versions", get(list).post(create))
        .route("/versions/{id}", get(show).patch(update))
        .route("/versions/{id}/submit", post(submit))
        .route("/versions/{id}/approve", post(approve))
        .route("/versions/{id}/reject", post(reject))
        .route("/versions/{id}/release", post(release))
        .route("/versions/{id}/archive", post(archive))
        .route("/versions/{id}/unarchive", post(unarchive))
        .route("/versions/{id}/compare", get(compare))
}

/// `Version.url` as the dashboard opens it: absolute with
/// `TERRARIUM_PUBLIC_URL`, else the path.
fn absolute<V: std::borrow::BorrowMut<Version>>(state: &Shared, mut v: V) -> V {
    let url = state.web.page_url(&v.borrow().url);
    v.borrow_mut().url = url;
    v
}

/// The event kinds that wake a `?wait=` list.
const VERSION_KINDS: [&str; 8] = [
    "version.created",
    "version.updated",
    "version.submitted",
    "version.approved",
    "version.rejected",
    "version.released",
    "version.archived",
    "version.unarchived",
];

async fn list(
    State(state): State<Shared>,
    actor: Actor,
    Path(name): Path<String>,
    ApiQuery(page): ApiQuery<Page>,
    ApiQuery(f): ApiQuery<VersionFilter>,
    ApiQuery(w): ApiQuery<WaitQuery>,
) -> ApiResult<Response> {
    policy::read_project(&actor, &name)?;
    let rows = hold(&state, w, &VERSION_KINDS, || {
        let (n, f2, p2) = (name.clone(), f.clone(), page.clone());
        let db = state.db.clone();
        async move { db.read(move |c| versions::list(c, &n, &f2, &p2)).await }
    })
    .await?;
    let mut rows = rows;
    for v in &mut rows.items {
        absolute(&state, v);
    }
    Ok(paged(rows))
}

async fn create(
    State(state): State<Shared>,
    actor: Actor,
    Path(name): Path<String>,
    ApiJson(body): ApiJson<NewVersion>,
) -> ApiResult<(StatusCode, Json<Version>)> {
    let v = state
        .db
        .write(actor.label(), move |t| {
            versions::create(t, &actor, &name, body)
        })
        .await?;
    Ok((StatusCode::CREATED, Json(absolute(&state, v))))
}

async fn show(
    State(state): State<Shared>,
    actor: Actor,
    Path(id): Path<String>,
) -> ApiResult<Json<Version>> {
    let v = state
        .db
        .read(move |c| versions::get(c, &actor, &id))
        .await?;
    Ok(Json(absolute(&state, v)))
}

async fn update(
    State(state): State<Shared>,
    actor: Actor,
    Path(id): Path<String>,
    ApiJson(patch): ApiJson<VersionPatch>,
) -> ApiResult<Json<Version>> {
    let v = state
        .db
        .write(actor.label(), move |t| {
            versions::update(t, &actor, &id, patch)
        })
        .await?;
    Ok(Json(absolute(&state, v)))
}

#[derive(Debug, Default, Deserialize)]
#[serde(deny_unknown_fields)]
struct CommentBody {
    #[serde(default)]
    comment: Option<String>,
}

/// The body is optional: none, or `{"comment"}`.
fn comment_of(body: &Bytes) -> ApiResult<Option<String>> {
    if body.is_empty() {
        return Ok(None);
    }
    let b: CommentBody = serde_json::from_slice(body)
        .map_err(|e| ApiError::bad_request(format!("bad body: {e}")))?;
    Ok(b.comment)
}

async fn submit(
    State(state): State<Shared>,
    actor: Actor,
    Path(id): Path<String>,
    body: Bytes,
) -> ApiResult<Json<Version>> {
    let c = comment_of(&body)?;
    let base = state.web.mail_base().to_owned();
    let v = state
        .db
        .write(actor.label(), move |t| {
            versions::submit(t, &actor, &id, c.as_deref(), &base)
        })
        .await?;
    Ok(Json(absolute(&state, v)))
}

async fn decide(
    state: Shared,
    actor: Actor,
    id: String,
    body: Bytes,
    approve: bool,
) -> ApiResult<Json<Version>> {
    let c = comment_of(&body)?;
    let v = state
        .db
        .write(actor.label(), move |t| {
            versions::decide(t, &actor, &id, approve, c.as_deref())
        })
        .await?;
    Ok(Json(absolute(&state, v)))
}

async fn approve(
    State(state): State<Shared>,
    actor: Actor,
    Path(id): Path<String>,
    body: Bytes,
) -> ApiResult<Json<Version>> {
    decide(state, actor, id, body, true).await
}

async fn reject(
    State(state): State<Shared>,
    actor: Actor,
    Path(id): Path<String>,
    body: Bytes,
) -> ApiResult<Json<Version>> {
    decide(state, actor, id, body, false).await
}

async fn release(
    State(state): State<Shared>,
    actor: Actor,
    Path(id): Path<String>,
    body: Bytes,
) -> ApiResult<Json<Version>> {
    let c = comment_of(&body)?;
    let v = state
        .db
        .write(actor.label(), move |t| {
            versions::release(t, &actor, &id, c.as_deref())
        })
        .await?;
    Ok(Json(absolute(&state, v)))
}

async fn set_archived(
    state: Shared,
    actor: Actor,
    id: String,
    archived: bool,
) -> ApiResult<Json<Version>> {
    let v = state
        .db
        .write(actor.label(), move |t| {
            versions::set_archived(t, &actor, &id, archived)
        })
        .await?;
    Ok(Json(absolute(&state, v)))
}

async fn archive(
    State(state): State<Shared>,
    actor: Actor,
    Path(id): Path<String>,
) -> ApiResult<Json<Version>> {
    set_archived(state, actor, id, true).await
}

async fn unarchive(
    State(state): State<Shared>,
    actor: Actor,
    Path(id): Path<String>,
) -> ApiResult<Json<Version>> {
    set_archived(state, actor, id, false).await
}

#[derive(Debug, Default, Deserialize)]
struct CompareQuery {
    to: Option<String>,
}

async fn compare(
    State(state): State<Shared>,
    actor: Actor,
    Path(id): Path<String>,
    ApiQuery(q): ApiQuery<CompareQuery>,
) -> ApiResult<Json<Compare>> {
    let to =
        q.to.ok_or_else(|| ApiError::bad_request("compare needs ?to=<version>"))?;
    Ok(Json(
        state
            .db
            .read(move |c| versions::compare(c, &actor, &id, &to))
            .await?,
    ))
}
