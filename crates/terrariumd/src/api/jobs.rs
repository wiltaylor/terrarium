//! The job queue of a project (TER-71, [`crate::jobs`]):
//!
//! - `GET /api/projects/{name}/delivery`, `PUT` (project admin or the
//!   operator): the queue config.
//! - `GET /api/projects/{name}/jobs[?state=]`: the config, the counts, and
//!   the jobs, newest first.
//! - `GET /api/jobs/{id}`, `DELETE /api/jobs/{id}` (cancel; a running job
//!   is killed).
//! - `POST /api/cards/{id}/validate` {commit?, repo?, branch?, comment?}:
//!   the card's assignee or a project admin. In one write: the labels
//!   `by:<agent>` and `validation:queued`, the move to `validating`, no
//!   assignee, and a queued validate job.
//! - `POST /api/cards/{id}/merge` {repo?, branch?}: a project admin. A
//!   queued merge job; the card stays where it is until the job ends.

use axum::extract::{Path, State};
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use axum::{Json, Router};
use serde::Deserialize;
use serde_json::{Value, json};
use terrarium_core::config::DeliveryDef;

use super::{ApiJson, ApiQuery, Shared};
use crate::actor::Actor;
use crate::db::Tx;
use crate::error::{ApiError, ApiResult};
use crate::jobs as rt;
use crate::policy;
use crate::store::board::{self, Card, CardPatch};
use crate::store::jobs::{self, Job, JobKind, JobState, NewJob};
use crate::store::{projects, repos};

pub fn routes() -> Router<Shared> {
    Router::new()
        .route(
            "/projects/{name}/delivery",
            get(show_delivery).put(set_delivery),
        )
        .route("/projects/{name}/jobs", get(list))
        .route("/jobs/{id}", get(show).delete(cancel))
        .route("/cards/{id}/validate", post(validate))
        .route("/cards/{id}/merge", post(merge))
}

async fn show_delivery(
    State(state): State<Shared>,
    actor: Actor,
    Path(name): Path<String>,
) -> ApiResult<Json<Value>> {
    policy::read_project(&actor, &name)?;
    let def = state
        .db
        .read(move |c| {
            projects::get(c, &name)?;
            jobs::delivery(c, &name)
        })
        .await?;
    Ok(Json(
        json!({"delivery": def, "enabled": def.enabled(), "slots": def.slots()}),
    ))
}

async fn set_delivery(
    State(state): State<Shared>,
    actor: Actor,
    Path(name): Path<String>,
    ApiJson(def): ApiJson<DeliveryDef>,
) -> ApiResult<Json<Value>> {
    policy::admin_project(&actor, &name, "setting the job queue of a project")?;
    let def = state
        .db
        .write(actor.label(), move |t| {
            projects::get(t, &name)?;
            for r in &def.next_repos {
                if repos::find_by_name(t, &name, r)?.is_none() {
                    return Err(ApiError::invalid(format!(
                        "`next_repos` names `{r}`, which is not a repo of `{name}`"
                    )));
                }
            }
            jobs::set_delivery(t, &name, &def)
        })
        .await?;
    rt::kick(&state);
    Ok(Json(
        json!({"delivery": def, "enabled": def.enabled(), "slots": def.slots()}),
    ))
}

#[derive(Debug, Default, Deserialize)]
struct ListQuery {
    #[serde(default)]
    state: Option<String>,
}

async fn list(
    State(state): State<Shared>,
    actor: Actor,
    Path(name): Path<String>,
    ApiQuery(q): ApiQuery<ListQuery>,
) -> ApiResult<Json<Value>> {
    policy::read_project(&actor, &name)?;
    let filter = match q.state.as_deref() {
        None | Some("") => None,
        Some(s) => Some(JobState::parse(s).ok_or_else(|| {
            ApiError::invalid(format!(
                "bad state `{s}`: queued, running, passed, failed, or cancelled"
            ))
        })?),
    };
    let (def, list) = state
        .db
        .read(move |c| {
            projects::get(c, &name)?;
            Ok((jobs::delivery(c, &name)?, jobs::list(c, &name, filter)?))
        })
        .await?;
    Ok(Json(rt::view(&def, &list)))
}

