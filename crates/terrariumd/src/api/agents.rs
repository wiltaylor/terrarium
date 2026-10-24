//! `/api/agents/...`: agent CRUD, lifecycle, turns, sessions, and roles.

use axum::extract::{Path, State};
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use axum::{Json, Router};
use serde::{Deserialize, Serialize};
use serde_json::json;
use terrarium_core::config::AgentDef;
use terrarium_core::{AgentId, Answer, Role};

use super::projects::Confirm;
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
        .route("/agents/{id}/answer", post(answer))
        .route("/agents/{id}/events", get(streams::agent_events))
        .route("/agents/{id}/pty", get(streams::pty))
        .route("/agents/{id}/sessions", get(sessions))
        .route("/agents/{id}/roles", get(roles).put(set_roles))
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
    let rows = state
        .db
        .read(move |c| agents::list(c, project.as_deref(), &page))
        .await?;
    Ok(paged(rows))
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

async fn create(
    State(state): State<Shared>,
    actor: Actor,
    ApiJson(def): ApiJson<AgentDef>,
) -> ApiResult<(StatusCode, Json<Created>)> {
    policy::admin_project(&actor, &def.project, "creating an agent")?;
    for role in &def.roles {
        policy::change_role(&actor, "", &def.project, *role)?;
    }
    let state_dir = state.state_dir.clone();
    let who = actor.clone();
    let a = state
        .db
        .write(actor.label(), move |t| {
            agents::check_policy(t, &who, &def.project, None, &def.policy)?;
            agents::create(t, &def, &state_dir, projects::SOURCE_API)
        })
        .await?;
    // PLAN §9: clone `checkout.on_agent_create` into the workspace before
    // the first start. A failure is a `repo.checkout` event and an entry
    // in `checkout`; the agent stays created.
    let checkout = super::repos::checkout_on_create(&state, &actor, &a).await;
    Ok((StatusCode::CREATED, Json(Created { view: a, checkout })))
}

async fn show(
    State(state): State<Shared>,
    actor: Actor,
    Path(id): Path<String>,
) -> ApiResult<Json<AgentView>> {
    Ok(Json(load(&state, &actor, id).await?))
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
            agents::write_def(t, &id, &patch)?;
            t.audit(
                "agent.update",
                &id,
                Some(&project),
                json!({
                    "harness": patch.harness,
                    "subscription": patch.subscription,
                    "model": patch.model,
                    "prompt": patch.prompt,
                    "policy": patch.policy,
                    "restart": patch.restart,
                }),
            )?;
            agents::get(t, &id)
        })
        .await?;
    Ok(Json(v))
}

async fn remove(
    State(state): State<Shared>,
    actor: Actor,
    Path(id): Path<String>,
    ApiQuery(c): ApiQuery<Confirm>,
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
    if live {
        crate::trash::stop_agent(&state, agent_id(&a))
            .await
            .map_err(supervisor_error)?;
    }
    let key = a.agent.id.to_string();
    state
        .db
        .write(actor.label(), move |t| agents::delete(t, &key))
        .await?;
    // The agent dir goes to the trash, not away: the operator can still
    // read the transcripts (`crate::trash`).
    crate::trash::trash_agent(&state, agent_id(&a)).await;
    Ok(StatusCode::NO_CONTENT.into_response())
}

async fn start(
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

async fn stop(
    State(state): State<Shared>,
    actor: Actor,
    Path(id): Path<String>,
) -> ApiResult<(StatusCode, Json<AgentView>)> {
    let a = load(&state, &actor, id).await?;
    policy::admin_project(&actor, &a.agent.project, "stopping an agent")?;
    let result = state.supervisor.stop(agent_id(&a)).await;
    let id = a.agent.id.to_string();
    let project = a.agent.project.clone();
    match result {
        Ok(()) => {}
        // The store thinks it runs, the supervisor does not: reconcile.
        Err(SupervisorError::NotRunning(_)) if a.agent.status.is_live() => {}
        Err(e) => return Err(supervisor_error(e)),
    }
    let was_live = a.agent.status.is_live();
    let v = state
        .db
        .write(actor.label(), move |t| {
            t.audit("agent.stop", &id, Some(&project), json!({}))?;
            if result.is_err() && was_live {
                crate::apply_status(
                    t,
                    &id,
                    terrarium_core::AgentStatus::Stopped,
                    Some("reconciled"),
                )?;
            }
            agents::get(t, &id)
        })
        .await?;
    Ok((StatusCode::ACCEPTED, Json(v)))
}

async fn restart(
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
    let id = a.agent.id.to_string();
    let v = state.db.read(move |c| agents::get(c, &id)).await?;
    Ok((StatusCode::ACCEPTED, Json(v)))
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct SendBody {
    text: String,
}

async fn send(
    State(state): State<Shared>,
    actor: Actor,
    Path(id): Path<String>,
    ApiJson(body): ApiJson<SendBody>,
) -> ApiResult<(StatusCode, Json<serde_json::Value>)> {
    let a = load(&state, &actor, id).await?;
    policy::admin_project(&actor, &a.agent.project, "typing a turn into an agent")?;
    if body.text.trim().is_empty() {
        return Err(ApiError::invalid("the turn text is empty"));
    }
    state
        .supervisor
        .send_turn(agent_id(&a), body.text.clone())
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

async fn sessions(
    State(state): State<Shared>,
    actor: Actor,
    Path(id): Path<String>,
    ApiQuery(page): ApiQuery<Page>,
) -> ApiResult<Response> {
    let a = load(&state, &actor, id).await?;
    let id = a.agent.id.to_string();
    let rows = state
        .db
        .read(move |c| agents::sessions(c, &id, &page))
        .await?;
    Ok(paged(rows))
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
