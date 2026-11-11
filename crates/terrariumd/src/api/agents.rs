//! `/api/agents/...`: agent CRUD, lifecycle, turns, sessions, and roles.

use axum::extract::{Path, State};
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use axum::{Json};
use serde::{Deserialize, Serialize};
use serde_json::json;
use terrarium_core::turn_tag::{self, TurnSource};
use terrarium_core::{AgentId, AgentStatus, Answer, Role};

use super::{ApiJson, ApiQuery, Shared, paged, streams};
use crate::actor::Actor;
use crate::error::{ApiError, ApiResult, Code};
use crate::policy;
use crate::store::agents::{self, AgentPatch, AgentView};
use crate::store::{Page, projects};
use crate::supervisor::{AnswerInput, SupervisorError};

pub fn routes() -> super::link_registry::RegisteredRouter {
    super::link_registry::RegisteredRouter::new()
        .route("/agents", get(list).post(create), &[(axum::http::Method::GET, super::link_registry::Access::Link), (axum::http::Method::POST, super::link_registry::Access::Link)])
        .route("/agents/{id}", get(show).patch(update).delete(remove), &[(axum::http::Method::GET, super::link_registry::Access::Link), (axum::http::Method::PATCH, super::link_registry::Access::Link), (axum::http::Method::DELETE, super::link_registry::Access::Link)])
        .route("/agents/{id}/start", post(start), &[(axum::http::Method::POST, super::link_registry::Access::Link)])
        .route("/agents/{id}/stop", post(stop), &[(axum::http::Method::POST, super::link_registry::Access::Link)])
        .route("/agents/{id}/restart", post(restart), &[(axum::http::Method::POST, super::link_registry::Access::Link)])
        .route("/agents/{id}/pause", post(pause), &[(axum::http::Method::POST, super::link_registry::Access::Link)])
        .route("/agents/{id}/unpause", post(unpause), &[(axum::http::Method::POST, super::link_registry::Access::Link)])
        .route("/agents/{id}/compact", post(compact), &[(axum::http::Method::POST, super::link_registry::Access::Link)])
        .route("/agents/{id}/new-session", post(new_session), &[(axum::http::Method::POST, super::link_registry::Access::Link)])
        .route("/agents/{id}/send", post(send), &[(axum::http::Method::POST, super::link_registry::Access::Link)])
        .route("/agents/{id}/keys", post(keys), &[(axum::http::Method::POST, super::link_registry::Access::Link)])
        .route("/agents/{id}/interrupt", post(interrupt), &[(axum::http::Method::POST, super::link_registry::Access::Link)])
        .route("/agents/{id}/answer", post(answer), &[(axum::http::Method::POST, super::link_registry::Access::Link)])
        .route("/agents/{id}/events", get(streams::agent_events), &[(axum::http::Method::GET, super::link_registry::Access::Link)])
        .route("/agents/{id}/pty", get(streams::pty), &[(axum::http::Method::GET, super::link_registry::Access::Link)])
        .route("/agents/{id}/roles", get(roles).put(set_roles), &[(axum::http::Method::GET, super::link_registry::Access::Link), (axum::http::Method::PUT, super::link_registry::Access::Link)])
        .route("/agents/{id}/subscription", post(swap), &[(axum::http::Method::POST, super::link_registry::Access::Link)])
        .route("/agents/{id}/queued", get(queued), &[(axum::http::Method::GET, super::link_registry::Access::Link)])
        .route(
            "/agents/{id}/queued/{turn}",
            axum::routing::delete(remove_queued), &[(axum::http::Method::DELETE, super::link_registry::Access::Link)])
}

pub fn supervisor_error(e: SupervisorError) -> ApiError {
    match e {
        SupervisorError::NotRunning(id) => {
            ApiError::new(Code::NotRunning, format!("agent {id} is not running"))
        }
        SupervisorError::AlreadyRunning(id) => ApiError::new(
            Code::AlreadyRunning,
            format!("agent {id} is running already"),
        ),
        SupervisorError::NotReady(m) => ApiError::invalid(m),
        SupervisorError::Unsupported(m) => ApiError::new(Code::Conflict, m),
        SupervisorError::Other(m) => ApiError::internal(m),
    }
}

/// Find the agent named by the path and check that the actor may see it.
pub async fn load(state: &Shared, actor: &Actor, key: String) -> ApiResult<AgentView> {
    let context = actor.project().map(str::to_owned);
    let a = state
        .db
        .read(move |c| agents::find(c, &key, context.as_deref()))
        .await?;
    policy::read_project(actor, &a.agent.project)?;
    Ok(a)
}

fn agent_id(a: &AgentView) -> AgentId {
    a.agent.id
}

pub(super) fn file_managed(a: &AgentView) -> ApiError {
    ApiError::new(
        Code::FileManaged,
        format!(
            "agent `{}/{}` is declared in a file under agents/; edit the file, then POST /api/reload",
            a.agent.project, a.agent.name
        ),
    )
}

#[derive(Debug, Default, Deserialize)]
struct ListQuery {
    project: Option<String>,
    /// `ephemeral`: the ephemeral agents too (PLAN §18, "Ephemeral
    /// agents"). `card`: only the card agents among them (TER-75), those
    /// bound to a card or the assignee of a card. Default: without them.
    include: Option<String>,
}

async fn list(
    State(state): State<Shared>,
    actor: Actor,
    ApiQuery(page): ApiQuery<Page>,
    ApiQuery(q): ApiQuery<ListQuery>,
) -> ApiResult<Response> {
    let project = match (&actor, q.project) {
        (Actor::Agent(a), None) if !a.reads_all() => Some(a.project.clone()),
        (_, p) => p,
    };
    if let Some(p) = &project {
        policy::read_project(&actor, p)?;
    }
    let parts: Vec<&str> = q
        .include
        .as_deref()
        .map(|i| i.split(',').map(str::trim).collect())
        .unwrap_or_default();
    let ephemeral = if parts.contains(&"ephemeral") {
        agents::Ephemeral::All
    } else if parts.contains(&"card") {
        agents::Ephemeral::Card
    } else {
        agents::Ephemeral::None
    };
    let mut rows = state
        .db
        .read(move |c| agents::list_with(c, project.as_deref(), &page, ephemeral))
        .await?;
    let runs = state.supervisor.harness_runs();
    for v in &mut rows.items {
        v.harness_version = harness_version(&runs, v.agent.id);
    }
    mark_skills(&state, &runs, &mut rows.items).await;
    Ok(paged(rows))
}

