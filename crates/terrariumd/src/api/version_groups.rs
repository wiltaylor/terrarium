//! Explicit lead acknowledgements, never inferred from a green pipeline.
use super::Shared;
use crate::{
    actor::Actor,
    error::{ApiError, ApiResult},
    policy,
    store::{groups, version_groups},
};
use axum::{
    Json, axum::Router,
    extract::{Path, State},
    routing::post,
};
use serde::Deserialize;
use serde_json::{Value, json};

pub fn routes() -> super::link_registry::RegisteredRouter {
    super::link_registry::RegisteredRouter::new()
        .route(
            "/projects/{project}/version-groups/{key}/built",
            post(built), &[(axum::http::Method::POST, super::link_registry::Access::Link)])
        .route(
            "/projects/{project}/version-groups/{key}/status",
            post(status), &[(axum::http::Method::POST, super::link_registry::Access::Link)])
        .route(
            "/projects/{project}/version-groups/{key}/remove",
            post(remove), &[(axum::http::Method::POST, super::link_registry::Access::Link)])
        .route("/projects/{project}/version-groups/{key}/add", post(add), &[(axum::http::Method::POST, super::link_registry::Access::Link)])
}

async fn add(
    State(state): State<Shared>,
    actor: Actor,
    Path((project, key)): Path<(String, String)>,
    Json(req): Json<Member>,
) -> ApiResult<Json<Value>> {
    policy::admin_project(&actor, &project, "adding a version group member")?;
    let label = actor.label();
    let result = state
        .db
        .write(&label, move |t| {
            let changed = version_groups::add(t, &actor, &project, &key, &req.card)?;
            Ok(json!({"changed":changed,"group":version_groups::get(t,&project,&key)?}))
        })
        .await?;
    Ok(Json(result))
}

#[derive(Deserialize)]
struct Built {
    version: String,
}

#[derive(Deserialize)]
struct Member {
    card: String,
}

async fn remove(
    State(state): State<Shared>,
    actor: Actor,
    Path((project, key)): Path<(String, String)>,
    Json(req): Json<Member>,
) -> ApiResult<Json<Value>> {
    policy::admin_project(&actor, &project, "removing a version group member")?;
    let result = state
        .db
        .write(actor.label(), move |t| {
            let g = version_groups::get(t, &project, &key)?;
            if g.closed_at.is_some() {
                return Err(ApiError::conflict("version group is closed"));
            }
            let changed = groups::remove(t, &g, &req.card)?;
            if changed {
                t.audit(
                    "version.member_removed",
                    &g.id,
                    Some(&project),
                    json!({"card":req.card}),
                )?;
            }
            Ok(json!({"changed":changed,"group":version_groups::get(t,&project,&key)?}))
        })
        .await?;
    Ok(Json(result))
}

async fn built(
    State(state): State<Shared>,
    actor: Actor,
    Path((project, key)): Path<(String, String)>,
    Json(req): Json<Built>,
) -> ApiResult<Json<Value>> {
    policy::admin_project(&actor, &project, "acknowledging a built version")?;
    let label = actor.label();
    let result = state
        .db
        .write(&label, move |t| {
            let (changed, also_done) =
                version_groups::built(t, &actor, &project, &key, &req.version)?;
            Ok(json!({
                "changed":changed,
                "also_done":also_done,
                "group":version_groups::get(t,&project,&key)?
            }))
        })
        .await?;
    Ok(Json(result))
}

#[derive(Deserialize)]
struct Status {
    step: String,
    state: String,
    text: Option<String>,
    eta: Option<String>,
}

async fn status(
    State(state): State<Shared>,
    actor: Actor,
    Path((project, key)): Path<(String, String)>,
    Json(req): Json<Status>,
) -> ApiResult<Json<Value>> {
    policy::admin_project(&actor, &project, "updating version steps")?;
    let label = actor.label();
    let result = state
        .db
        .write(&label, move |t| {
            let g = version_groups::get(t, &project, &key)?;
            if g.closed_at.is_some() {
                return Err(ApiError::conflict("version group is closed"));
            }
            let changed = version_groups::step(
                t,
                &g,
                &req.step,
                &req.state,
                req.text.as_deref(),
                req.eta.as_deref(),
            )?;
            Ok(json!({"changed":changed,"group":version_groups::get(t,&project,&key)?}))
        })
        .await?;
    Ok(Json(result))
}
