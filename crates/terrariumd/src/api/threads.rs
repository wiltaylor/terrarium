//! Inbox threads (TER-91, docs/api.md §6.13, [`crate::store::threads`]):
//!
//! - `POST /api/threads`: open a thread. The operator, or an agent with
//!   the right `thread:open` (TER-93) with a template that does not exceed
//!   its own rights (A1). With a template and no named agent, the daemon
//!   spawns the thread's agent ([`crate::thread_agent`], TER-92).
//! - `GET /api/threads[?status=&scope=]`: the operator and an agent that
//!   reads all (sysadmin, architect) see all; another agent sees the
//!   threads it opened, is attached to, or is the report-to lead of.
//! - `GET /api/threads/{id}`: the thread with its messages and questions,
//!   and the linked cards whose project the reader may read (TER-146).
//! - `POST /api/threads/{id}/messages` (the operator, the attached agent).
//! - `POST /api/threads/{id}/respond` (the operator): the overall response.
//! - `POST /api/threads/{id}/close`: the operator closes (or, with
//!   `confirm`, confirms the agent's proposal); the attached agent
//!   proposes the close (TER-95).
//! - `POST /api/threads/{id}/track` (the attached agent): move the thread
//!   into tracking after the handoff (TER-145), and link the `cards` of
//!   the body first (TER-146).
//! - `POST /api/threads/{id}/cards`: link a card of any project (TER-146,
//!   TER-144 D2). A reader of the thread who reads the card's project.

use axum::extract::{Path, State};
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use axum::{Json, Router};
use serde::Deserialize;

use super::{ApiJson, ApiQuery, Shared};
use terrarium_core::agent_template;
use terrarium_core::model::Agent;

use crate::actor::{Actor, AgentActor};
use crate::error::{ApiError, ApiResult};
use crate::policy;
use crate::store::threads::{self, Filter, Link, NewThread, Thread, ThreadStatus, readable};
use crate::store::{agents, templates};