/// Fill `effective_skills` of each agent of `views`, and set
/// `skills_restart` on each live agent whose delivered skills differ from
/// its effective skills now (PLAN §8.6): a change of its skill settings,
/// of a skill, or of the default-on skills of the library.
async fn mark_skills(
    state: &Shared,
    runs: &[crate::supervisor::HarnessRun],
    views: &mut [AgentView],
) {
    let Some(lib) = super::skills::library_opt(state) else {
        return;
    };
    let index = {
        let lib = lib.clone();
        tokio::task::spawn_blocking(move || crate::skills::LibraryIndex::read(Some(&lib)))
            .await
            .unwrap_or_default()
    };
    for v in views.iter_mut() {
        v.effective_skills = index.effective(v.agent.skill_selection());
    }
    type Live = (usize, std::path::PathBuf, Vec<String>, Vec<String>, bool);
    let live: Vec<Live> = views
        .iter()
        .enumerate()
        .filter(|(_, v)| runs.iter().any(|r| r.agent == v.agent.id))
        .map(|(i, v)| {
            (
                i,
                terrarium_runner::private_dir(&state.state_dir, v.agent.id),
                v.agent.skills.clone(),
                v.agent.skills_off.clone(),
                v.agent.default_skills,
            )
        })
        .collect();
    if live.is_empty() {
        return;
    }
    let marks = tokio::task::spawn_blocking(move || {
        live.into_iter()
            .filter(|(_, private, skills, off, default_skills)| {
                crate::skills::restart_needed(
                    &lib,
                    private,
                    terrarium_core::skills::Selection {
                        skills,
                        off,
                        default_skills: *default_skills,
                    },
                )
            })
            .map(|(i, ..)| i)
            .collect::<Vec<_>>()
    })
    .await
    .unwrap_or_default();
    for i in marks {
        if let Some(v) = views.get_mut(i) {
            v.skills_restart = true;
        }
    }
}

/// The harness version that the live agent `id` runs.
fn harness_version(runs: &[crate::supervisor::HarnessRun], id: AgentId) -> Option<String> {
    runs.iter()
        .find(|r| r.agent == id)
        .and_then(|r| r.version.clone())
}

/// The answer of `POST /api/agents`: the agent, and the result of each
/// `checkout.on_agent_create` clone.
#[derive(Debug, Serialize)]
struct Created {
    #[serde(flatten)]
    view: AgentView,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    checkout: Vec<serde_json::Value>,
}

/// `POST /api/agents`: an agent definition, or a template and the fields
/// that differ from it (docs/api.md §21). The rights checks run on the
/// definition after the template step, so a template grants nothing that
/// the caller could not grant itself.
async fn create(
    State(state): State<Shared>,
    actor: Actor,
    ApiJson(body): ApiJson<serde_json::Value>,
) -> ApiResult<(StatusCode, Json<Created>)> {
    let (def, template, template_grants) =
        super::templates::create_body(&state, &actor, body).await?;
    policy::admin_project(&actor, &def.project, "creating an agent")?;
    for role in &def.roles {
        policy::change_role(&actor, "", &def.project, *role)?;
    }
    // Each skill is in the library (PLAN §8.6).
    crate::skills::check_assignment(super::skills::library_opt(&state).as_deref(), &def.skills)?;
    let state_dir = state.work_dir.clone();
    let who = actor.clone();
    let mut def = def;
    let a = state
        .db
        .write(actor.label(), move |t| {
            agents::check_policy(t, &who, &def.project, None, &def.policy)?;
            policy::set_spawn_policy(&who, None, &def.roles, None, &def.policy)?;
            policy::set_thread_open(&who, None, &def.roles, None, &def.policy)?;
            // A missing subscription fails at the first wake: refuse it now.
            agents::check_subscription_exists(t, def.harness, def.subscription.as_deref())?;
            def.subscription_pool = agents::check_pool(
                t,
                def.harness,
                def.subscription.as_deref(),
                &def.subscription_pool,
            )?;
            let v =
                agents::create_from(t, &def, &state_dir, projects::SOURCE_API, template.as_ref())?;
            // The template's secret grants (TER-88), from the row that
            // `def` was built from: each needs the caller's right to grant
            // that secret.
            if let Some(tg) = &template_grants {
                crate::spawn_rights::grant_at_create(t, &who, &v, tg)?;
            }
            Ok(v)
        })
        .await?;
    // PLAN §9: clone `checkout.on_agent_create` into the workspace before
    // the first start. A failure is a `repo.checkout` event and an entry
    // in `checkout`; the agent stays created.
    let checkout = super::repos::checkout_on_create(&state, &actor, &a).await;
    let mut a = a;
    let runs = state.supervisor.harness_runs();
    mark_skills(&state, &runs, std::slice::from_mut(&mut a)).await;
    Ok((StatusCode::CREATED, Json(Created { view: a, checkout })))
}

/// The agent's subscription in `GET /api/agents/{id}`: no secret, no
/// quota numbers (those are in `GET /api/usage/me`).
#[derive(Debug, Serialize)]
struct SubscriptionDetail {
    name: String,
    /// The name for people (`store::subscriptions::display_name`).
    display_name: String,
    provider: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    plan: Option<String>,
    login_status: String,
    /// The harnesses that may spend it. The agent's harness is not always
    /// one of them: then a start fails.
    harnesses: Vec<terrarium_core::HarnessId>,
    /// The model the agent's harness gets when the agent sets no `model`
    /// (`crate::models::default_models`).
    #[serde(skip_serializing_if = "Option::is_none")]
    default_model: Option<String>,
}

/// `GET /api/agents/{id}`: the agent, and `subscription_detail` when its
/// subscription exists.
#[derive(Debug, Serialize)]
struct Shown {
    #[serde(flatten)]
    view: AgentView,
    #[serde(skip_serializing_if = "Option::is_none")]
    subscription_detail: Option<SubscriptionDetail>,
    /// The open waits of the agent (docs/api.md §6.7).
    open_waits: Vec<serde_json::Value>,
}

async fn show(
    State(state): State<Shared>,
    actor: Actor,
    Path(id): Path<String>,
) -> ApiResult<Json<Shown>> {
    let mut view = load(&state, &actor, id).await?;
    let runs = state.supervisor.harness_runs();
    view.harness_version = harness_version(&runs, view.agent.id);
    mark_skills(&state, &runs, std::slice::from_mut(&mut view)).await;
    let subscription_detail = match view.agent.subscription.clone() {
        Some(name) => {
            let harness = view.agent.harness;
            state
                .db
                .read(move |c| {
                    let s = crate::store::subscriptions::get(c, &name)?;
                    let d = crate::store::subscriptions::display_name(c, &s)?;
                    Ok((s, d))
                })
                .await
                .ok()
                .map(|(s, display_name)| SubscriptionDetail {
                    display_name,
                    default_model: crate::models::default_models(&s).remove(harness.as_str()),
                    name: s.name,
                    provider: s.provider,
                    plan: s.plan,
                    login_status: s.login_status,
                    harnesses: s.harnesses,
                })
        }
        None => None,
    };
    let open_waits = super::waits::open_waits(&state, view.agent.id.to_string()).await?;
    Ok(Json(Shown {
        view,
        subscription_detail,
        open_waits,
    }))
}

