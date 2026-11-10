//! The job queue of a project (TER-71, [`crate::jobs`]):
//!
//! - `GET /api/projects/{name}/delivery`, `PUT` (project admin or the
//!   operator): the queue config.
//! - `GET /api/projects/{name}/jobs[?state=]`: the config, the counts, and
//!   the jobs, newest first.
//! - `GET /api/projects/{name}/jobs/stats[?since=]` (TER-135): the
//!   first-attempt pass rate, the counts, and the failed test targets of
//!   the jobs created since `since` (RFC 3339, or a duration back from
//!   now such as `7d`; default 7 days).
//! - `GET /api/jobs/{id}`, `DELETE /api/jobs/{id}` (cancel; a running job
//!   is killed).
//! - `POST /api/cards/{id}/validate` {commit?, repo?, branch?, comment?, eta?}:
//!   the card's assignee or a project admin. In one write: the labels
//!   `by:<agent>` and `validation:queued`, the move to `validating`, and a
//!   queued validate job. The worker stays assigned while the card
//!   validates; the daemon clears the name on the final move.
//! - `POST /api/cards/{id}/merge` {repo?, branch?, eta?}: a project admin. A
//!   queued merge job; the card stays where it is until the job ends.
//! - `POST /api/merges` {cards, repo?, eta?} (TER-112): a project admin. One
//!   merge job for the cards of one project, merged in the given order and
//!   validated once. One card is the same as `POST /api/cards/{id}/merge`.
//! - With `board_flow = rules` (TER-170) a validate, a merge, a batch,
//!   and a cancel only queue or cancel the job: its events are all, and
//!   the board rules set the labels, move, and comment (the built-in pack
//!   for a merge; for a validate only the opt-in rules of
//!   `rules.validate.wcl`, as the pack has no validating column). The
//!   `comment` of a validate goes on the card as the actor's own comment.
//! - `eta` (TER-155 D2) is a planned duration such as `2h`, not a time.
//!   The job's cards have no ETA while it is queued; when it starts, each
//!   gets the start plus the duration. No `eta`, no ETA.
//! - `GET /api/job-hosts` (any actor): the host caps (TER-97) and the
//!   running jobs per host. `PUT /api/job-hosts/{host}` {heavy_jobs}
//!   (sysadmin or the operator): set the cap of a host; `DELETE`: no cap.
//!   `PUT /api/job-hosts/{host}/aliases` {aliases} (sysadmin or the
//!   operator, TER-105): the addresses of a host agent, so an SSH target
//!   on one of them counts on the host agent's cap.

use std::collections::HashMap;

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
use crate::jobs as rt;
use crate::policy;
use crate::store::board::{self, Card, CardPatch};
use crate::store::jobs::{self, Job, JobKind, JobState, NewJob};
use crate::store::{accepted_heads, github_gate, hosts, projects, repos};

