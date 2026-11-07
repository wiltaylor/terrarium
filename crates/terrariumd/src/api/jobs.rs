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
//!   `by:<agent>` and `validation:queued`, the move to `validating`, and a
//!   queued validate job. The worker stays assigned while the card
//!   validates; the daemon clears the name on the final move.
//! - `POST /api/cards/{id}/merge` {repo?, branch?}: a project admin. A
//!   queued merge job; the card stays where it is until the job ends.
//! - `POST /api/merges` {cards, repo?} (TER-112): a project admin. One
//!   merge job for the cards of one project, merged in the given order and
//!   validated once. One card is the same as `POST /api/cards/{id}/merge`.
//! - `GET /api/job-hosts` (any actor): the host caps (TER-97) and the
//!   running jobs per host. `PUT /api/job-hosts/{host}` {heavy_jobs}
//!   (sysadmin or the operator): set the cap of a host; `DELETE`: no cap.
//!   `PUT /api/job-hosts/{host}/aliases` {aliases} (sysadmin or the
//!   operator, TER-105): the addresses of a host agent, so an SSH target
//!   on one of them counts on the host agent's cap.

use axum::extract::{Path, State};
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post, put};
use axum::{Json, Router};
use serde::Deserialize;
use serde_json::{Value, json};
use terrarium_core::config::DeliveryDef;

use super::{ApiJson, ApiQuery, Shared};
use crate::actor::Actor;
use crate::error::{ApiError, ApiResult};
use crate::executor;
use crate::jobs as rt;
use crate::policy;
use crate::store::board::{self, Card, CardPatch};
use crate::store::jobs::{self, Job, JobKind, JobState, NewJob};
use crate::store::{github_gate, hosts, projects, repos, targets};

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
        .route("/merges", post(merge_batch))
        .route("/job-hosts", get(list_hosts))
        .route("/job-hosts/{host}", put(set_host).delete(clear_host))
        .route("/job-hosts/{host}/aliases", put(set_aliases))
}

/// The host caps and the running jobs on each host.
async fn hosts_view(state: &Shared, actor: &Actor) -> ApiResult<Value> {
    rt::prepare_hosts(&state.db, None).await?;
    let (caps, jobs, agents, aliases) = state
        .db
        .read(|c| {
            Ok((
                jobs::host_caps(c)?,
                jobs::running(c)?,
                hosts::list(c)?,
                jobs::host_aliases(c)?,
            ))
        })
        .await?;
    let mut running =
        std::collections::BTreeMap::<String, std::collections::BTreeMap<String, usize>>::new();
    for job in jobs {
        if let Some(host) = job.host {
            *running
                .entry(host)
                .or_default()
                .entry(job.project)
                .or_default() += 1;
        }
    }
    let mut names: Vec<String> = caps
        .iter()
        .map(|c| c.host.clone())
        .chain(running.keys().cloned())
        .chain(aliases.keys().cloned())
        .collect();
    names.sort();
    names.dedup();
    let hosts: Vec<Value> = names
        .into_iter()
        .map(|h| {
            let cap = caps.iter().find(|c| c.host == h);
            let counts = running.get(&h).cloned().unwrap_or_default();
            let total: usize = counts.values().sum();
            let mut view = json!({"host": h, "heavy_jobs": cap.map(|c| c.heavy_jobs),
                   "running": total,
                   "updated_by": cap.map(|c| c.updated_by.clone()),
                   "updated_at": cap.map(|c| c.updated_at.clone()), "projects": counts});
            // The host agent of the key, and the addresses that count on it.
            if let Some(agent) = agents.iter().find(|a| a.id == h) {
                view["name"] = json!(agent.name);
                view["aliases"] = json!(aliases.get(&h).cloned().unwrap_or_default());
            }
            if let Actor::Agent(a) = actor
                && !a.reads_all()
            {
                if let Some(object) = view.as_object_mut() {
                    object.remove("updated_by");
                    object.remove("updated_at");
                    object.remove("aliases");
                }
                view["projects"] =
                    json!({a.project.clone(): counts.get(&a.project).copied().unwrap_or(0)});
            }
            view
        })
        .collect();
    Ok(json!({"hosts": hosts}))
}