async fn update(
    State(state): State<Shared>,
    actor: Actor,
    Path(id): Path<String>,
    ApiJson(patch): ApiJson<AgentPatch>,
) -> ApiResult<Json<AgentView>> {
    let a = load(&state, &actor, id).await?;
    policy::admin_project(
        &actor,
        &a.agent.project,
        "setting an agent's prompt, harness, or policy",
    )?;
    crate::links::refuse_sysadmin_agent(&a.agent.roles, "change")?;
    if a.source == projects::SOURCE_FILE {
        return Err(file_managed(&a));
    }
    if let Some(list) = &patch.skills {
        crate::skills::check_assignment(super::skills::library_opt(&state).as_deref(), list)?;
    }
    let id = a.agent.id.to_string();
    let project = a.agent.project.clone();
    let who = actor.clone();
    let v = state
        .db
        .write(actor.label(), move |t| {
            if let Some(new) = &patch.policy {
                let now = agents::get(t, &id)?.agent;
                agents::check_policy(t, &who, &project, Some(&now.policy), new)?;
                policy::set_spawn_policy(&who, Some(&id), &now.roles, Some(&now.policy), new)?;
                policy::set_thread_open(&who, Some(&id), &now.roles, Some(&now.policy), new)?;
            }
            let mut patch = patch;
            // A missing subscription fails at the next wake: refuse it now.
            if let Some(Some(name)) = &patch.subscription {
                let harness = match patch.harness {
                    Some(h) => h,
                    None => agents::get(t, &id)?.agent.harness,
                };
                agents::check_subscription_exists(t, harness, Some(name))?;
            }
            // The pool after the patch: each member exists, has the
            // provider, and the harness can spend it (PLAN §16.3).
            if patch.subscription_pool.is_some() || patch.subscription.is_some() {
                let now = agents::get(t, &id)?.agent;
                let harness = patch.harness.unwrap_or(now.harness);
                let current = match &patch.subscription {
                    Some(s) => s.clone(),
                    None => now.subscription.clone(),
                };
                let pool = patch
                    .subscription_pool
                    .clone()
                    .unwrap_or(now.subscription_pool);
                let checked = agents::check_pool(t, harness, current.as_deref(), &pool)?;
                if patch.subscription_pool.is_some() || checked != pool {
                    patch.subscription_pool = Some(checked);
                }
            }
            agents::write_def(t, &id, &patch)?;
            t.audit(
                "agent.update",
                &id,
                Some(&project),
                json!({
                    "harness": patch.harness,
                    "subscription": patch.subscription,
                    "subscription_pool": patch.subscription_pool,
                    "model": patch.model,
                    "prompt": patch.prompt,
                    // Not the text: it can be long.
                    "system_prompt": patch.system_prompt.as_ref().map(|v| {
                        if v.as_deref().is_some_and(|t| !t.trim().is_empty()) {
                            "set"
                        } else {
                            "cleared"
                        }
                    }),
                    "persona": patch.persona,
                    "skills": patch.skills,
                    "skills_off": patch.skills_off,
                    "default_skills": patch.default_skills,
                    "policy": patch.policy,
                    "restart": patch.restart,
                    "idle_stop": patch.idle_stop,
                    "compact_on_idle": patch.compact_on_idle,
                    "compact_max_age": patch.compact_max_age,
                    "card_reminder": patch.card_reminder,
                }),
            )?;
            agents::get(t, &id)
        })
        .await?;
    let mut v = v;
    let runs = state.supervisor.harness_runs();
    mark_skills(&state, &runs, std::slice::from_mut(&mut v)).await;
    Ok(Json(v))
}

/// The body of `POST /api/agents/{id}/subscription`.
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct SwapBody {
    subscription: String,
}

/// `POST /api/agents/{id}/subscription`: move the agent to another plan
/// account (PLAN §16.3). The answer says how it applied: `live`,
/// `restart_scheduled`, `restarted`, or `next_start`. The same rights as
/// a change of the agent's fields; an agent of a file definition may swap
/// too, among the accounts of its pool.
async fn swap(
    State(state): State<Shared>,
    actor: Actor,
    Path(id): Path<String>,
    ApiJson(body): ApiJson<SwapBody>,
) -> ApiResult<Json<crate::account_swap::SwapOutcome>> {
    let a = load(&state, &actor, id).await?;
    policy::admin_project(
        &actor,
        &a.agent.project,
        "moving an agent to another account",
    )?;
    if a.source == projects::SOURCE_FILE && !a.agent.subscription_pool.contains(&body.subscription)
    {
        return Err(file_managed(&a));
    }
    let to = body.subscription.trim().to_owned();
    Ok(Json(
        crate::account_swap::swap(&state, actor.label(), a, &to, None).await?,
    ))
}

/// The query of `DELETE /api/agents/{id}`.
#[derive(Debug, Default, Deserialize)]
struct RemoveQuery {
    #[serde(default)]
    confirm: bool,
    /// Leave the workspace in the work root.
    #[serde(default)]
    keep_workspace: bool,
}

async fn remove(
    State(state): State<Shared>,
    actor: Actor,
    Path(id): Path<String>,
    ApiQuery(c): ApiQuery<RemoveQuery>,
) -> ApiResult<Response> {
    let a = load(&state, &actor, id).await?;
    // An ephemeral agent: the delete is a cancel (PLAN §18, "Ephemeral
    // agents"), with the rights of a cancel.
    if a.ephemeral {
        crate::spawn::cancel(&state, &actor, &a.agent.id.to_string()).await?;
        return Ok(StatusCode::NO_CONTENT.into_response());
    }
    policy::admin_project(&actor, &a.agent.project, "deleting an agent")?;
    if actor
        .agent()
        .is_some_and(|me| me.id == a.agent.id.to_string())
    {
        return Err(ApiError::forbidden("an agent cannot delete itself"));
    }
    if a.source == projects::SOURCE_FILE {
        return Err(file_managed(&a));
    }
    let live = a.agent.status.is_live()
        || state
            .supervisor
            .status(agent_id(&a))
            .await
            .is_some_and(|s| s.is_live());
    if live && !c.confirm {
        return Err(ApiError::new(
            Code::ConfirmRequired,
            format!(
                "agent `{}` has a live session; add ?confirm=true to stop and delete it",
                a.agent.name
            ),
        ));
    }
    // The guard keeps the `stopped` status of the stop from ending the
    // waits on the agent: the delete ends them as `deleted`.
    let _deleting = if live {
        let g = crate::trash::stop_agent(&state, agent_id(&a))
            .await
            .map_err(supervisor_error)?;
        Some(g)
    } else {
        None
    };
    let key = a.agent.id.to_string();
    state
        .db
        .write(actor.label(), move |t| agents::delete(t, &key))
        .await?;
    // The agent dir goes to the trash, not away: the operator can still
    // read the transcripts (`crate::trash`). The workspace in the work root
    // goes with it; a workspace elsewhere (a definition with its own
    // path) stays.
    let ws = (!c.keep_workspace
        && crate::workspace::rel(&state.work_dir, &a.agent.workspace).is_some())
    .then(|| a.agent.workspace.clone());
    crate::trash::trash_agent(&state, agent_id(&a), ws).await;
    // Its ephemeral agents go with it (PLAN §18, "Ephemeral agents").
    crate::spawn::owner_removed(&state, &a.agent.id.to_string()).await;
    Ok(StatusCode::NO_CONTENT.into_response())
}

