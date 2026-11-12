//! Project page CRUD. Writes use the dashboard administration right.
use axum::{Json, extract::{Path, State}, http::{Method, StatusCode}, routing::get};
use serde_json::{Value, json};
use super::{ApiJson, Shared, link_registry::{Access, RegisteredRouter}};
use crate::{actor::Actor, error::ApiResult, policy, store::pages::{self, PageInput, ProjectPage}};

pub fn routes() -> RegisteredRouter {
    RegisteredRouter::new()
        .route("/projects/{name}/pages", get(list), &[(Method::GET, Access::Link)])
        .route("/projects/{name}/pages/{slug}", get(show).put(set).delete(remove),
            &[(Method::GET, Access::Link), (Method::PUT, Access::Link), (Method::DELETE, Access::Link)])
}

async fn list(State(s): State<Shared>, actor: Actor, Path(project): Path<String>) -> ApiResult<Json<Value>> {
    policy::read_project(&actor, &project)?;
    let items = s.db.read(move |c| pages::list(c, &project)).await?;
    Ok(Json(json!({"items": items})))
}

async fn show(State(s): State<Shared>, actor: Actor, Path((project, slug)): Path<(String,String)>) -> ApiResult<Json<ProjectPage>> {
    policy::read_project(&actor, &project)?;
    Ok(Json(s.db.read(move |c| pages::get(c, &project, &slug)).await?))
}

async fn set(State(s): State<Shared>, actor: Actor, Path((project, slug)): Path<(String,String)>, ApiJson(input): ApiJson<PageInput>) -> ApiResult<Json<ProjectPage>> {
    policy::admin_project(&actor, &project, "editing project pages")?;
    let label = actor.label();
    let author = label.clone();
    Ok(Json(s.db.write(label, move |t| pages::set(t, &project, &slug, &input, &author)).await?))
}

async fn remove(State(s): State<Shared>, actor: Actor, Path((project, slug)): Path<(String,String)>) -> ApiResult<StatusCode> {
    policy::admin_project(&actor, &project, "deleting project pages")?;
    s.db.write(actor.label(), move |t| pages::remove(t, &project, &slug)).await?;
    Ok(StatusCode::NO_CONTENT)
}