pub fn routes() -> Router<Shared> {
    Router::new()
        .route("/threads", get(list).post(open))
        .route("/threads/{id}", get(show))
        .route("/threads/{id}/messages", post(post_message))
        .route("/threads/{id}/respond", post(respond))
        .route("/threads/{id}/close", post(close))
        .route("/threads/{id}/track", post(track))
        .route("/threads/{id}/cards", post(link_card))
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct OpenBody {
    topic: String,
    #[serde(default)]
    scope: Option<String>,
    #[serde(default)]
    template: Option<String>,
    /// The attached agent: a ULID or `project/name`.
    #[serde(default)]
    agent: Option<String>,
    /// The first message.
    #[serde(default)]
    text: Option<String>,
}

async fn open(
    State(state): State<Shared>,
    actor: Actor,
    ApiJson(b): ApiJson<OpenBody>,
) -> ApiResult<Response> {
    // The scope: `global` by default for the operator, the own project
    // for an agent.
    let scope = match b.scope.as_deref().map(str::trim) {
        Some(s) if !s.is_empty() => s.to_owned(),
        _ => match actor.project() {
            Some(p) => p.to_owned(),
            None => threads::GLOBAL.to_owned(),
        },
    };
    policy::open_thread(&actor, &scope)?;
    let opener = actor.agent().cloned();
    if opener.is_some() {
        crate::template_files::refresh_quiet(&state).await;
    }
    let th = state
        .db
        .write(actor.label(), move |t| {
            let project = (scope != threads::GLOBAL).then_some(scope.as_str());
            let agent = match b.agent.as_deref().map(str::trim).filter(|a| !a.is_empty()) {
                Some(key) => Some(agents::find(t, key, project)?.agent),
                None => None,
            };
            if let Some(a) = &opener {
                check_rights(t, a, &scope, b.template.as_deref(), agent.as_ref())?;
            }
            threads::open(
                t,
                NewThread {
                    topic: b.topic,
                    scope,
                    template: b.template,
                    opened_by: opener.map_or_else(|| "operator".into(), |a| a.id),
                    agent: agent.map(|a| a.id.to_string()),
                    text: b.text,
                },
            )
        })
        .await?;
    // A template and no named agent: the daemon spawns the thread's agent
    // now (TER-92), and checks A1 again against the template as it is then.
    let th = if th.template.is_some() && th.agent.is_none() {
        let lib = super::skills::library_opt(&state);
        crate::thread_agent::start(&state, th, lib).await?
    } else {
        th
    };
    Ok((StatusCode::CREATED, Json(th)).into_response())
}

/// The rights check of an agent-opened thread (A1, [`policy::thread_rights`]):
/// the template is required, and neither it nor a named attached agent
/// may hold a right that the opener does not hold. The template resolves
/// in the project of the thread's agent: the scope project, or `meta` for
/// `global`.
fn check_rights(
    conn: &rusqlite::Connection,
    opener: &AgentActor,
    scope: &str,
    template: Option<&str>,
    agent: Option<&Agent>,
) -> ApiResult<()> {
    let Some(key) = template.map(str::trim).filter(|k| !k.is_empty()) else {
        return Err(ApiError::invalid(
            "`template` is empty: an agent names the template of the thread it opens \
             (`terrarium thread new --template <t>`)",
        ));
    };
    let project = if scope == threads::GLOBAL {
        threads::GLOBAL_LEAD_PROJECT
    } else {
        scope
    };
    let tp = templates::resolve(conn, project, key)?;
    let def = agent_template::to_def(&tp.spec, &tp.name, project)
        .map_err(|e| ApiError::invalid(format!("template `{}`: {e}", tp.use_key())))?;
    let refuse = |what: String, excess: Vec<String>| -> ApiResult<()> {
        if excess.is_empty() {
            return Ok(());
        }
        Err(ApiError::forbidden(format!(
            "opening a thread: {what} has {}, which you do not hold. The template of a thread \
             that an agent opens may not exceed the opener's rights (A1); the operator opens \
             it from the Inbox",
            excess.join(", ")
        )))
    };
    refuse(
        format!("template `{}`", tp.use_key()),
        policy::thread_rights(
            opener,
            policy::ThreadAgentRights {
                project,
                roles: &def.roles,
                policy: &def.policy,
            },
            &|n: &str| agents::project_target(conn, project, n),
        ),
    )?;
    if let Some(a) = agent {
        refuse(
            format!("agent {}/{}", a.project, a.name),
            policy::thread_rights(
                opener,
                policy::ThreadAgentRights {
                    project: &a.project,
                    roles: &a.roles,
                    policy: &a.policy,
                },
                &|n: &str| agents::project_target(conn, &a.project, n),
            ),
        )?;
    }
    Ok(())
}

#[derive(Debug, Default, Deserialize)]
struct ListQuery {
    #[serde(default)]
    status: Option<String>,
    #[serde(default)]
    scope: Option<String>,
}

async fn list(
    State(state): State<Shared>,
    actor: Actor,
    ApiQuery(q): ApiQuery<ListQuery>,
) -> ApiResult<Json<Vec<Thread>>> {
    let (status, all) = match q.status.as_deref().map(str::trim) {
        None | Some("") | Some("open") => (None, false),
        Some("all") => (None, true),
        Some(s) => (
            Some(ThreadStatus::parse(s).ok_or_else(|| {
                ApiError::invalid(format!(
                    "bad status `{s}`: use waiting_operator, waiting_agent, tracking, closed, open, or all"
                ))
            })?),
            false,
        ),
    };
    let f = Filter {
        status,
        all,
        scope: q.scope.filter(|s| !s.trim().is_empty()),
        involving: actor
            .agent()
            .filter(|a| !a.reads_all())
            .map(|a| a.id.clone()),
    };
    Ok(Json(
        state
            .db
            .read(move |c| threads::list_for(c, &f, &actor))
            .await?,
    ))
}

async fn show(
    State(state): State<Shared>,
    actor: Actor,
    Path(id): Path<String>,
) -> ApiResult<Json<Thread>> {
    let th = state.db.read(move |c| threads::get(c, &id)).await?;
    Ok(Json(readable(&actor, th)?))
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct TextBody {
    text: String,
}

async fn post_message(
    State(state): State<Shared>,
    actor: Actor,
    Path(id): Path<String>,
    ApiJson(b): ApiJson<TextBody>,
) -> ApiResult<Response> {
    let author = actor.address();
    let label = actor.label();
    let th = state
        .db
        .write(label, move |t| {
            let th = readable(&actor, threads::get_bare(t, &id)?)?;
            if let Actor::Agent(a) = &actor
                && th.agent.as_deref() != Some(a.id.as_str())
            {
                return Err(ApiError::forbidden(format!(
                    "posting to thread {}: only its attached agent and the operator post; the \
                     report-to lead reads it",
                    th.id
                )));
            }
            threads::post(t, &th.id, &author, &b.text).map(|th| threads::visible(&actor, th))
        })
        .await?;
    Ok((StatusCode::CREATED, Json(th)).into_response())
}

async fn respond(
    State(state): State<Shared>,
    actor: Actor,
    Path(id): Path<String>,
    ApiJson(b): ApiJson<TextBody>,
) -> ApiResult<Json<Thread>> {
    if !actor.is_operator() {
        return Err(ApiError::forbidden(
            "responding to a thread: the overall response is the operator's",
        ));
    }
    Ok(Json(
        state
            .db
            .write(actor.label(), move |t| threads::respond(t, &id, &b.text))
            .await?,
    ))
}

#[derive(Debug, Default, Deserialize)]
#[serde(deny_unknown_fields)]
struct CloseBody {
    #[serde(default)]
    summary: Option<String>,
    #[serde(default)]
    links: Vec<Link>,
    /// The operator confirms the agent's close proposal (TER-95).
    #[serde(default)]
    confirm: bool,
}

async fn close(
    State(state): State<Shared>,
    actor: Actor,
    Path(id): Path<String>,
    body: axum::body::Bytes,
) -> ApiResult<Json<Thread>> {
    // The body is optional: no body is a close with no handoff.
    let b: CloseBody = if body.iter().all(u8::is_ascii_whitespace) {
        CloseBody::default()
    } else {
        serde_json::from_slice(&body).map_err(|e| ApiError::bad_request(e.to_string()))?
    };
    let by = actor.address();
    let label = actor.label();
    Ok(Json(
        state
            .db
            .write(label, move |t| {
                let th = readable(&actor, threads::get_bare(t, &id)?)?;
                match &actor {
                    Actor::Agent(a) if th.agent.as_deref() != Some(a.id.as_str()) => {
                        Err(ApiError::forbidden(format!(
                            "closing thread {}: only its attached agent and the operator close it",
                            th.id
                        )))
                    }
                    // The agent's close is a proposal that the operator
                    // confirms (TER-95).
                    Actor::Agent(_) if b.confirm => Err(ApiError::forbidden(format!(
                        "confirming the close of thread {}: the operator confirms",
                        th.id
                    ))),
                    Actor::Agent(_) => {
                        threads::propose_close(t, &th.id, &by, b.summary.as_deref(), b.links)
                    }
                    Actor::Operator if b.confirm => {
                        if b.summary.is_some() || !b.links.is_empty() {
                            return Err(ApiError::invalid(
                                "`confirm` closes with the proposed handoff: send no `summary` \
                                 or `links` with it",
                            ));
                        }
                        threads::confirm_close(t, &th.id)
                    }
                    Actor::Operator => threads::close(
                        t,
                        &th.id,
                        &by,
                        threads::CloseReason::Operator,
                        b.summary.as_deref(),
                        b.links,
                    ),
                }
                .map(|th| threads::visible(&actor, th))
            })
            .await?,
    ))
}

#[derive(Debug, Default, Deserialize)]
#[serde(deny_unknown_fields)]
struct TrackBody {
    monitor: Option<String>,
    /// Cards to link first (TER-146): `<project>:<card>` or `<card>`.
    #[serde(default)]
    cards: Vec<String>,
}

/// The attached agent moves its thread into tracking after the handoff
/// (TER-145, TER-144 D1). The body is optional: `{"cards"}` links those
/// cards in the same write (TER-146, D2); one refused card refuses all.
async fn track(
    State(state): State<Shared>,
    actor: Actor,
    Path(id): Path<String>,
    body: axum::body::Bytes,
) -> ApiResult<Json<Thread>> {
    let b: TrackBody = if body.iter().all(u8::is_ascii_whitespace) {
        TrackBody::default()
    } else {
        serde_json::from_slice(&body).map_err(|e| ApiError::bad_request(e.to_string()))?
    };
    if b.cards.len() > threads::MAX_LINKS {
        return Err(ApiError::invalid(format!(
            "`cards` has {} cards; the most is {}",
            b.cards.len(),
            threads::MAX_LINKS
        )));
    }
    let by = actor.address();
    let label = actor.label();
    Ok(Json(
        state
            .db
            .write(label, move |t| {
                let th = readable(&actor, threads::get_bare(t, &id)?)?;
                match &actor {
                    Actor::Agent(a) if th.agent.as_deref() == Some(a.id.as_str()) => {
                        for c in &b.cards {
                            threads::link_card(t, &actor, &th.id, c)?;
                        }
                        if let Some(monitor) = &b.monitor {
                            threads::link_monitor(t, &actor, &th.id, monitor)?;
                        }
                        threads::track(t, &th.id, &by).map(|th| threads::visible(&actor, th))
                    }
                    _ => Err(ApiError::forbidden(format!(
                        "tracking thread {}: only its attached agent moves it into tracking",
                        th.id
                    ))),
                }
            })
            .await?,
    ))
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct CardBody {
    /// `<project>:<card>` or `<card>`.
    card: String,
}

/// Link a card of any project to the thread (TER-146, TER-144 D2): the
/// lead after `card new`, or any reader of the thread who reads the card's
/// project. `200` the thread with its `cards`, also when the card was
/// linked already.
async fn link_card(
    State(state): State<Shared>,
    actor: Actor,
    Path(id): Path<String>,
    ApiJson(b): ApiJson<CardBody>,
) -> ApiResult<Json<Thread>> {
    let label = actor.label();
    Ok(Json(
        state
            .db
            .write(label, move |t| threads::link_card(t, &actor, &id, &b.card))
            .await?,
    ))
}
