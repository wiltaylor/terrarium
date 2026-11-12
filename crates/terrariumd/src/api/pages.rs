//! Project page CRUD. Writes use the dashboard administration right.
use super::{
    ApiJson, Shared,
    link_registry::{Access, RegisteredRouter},
};
use crate::{
    actor::Actor,
    error::{ApiError, ApiResult},
    policy,
    store::pages::{self, PageInput, ProjectPage},
};
use axum::{
    Json,
    extract::{Path, State},
    http::{Method, StatusCode},
    routing::get,
};
use serde_json::{Value, json};

pub fn routes() -> RegisteredRouter {
    RegisteredRouter::new()
        .route(
            "/projects/{name}/pages",
            get(list),
            &[(Method::GET, Access::Link)],
        )
        .route(
            "/projects/{name}/pages/{slug}",
            get(show).put(set).delete(remove),
            &[
                (Method::GET, Access::Link),
                (Method::PUT, Access::Link),
                (Method::DELETE, Access::Link),
            ],
        )
}

async fn list(
    State(s): State<Shared>,
    actor: Actor,
    Path(project): Path<String>,
) -> ApiResult<Json<Value>> {
    policy::read_project(&actor, &project)?;
    let can_edit = policy::admin_project(&actor, &project, "editing project pages").is_ok();
    let (items, dashboard_slug) =
        s.db.read(move |c| {
            Ok((
                pages::list(c, &project)?,
                pages::dashboard_slug(c, &project)?,
            ))
        })
        .await?;
    Ok(Json(
        json!({"items": items, "can_edit": can_edit, "dashboard_slug": dashboard_slug}),
    ))
}

async fn show(
    State(s): State<Shared>,
    actor: Actor,
    Path((project, slug)): Path<(String, String)>,
) -> ApiResult<Json<ProjectPage>> {
    policy::read_project(&actor, &project)?;
    Ok(Json(
        s.db.read(move |c| pages::get(c, &project, &slug)).await?,
    ))
}

async fn set(
    State(s): State<Shared>,
    actor: Actor,
    Path((project, slug)): Path<(String, String)>,
    ApiJson(input): ApiJson<Value>,
) -> ApiResult<Json<ProjectPage>> {
    policy::admin_project(&actor, &project, "editing project pages")?;
    let input: PageInput = serde_json::from_value(input)
        .map_err(|e| ApiError::invalid(format!("invalid page: {e}")))?;
    let label = actor.label();
    let author = label.clone();
    Ok(Json(
        s.db.write(label, move |t| {
            pages::set(t, &project, &slug, &input, &author)
        })
        .await?,
    ))
}

async fn remove(
    State(s): State<Shared>,
    actor: Actor,
    Path((project, slug)): Path<(String, String)>,
) -> ApiResult<StatusCode> {
    policy::admin_project(&actor, &project, "deleting project pages")?;
    s.db.write(actor.label(), move |t| pages::remove(t, &project, &slug))
        .await?;
    Ok(StatusCode::NO_CONTENT)
}