pub(super) async fn start(
    State(state): State<Shared>,
    actor: Actor,
    Path(id): Path<String>,
) -> ApiResult<(StatusCode, Json<AgentView>)> {
    let a = load(&state, &actor, id).await?;
    policy::admin_project(&actor, &a.agent.project, "starting an agent")?;
    refuse_paused(&a, "start")?;
    refuse_disabled(&state, &a).await?;
    if state
        .supervisor
        .status(agent_id(&a))
        .await
        .is_some_and(|s| s.is_live())
    {
        return Err(supervisor_error(SupervisorError::AlreadyRunning(agent_id(
            &a,
        ))));
    }
    let id = a.agent.id.to_string();
    let project = a.agent.project.clone();
    let (resume, plan) = state
        .db
        .write(actor.label(), move |t| {
            t.audit("agent.start", &id, Some(&project), json!({}))?;
            // A pending new session (`crate::reset`): no resume.
            if let Some(plan) = crate::reset::take_new_session(t, &id)? {
                return Ok((None, Some(plan)));
            }
            Ok((agents::begin_resume(t, &id, "superseded")?, None))
        })
        .await?;
    // A start is a retry: a usage-limit hold ends (`crate::usage_limit`).
    crate::usage_limit::release_all(&state, &a.agent.id.to_string()).await?;
    let started = match plan {
        Some(plan) => state.supervisor.new_session(a.agent.clone(), plan).await,
        None => state.supervisor.start(a.agent.clone(), resume).await,
    };
    if let Err(e) = started {
        let id = a.agent.id.to_string();
        state
            .db
            .write("system", move |t| {
                agents::end_session(t, &id, "start_failed")
            })
            .await?;
        return Err(supervisor_error(e));
    }
    // A start is a wake: the queued turns go to the agent (PLAN §18).
    crate::lifecycle::wake_soon(&state, agent_id(&a), "start");
    let id = a.agent.id.to_string();
    let v = state.db.read(move |c| agents::get(c, &id)).await?;
    Ok((StatusCode::ACCEPTED, Json(v)))
}

/// `409 conflict` for an agent whose subscription is disabled (PLAN §16):
/// a start or a restart needs the enable first. A running agent goes on.
async fn refuse_disabled(state: &Shared, a: &AgentView) -> ApiResult<()> {
    let Some(name) = a.agent.subscription.clone() else {
        return Ok(());
    };
    let sub = state
        .db
        .read(move |c| crate::store::subscriptions::find(c, &name))
        .await?;
    match sub
        .as_ref()
        .and_then(crate::store::subscriptions::disabled_refusal)
    {
        Some(why) => Err(ApiError::new(Code::Conflict, why)),
        None => Ok(()),
    }
}

/// `409 conflict` for a paused agent: a start or a restart needs an
/// unpause first (PLAN §18).
fn refuse_paused(a: &AgentView, verb: &str) -> ApiResult<()> {
    if a.paused {
        return Err(ApiError::new(
            Code::Conflict,
            format!(
                "agent `{}/{}` is paused; unpause it before a {verb} \
                 (`terrarium agent unpause {}`)",
                a.agent.project, a.agent.name, a.agent.name
            ),
        ));
    }
    Ok(())
}

/// `POST /api/agents/{id}/pause`: set the `paused` flag, and stop the
/// agent when it runs (status reason `paused`). While the flag is set,
/// nothing wakes the agent; its turns and mail wait (PLAN §18). The same
/// rights as `stop`. A second pause changes nothing.
pub(super) async fn pause(
    State(state): State<Shared>,
    actor: Actor,
    Path(id): Path<String>,
) -> ApiResult<(StatusCode, Json<AgentView>)> {
    let a = load(&state, &actor, id).await?;
    policy::admin_project(&actor, &a.agent.project, "pausing an agent")?;
    set_paused(&state, &actor, &a, true).await?;
    let live = state
        .supervisor
        .status(agent_id(&a))
        .await
        .is_some_and(|s| s.is_live() || s == AgentStatus::Restarting);
    if live {
        let id = agent_id(&a);
        state
            .lifecycle
            .expect_stop(id, crate::lifecycle::REASON_PAUSED);
        let result = match actor.agent() {
            Some(by) => {
                let label = format!("agent:{}/{}", by.project, by.name);
                state
                    .supervisor
                    .stop_by(id, terrarium_runner::StoppedBy::Actor(label))
                    .await
            }
            None => state.supervisor.stop(id).await,
        };
        match result {
            Ok(()) => {}
            Err(SupervisorError::NotRunning(_)) => state.lifecycle.forget_stop(id),
            Err(e) => {
                state.lifecycle.forget_stop(id);
                return Err(supervisor_error(e));
            }
        }
    }
    let id = a.agent.id.to_string();
    let v = state.db.read(move |c| agents::get(c, &id)).await?;
    Ok((StatusCode::ACCEPTED, Json(v)))
}

/// `POST /api/agents/{id}/unpause`: clear the `paused` flag. An agent with
/// queued work wakes now; else it stays stopped until its next work
/// (PLAN §18). A second unpause changes nothing.
pub(super) async fn unpause(
    State(state): State<Shared>,
    actor: Actor,
    Path(id): Path<String>,
) -> ApiResult<(StatusCode, Json<AgentView>)> {
    let a = load(&state, &actor, id).await?;
    policy::admin_project(&actor, &a.agent.project, "unpausing an agent")?;
    set_paused(&state, &actor, &a, false).await?;
    crate::lifecycle::wake_with_work(&state, Some(agent_id(&a)), "unpause").await;
    let id = a.agent.id.to_string();
    let v = state.db.read(move |c| agents::get(c, &id)).await?;
    Ok((StatusCode::ACCEPTED, Json(v)))
}

/// The body of `POST /api/agents/{id}/compact`. An empty body is `{}`.
#[derive(Debug, Default, Deserialize)]
#[serde(deny_unknown_fields)]
struct CompactBody {
    /// The prompt to send after the compaction (at most 32 KiB).
    #[serde(default)]
    then: Option<String>,
    /// Wait for the end of the current turn.
    #[serde(default)]
    defer: bool,
}

/// The body of `POST /api/agents/{id}/new-session`. An empty body is `{}`.
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct NewSessionBody {
    /// The prompt to send after the new session is ready (at most 32 KiB).
    #[serde(default)]
    then: Option<String>,
    /// Start with the recap turn. Default true.
    #[serde(default = "yes")]
    recap: bool,
    /// Wait for the end of the current turn.
    #[serde(default)]
    defer: bool,
}

fn yes() -> bool {
    true
}

impl Default for NewSessionBody {
    fn default() -> Self {
        Self {
            then: None,
            recap: true,
            defer: false,
        }
    }
}

/// An optional JSON body: empty (or white space) gives the default.
fn optional_body<T: serde::de::DeserializeOwned + Default>(body: &[u8]) -> ApiResult<T> {
    if body.iter().all(u8::is_ascii_whitespace) {
        return Ok(T::default());
    }
    serde_json::from_slice(body).map_err(|e| ApiError::bad_request(e.to_string()))
}

/// The answer of a reset request: the agent, and what the request did.
#[derive(Debug, Serialize)]
pub(super) struct ResetAnswer {
    #[serde(flatten)]
    agent: AgentView,
    reset: crate::reset::Outcome,
}

/// `POST /api/agents/{id}/compact` (`{then?, defer?}`): compact the
/// session of the agent (PLAN §18, "Compact and new session on request",
/// `crate::reset`). The agent itself may ask; for another agent, the
/// rights are those of `restart`. At rest, the compaction starts now;
/// for a busy agent, a request of the agent itself, or `defer`, it waits
/// for the end of the current turn. `then` goes as the next turn after
/// the end. The event `agent.compacted` gives the end. `409 conflict`: the
/// harness has no compaction, or the agent is paused or held. `409
/// not_running`: the agent does not run. A request on another agent
/// writes the audit row `agent.compact`.
pub(super) async fn compact(
    State(state): State<Shared>,
    actor: Actor,
    Path(id): Path<String>,
    body: axum::body::Bytes,
) -> ApiResult<(StatusCode, Json<ResetAnswer>)> {
    let b: CompactBody = optional_body(&body)?;
    let a = load(&state, &actor, id).await?;
    let req = crate::reset::Request {
        kind: agents::ResetKind::Compact,
        then: b.then,
        recap: false,
        defer: b.defer,
    };
    reset(&state, &actor, &a, req).await
}

