//! `/api/projects/{name}/db/...`: the DB of a project (PLAN §15.4).

use axum::extract::{Path, State};
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use axum::routing::get;
use axum::{Json, Router};
use serde::Deserialize;

use super::{ApiJson, ApiQuery, Shared, paged};
use crate::actor::Actor;
use crate::error::ApiResult;
use crate::store::Page;
use crate::store::records::{
    self, DbType, NewRecord, NewType, QueryResult, Record, RecordPatch, TypePatch,
};

pub fn routes() -> Router<Shared> {
    Router::new()
        .route(
            "/projects/{name}/db/types",
            get(list_types).post(create_type),
        )
        .route(
            "/projects/{name}/db/types/{type}",
            get(show_type).patch(update_type).delete(delete_type),
        )
        .route(
            "/projects/{name}/db/records",
            axum::routing::post(create_record),
        )
        .route(
            "/projects/{name}/db/records/{id}",
            get(show_record).patch(update_record).delete(delete_record),
        )
        .route("/projects/{name}/db/query", get(query))
}

async fn list_types(
    State(state): State<Shared>,
    actor: Actor,
    Path(name): Path<String>,
    ApiQuery(page): ApiQuery<Page>,
) -> ApiResult<Response> {
    let rows = state
        .db
        .read(move |c| records::list_types(c, &actor, &name, &page))
        .await?;
    Ok(paged(rows))
}

async fn create_type(
    State(state): State<Shared>,
    actor: Actor,
    Path(name): Path<String>,
    ApiJson(body): ApiJson<NewType>,
) -> ApiResult<(StatusCode, Json<DbType>)> {
    let t = state
        .db
        .write(actor.label(), move |t| {
            records::create_type(t, &actor, &name, body)
        })
        .await?;
    Ok((StatusCode::CREATED, Json(t)))
}

async fn show_type(
    State(state): State<Shared>,
    actor: Actor,
    Path((name, ty)): Path<(String, String)>,
) -> ApiResult<Json<DbType>> {
    Ok(Json(
        state
            .db
            .read(move |c| records::get_type(c, &actor, &name, &ty))
            .await?,
    ))
}

async fn update_type(
    State(state): State<Shared>,
    actor: Actor,
    Path((name, ty)): Path<(String, String)>,
    ApiJson(body): ApiJson<TypePatch>,
) -> ApiResult<Json<DbType>> {
    let t = state
        .db
        .write(actor.label(), move |t| {
            records::update_type(t, &actor, &name, &ty, body)
        })
        .await?;
    Ok(Json(t))
}

#[derive(Debug, Default, Deserialize)]
struct ConfirmQuery {
    confirm: Option<String>,
}

async fn delete_type(
    State(state): State<Shared>,
    actor: Actor,
    Path((name, ty)): Path<(String, String)>,
    ApiQuery(q): ApiQuery<ConfirmQuery>,
) -> ApiResult<Response> {
    state
        .db
        .write(actor.label(), move |t| {
            records::delete_type(t, &actor, &name, &ty, q.confirm.as_deref())
        })
        .await?;
    Ok(StatusCode::NO_CONTENT.into_response())
}

async fn create_record(
    State(state): State<Shared>,
    actor: Actor,
    Path(name): Path<String>,
    ApiJson(body): ApiJson<NewRecord>,
) -> ApiResult<(StatusCode, Json<Record>)> {
    let r = state
        .db
        .write(actor.label(), move |t| {
            records::create_record(t, &actor, &name, body)
        })
        .await?;
    Ok((StatusCode::CREATED, Json(r)))
}

async fn show_record(
    State(state): State<Shared>,
    actor: Actor,
    Path((name, id)): Path<(String, String)>,
) -> ApiResult<Json<Record>> {
    Ok(Json(
        state
            .db
            .read(move |c| records::get_record(c, &actor, &name, &id))
            .await?,
    ))
}

async fn update_record(
    State(state): State<Shared>,
    actor: Actor,
    Path((name, id)): Path<(String, String)>,
    ApiJson(body): ApiJson<RecordPatch>,
) -> ApiResult<Json<Record>> {
    let r = state
        .db
        .write(actor.label(), move |t| {
            records::update_record(t, &actor, &name, &id, body)
        })
        .await?;
    Ok(Json(r))
}

async fn delete_record(
    State(state): State<Shared>,
    actor: Actor,
    Path((name, id)): Path<(String, String)>,
) -> ApiResult<Response> {
    state
        .db
        .write(actor.label(), move |t| {
            records::delete_record(t, &actor, &name, &id)
        })
        .await?;
    Ok(StatusCode::NO_CONTENT.into_response())
}

#[derive(Debug, Default, Deserialize)]
struct QueryParams {
    #[serde(default)]
    q: String,
    #[serde(rename = "type")]
    type_name: Option<String>,
    cursor: Option<String>,
}

async fn query(
    State(state): State<Shared>,
    actor: Actor,
    Path(name): Path<String>,
    ApiQuery(p): ApiQuery<QueryParams>,
) -> ApiResult<Json<QueryResult>> {
    Ok(Json(
        state
            .db
            .read(move |c| {
                records::run_query(
                    c,
                    &actor,
                    &name,
                    &p.q,
                    p.type_name.as_deref().filter(|t| !t.is_empty()),
                    p.cursor.as_deref(),
                )
            })
            .await?,
    ))
}