pub fn routes() -> Router<Shared> {
    Router::new()
        .route(
            "/projects/{name}/delivery",
            get(show_delivery).put(set_delivery),
        )
        .route("/projects/{name}/jobs", get(list))
        .route("/projects/{name}/jobs/stats", get(stats))
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
            rt::check_delivery(t, &actor, &name, &def)?;
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

/// The default window of the job stats.
const STATS_WINDOW: &str = "7d";

#[derive(Debug, Default, Deserialize)]
struct StatsQuery {
    #[serde(default)]
    since: Option<String>,
}

/// The start of the stats window: an RFC 3339 time, or a duration back
/// from `now` (`7d`, `12h`). The result is in the form of `created_at`.
fn stats_since(since: Option<&str>, now: chrono::DateTime<chrono::Utc>) -> ApiResult<String> {
    let s = since
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .unwrap_or(STATS_WINDOW);
    let at = match chrono::DateTime::parse_from_rfc3339(s) {
        Ok(t) => t.with_timezone(&chrono::Utc),
        Err(_) => {
            let d = terrarium_core::eta::parse_duration(s).map_err(|e| {
                ApiError::bad_request(format!(
                    "bad since: {e}; give an RFC 3339 time or a duration such as 7d"
                ))
            })?;
            now.checked_sub_signed(d)
                .ok_or_else(|| ApiError::bad_request(format!("bad since `{s}`: too far back")))?
        }
    };
    Ok(at.to_rfc3339_opts(chrono::SecondsFormat::Millis, true))
}

async fn stats(
    State(state): State<Shared>,
    actor: Actor,
    Path(name): Path<String>,
    ApiQuery(q): ApiQuery<StatsQuery>,
) -> ApiResult<Json<jobs::JobStats>> {
    policy::read_project(&actor, &name)?;
    let since = stats_since(q.since.as_deref(), chrono::Utc::now())?;
    let s = state
        .db
        .read(move |c| {
            projects::get(c, &name)?;
            jobs::stats(c, &name, &since)
        })
        .await?;
    Ok(Json(s))
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
            rt::emit_verdicts(t, &j)?;
            // With `board_flow = rules` the pack clears them (TER-170).
            if rt::code_flow(t, &j.project)? {
                rt::clear_job_labels(t, &j)?;
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
    /// The planned duration (`2h`, TER-155 D2): the card's ETA is the job
    /// start plus it. Not a time. Without it, the job sets no ETA.
    #[serde(default)]
    eta: Option<String>,
}

#[derive(Debug, Default, Deserialize)]
#[serde(deny_unknown_fields)]
struct BatchBody {
    /// The cards, in merge order.
    cards: Vec<String>,
    #[serde(default)]
    repo: Option<String>,
    /// The planned duration, as for a validate.
    #[serde(default)]
    eta: Option<String>,
}

#[derive(Debug, Default, Deserialize)]
#[serde(deny_unknown_fields)]
struct MergeBody {
    #[serde(default)]
    repo: Option<String>,
    #[serde(default)]
    branch: Option<String>,
    /// The planned duration, as for a validate.
    #[serde(default)]
    eta: Option<String>,
}

/// The planned duration of a body's `eta`: `None` without one.
fn planned(eta: Option<&str>) -> ApiResult<Option<i64>> {
    eta.map(jobs::planned_secs).transpose()
}

/// The repo of a job: the given one, the card's, or the project's only
/// repo.
pub(crate) fn repo_of(
    t: &rusqlite::Connection,
    card: &Card,
    given: Option<String>,
) -> ApiResult<String> {
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
pub(crate) fn branch_of(card: &Card, given: Option<String>) -> ApiResult<String> {
    let b = given
        .or_else(|| card.branch.clone())
        .unwrap_or_else(|| card.id.to_lowercase());
    rt::check_ref(&b)?;
    Ok(b)
}

pub(crate) fn enabled(def: &DeliveryDef, project: &str) -> ApiResult<()> {
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
pub(crate) fn not_held(card: &Card) -> ApiResult<()> {
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
pub(crate) fn may_validate(actor: &Actor, card: &Card) -> ApiResult<()> {
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

/// The worker of a validate, the name in its `by:<agent>` label and the
/// `by_agent` of the job (the final mail goes to it, docs/api.md 35.3). The
/// worker is the card's assignee when it queues. A project admin who queues
/// another agent's card keeps the card's `by:` label, so the worker does
/// not change. A card with no `by:` label gets the queuing agent.
fn validate_by(actor: &Actor, card: &Card) -> Option<String> {
    let me = actor.agent();
    match me {
        Some(a) if card.assignee.as_deref() == Some(actor.address().as_str()) => {
            Some(a.name.clone())
        }
        _ => rt::by_agent_of(&card.labels).or_else(|| me.map(|a| a.name.clone())),
    }
}

/// The labels of a validate: one `validation:*` label (`validation:queued`)
/// and one `by:<agent>` label. Every `validation:*` and `by:*` label that
/// the card has goes, so a stale one does not stay beside the new one. The
/// `by` name is the worker, see [`validate_by`]. Returns the labels to add
/// and the labels to remove.
fn job_labels(labels: &[String], by: Option<&str>) -> (Vec<String>, Vec<String>) {
    let keep = by.map(|n| format!("by:{n}"));
    let remove = labels
        .iter()
        .filter(|l| l.starts_with("validation:") || l.starts_with("by:"))
        .cloned()
        .collect();
    let mut add = vec![rt::LABEL_QUEUED.to_owned()];
    add.extend(keep);
    (add, remove)
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
    let planned_secs = planned(b.eta.as_deref())?;
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
            let by = validate_by(&actor, &card);
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
                    planned_secs,
                },
            )?;
            let given = b
                .comment
                .as_deref()
                .map(str::trim)
                .filter(|c| !c.is_empty());
            if let Some(c) = given {
                board::add_comment(t, &actor, &card.id, c)?;
            }
            // With `board_flow = rules` the job's `queued` event is all:
            // the built-in pack labels, moves, and comments (TER-170). The
            // worker's own comment goes on the card as its own.
            if !rt::code_flow(t, &card.project)? {
                let place = jobs::place(t, &job)?;
                let room = jobs::host_room(t, &job.project)?;
                return Ok((job, (place, room)));
            }
            let (add, remove) = job_labels(&card.labels, by.as_deref());
            let patch = CardPatch {
                add_labels: add,
                remove_labels: remove,
                ..CardPatch::default()
            };
            board::update_card(t, &Actor::Operator, &card.id, patch)?;
            let note = rt::queue_comment(&job, &card.id);
            let validating =
                board::role_name(t, &card.project, board::VALIDATING, rt::COL_VALIDATING)?;
            if card.column != validating {
                board::move_card(
                    t,
                    &Actor::Operator,
                    &card.id,
                    &validating,
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
    let planned_secs = planned(b.eta.as_deref())?;
    rt::prepare_hosts(&state.db, None).await?;
    let commits = merge_commits(
        &state,
        &actor,
        vec![id.clone()],
        b.repo.clone(),
        b.branch.clone(),
    )
    .await?;
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
            let commit = merge_commit(&commits, &card.id, &branch)?;
            let job = jobs::create(
                t,
                NewJob {
                    project: card.project.clone(),
                    card: card.id.clone(),
                    kind: JobKind::Merge,
                    repo: repo.clone(),
                    branch: branch.clone(),
                    commit: Some(commit),
                    by_agent: rt::by_agent_of(&card.labels),
                    batch: Vec::new(),
                    planned_secs,
                },
            )?;
            if rt::code_flow(t, &card.project)? {
                let patch = CardPatch {
                    add_labels: vec![rt::LABEL_QUEUED.into()],
                    ..CardPatch::default()
                };
                board::update_card(t, &Actor::Operator, &card.id, patch)?;
                board::add_comment(
                    t,
                    &Actor::Operator,
                    &card.id,
                    &rt::queue_comment(&job, &card.id),
                )?;
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

/// The commit that a merge takes for each of the cards `ids`, with its
/// branch, by card ID (TER-553): the reviewed commit that the merge queue
/// recorded at the accept, else the head of the branch on its remote now.
/// The merge then takes that commit, never a later push. Read before the
/// write of the job: the head is a `git ls-remote`.
async fn merge_commits(
    state: &Shared,
    actor: &Actor,
    ids: Vec<String>,
    repo: Option<String>,
    branch: Option<String>,
) -> ApiResult<HashMap<String, (String, String)>> {
    let a = actor.clone();
    let places = state
        .db
        .read(move |c| {
            let mut out = Vec::new();
            for id in &ids {
                let card = board::get_card(c, &board::normalise_id(id))?;
                // The checks of the write, in its order: a held or archived
                // card gets that error, not one of its repo.
                policy::admin_project(&a, &card.project, "queueing a merge onto next")?;
                not_held(&card)?;
                if card.archived {
                    return Err(ApiError::conflict(format!("card {} is archived", card.id)));
                }
                enabled(&jobs::delivery(c, &card.project)?, &card.project)?;
                let r = repo_of(c, &card, repo.clone())?;
                let b = branch_of(&card, branch.clone())?;
                let recorded = accepted_heads::commit_for(c, &card.id, &b)?;
                out.push((card.id, card.project, r, b, recorded));
            }
            Ok(out)
        })
        .await?;
    let mut map = HashMap::new();
    for (card, project, repo, branch, recorded) in places {
        let commit = match recorded {
            Some(c) => c,
            None => match rt::remote_head(state, &project, &repo, &branch).await? {
                Some(h) => h,
                None => {
                    return Err(ApiError::conflict(format!(
                        "branch `{branch}` of card {card} is not on the remote of `{repo}`: push it first"
                    )));
                }
            },
        };
        map.insert(card, (branch, commit));
    }
    Ok(map)
}

/// The commit of `card` from [`merge_commits`], for `branch`.
fn merge_commit(
    commits: &HashMap<String, (String, String)>,
    card: &str,
    branch: &str,
) -> ApiResult<String> {
    match commits.get(card) {
        Some((b, c)) if b == branch => Ok(c.clone()),
        _ => Err(ApiError::conflict(format!(
            "card {card} changed while the merge was queued: queue it again"
        ))),
    }
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
    let planned_secs = planned(b.eta.as_deref())?;
    let commits = merge_commits(&state, &actor, b.cards.clone(), b.repo.clone(), None).await?;
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
                let branch = branch_of(card, None)?;
                let commit = merge_commit(&commits, &card.id, &branch)?;
                batch.push(jobs::BatchCard {
                    card: card.id.clone(),
                    branch,
                    by_agent: rt::by_agent_of(&card.labels),
                    commit: Some(commit),
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
                    commit: first.commit.clone(),
                    by_agent: first.by_agent.clone(),
                    batch: if batch.len() > 1 {
                        batch.clone()
                    } else {
                        Vec::new()
                    },
                    planned_secs,
                },
            )?;
            let code = rt::code_flow(t, &project)?;
            for c in batch.iter().filter(|_| code) {
                let patch = CardPatch {
                    add_labels: vec![rt::LABEL_QUEUED.into()],
                    ..CardPatch::default()
                };
                board::update_card(t, &Actor::Operator, &c.card, patch)?;
                board::add_comment(
                    t,
                    &Actor::Operator,
                    &c.card,
                    &rt::queue_comment(&job, &c.card),
                )?;
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