/// `POST /api/agents/{id}/new-session` (`{then?, recap?, defer?}`): end
/// the harness session of the agent and start a new one, not a resume
/// (`crate::reset`). The same rights and the same wait as `compact`. The
/// new session starts with the recap turn (unless `recap: false`), then
/// `then`. An agent that does not run starts now when `then` is set, else
/// at its next wake. A request on another agent writes the audit row
/// `agent.new_session`.
pub(super) async fn new_session(
    State(state): State<Shared>,
    actor: Actor,
    Path(id): Path<String>,
    body: axum::body::Bytes,
) -> ApiResult<(StatusCode, Json<ResetAnswer>)> {
    let b: NewSessionBody = optional_body(&body)?;
    let a = load(&state, &actor, id).await?;
    let req = crate::reset::Request {
        kind: agents::ResetKind::NewSession,
        then: b.then,
        recap: b.recap,
        defer: b.defer,
    };
    reset(&state, &actor, &a, req).await
}

async fn reset(
    state: &Shared,
    actor: &Actor,
    a: &AgentView,
    req: crate::reset::Request,
) -> ApiResult<(StatusCode, Json<ResetAnswer>)> {
    // `then` is a turn after the reset.
    if req.then.is_some() {
        crate::links::refuse_sysadmin_agent(&a.agent.roles, "send a `then` turn to")?;
    }
    let outcome = crate::reset::request(state, actor, a, req).await?;
    let id = a.agent.id.to_string();
    let v = state.db.read(move |c| agents::get(c, &id)).await?;
    Ok((
        StatusCode::ACCEPTED,
        Json(ResetAnswer {
            agent: v,
            reset: outcome,
        }),
    ))
}

/// Write the `paused` flag. A change writes the audit row `agent.pause` or
/// `agent.unpause` and the event `agent.paused` or `agent.unpaused`.
async fn set_paused(state: &Shared, actor: &Actor, a: &AgentView, paused: bool) -> ApiResult<()> {
    if paused {
        crate::spawn::refuse_pause(a)?;
    }
    let (id, project, name) = (
        a.agent.id.to_string(),
        a.agent.project.clone(),
        a.agent.name.clone(),
    );
    state
        .db
        .write(actor.label(), move |t| {
            let before = agents::set_paused(t, &id, paused)?;
            if before == paused {
                return Ok(());
            }
            let (verb, kind) = if paused {
                ("pause", "agent.paused")
            } else {
                ("unpause", "agent.unpaused")
            };
            let queued = agents::queued_count(t, &id)?;
            t.audit(
                &format!("agent.{verb}"),
                &id,
                Some(&project),
                json!({"queued": queued}),
            )?;
            t.emit(
                crate::events::EventDraft::new(
                    kind,
                    format!("{name}: {kind}"),
                    json!({"paused": paused, "queued": queued}),
                )
                .agent(id.clone(), name.clone())
                .project(project.clone()),
            )?;
            Ok(())
        })
        .await
}

pub(super) async fn stop(
    State(state): State<Shared>,
    actor: Actor,
    Path(id): Path<String>,
) -> ApiResult<(StatusCode, Json<AgentView>)> {
    let a = load(&state, &actor, id).await?;
    policy::admin_project(&actor, &a.agent.project, "stopping an agent")?;
    let errored = a.agent.status == AgentStatus::Error
        || state.supervisor.status(agent_id(&a)).await == Some(AgentStatus::Error);
    // `runner-exit.json` names the agent that asked for the stop.
    let result = match actor.agent() {
        Some(by) => {
            let label = format!("agent:{}/{}", by.project, by.name);
            state
                .supervisor
                .stop_by(agent_id(&a), terrarium_runner::StoppedBy::Actor(label))
                .await
        }
        None => state.supervisor.stop(agent_id(&a)).await,
    };
    let id = a.agent.id.to_string();
    let project = a.agent.project.clone();
    let was_live = a.agent.status.is_live();
    match result {
        Ok(()) => {}
        // The store thinks it runs, the supervisor does not: reconcile.
        Err(SupervisorError::NotRunning(_)) if was_live => {}
        // An agent in `error`: the operator clears it to `stopped`.
        Err(SupervisorError::NotRunning(_)) if errored => {
            state.supervisor.clear_error(agent_id(&a)).await;
        }
        Err(e) => return Err(supervisor_error(e)),
    }
    let v = state
        .db
        .write(actor.label(), move |t| {
            t.audit("agent.stop", &id, Some(&project), json!({}))?;
            if result.is_err() {
                let reason = if was_live {
                    "reconciled"
                } else {
                    "cleared by operator"
                };
                crate::apply_status(t, &id, AgentStatus::Stopped, Some(reason))?;
            }
            agents::get(t, &id)
        })
        .await?;
    Ok((StatusCode::ACCEPTED, Json(v)))
}