async fn find(state: &Shared, actor: &Actor, id: String) -> ApiResult<Job> {
    let job = state.db.read(move |c| jobs::get(c, &id)).await?;
    policy::read_project(actor, &job.project)?;
    Ok(job)
}

async fn show(
    State(state): State<Shared>,
    actor: Actor,
    Path(id): Path<String>,
) -> ApiResult<Json<Job>> {
    Ok(Json(find(&state, &actor, id).await?))
}

/// Cancel: the actor that queued the job, the card's worker, or a
/// project admin.
async fn cancel(
    State(state): State<Shared>,
    actor: Actor,
    Path(id): Path<String>,
) -> ApiResult<Json<Job>> {
    let job = find(&state, &actor, id).await?;
    let own = actor.label() == job.created_by
        || actor
            .agent()
            .is_some_and(|a| job.by_agent.as_deref() == Some(a.name.as_str()));
    if !own {
        policy::admin_project(&actor, &job.project, "cancelling another agent's job")?;
    }
    let jid = job.id.clone();
    let done = state
        .db
        .write(actor.label(), move |t| {
            let j = jobs::cancel(t, &jid)?;
            let patch = CardPatch {
                remove_labels: vec![rt::LABEL_QUEUED.into(), rt::LABEL_RUNNING.into()],
                ..CardPatch::default()
            };
            if let Err(e) = board::update_card(t, &Actor::Operator, &j.card, patch)
                && !e.code.status().is_client_error()
            {
                return Err(e);
            }
            Ok(j)
        })
        .await?;
    if job.state == JobState::Running {
        rt::kill(&job.id);
    }
    rt::kick(&state);
    Ok(Json(done))
}

#[derive(Debug, Default, Deserialize)]
#[serde(deny_unknown_fields)]
struct ValidateBody {
    /// The pushed commit to validate. Without it, the job takes the head
    /// of the branch when it starts.
    #[serde(default)]
    commit: Option<String>,
    #[serde(default)]
    repo: Option<String>,
    #[serde(default)]
    branch: Option<String>,
    /// Added to the move comment.
    #[serde(default)]
    comment: Option<String>,
}

#[derive(Debug, Default, Deserialize)]
#[serde(deny_unknown_fields)]
struct MergeBody {
    #[serde(default)]
    repo: Option<String>,
    #[serde(default)]
    branch: Option<String>,
}

/// The repo of a job: the given one, the card's, or the project's only
/// repo.
fn repo_of(t: &Tx<'_>, card: &Card, given: Option<String>) -> ApiResult<String> {
    if let Some(r) = given.or_else(|| card.repo.clone()) {
        if repos::find_by_name(t, &card.project, &r)?.is_none() {
            return Err(ApiError::invalid(format!(
                "`{r}` is not a repo of `{}`",
                card.project
            )));
        }
        return Ok(r);
    }
    let all: Vec<String> = repos::all_for_project(t, &card.project)?
        .into_iter()
        .map(|r| r.name)
        .collect();
    match all.as_slice() {
        [one] => Ok(one.clone()),
        [] => Err(ApiError::invalid(format!(
            "project `{}` has no repo",
            card.project
        ))),
        _ => Err(ApiError::invalid(format!(
            "project `{}` has more than one repo ({}); give `repo`",
            card.project,
            all.join(", ")
        ))),
    }
}

/// The branch of a job: the given one, the card's, or the card ID in
/// lower case.
fn branch_of(card: &Card, given: Option<String>) -> ApiResult<String> {
    let b = given
        .or_else(|| card.branch.clone())
        .unwrap_or_else(|| card.id.to_lowercase());
    rt::check_ref(&b)?;
    Ok(b)
}

fn enabled(def: &DeliveryDef, project: &str) -> ApiResult<()> {
    if def.enabled() {
        return Ok(());
    }
    Err(ApiError::conflict(format!(
        "project `{project}` has no job queue: set its target and recipe with `terrarium project delivery set` (PUT /api/projects/{project}/delivery)"
    )))
}

