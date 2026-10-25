//! `/api/projects/{name}/memories`, `/api/memories/...` (PLAN §14).

use axum::body::Bytes;
use axum::extract::{Path, State};
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use axum::{Json, Router};
use serde::Deserialize;

use super::{ApiJson, ApiQuery, Shared, paged};
use crate::actor::Actor;
use crate::error::{ApiError, ApiResult};
use crate::policy;
use crate::store::Page;
use crate::store::memory::{
    self, Created, Graph, Memory, MemoryFilter, MemoryPatch, NewLinkIn, NewMemory,
};

pub fn routes() -> Router<Shared> {
    Router::new()
        .route("/projects/{name}/memories", get(list).post(create))
        .route("/memories/{id}", get(show).patch(update).delete(remove))
        .route("/memories/{id}/graph", get(graph))
        .route("/memories/{id}/links", post(link).delete(unlink))
        .route("/memories/{id}/supersede", post(supersede))
}

async fn list(
    State(state): State<Shared>,
    actor: Actor,
    Path(name): Path<String>,
    ApiQuery(page): ApiQuery<Page>,
    ApiQuery(f): ApiQuery<MemoryFilter>,
) -> ApiResult<Response> {
    policy::read_project(&actor, &name)?;
    let rows = state
        .db
        .read(move |c| {
            let author = match f.author.as_deref() {
                None => None,
                Some("me") => Some(actor.address()),
                Some(a) => Some(crate::store::mail::resolve_known(c, a, Some(&name))?.key()),
            };
            memory::list(c, &name, &f, author.as_deref(), &page)
        })
        .await?;
    Ok(paged(rows))
}

async fn create(
    State(state): State<Shared>,
    actor: Actor,
    Path(name): Path<String>,
    ApiJson(body): ApiJson<NewMemory>,
) -> ApiResult<(StatusCode, Json<Created>)> {
    let m = state
        .db
        .write(actor.label(), move |t| {
            memory::create(t, &actor, &name, body)
        })
        .await?;
    Ok((StatusCode::CREATED, Json(m)))
}

async fn show(
    State(state): State<Shared>,
    actor: Actor,
    Path(id): Path<String>,
) -> ApiResult<Json<Memory>> {
    Ok(Json(
        state.db.read(move |c| memory::get(c, &actor, &id)).await?,
    ))
}

async fn update(
    State(state): State<Shared>,
    actor: Actor,
    Path(id): Path<String>,
    ApiJson(patch): ApiJson<MemoryPatch>,
) -> ApiResult<Json<Memory>> {
    let m = state
        .db
        .write(actor.label(), move |t| {
            memory::update(t, &actor, &id, patch)
        })
        .await?;
    Ok(Json(m))
}

async fn remove(
    State(state): State<Shared>,
    actor: Actor,
    Path(id): Path<String>,
) -> ApiResult<Response> {
    state
        .db
        .write(actor.label(), move |t| memory::delete(t, &actor, &id))
        .await?;
    Ok(StatusCode::NO_CONTENT.into_response())
}

#[derive(Debug, Default, Deserialize)]
struct DepthQuery {
    depth: Option<usize>,
}

async fn graph(
    State(state): State<Shared>,
    actor: Actor,
    Path(id): Path<String>,
    ApiQuery(q): ApiQuery<DepthQuery>,
) -> ApiResult<Json<Graph>> {
    let depth = q.depth.unwrap_or(1);
    Ok(Json(
        state
            .db
            .read(move |c| memory::graph(c, &actor, &id, depth))
            .await?,
    ))
}

async fn link(
    State(state): State<Shared>,
    actor: Actor,
    Path(id): Path<String>,
    ApiJson(l): ApiJson<NewLinkIn>,
) -> ApiResult<(StatusCode, Json<Memory>)> {
    let m = state
        .db
        .write(actor.label(), move |t| memory::add_link(t, &actor, &id, &l))
        .await?;
    Ok((StatusCode::CREATED, Json(m)))
}

#[derive(Debug, Default, Deserialize)]
struct LinkKey {
    to: Option<String>,
    relation: Option<String>,
}

/// `DELETE /api/memories/{id}/links`: the link in the query
/// (`?to=&relation=`) or in a JSON body.
async fn unlink(
    State(state): State<Shared>,
    actor: Actor,
    Path(id): Path<String>,
    ApiQuery(q): ApiQuery<LinkKey>,
    body: Bytes,
) -> ApiResult<Json<Memory>> {
    let key = if body.is_empty() {
        q
    } else {
        #[derive(Deserialize)]
        struct B {
            to: String,
            relation: String,
            #[serde(default)]
            #[allow(dead_code)]
            note: Option<String>,
        }
        let b: B = serde_json::from_slice(&body)
            .map_err(|e| ApiError::bad_request(format!("bad JSON body: {e}")))?;
        LinkKey {
            to: Some(b.to),
            relation: Some(b.relation),
        }
    };
    let (Some(to), Some(relation)) = (key.to, key.relation) else {
        return Err(ApiError::bad_request(
            "removing a link needs `to` and `relation`",
        ));
    };
    let m = state
        .db
        .write(actor.label(), move |t| {
            memory::remove_link(t, &actor, &id, &to, &relation)
        })
        .await?;
    Ok(Json(m))
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct SupersedeBody {
    by: String,
}

async fn supersede(
    State(state): State<Shared>,
    actor: Actor,
    Path(id): Path<String>,
    ApiJson(b): ApiJson<SupersedeBody>,
) -> ApiResult<Json<Memory>> {
    let m = state
        .db
        .write(actor.label(), move |t| {
            memory::supersede(t, &actor, &id, &b.by)
        })
        .await?;
    Ok(Json(m))
}