pub(super) async fn restart(
    State(state): State<Shared>,
    actor: Actor,
    Path(id): Path<String>,
) -> ApiResult<(StatusCode, Json<AgentView>)> {
    let a = load(&state, &actor, id).await?;
    policy::admin_project(&actor, &a.agent.project, "restarting an agent")?;
    refuse_paused(&a, "restart")?;
    refuse_disabled(&state, &a).await?;
    let live = state
        .supervisor
        .status(agent_id(&a))
        .await
        .is_some_and(|s| s.is_live());
    let id = a.agent.id.to_string();
    let project = a.agent.project.clone();
    let resume = state
        .db
        .write(actor.label(), move |t| {
            let resume = agents::begin_resume(t, &id, "restarted")?;
            t.audit(
                "agent.restart",
                &id,
                Some(&project),
                json!({"was_live": live}),
            )?;
            Ok(resume)
        })
        .await?;
    let mut agent = a.agent.clone();
    agent.session = resume.clone();
    let result = if live {
        state.supervisor.restart(agent).await
    } else {
        state.supervisor.start(agent, resume).await
    };
    result.map_err(supervisor_error)?;
    // A restart is a retry: a usage-limit hold ends (`crate::usage_limit`).
    crate::usage_limit::release_all(&state, &a.agent.id.to_string()).await?;
    let id = a.agent.id.to_string();
    let v = state.db.read(move |c| agents::get(c, &id)).await?;
    Ok((StatusCode::ACCEPTED, Json(v)))
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct SendBody {
    pub(super) text: String,
    /// Type the text as-is, at once (D3): a leading `/` or `!` runs as a
    /// harness command. Without it, the turn waits for `idle`, and starts
    /// with the tag `[User]`, or `[Agent:<project>/<name>]` from an agent
    /// (PLAN §8.5), so it stays a prompt.
    #[serde(default)]
    pub(super) raw: bool,
}

/// The first word of a raw text for the audit log (`/compact`): at most
/// 40 characters, never the rest of the text.
pub fn first_word(text: &str) -> String {
    text.split_whitespace()
        .next()
        .unwrap_or_default()
        .chars()
        .take(40)
        .collect()
}

pub(super) async fn send(
    State(state): State<Shared>,
    actor: Actor,
    Path(id): Path<String>,
    ApiJson(body): ApiJson<SendBody>,
) -> ApiResult<(StatusCode, Json<serde_json::Value>)> {
    // `[User]` is only the operator; an agent's turn names the agent
    // (PLAN §8.5).
    let source = match actor.agent() {
        Some(a) => TurnSource::Agent {
            sender: format!("{}/{}", a.project, a.name),
        },
        None => TurnSource::User,
    };
    send_as(&state, actor, id, body, &source).await
}

/// `send` with the source of the turn: `[User]` for the operator,
/// `[Agent:<project>/<name>]` for an agent, and
/// `[System:schedule <name>]` for a schedule (PLAN §8.5). A raw send has
/// no tag.
pub(super) async fn send_as(
    state: &Shared,
    actor: Actor,
    id: String,
    body: SendBody,
    source: &TurnSource,
) -> ApiResult<(StatusCode, Json<serde_json::Value>)> {
    let a = load(state, &actor, id).await?;
    policy::admin_project(&actor, &a.agent.project, "typing a turn into an agent")?;
    crate::links::refuse_sysadmin_agent(&a.agent.roles, "send a turn to")?;
    if body.text.trim().is_empty() {
        return Err(ApiError::invalid("the turn text is empty"));
    }
    if body.raw {
        return send_raw(state, &actor, &a, body.text).await;
    }
    let text = turn_tag::render(source, &body.text);
    // The delivery log of the turn (`docs/api.md` §6.6). It goes in
    // before the turn is typed.
    let (id, logged) = (a.agent.id.to_string(), text.clone());
    state
        .db
        .write(actor.label(), move |t| {
            crate::store::transcripts::record_turn(t, &id, &logged)
        })
        .await?;
    // An agent that does not run (stopped, paused, in `error`) gets the
    // turn at its next wake (PLAN §18): the turn waits in the store, and a
    // wake starts the agent unless it may not wake.
    let runs = state
        .supervisor
        .status(agent_id(&a))
        .await
        .is_some_and(|s| s.is_live() || s == AgentStatus::Restarting);
    // A turn that already waits in the store goes first: a turn that came
    // while a wake was on the way can wait there after the wake read the
    // queue (TER-25). This turn then waits behind it, and the wake below
    // gives both to the supervisor in order.
    let waiting = runs && {
        let key = a.agent.id.to_string();
        state
            .db
            .read(move |c| agents::has_queued_turns(c, &key))
            .await?
    };
    let queued = if runs && !waiting {
        match state.supervisor.send_turn(agent_id(&a), text.clone()).await {
            Ok(()) => false,
            // It stopped meanwhile.
            Err(SupervisorError::NotRunning(_)) => true,
            Err(e) => return Err(supervisor_error(e)),
        }
    } else {
        true
    };
    let id = a.agent.id.to_string();
    let project = a.agent.project.clone();
    let len = body.text.len();
    state
        .db
        .write(actor.label(), move |t| {
            if queued {
                agents::queue_turn(t, &id, &text)?;
            }
            let mut detail = json!({"bytes": len});
            if queued {
                detail["queued"] = json!(true);
            }
            t.audit("agent.send", &id, Some(&project), detail)
        })
        .await?;
    if queued {
        if !a.paused {
            crate::lifecycle::wake(state, agent_id(&a), "turn").await;
        }
        return Ok((
            StatusCode::ACCEPTED,
            Json(json!({"accepted": true, "queued": true})),
        ));
    }
    Ok((StatusCode::ACCEPTED, Json(json!({"accepted": true}))))
}

/// `send` with `raw: true`: the supervisor types the text at once, with
/// no escape and no tag. A line that would read as a tag is defused
/// (`[User]` becomes `[\User]`), so a raw send cannot pose as a mail or
/// as Terrarium (PLAN §8.5). The audit row has the length and the first
/// word only.
async fn send_raw(
    state: &Shared,
    actor: &Actor,
    a: &AgentView,
    text: String,
) -> ApiResult<(StatusCode, Json<serde_json::Value>)> {
    if text.chars().count() > terrarium_core::display::MAX_TERMINAL_TEXT {
        return Err(ApiError::invalid(format!(
            "a raw text has at most {} characters",
            terrarium_core::display::MAX_TERMINAL_TEXT
        )));
    }
    if !KEY_LIMITER.check(&a.agent.id.to_string(), 1) {
        return Err(key_limit_error());
    }
    let (len, word) = (text.len(), first_word(&text));
    let text = turn_tag::defuse(&text).into_owned();
    state
        .supervisor
        .send_raw(agent_id(a), text)
        .await
        .map_err(supervisor_error)?;
    let id = a.agent.id.to_string();
    let project = a.agent.project.clone();
    state
        .db
        .write(actor.label(), move |t| {
            t.audit(
                "agent.send_raw",
                &id,
                Some(&project),
                json!({"bytes": len, "first_word": word}),
            )
        })
        .await?;
    Ok((
        StatusCode::ACCEPTED,
        Json(json!({"accepted": true, "raw": true})),
    ))
}

/// The pause between two keys of one request. Some TUIs read a lone Esc
/// that comes right before another key as Alt with that key.
const KEY_GAP: std::time::Duration = std::time::Duration::from_millis(50);

/// The rate limit of the keys and the raw sends of one agent: at most
/// [`terrarium_core::display::TERMINAL_KEYS_PER_SEC`] in one second (a
/// fixed window for each agent).
#[derive(Debug, Default)]
pub struct KeyLimiter {
    windows: std::sync::Mutex<std::collections::HashMap<String, (std::time::Instant, u32)>>,
}

impl KeyLimiter {
    /// Take `n` keys of `agent`'s window. False: the limit is reached, and
    /// nothing is taken.
    pub fn check(&self, agent: &str, n: u32) -> bool {
        self.check_at(agent, n, std::time::Instant::now())
    }

    pub fn check_at(&self, agent: &str, n: u32, now: std::time::Instant) -> bool {
        let second = std::time::Duration::from_secs(1);
        let mut w = self
            .windows
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if w.len() > 1024 {
            w.retain(|_, (start, _)| now.duration_since(*start) < second);
        }
        let entry = w.entry(agent.to_owned()).or_insert((now, 0));
        if now.duration_since(entry.0) >= second {
            *entry = (now, 0);
        }
        if entry.1 + n > terrarium_core::display::TERMINAL_KEYS_PER_SEC {
            return false;
        }
        entry.1 += n;
        true
    }
}

static KEY_LIMITER: std::sync::LazyLock<KeyLimiter> = std::sync::LazyLock::new(KeyLimiter::default);

fn key_limit_error() -> ApiError {
    ApiError::new(
        Code::RateLimited,
        format!(
            "more than {} keys in one second to this agent; wait, and send fewer keys",
            terrarium_core::display::TERMINAL_KEYS_PER_SEC
        ),
    )
}

/// `POST /api/agents/{id}/keys`.
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct KeysBody {
    keys: Vec<String>,
}

/// `POST /api/agents/{id}/keys` (D3): key presses for the harness TUI, at
/// once, with no wait for `idle`: the chord names of the display keys
/// (`esc`, `down`, `enter`, `ctrl-c`, `shift-tab`, `f5`), and `text:<text>`
/// for literal text ([`terrarium_core::display::TerminalKey`]). The rights
/// of the PTY attach: project_admin of the agent's project, sysadmin, or
/// the operator. The audit row names the keys; a text key has its length
/// only.
async fn keys(
    State(state): State<Shared>,
    actor: Actor,
    Path(id): Path<String>,
    ApiJson(body): ApiJson<KeysBody>,
) -> ApiResult<(StatusCode, Json<serde_json::Value>)> {
    use terrarium_core::display::{MAX_TERMINAL_KEYS, TerminalKey};
    let a = load(&state, &actor, id).await?;
    policy::admin_project(
        &actor,
        &a.agent.project,
        "sending keys to an agent terminal",
    )?;
    crate::links::refuse_sysadmin_agent(&a.agent.roles, "send keys to")?;
    if body.keys.is_empty() {
        return Err(ApiError::invalid("send at least one key"));
    }
    if body.keys.len() > MAX_TERMINAL_KEYS {
        return Err(ApiError::invalid(format!(
            "at most {MAX_TERMINAL_KEYS} keys in one request"
        )));
    }
    let parsed = body
        .keys
        .iter()
        .map(|k| TerminalKey::parse(k).map_err(|e| ApiError::invalid(format!("key {k:?}: {e}"))))
        .collect::<ApiResult<Vec<_>>>()?;
    let n = u32::try_from(parsed.len()).unwrap_or(u32::MAX);
    if !KEY_LIMITER.check(&a.agent.id.to_string(), n) {
        return Err(key_limit_error());
    }
    let names: Vec<String> = parsed.iter().map(TerminalKey::audit_name).collect();
    for (i, k) in parsed.iter().enumerate() {
        if i > 0 {
            tokio::time::sleep(KEY_GAP).await;
        }
        state
            .supervisor
            .write_keys(agent_id(&a), k.bytes())
            .await
            .map_err(supervisor_error)?;
    }
    let id = a.agent.id.to_string();
    let project = a.agent.project.clone();
    let audit_names = names.clone();
    state
        .db
        .write(actor.label(), move |t| {
            t.audit(
                "agent.keys",
                &id,
                Some(&project),
                json!({"keys": audit_names}),
            )
        })
        .await?;
    Ok((
        StatusCode::ACCEPTED,
        Json(json!({"accepted": true, "keys": names})),
    ))
}

/// `POST /api/agents/{id}/interrupt` (issue #12, the Stop of the
/// dashboard): stop the running turn of the agent with the interrupt keys
/// of its harness (`Adapter::interrupt_keys`: Esc, or Esc Esc for
/// Copilot; never Ctrl-C), and keep the harness running. The agent goes
/// `idle` with the reason `interrupted`. The rights of a send: the
/// project_admin of the agent's project, sysadmin, or the operator. The
/// keys count against the key rate limit. `409 conflict`: no turn runs, a
/// compaction runs, or the harness has no interrupt keys.
async fn interrupt(
    State(state): State<Shared>,
    actor: Actor,
    Path(id): Path<String>,
) -> ApiResult<(StatusCode, Json<serde_json::Value>)> {
    let a = load(&state, &actor, id).await?;
    policy::admin_project(&actor, &a.agent.project, "interrupting an agent's turn")?;
    let live = state.supervisor.status(agent_id(&a)).await;
    if !matches!(live, Some(AgentStatus::Working | AgentStatus::Blocked)) {
        if live.is_none_or(|s| !s.is_live()) {
            return Err(supervisor_error(SupervisorError::NotRunning(agent_id(&a))));
        }
        return Err(ApiError::new(Code::Conflict, crate::supervisor::NO_TURN));
    }
    let n = u32::try_from(crate::supervisor::interrupt_keys(a.agent.harness).len()).unwrap_or(1);
    if !KEY_LIMITER.check(&a.agent.id.to_string(), n.max(1)) {
        return Err(key_limit_error());
    }
    state
        .supervisor
        .interrupt(agent_id(&a))
        .await
        .map_err(|e| match e {
            SupervisorError::NotReady(m) => ApiError::new(Code::Conflict, m),
            e => supervisor_error(e),
        })?;
    let id = a.agent.id.to_string();
    let project = a.agent.project.clone();
    let harness = a.agent.harness.to_string();
    state
        .db
        .write(actor.label(), move |t| {
            t.audit(
                "agent.interrupt",
                &id,
                Some(&project),
                json!({"harness": harness}),
            )
        })
        .await?;
    Ok((
        StatusCode::ACCEPTED,
        Json(json!({"accepted": true, "interrupted": true})),
    ))
}

/// `{request_id, answer}` for a known question, or `{text}` for raw keys.
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct AnswerBody {
    #[serde(default)]
    request_id: Option<String>,
    #[serde(default)]
    answer: Option<Answer>,
    #[serde(default)]
    text: Option<String>,
}

async fn answer(
    State(state): State<Shared>,
    actor: Actor,
    Path(id): Path<String>,
    ApiJson(body): ApiJson<AnswerBody>,
) -> ApiResult<(StatusCode, Json<serde_json::Value>)> {
    let a = load(&state, &actor, id).await?;
    policy::admin_project(&actor, &a.agent.project, "answering an agent's question")?;
    crate::links::refuse_sysadmin_agent(&a.agent.roles, "answer")?;
    let input = match body {
        AnswerBody {
            request_id: Some(request_id),
            answer: Some(answer),
            text: None,
        } => AnswerInput::Answer { request_id, answer },
        AnswerBody {
            request_id: None,
            answer: None,
            text: Some(text),
        } => AnswerInput::Keys(text),
        _ => {
            return Err(ApiError::bad_request(
                "send {\"request_id\", \"answer\"} or {\"text\"}",
            ));
        }
    };
    state
        .supervisor
        .answer(agent_id(&a), input)
        .await
        .map_err(supervisor_error)?;
    let id = a.agent.id.to_string();
    let project = a.agent.project.clone();
    state
        .db
        .write(actor.label(), move |t| {
            t.audit("agent.answer", &id, Some(&project), json!({}))
        })
        .await?;
    Ok((StatusCode::ACCEPTED, Json(json!({"accepted": true}))))
}

async fn roles(
    State(state): State<Shared>,
    actor: Actor,
    Path(id): Path<String>,
) -> ApiResult<Json<Vec<Role>>> {
    Ok(Json(load(&state, &actor, id).await?.agent.roles))
}

#[derive(Debug, Deserialize)]
#[serde(untagged)]
enum RolesBody {
    List(Vec<Role>),
    Object { roles: Vec<Role> },
}

async fn set_roles(
    State(state): State<Shared>,
    actor: Actor,
    Path(id): Path<String>,
    ApiJson(body): ApiJson<RolesBody>,
) -> ApiResult<Json<Vec<Role>>> {
    let a = load(&state, &actor, id).await?;
    if a.source == projects::SOURCE_FILE {
        return Err(file_managed(&a));
    }
    let wanted = match body {
        RolesBody::List(r) | RolesBody::Object { roles: r } => r,
    };
    let id = a.agent.id.to_string();
    for role in [Role::ProjectAdmin, Role::Sysadmin, Role::Architect] {
        if wanted.contains(&role) != a.agent.roles.contains(&role) {
            policy::change_role(&actor, &id, &a.agent.project, role)?;
        }
    }
    if let Actor::Agent(me) = &actor
        && me.id == id
    {
        return Err(ApiError::forbidden("an agent cannot change its own roles"));
    }
    let project = a.agent.project.clone();
    let roles = state
        .db
        .write(actor.label(), move |t| {
            agents::set_roles(t, &id, &project, &wanted)
        })
        .await?;
    Ok(Json(roles))
}

/// One queued turn in the answer of `GET /api/agents/{id}/queued`.
#[derive(Debug, Serialize)]
struct QueuedView {
    id: serde_json::Value,
    /// Where the turn waits: `store`, `supervisor`, or `runner`.
    source: &'static str,
    /// The turn as it will be typed, with its tag (`[User] ...`).
    text: String,
    /// The text after the tag: what the composer gets for an edit.
    body: String,
    created_at: String,
}

/// The text of a turn after its tag (`[User] `, `[Agent:p/a] `,
/// `[System:schedule s] `). A text with no tag stays.
fn untagged(text: &str) -> &str {
    if let Some(rest) = text.strip_prefix('[')
        && let Some(end) = rest.find(']')
        && !rest[..end].contains('\n')
    {
        let after = &rest[end + 1..];
        return after.strip_prefix(' ').unwrap_or(after);
    }
    text
}

/// The `source` of a turn in the store (`queued_turns`).
const QUEUE_STORE: &str = "store";

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct QueuedQuery {
    /// The queue of the turn (`source` of the list). Default: `store`.
    #[serde(default)]
    source: Option<String>,
}

/// Waiting turns in delivery order, redacted with the agent's read right.
async fn queued(
    State(state): State<Shared>,
    actor: Actor,
    Path(id): Path<String>,
) -> ApiResult<Json<serde_json::Value>> {
    let a = load(&state, &actor, id).await?;
    let key = a.agent.id.to_string();
    let turns = state
        .db
        .read(move |c| agents::queued_turns(c, &key))
        .await?;
    let redactor = state.db.redactor().get();
    let view = crate::secrets::redact::view_of(&actor);
    let mut items: Vec<QueuedView> = turns
        .into_iter()
        .map(|t| {
            let text = redactor.redact_string_as(t.text, &view).0;
            QueuedView {
                id: json!(t.id),
                source: QUEUE_STORE,
                body: untagged(&text).to_owned(),
                text,
                created_at: t.created_at,
            }
        })
        .collect();
    let live = match state.supervisor.list_pending(agent_id(&a)).await {
        Ok(live) => live,
        Err(e) => crate::supervisor::PendingTurns {
            items: Vec::new(),
            runner: Some(if matches!(e, SupervisorError::Unsupported(_)) {
                "unsupported"
            } else {
                "unavailable"
            }),
        },
    };
    let mut live_items = Vec::new();
    for turn in live.items {
        let text = redactor.redact_string_as(turn.text, &view).0;
        live_items.push(QueuedView {
            id: json!(turn.id),
            source: turn.source,
            body: untagged(&text).to_owned(),
            text,
            created_at: String::new(),
        });
    }
    live_items.append(&mut items);
    let items = live_items;
    Ok(Json(
        json!({"items": items, "queues": {"runner": live.runner.unwrap_or("available")}}),
    ))
}

/// `DELETE /api/agents/{id}/queued/{turn}` (TER-54): drop one queued turn
/// before the wake types it. The right of a turn: project_admin of the
/// agent's project, sysadmin, or the operator. 404 when the turn does not
/// wait (any more): the wake gave it to the agent, or it never was. The
/// turns after it keep their order.
async fn remove_queued(
    State(state): State<Shared>,
    actor: Actor,
    Path((id, turn)): Path<(String, String)>,
    ApiQuery(q): ApiQuery<QueuedQuery>,
) -> ApiResult<StatusCode> {
    let a = load(&state, &actor, id).await?;
    if let Some(s) = q.source.as_deref()
        && ![QUEUE_STORE, "supervisor", "runner"].contains(&s)
    {
        return Err(ApiError::invalid(format!(
            "unknown queue `{s}`: give the source of the turn in the list"
        )));
    }
    policy::admin_project(&actor, &a.agent.project, "removing a queued turn")?;
    let lock = state.lifecycle.turn_lock(agent_id(&a));
    let _held = lock.lock().await;
    if let Some(source @ ("runner" | "supervisor")) = q.source.as_deref() {
        if !state
            .supervisor
            .cancel_pending(agent_id(&a), source, turn.clone())
            .await
            .map_err(supervisor_error)?
        {
            return Err(ApiError::not_found("turn no longer waits for the agent"));
        }
        let (key, project, source) = (
            a.agent.id.to_string(),
            a.agent.project.clone(),
            source.to_owned(),
        );
        state
            .db
            .write(actor.label(), move |t| {
                t.audit(
                    "agent.queued.remove",
                    &key,
                    Some(&project),
                    json!({"turn": turn, "source": source}),
                )
            })
            .await?;
        return Ok(StatusCode::NO_CONTENT);
    }
    let turn: i64 = turn
        .parse()
        .map_err(|_| ApiError::invalid("invalid store turn ID"))?;
    let (key, project) = (a.agent.id.to_string(), a.agent.project.clone());
    let removed = state
        .db
        .write(actor.label(), move |t| {
            let r = agents::remove_queued_turn(t, &key, turn)?;
            if let Some(q) = &r {
                t.audit(
                    "agent.queued.remove",
                    &key,
                    Some(&project),
                    json!({"turn": q.id, "bytes": q.text.len()}),
                )?;
            }
            Ok(r)
        })
        .await?;
    match removed {
        Some(_) => Ok(StatusCode::NO_CONTENT),
        None => Err(ApiError::not_found(format!(
            "turn {turn} does not wait for the agent (it went out, or it never was)"
        ))),
    }
}

#[cfg(test)]
mod queued_tests {
    use super::untagged;

    #[test]
    fn the_body_is_the_text_after_the_tag() {
        assert_eq!(untagged("[User] Run the tests"), "Run the tests");
        assert_eq!(untagged("[Agent:p/lead] Do TER-4."), "Do TER-4.");
        assert_eq!(untagged("[System:schedule s] go"), "go");
        assert_eq!(untagged("[User]"), "");
        assert_eq!(untagged("no tag"), "no tag");
        assert_eq!(untagged("[not\nclosed] x"), "[not\nclosed] x");
    }
}

#[cfg(test)]
mod key_tests {
    use super::*;
    use std::time::{Duration, Instant};

    #[test]
    fn the_key_limit_is_a_window_per_agent() {
        let l = KeyLimiter::default();
        let t0 = Instant::now();
        assert!(l.check_at("a", 15, t0));
        assert!(l.check_at("a", 5, t0 + Duration::from_millis(500)));
        // The window is full: nothing is taken.
        assert!(!l.check_at("a", 1, t0 + Duration::from_millis(900)));
        // Another agent has its own window.
        assert!(l.check_at("b", 20, t0));
        // A new second, a new window.
        assert!(l.check_at("a", 20, t0 + Duration::from_millis(1000)));
        assert!(!l.check_at("a", 21, t0 + Duration::from_millis(2500)));
    }

    #[test]
    fn the_first_word_is_short() {
        assert_eq!(first_word("  /compact now please"), "/compact");
        assert_eq!(first_word(""), "");
        assert_eq!(first_word(&"x".repeat(100)).len(), 40);
    }
}
