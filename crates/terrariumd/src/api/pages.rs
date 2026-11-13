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
    routing::{get, post},
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
            "/projects/{name}/page-layout",
            get(layout_show).put(layout_set),
            &[(Method::GET, Access::Link), (Method::PUT, Access::Link)],
        )
        .route(
            "/projects/{name}/page-layout/reset",
            post(layout_reset),
            &[(Method::POST, Access::Link)],
        )
        .route(
            "/projects/{name}/page-layout/copy",
            post(layout_copy),
            &[(Method::POST, Access::Link)],
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
        json!({"items": items, "can_edit": can_edit, "dashboard_slug": dashboard_slug,
            "capabilities": {"project_pages_v2": s.web.project_pages_v2}}),
    ))
}

fn enabled(s: &Shared) -> ApiResult<()> {
    if !s.web.project_pages_v2 {
        return Err(ApiError::not_found("project pages v2 is disabled"));
    }
    Ok(())
}

async fn layout_show(
    State(s): State<Shared>,
    actor: Actor,
    Path(project): Path<String>,
) -> ApiResult<Json<pages::Snapshot>> {
    policy::read_project(&actor, &project)?;
    enabled(&s)?;
    Ok(Json(
        s.db.read(move |c| pages::snapshot(c, &project)).await?,
    ))
}

async fn layout_set(
    State(s): State<Shared>,
    actor: Actor,
    Path(project): Path<String>,
    ApiJson(value): ApiJson<Value>,
) -> ApiResult<Json<pages::Snapshot>> {
    policy::admin_project(&actor, &project, "editing project pages")?;
    enabled(&s)?;
    let input: pages::LayoutInput = serde_json::from_value(value)
        .map_err(|e| ApiError::invalid(format!("invalid layout: {e}")))?;
    let author = actor.label();
    Ok(Json(
        s.db.write(author.clone(), move |t| {
            pages::replace_layout(t, &project, &input, &author, "replace")
        })
        .await?,
    ))
}

#[derive(serde::Deserialize)]
#[serde(deny_unknown_fields)]
struct ResetInput {
    base_revision: u64,
    builtin: pages::Builtin,
}

async fn layout_reset(
    State(s): State<Shared>,
    actor: Actor,
    Path(project): Path<String>,
    ApiJson(value): ApiJson<Value>,
) -> ApiResult<Json<pages::Snapshot>> {
    policy::admin_project(&actor, &project, "resetting project pages")?;
    enabled(&s)?;
    let input: ResetInput = serde_json::from_value(value)
        .map_err(|e| ApiError::invalid(format!("invalid reset: {e}")))?;
    let author = actor.label();
    Ok(Json(
        s.db.write(author.clone(), move |t| {
            pages::reset_builtin(t, &project, input.base_revision, input.builtin, &author)
        })
        .await?,
    ))
}

#[derive(serde::Deserialize)]
#[serde(deny_unknown_fields)]
struct CopyInput {
    base_revision: u64,
    source_project: String,
    confirm: bool,
}

async fn layout_copy(
    State(s): State<Shared>,
    actor: Actor,
    Path(project): Path<String>,
    ApiJson(value): ApiJson<Value>,
) -> ApiResult<Json<pages::Snapshot>> {
    policy::admin_project(&actor, &project, "copying project pages")?;
    enabled(&s)?;
    let input: CopyInput = serde_json::from_value(value)
        .map_err(|e| ApiError::invalid(format!("invalid copy: {e}")))?;
    policy::read_project(&actor, &input.source_project)?;
    let author = actor.label();
    Ok(Json(
        s.db.write(author.clone(), move |t| {
            pages::copy_layout(
                t,
                &project,
                &input.source_project,
                input.base_revision,
                input.confirm,
                &author,
            )
        })
        .await?,
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