async fn validate(
    State(state): State<Shared>,
    actor: Actor,
    Path(id): Path<String>,
    ApiJson(b): ApiJson<ValidateBody>,
) -> ApiResult<Response> {
    if let Some(c) = &b.commit {
        rt::check_commit(c)?;
    }
    let job = state
        .db
        .write(actor.label(), move |t| {
            let card = board::get_card(t, &board::normalise_id(&id))?;
            policy::read_project(&actor, &card.project)?;
            let mine = card.assignee.as_deref() == Some(actor.address().as_str());
            if !mine {
                policy::admin_project(
                    &actor,
                    &card.project,
                    "queueing a validation of a card that is not yours",
                )?;
            }
            if card.archived {
                return Err(ApiError::conflict(format!("card {} is archived", card.id)));
            }
            let def = jobs::delivery(t, &card.project)?;
            enabled(&def, &card.project)?;
            let repo = repo_of(t, &card, b.repo)?;
            let branch = branch_of(&card, b.branch)?;
            let by = actor
                .agent()
                .map(|a| a.name.clone())
                .or_else(|| rt::by_agent_of(&card.labels));
            let job = jobs::create(
                t,
                NewJob {
                    project: card.project.clone(),
                    card: card.id.clone(),
                    kind: JobKind::Validate,
                    repo: repo.clone(),
                    branch: branch.clone(),
                    commit: b.commit.clone(),
                    by_agent: by.clone(),
                },
            )?;
            let mut add = vec![rt::LABEL_QUEUED.to_owned()];
            if let Some(n) = &by {
                let label = format!("by:{n}");
                if !card.labels.contains(&label) {
                    add.push(label);
                }
            }
            let patch = CardPatch {
                add_labels: add,
                remove_labels: vec![rt::LABEL_RUNNING.into()],
                ..CardPatch::default()
            };
            board::update_card(t, &Actor::Operator, &card.id, patch)?;
            let mut note = format!(
                "Job queue: validate job {} queued. Repo {repo}, branch {branch}, commit {}.",
                job.id,
                b.commit.as_deref().unwrap_or("the branch head at the start")
            );
            if let Some(c) = b.comment.as_deref().map(str::trim).filter(|c| !c.is_empty()) {
                note = format!("{c}\n\n{note}");
            }
            if card.column != rt::COL_VALIDATING {
                board::move_card(
                    t,
                    &Actor::Operator,
                    &card.id,
                    rt::COL_VALIDATING,
                    None,
                    Some(&note),
                )?;
            } else {
                board::add_comment(t, &Actor::Operator, &card.id, &note)?;
            }
            if card.assignee.is_some() {
                board::assign_card(t, &Actor::Operator, &card.id, None)?;
            }
            Ok(job)
        })
        .await?;
    rt::kick(&state);
    Ok((StatusCode::CREATED, Json(job)).into_response())
}

async fn merge(
    State(state): State<Shared>,
    actor: Actor,
    Path(id): Path<String>,
    ApiJson(b): ApiJson<MergeBody>,
) -> ApiResult<Response> {
    let job = state
        .db
        .write(actor.label(), move |t| {
            let card = board::get_card(t, &board::normalise_id(&id))?;
            policy::admin_project(&actor, &card.project, "queueing a merge onto next")?;
            if card.archived {
                return Err(ApiError::conflict(format!("card {} is archived", card.id)));
            }
            let def = jobs::delivery(t, &card.project)?;
            enabled(&def, &card.project)?;
            let repo = repo_of(t, &card, b.repo)?;
            if !def.next_repos.contains(&repo) {
                return Err(ApiError::conflict(format!(
                    "repo `{repo}` has no `next` branch in the queue config: add it to `next_repos`"
                )));
            }
            let branch = branch_of(&card, b.branch)?;
            let job = jobs::create(
                t,
                NewJob {
                    project: card.project.clone(),
                    card: card.id.clone(),
                    kind: JobKind::Merge,
                    repo: repo.clone(),
                    branch: branch.clone(),
                    commit: None,
                    by_agent: rt::by_agent_of(&card.labels),
                },
            )?;
            let patch = CardPatch {
                add_labels: vec![rt::LABEL_QUEUED.into()],
                ..CardPatch::default()
            };
            board::update_card(t, &Actor::Operator, &card.id, patch)?;
            board::add_comment(
                t,
                &Actor::Operator,
                &card.id,
                &format!(
                    "Job queue: merge job {} queued: {branch} onto next of {repo}.",
                    job.id
                ),
            )?;
            Ok(job)
        })
        .await?;
    rt::kick(&state);
    Ok((StatusCode::CREATED, Json(job)).into_response())
}
