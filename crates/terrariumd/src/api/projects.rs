//! `/api/projects/...`: projects, their agents, targets, and repos.

use axum::extract::{Path, State};
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use axum::routing::get;
use axum::{Json, Router};
use serde::Deserialize;
use serde_json::Value;

use super::targets::TargetView;
use super::{ApiJson, ApiQuery, Shared, paged};
use crate::actor::Actor;
use crate::error::{ApiError, ApiResult, Code};
use crate::policy;
use crate::store::projects::{self, NewProject, NewRepo, Project, ProjectPatch, Repo};
use crate::store::{Page, agents};

pub fn routes() -> Router<Shared> {
    Router::new()
        .route("/projects", get(list).post(create))
        .route("/projects/{name}", get(show).patch(update).delete(remove))
        .route("/projects/{name}/agents", get(list_agents))
        .route("/projects/{name}/targets", get(list_targets))
        .route("/projects/{name}/repos", get(list_repos).post(attach_repo))
}

async fn list(
    State(state): State<Shared>,
    actor: Actor,
    ApiQuery(page): ApiQuery<Page>,
) -> ApiResult<Response> {
    let only = match &actor {
        Actor::Agent(a) if !a.is_sysadmin() => Some(a.project.clone()),
        _ => None,
    };
    let rows = state
        .db
        .read(move |c| projects::list(c, &page, only.as_deref()))
        .await?;
    Ok(paged(rows))
}

async fn create(
    State(state): State<Shared>,
    actor: Actor,
    ApiJson(body): ApiJson<NewProject>,
) -> ApiResult<(StatusCode, Json<Project>)> {
    policy::sysadmin(&actor, "creating a project")?;
    let p = state
        .db
        .write(actor.label(), move |t| {
            projects::create(t, &body, projects::SOURCE_API)
        })
        .await?;
    Ok((StatusCode::CREATED, Json(p)))
}

async fn show(
    State(state): State<Shared>,
    actor: Actor,
    Path(name): Path<String>,
) -> ApiResult<Json<Project>> {
    policy::read_project(&actor, &name)?;
    Ok(Json(state.db.read(move |c| projects::get(c, &name)).await?))
}

fn file_managed(p: &Project) -> ApiError {
    ApiError::new(
        Code::FileManaged,
        format!(
            "project `{}` is declared in projects/{}.toml; edit the file, then POST /api/reload",
            p.name, p.name
        ),
    )
}

async fn update(
    State(state): State<Shared>,
    actor: Actor,
    Path(name): Path<String>,
    ApiJson(patch): ApiJson<ProjectPatch>,
) -> ApiResult<Json<Project>> {
    policy::admin_project(&actor, &name, "changing a project")?;
    let p = state
        .db
        .write(actor.label(), move |t| {
            let p = projects::get(t, &name)?;
            if p.source == projects::SOURCE_FILE {
                return Err(file_managed(&p));
            }
            projects::update(t, &name, &patch)
        })
        .await?;
    Ok(Json(p))
}

#[derive(Debug, Default, Deserialize)]
pub struct Confirm {
    #[serde(default)]
    pub confirm: bool,
}

async fn remove(
    State(state): State<Shared>,
    actor: Actor,
    Path(name): Path<String>,
    ApiQuery(c): ApiQuery<Confirm>,
) -> ApiResult<Response> {
    policy::sysadmin(&actor, "deleting a project")?;
    if !c.confirm {
        return Err(ApiError::new(
            Code::ConfirmRequired,
            format!("deleting project `{name}` deletes its board; add ?confirm=true"),
        ));
    }
    let repo = crate::template_files::repo_dir(&state);
    state
        .db
        .write(actor.label(), move |t| {
            let p = projects::get(t, &name)?;
            if p.source == projects::SOURCE_FILE {
                return Err(file_managed(&p));
            }
            // The project's templates go with it, and their files too.
            for tpl in crate::store::templates::of_scope(t, Some(&name))? {
                crate::store::templates::delete(t, &tpl.id, repo.as_deref())?;
            }
            projects::delete(t, &name)
        })
        .await?;
    // The project's secrets are gone (PLAN §16b.6).
    crate::secrets::redact::rebuild(&state).await;
    Ok(StatusCode::NO_CONTENT.into_response())
}

async fn list_agents(
    State(state): State<Shared>,
    actor: Actor,
    Path(name): Path<String>,
    ApiQuery(page): ApiQuery<Page>,
) -> ApiResult<Response> {
    policy::read_project(&actor, &name)?;
    let rows = state
        .db
        .read(move |c| {
            projects::get(c, &name)?;
            agents::list(c, Some(&name), &page)
        })
        .await?;
    Ok(paged(rows))
}

async fn list_targets(
    State(state): State<Shared>,
    actor: Actor,
    Path(name): Path<String>,
) -> ApiResult<Json<Vec<TargetView>>> {
    policy::read_project(&actor, &name)?;
    let n = name.clone();
    state.db.read(move |c| projects::get(c, &n)).await?;
    Ok(Json(
        super::targets::for_project(&state, &actor, &name).await?,
    ))
}

async fn list_repos(
    State(state): State<Shared>,
    actor: Actor,
    Path(name): Path<String>,
    ApiQuery(page): ApiQuery<Page>,
) -> ApiResult<Response> {
    policy::read_project(&actor, &name)?;
    let rows = state
        .db
        .read(move |c| {
            projects::get(c, &name)?;
            projects::list_repos(c, &name, &page)
        })
        .await?;
    Ok(paged(rows))
}

/// The fields of a repo body, for the error of a bad one.
const REPO_FIELDS: &str = "`url`, `name`, `default_branch` (or its alias `branch`), \
                           `credential`, `path`, `hooks_dir`, and `push_deny`";

/// Parse a repo body; the error names the fields, `branch` and
/// `default_branch` both.
fn new_repo(body: Value) -> ApiResult<NewRepo> {
    if body.get("branch").is_some() && body.get("default_branch").is_some() {
        return Err(ApiError::bad_request(
            "give `default_branch` or its alias `branch`, not both",
        ));
    }
    serde_json::from_value(body).map_err(|e| {
        ApiError::bad_request(format!("bad repo body: {e}; the fields are {REPO_FIELDS}"))
    })
}

async fn attach_repo(
    State(state): State<Shared>,
    actor: Actor,
    Path(name): Path<String>,
    ApiJson(body): ApiJson<Value>,
) -> ApiResult<(StatusCode, Json<Repo>)> {
    policy::admin_project(&actor, &name, "attaching a repo")?;
    let body = new_repo(body)?;
    let r = state
        .db
        .write(actor.label(), move |t| {
            projects::attach_repo(t, &name, &body, projects::SOURCE_API)
        })
        .await?;
    Ok((StatusCode::CREATED, Json(r)))
}