async fn list_hosts(State(state): State<Shared>, actor: Actor) -> ApiResult<Json<Value>> {
    Ok(Json(hosts_view(&state, &actor).await?))
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct HostCapBody {
    heavy_jobs: u32,
}

async fn set_host(
    State(state): State<Shared>,
    actor: Actor,
    Path(host): Path<String>,
    ApiJson(body): ApiJson<HostCapBody>,
) -> ApiResult<Json<Value>> {
    policy::sysadmin(&actor, "setting the job cap of a host")?;
    rt::prepare_hosts(&state.db, Some(host.clone())).await?;
    state
        .db
        .write(actor.label(), move |t| {
            jobs::set_host_cap(t, &host, Some(body.heavy_jobs))
        })
        .await?;
    rt::kick(&state);
    Ok(Json(hosts_view(&state, &actor).await?))
}

async fn clear_host(
    State(state): State<Shared>,
    actor: Actor,
    Path(host): Path<String>,
) -> ApiResult<Json<Value>> {
    policy::sysadmin(&actor, "taking away the job cap of a host")?;
    rt::prepare_hosts(&state.db, Some(host.clone())).await?;
    state
        .db
        .write(actor.label(), move |t| jobs::set_host_cap(t, &host, None))
        .await?;
    rt::kick(&state);
    Ok(Json(hosts_view(&state, &actor).await?))
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct AliasesBody {
    aliases: Vec<String>,
}

/// Set the addresses of a host agent (TER-105). The alias names are
/// pinned outside the DB lock after the write, as target addresses are.
async fn set_aliases(
    State(state): State<Shared>,
    actor: Actor,
    Path(host): Path<String>,
    ApiJson(body): ApiJson<AliasesBody>,
) -> ApiResult<Json<Value>> {
    policy::sysadmin(&actor, "setting the aliases of a job host")?;
    state
        .db
        .write(actor.label(), move |t| {
            jobs::set_host_aliases(t, &host, &body.aliases)
        })
        .await?;
    rt::prepare_hosts(&state.db, None).await?;
    rt::kick(&state);
    Ok(Json(hosts_view(&state, &actor).await?))
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
            // The recipe runs on the target: a target of this project, or a
            // global one only when a sysadmin or the operator sets it.
            if let Some(target) = def.target.as_deref().filter(|t| !t.is_empty()) {
                let row = targets::get(t, target)?;
                match &row.def.project {
                    Some(p) if *p == name => {}
                    Some(p) => {
                        return Err(ApiError::forbidden(format!(
                            "target `{target}` belongs to project `{p}`; the job queue of `{name}` uses its own targets"
                        )));
                    }
                    None => policy::sysadmin(
                        &actor,
                        &format!("running the job queue of `{name}` on the global target `{target}`"),
                    )?,
                }
                executor::no_exec(&row.def)?;
            }
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
/// project admin. A batch merge holds the cards of several workers, so
/// only the actor that queued it or a project admin cancels it (TER-112
/// review, F3).
async fn cancel(
    State(state): State<Shared>,
    actor: Actor,
    Path(id): Path<String>,
) -> ApiResult<Json<Job>> {
    let job = find(&state, &actor, id).await?;
    let own = actor.label() == job.created_by
        || (job.batch.is_empty()
            && actor
                .agent()
                .is_some_and(|a| job.by_agent.as_deref() == Some(a.name.as_str())));
    if !own {
        policy::admin_project(&actor, &job.project, "cancelling another agent's job")?;
    }
    let jid = job.id.clone();
    let done = state
        .db
        .write(actor.label(), move |t| {
            let j = jobs::cancel(t, &jid)?;
            for c in j.cards() {
                let patch = CardPatch {
                    remove_labels: vec![rt::LABEL_QUEUED.into(), rt::LABEL_RUNNING.into()],
                    ..CardPatch::default()
                };
                if let Err(e) = board::update_card(t, &Actor::Operator, &c.card, patch)
                    && !e.code.status().is_client_error()
                {
                    return Err(e);
                }
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
struct BatchBody {
    /// The cards, in merge order.
    cards: Vec<String>,
    #[serde(default)]
    repo: Option<String>,
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
fn repo_of(t: &rusqlite::Connection, card: &Card, given: Option<String>) -> ApiResult<String> {
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

/// The answer of a queue: the job and its place. `room`: the host of the
/// project's target, its running jobs, and its cap, when it has one.
fn queued_answer(
    job: Job,
    place: (i64, i64),
    room: Option<(String, usize, u32)>,
    slots: usize,
) -> Response {
    let (position, running) = place;
    let host_full = room
        .as_ref()
        .is_some_and(|(_, used, cap)| *used >= *cap as usize);
    let mut note = if position as usize <= slots.saturating_sub(running as usize) && !host_full {
        format!("starts now ({running} of {slots} slots in use)")
    } else {
        format!("queued at place {position}; {running} of {slots} slots in use")
    };
    if let Some((host, used, cap)) = &room {
        note.push_str(&format!("; host `{host}` runs {used} of {cap} jobs"));
    }
    let host = room.map(|(h, used, cap)| json!({"host": h, "running": used, "heavy_jobs": cap}));
    (
        StatusCode::CREATED,
        Json(
            json!({"job": job, "queue": {"position": position, "running": running,
                                          "slots": slots, "note": note, "host": host}}),
        ),
    )
        .into_response()
}

/// The GitHub author gate (TER-85): no job for a held card. A job moves
/// the card as the operator (to `validating`, then on with its result),
/// so a job would take a held card out of the blocked column. This holds
/// for the operator too: the operator releases the card first.
fn not_held(card: &Card) -> ApiResult<()> {
    if github_gate::is_held(&card.title, &card.labels) {
        return Err(ApiError::forbidden(format!(
            "{} is held by the GitHub author gate: no validate or merge job for it until the \
             operator releases it",
            card.id
        )));
    }
    Ok(())
}

/// Who may queue a validation of a card: its assignee or a project
/// admin. Never for a held card ([`not_held`]).
fn may_validate(actor: &Actor, card: &Card) -> ApiResult<()> {
    policy::read_project(actor, &card.project)?;
    not_held(card)?;
    if card.assignee.as_deref() == Some(actor.address().as_str()) {
        return Ok(());
    }
    policy::admin_project(
        actor,
        &card.project,
        "queueing a validation of a card that is not yours",
    )
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
    // Read and check first; the remote check runs outside the write.
    let card_id = board::normalise_id(&id);
    let (a, given_repo, given_branch) = (actor.clone(), b.repo.clone(), b.branch.clone());
    let (card, def, repo, branch) = state
        .db
        .read(move |c| {
            let card = board::get_card(c, &card_id)?;
            may_validate(&a, &card)?;
            if card.archived {
                return Err(ApiError::conflict(format!("card {} is archived", card.id)));
            }
            let def = jobs::delivery(c, &card.project)?;
            enabled(&def, &card.project)?;
            let repo = repo_of(c, &card, given_repo)?;
            let branch = branch_of(&card, given_branch)?;
            Ok((card, def, repo, branch))
        })
        .await?;
    // The job must test what was pushed: the commit is the head of the
    // branch on the remote.
    if let Some(commit) = &b.commit {
        let head = rt::remote_head(&state, &card.project, &repo, &branch).await?;
        match head {
            Some(h) if h.starts_with(&commit.to_lowercase()) => {}
            Some(h) => {
                return Err(ApiError::conflict(format!(
                    "commit {commit} is not the head of `{branch}` on the remote (the head is {h}): push it first, or give the head"
                )));
            }
            None => {
                return Err(ApiError::conflict(format!(
                    "branch `{branch}` is not on the remote of `{repo}`: push it first"
                )));
            }
        }
    }
    let slots = def.slots();
    rt::prepare_hosts(&state.db, None).await?;
    let (job, place) = state
        .db
        .write(actor.label(), move |t| {
            let card = board::get_card(t, &card.id)?;
            may_validate(&actor, &card)?;
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
                    batch: Vec::new(),
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
                b.commit
                    .as_deref()
                    .unwrap_or("the branch head at the start")
            );
            if let Some(c) = b
                .comment
                .as_deref()
                .map(str::trim)
                .filter(|c| !c.is_empty())
            {
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
            let place = jobs::place(t, &job)?;
            let room = jobs::host_room(t, &job.project)?;
            Ok((job, (place, room)))
        })
        .await?;
    rt::kick(&state);
    Ok(queued_answer(job, place.0, place.1, slots))
}

async fn merge(
    State(state): State<Shared>,
    actor: Actor,
    Path(id): Path<String>,
    ApiJson(b): ApiJson<MergeBody>,
) -> ApiResult<Response> {
    rt::prepare_hosts(&state.db, None).await?;
    let job = state
        .db
        .write(actor.label(), move |t| {
            let card = board::get_card(t, &board::normalise_id(&id))?;
            policy::admin_project(&actor, &card.project, "queueing a merge onto next")?;
            not_held(&card)?;
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
                    batch: Vec::new(),
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
            let place = jobs::place(t, &job)?;
            let room = jobs::host_room(t, &job.project)?;
            // Merges have their own lane of one (TER-159).
            Ok((job, place, room, 1))
        })
        .await?;
    rt::kick(&state);
    Ok(queued_answer(job.0, job.1, job.2, job.3))
}

/// One merge job for several cards (TER-112): merged onto `next` in the
/// given order, validated once, pushed when green. All cards are of one
/// project and one repo; each branch is the card's (or its ID in lower
/// case).
async fn merge_batch(
    State(state): State<Shared>,
    actor: Actor,
    ApiJson(b): ApiJson<BatchBody>,
) -> ApiResult<Response> {
    if b.cards.is_empty() {
        return Err(ApiError::invalid(
            "`cards` is empty: give one or more card IDs",
        ));
    }
    if b.cards.len() > jobs::BATCH_MAX {
        return Err(ApiError::invalid(format!(
            "a batch has at most {} cards",
            jobs::BATCH_MAX
        )));
    }
    let job = state
        .db
        .write(actor.label(), move |t| {
            let mut cards: Vec<Card> = Vec::new();
            for id in &b.cards {
                let card = board::get_card(t, &board::normalise_id(id))?;
                policy::admin_project(&actor, &card.project, "queueing a merge onto next")?;
                not_held(&card)?;
                if card.archived {
                    return Err(ApiError::conflict(format!("card {} is archived", card.id)));
                }
                if cards.iter().any(|c| c.id == card.id) {
                    return Err(ApiError::invalid(format!(
                        "card {} is twice in `cards`",
                        card.id
                    )));
                }
                if let Some(first) = cards.first()
                    && first.project != card.project
                {
                    return Err(ApiError::invalid(format!(
                        "card {} is in project `{}`, card {} in `{}`: a batch is one project",
                        card.id, card.project, first.id, first.project
                    )));
                }
                cards.push(card);
            }
            let project = cards[0].project.clone();
            let def = jobs::delivery(t, &project)?;
            enabled(&def, &project)?;
            let mut repo: Option<String> = None;
            let mut batch = Vec::new();
            for card in &cards {
                let r = repo_of(t, card, b.repo.clone())?;
                match &repo {
                    Some(have) if *have != r => {
                        return Err(ApiError::invalid(format!(
                            "card {} is in repo `{r}`, the batch in `{have}`: a batch is one repo",
                            card.id
                        )));
                    }
                    _ => repo = Some(r),
                }
                batch.push(jobs::BatchCard {
                    card: card.id.clone(),
                    branch: branch_of(card, None)?,
                    by_agent: rt::by_agent_of(&card.labels),
                    outcome: None,
                    note: None,
                });
            }
            let repo = repo.unwrap_or_default();
            if !def.next_repos.contains(&repo) {
                return Err(ApiError::conflict(format!(
                    "repo `{repo}` has no `next` branch in the queue config: add it to `next_repos`"
                )));
            }
            let first = batch[0].clone();
            let job = jobs::create(
                t,
                NewJob {
                    project: project.clone(),
                    card: first.card.clone(),
                    kind: JobKind::Merge,
                    repo: repo.clone(),
                    branch: first.branch.clone(),
                    commit: None,
                    by_agent: first.by_agent.clone(),
                    batch: if batch.len() > 1 { batch.clone() } else { Vec::new() },
                },
            )?;
            let names: Vec<&str> = batch.iter().map(|c| c.card.as_str()).collect();
            for (i, c) in batch.iter().enumerate() {
                let patch = CardPatch {
                    add_labels: vec![rt::LABEL_QUEUED.into()],
                    ..CardPatch::default()
                };
                board::update_card(t, &Actor::Operator, &c.card, patch)?;
                let note = if batch.len() > 1 {
                    format!(
                        "Job queue: merge job {} queued: {} onto next of {repo}, card {} of {} in the batch {}.",
                        job.id,
                        c.branch,
                        i + 1,
                        batch.len(),
                        names.join(" ")
                    )
                } else {
                    format!(
                        "Job queue: merge job {} queued: {} onto next of {repo}.",
                        job.id, c.branch
                    )
                };
                board::add_comment(t, &Actor::Operator, &c.card, &note)?;
            }
            let place = jobs::place(t, &job)?;
            let room = jobs::host_room(t, &job.project)?;
            // Merges have their own lane of one (TER-159).
            Ok((job, place, room, 1))
        })
        .await?;
    rt::kick(&state);
    Ok(queued_answer(job.0, job.1, job.2, job.3))
}
