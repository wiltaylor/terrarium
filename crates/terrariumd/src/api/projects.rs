//! `/api/projects/...`: projects, their agents, targets, and repos.

use axum::Json;
use axum::extract::{Path, State};
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use axum::routing::{delete, get};
use serde::{Deserialize, Serialize};
use serde_json::Value;

use super::targets::TargetView;
use super::{ApiJson, ApiQuery, Shared, paged};
use crate::actor::Actor;
use crate::error::{ApiError, ApiResult, Code};
use crate::policy;
use crate::store::projects::{self, NewProject, NewRepo, Project, ProjectPatch, Repo};
use crate::store::questions::{self, QuestionKind};
use crate::store::target_grants::{self, Purpose, TargetGrant};
use crate::store::{Page, agents, board};

pub fn routes() -> super::link_registry::RegisteredRouter {
    super::link_registry::RegisteredRouter::new()
        .route(
            "/projects",
            get(list).post(create),
            &[
                (axum::http::Method::GET, super::link_registry::Access::Link),
                (axum::http::Method::POST, super::link_registry::Access::Link),
            ],
        )
        .route(
            "/projects/{name}",
            get(show).patch(update).delete(remove),
            &[
                (
                    axum::http::Method::GET,
                    super::link_registry::Access::Conditional,
                ),
                (
                    axum::http::Method::PATCH,
                    super::link_registry::Access::Conditional,
                ),
                (
                    axum::http::Method::DELETE,
                    super::link_registry::Access::Conditional,
                ),
            ],
        )
        .route(
            "/projects/{name}/git",
            get(git_settings).patch(set_git_settings),
            &[
                (axum::http::Method::GET, super::link_registry::Access::Link),
                (
                    axum::http::Method::PATCH,
                    super::link_registry::Access::Link,
                ),
            ],
        )
        .route(
            "/projects/{name}/agents",
            get(list_agents),
            &[(axum::http::Method::GET, super::link_registry::Access::Link)],
        )
        .route(
            "/projects/{name}/counts",
            get(counts),
            &[(axum::http::Method::GET, super::link_registry::Access::Link)],
        )
        .route(
            "/projects/{name}/targets",
            get(list_targets),
            &[(axum::http::Method::GET, super::link_registry::Access::Link)],
        )
        .route(
            "/projects/{name}/target-grants",
            get(list_grants).post(add_grant),
            &[
                (axum::http::Method::GET, super::link_registry::Access::Link),
                (axum::http::Method::POST, super::link_registry::Access::Link),
            ],
        )
        .route(
            "/projects/{name}/target-grants/{target}/{purpose}",
            delete(remove_grant),
            &[(
                axum::http::Method::DELETE,
                super::link_registry::Access::Link,
            )],
        )
        .route(
            "/projects/{name}/repos",
            get(list_repos).post(attach_repo),
            &[
                (axum::http::Method::GET, super::link_registry::Access::Link),
                (axum::http::Method::POST, super::link_registry::Access::Link),
            ],
        )
}

async fn git_settings(
    State(state): State<Shared>,
    actor: Actor,
    Path(name): Path<String>,
) -> ApiResult<Json<crate::git_identity::Settings>> {
    policy::read_project(&actor, &name)?;
    state
        .db
        .read(move |c| {
            projects::get(c, &name)?;
            Ok(Json(crate::git_identity::settings(c, false, &name)?))
        })
        .await
}

async fn set_git_settings(
    State(state): State<Shared>,
    actor: Actor,
    Path(name): Path<String>,
    ApiJson(patch): ApiJson<crate::git_identity::Settings>,
) -> ApiResult<Json<crate::git_identity::Settings>> {
    policy::admin_project(&actor, &name, "changing project git settings")?;
    if patch.warn_only.is_some() {
        policy::operator_only(&actor, "changing the provenance push override")?;
    }
    state
        .db
        .write(actor.label(), move |t| {
            projects::get(t, &name)?;
            Ok(Json(crate::git_identity::save_settings(
                t, false, &name, patch,
            )?))
        })
        .await
}

