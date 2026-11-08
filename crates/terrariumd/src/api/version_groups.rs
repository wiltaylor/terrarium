//! Explicit lead acknowledgements, never inferred from a green pipeline.
use axum::{Json, Router, extract::{Path, State}, routing::post};
use serde::Deserialize;
use serde_json::{Value, json};
use super::Shared;
use crate::{actor::Actor, error::{ApiError, ApiResult}, policy, store::{groups, version_groups}};

pub fn routes() -> Router<Shared> {
    Router::new()
        .route("/projects/{project}/version-groups/{key}/built", post(built))
        .route("/projects/{project}/version-groups/{key}/status", post(status))
}

#[derive(Deserialize)]
struct Built { version: String }

async fn built(State(state): State<Shared>, actor: Actor, Path((project, key)): Path<(String, String)>, Json(req): Json<Built>) -> ApiResult<Json<Value>> {
    policy::admin_project(&actor, &project, "acknowledging a built version")?;
    let label = actor.label();
    let result = state.db.write(&label, move |t| {
        let changed = version_groups::built(t, &actor, &project, &key, &req.version)?;
        Ok(json!({"changed":changed,"group":version_groups::get(t,&project,&key)?}))
    }).await?;
    Ok(Json(result))
}

#[derive(Deserialize)]
struct Status { step: String, state: String, text: Option<String>, eta: Option<String> }

async fn status(State(state): State<Shared>, actor: Actor, Path((project, key)): Path<(String, String)>, Json(req): Json<Status>) -> ApiResult<Json<Value>> {
    policy::admin_project(&actor, &project, "updating version steps")?;
    let label = actor.label();
    let result = state.db.write(&label, move |t| {
        let g = version_groups::get(t,&project,&key)?;
        if g.closed_at.is_some() { return Err(ApiError::conflict("version group is closed")); }
        if !version_groups::STEPS.contains(&req.step.as_str()) {
            return Err(ApiError::invalid("unknown version step"));
        }
        let changed = groups::step(t,&g,&req.step,Some(&req.state),req.text.as_deref(),req.eta.as_deref())?;
        Ok(json!({"changed":changed,"group":version_groups::get(t,&project,&key)?}))
    }).await?;
    Ok(Json(result))
}
