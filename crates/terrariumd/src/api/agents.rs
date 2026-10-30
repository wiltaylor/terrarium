//! `/api/agents/...`: agent CRUD, lifecycle, turns, sessions, and roles.

use axum::extract::{Path, State};
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use axum::{Json, Router};
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

pub fn routes() -> Router<Shared> {
    Router::new()
        .route("/agents", get(list).post(create))
        .route("/agents/{id}", get(show).patch(update).delete(remove))
        .route("/agents/{id}/start", post(start))
        .route("/agents/{id}/stop", post(stop))
        .route("/agents/{id}/restart", post(restart))
        .route("/agents/{id}/send", post(send))
        .route("/agents/{id}/keys", post(keys))
        .route("/agents/{id}/answer", post(answer))
        .route("/agents/{id}/events", get(streams::agent_events))
        .route("/agents/{id}/pty", get(streams::pty))
        .route("/agents/{id}/roles", get(roles).put(set_roles))
        .route("/agents/{id}/subscription", post(swap))
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

fn file_managed(a: &AgentView) -> ApiError {
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
}

async fn list(
    State(state): State<Shared>,
    actor: Actor,
    ApiQuery(page): ApiQuery<Page>,
    ApiQuery(q): ApiQuery<ListQuery>,
) -> ApiResult<Response> {
    let project = match (&actor, q.project) {
        (Actor::Agent(a), None) if !a.is_sysadmin() => Some(a.project.clone()),
        (_, p) => p,
    };
    if let Some(p) = &project {
        policy::read_project(&actor, p)?;
    }
    let mut rows = state
        .db
        .read(move |c| agents::list(c, project.as_deref(), &page))
        .await?;
    let runs = state.supervisor.harness_runs();
    for v in &mut rows.items {
        v.harness_version = harness_version(&runs, v.agent.id);
    }
    mark_skills_restart(&state, &runs, &mut rows.items).await;
    Ok(paged(rows))
}

/// Set `skills_restart` on each live agent of `views` whose delivered
/// skills differ from what its `skills` list picks from the library now
/// (PLAN §8.6).
async fn mark_skills_restart(
    state: &Shared,
    runs: &[crate::supervisor::HarnessRun],
    views: &mut [AgentView],
) {
    let Some(lib) = super::skills::library_opt(state) else {
        return;
    };
    let live: Vec<(usize, std::path::PathBuf, Vec<String>)> = views
        .iter()
        .enumerate()
        .filter(|(_, v)| runs.iter().any(|r| r.agent == v.agent.id))
        .map(|(i, v)| {
            (
                i,
                terrarium_runner::private_dir(&state.state_dir, v.agent.id),
                v.agent.skills.clone(),
            )
        })
        .collect();
    if live.is_empty() {
        return;
    }
    let marks = tokio::task::spawn_blocking(move || {
        live.into_iter()
            .filter(|(_, private, list)| crate::skills::restart_needed(&lib, private, list))
            .map(|(i, _, _)| i)
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
    let (def, template) = super::templates::create_body(&state, &actor, body).await?;
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
            def.subscription_pool = agents::check_pool(
                t,
                def.harness,
                def.subscription.as_deref(),
                &def.subscription_pool,
            )?;
            agents::create_from(t, &def, &state_dir, projects::SOURCE_API, template.as_ref())
        })
        .await?;
    // PLAN §9: clone `checkout.on_agent_create` into the workspace before
    // the first start. A failure is a `repo.checkout` event and an entry
    // in `checkout`; the agent stays created.
    let checkout = super::repos::checkout_on_create(&state, &actor, &a).await;
    Ok((StatusCode::CREATED, Json(Created { view: a, checkout })))
}

/// The agent's subscription in `GET /api/agents/{id}`: no secret, no
/// quota numbers (those are in `GET /api/usage/me`).
#[derive(Debug, Serialize)]
struct SubscriptionDetail {
    name: String,
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
    mark_skills_restart(&state, &runs, std::slice::from_mut(&mut view)).await;
    let subscription_detail = match view.agent.subscription.clone() {
        Some(name) => {
            let harness = view.agent.harness;
            state
                .db
                .read(move |c| crate::store::subscriptions::get(c, &name))
                .await
                .ok()
                .map(|s| SubscriptionDetail {
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
                let old = agents::get(t, &id)?.agent.policy;
                agents::check_policy(t, &who, &project, Some(&old), new)?;
            }
            let mut patch = patch;
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
                    "policy": patch.policy,
                    "restart": patch.restart,
                }),
            )?;
            agents::get(t, &id)
        })
        .await?;
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
    Ok(StatusCode::NO_CONTENT.into_response())
}

pub(super) async fn start(
    State(state): State<Shared>,
    actor: Actor,
    Path(id): Path<String>,
) -> ApiResult<(StatusCode, Json<AgentView>)> {
    let a = load(&state, &actor, id).await?;
    policy::admin_project(&actor, &a.agent.project, "starting an agent")?;
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
    let resume = state
        .db
        .write(actor.label(), move |t| {
            agents::begin_session(t, &id, "superseded")?;
            t.audit("agent.start", &id, Some(&project), json!({}))?;
            agents::last_harness_session(t, &id)
        })
        .await?;
    // A start is a retry: a usage-limit hold ends (`crate::usage_limit`).
    crate::usage_limit::release_all(&state, &a.agent.id.to_string()).await?;
    if let Err(e) = state.supervisor.start(a.agent.clone(), resume).await {
        let id = a.agent.id.to_string();
        state
            .db
            .write("system", move |t| {
                agents::end_session(t, &id, "start_failed")
            })
            .await?;
        return Err(supervisor_error(e));
    }
    let id = a.agent.id.to_string();
    let v = state.db.read(move |c| agents::get(c, &id)).await?;
    Ok((StatusCode::ACCEPTED, Json(v)))
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
            let resume = agents::last_harness_session(t, &id)?;
            agents::begin_session(t, &id, "restarted")?;
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
    state
        .supervisor
        .send_turn(agent_id(&a), text)
        .await
        .map_err(supervisor_error)?;
    let id = a.agent.id.to_string();
    let project = a.agent.project.clone();
    let len = body.text.len();
    state
        .db
        .write(actor.label(), move |t| {
            t.audit("agent.send", &id, Some(&project), json!({"bytes": len}))
        })
        .await?;
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
    for role in [Role::ProjectAdmin, Role::Sysadmin] {
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