async fn list(
    State(state): State<Shared>,
    actor: Actor,
    ApiQuery(page): ApiQuery<Page>,
) -> ApiResult<Response> {
    let only = match &actor {
        Actor::Agent(a) if !a.reads_all() => Some(a.project.clone()),
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

/// The open questions of [`Counts`]: questions, bundles, and tool run
/// approvals, also those in a thread. A notice needs no decision.
#[derive(Debug, Serialize)]
struct QuestionCounts {
    open: usize,
    /// The urgent and the critical ones.
    urgent: usize,
    critical: usize,
}

/// The answer of `GET /api/projects/{name}/counts`.
#[derive(Debug, Serialize)]
struct Counts {
    /// The cards in the `blocked` role columns.
    blocked_cards: i64,
    /// `null` for an actor who may not list the questions of the project.
    questions: Option<QuestionCounts>,
}

/// `GET /api/projects/{name}/counts`: the counts for the project tabs of the
/// dashboard (TER-154). Indexed counts only, no row reads: the dashboard
/// reads it again on each card and question event of the project.
async fn counts(
    State(state): State<Shared>,
    actor: Actor,
    Path(name): Path<String>,
) -> ApiResult<Json<Counts>> {
    policy::read_project(&actor, &name)?;
    // The rule of `GET /api/questions?project=`.
    let questions_allowed =
        policy::is_admin_of(&actor, &name) || matches!(&actor, Actor::Agent(a) if a.reads_all());
    Ok(Json(
        state
            .db
            .read(move |c| {
                projects::get(c, &name)?;
                let blocked_cards = board::blocked_count(c, &name)?;
                let questions = if questions_allowed {
                    let mut out = QuestionCounts {
                        open: 0,
                        urgent: 0,
                        critical: 0,
                    };
                    for kind in [
                        QuestionKind::Question,
                        QuestionKind::Bundle,
                        QuestionKind::ToolRun,
                    ] {
                        let t = questions::open_total(c, Some(&name), Some(kind))?;
                        out.open += t.open;
                        out.urgent += t.urgent;
                        out.critical += t.critical;
                    }
                    Some(out)
                } else {
                    None
                };
                Ok(Counts {
                    blocked_cards,
                    questions,
                })
            })
            .await?,
    ))
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
    // The GitHub author gate (TER-85) holds work back from agents: no
    // agent changes its allow list, a sysadmin neither.
    if patch.github_authors.is_some() {
        policy::operator_only(&actor, "setting github_authors (the GitHub author gate)")?;
    }
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

/// `GET /api/projects/{name}/target-grants`: the global targets granted
/// to the project for its project-owned monitors and schedules
/// (docs/api.md §30.7). Members read them.
async fn list_grants(
    State(state): State<Shared>,
    actor: Actor,
    Path(name): Path<String>,
) -> ApiResult<Json<Vec<TargetGrant>>> {
    policy::read_project(&actor, &name)?;
    Ok(Json(
        state
            .db
            .read(move |c| {
                projects::get(c, &name)?;
                target_grants::list(c, &name)
            })
            .await?,
    ))
}

/// The body of `POST /api/projects/{name}/target-grants`.
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct GrantBody {
    target: String,
    /// `monitors` or `schedules`.
    #[serde(rename = "for")]
    purpose: String,
}

/// `POST /api/projects/{name}/target-grants`: grant a global target to
/// the project for `monitors` or `schedules`. Sysadmin or the operator: a
/// project-owned item then runs commands on the target with no agent
/// behind it. `201` with the grant (`200` when it was there).
async fn add_grant(
    State(state): State<Shared>,
    actor: Actor,
    Path(name): Path<String>,
    ApiJson(b): ApiJson<GrantBody>,
) -> ApiResult<(StatusCode, Json<TargetGrant>)> {
    policy::sysadmin(&actor, "granting a global target to a project")?;
    let purpose = Purpose::parse(&b.purpose)?;
    let label = actor.label();
    let (g, new) = state
        .db
        .write(actor.label(), move |t| {
            let had = target_grants::has(t, &b.target, &name, purpose)?;
            let g = target_grants::grant(t, &b.target, &name, purpose, &label)?;
            Ok((g, !had))
        })
        .await?;
    let code = if new {
        StatusCode::CREATED
    } else {
        StatusCode::OK
    };
    Ok((code, Json(g)))
}

/// `DELETE /api/projects/{name}/target-grants/{target}/{purpose}`: take a
/// grant back. Sysadmin or the operator.
async fn remove_grant(
    State(state): State<Shared>,
    actor: Actor,
    Path((name, target, purpose)): Path<(String, String, String)>,
) -> ApiResult<StatusCode> {
    policy::sysadmin(&actor, "revoking a target grant")?;
    let purpose = Purpose::parse(&purpose)?;
    state
        .db
        .write(actor.label(), move |t| {
            target_grants::revoke(t, &target, &name, purpose)
        })
        .await?;
    Ok(StatusCode::NO_CONTENT)
}

async fn list_repos(
    State(state): State<Shared>,
    actor: Actor,
    Path(name): Path<String>,
    ApiQuery(page): ApiQuery<Page>,
) -> ApiResult<Response> {
    policy::read_project(&actor, &name)?;
    let proxy_on = state.git_proxy.is_on();
    let rows = state
        .db
        .read(move |c| {
            projects::get(c, &name)?;
            let mut rows = projects::list_repos(c, &name, &page)?;
            crate::git_proxy::annotate(c, &mut rows.items, proxy_on)?;
            Ok(rows)
        })
        .await?;
    Ok(paged(rows))
}

/// The fields of a repo body, for the error of a bad one.
const REPO_FIELDS: &str = "`url`, `name`, `default_branch` (or its alias `branch`), \
                           `credential`, `path`, `hooks_dir`, `push_deny`, and `push_via`";

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
    let proxy_on = state.git_proxy.is_on();
    let r = state
        .db
        .write(actor.label(), move |t| {
            let r = projects::attach_repo(t, &name, &body, projects::SOURCE_API)?;
            let mut one = [r];
            crate::git_proxy::annotate(t, &mut one, proxy_on)?;
            let [r] = one;
            Ok(r)
        })
        .await?;
    for w in &r.warnings {
        tracing::warn!(repo = %format!("{}/{}", r.project, r.name), "{w}");
    }
    Ok((StatusCode::CREATED, Json(r)))
}
