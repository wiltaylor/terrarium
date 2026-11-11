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
//! - `POST /api/threads/{id}/status` (the attached agent): set
//!   `waiting_agent` or `tracking` without a message (GitHub #28). A
//!   message of the agent can set them too (`status` in the body).
//! - `POST /api/threads/{id}/cards`: link a card of any project (TER-146,
//!   TER-144 D2). A reader of the thread who reads the card's project.
//!   A card linked while the thread waits on the operator only because of
//!   the agent's last message moves the thread into tracking (GitHub #28).

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
        .route("/threads/{id}/status", post(set_status))
        .route("/threads/{id}/cards", post(link_card))
        .route(
            "/projects/{project}/version-thread/messages",
            post(post_version),
        )
        .route("/threads/{id}/reopen", post(reopen))
        .route("/threads/{id}/swap", post(swap))
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct VersionPostBody {
    version: String,
    changed: String,
    test: String,
}

async fn post_version(
    State(state): State<Shared>,
    actor: Actor,
    Path(project): Path<String>,
    ApiJson(b): ApiJson<VersionPostBody>,
) -> ApiResult<Response> {
    policy::admin_project(&actor, &project, "posting a version update")?;
    let th = state
        .db
        .write(actor.label(), move |t| {
            threads::version_post(t, &project, &b.version, &b.changed, &b.test)
        })
        .await?;
    Ok((StatusCode::CREATED, Json(th)).into_response())
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct OpenBody {
    /// A request reports to the operator's selected project lead.
    #[serde(default)]
    report_to: Option<String>,
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
    if b.report_to.is_some() {
        crate::policy::operator_only(&actor, "opening a request")?;
    }
    let opener = actor.agent().cloned();
    if opener.is_some() {
        crate::template_files::refresh_quiet(&state).await;
    }
    let th = state
        .db
        .write(actor.label(), move |t| {
            let lead = b
                .report_to
                .as_deref()
                .map(|key| {
                    let a = agents::find(t, key, None)?.agent;
                    if !a.roles.contains(&terrarium_core::Role::ProjectAdmin) {
                        return Err(ApiError::invalid("report_to must be a project lead"));
                    }
                    Ok(a)
                })
                .transpose()?;
            let scope = lead.as_ref().map_or(scope, |a| a.project.clone());
            if lead.is_some() && (b.agent.is_some() || b.template.is_some()) {
                return Err(ApiError::invalid(
                    "a request uses global:request and no named agent",
                ));
            }
            let project = (scope != threads::GLOBAL).then_some(scope.as_str());
            let agent = match b.agent.as_deref().map(str::trim).filter(|a| !a.is_empty()) {
                Some(key) => Some(agents::find(t, key, project)?.agent),
                None => None,
            };
            if let Some(a) = &opener {
                check_rights(t, a, &scope, b.template.as_deref(), agent.as_ref())?;
            }
            let th = threads::open(
                t,
                NewThread {
                    topic: b.topic,
                    scope,
                    template: if lead.is_some() {
                        Some("global:request".into())
                    } else {
                        b.template
                    },
                    opened_by: opener.map_or_else(|| "operator".into(), |a| a.id),
                    agent: agent.map(|a| a.id.to_string()),
                    text: b.text,
                },
            )?;
            if let Some(lead) = lead {
                t.execute(
                    "UPDATE threads SET report_to = ?2, request = 1 WHERE id = ?1",
                    rusqlite::params![th.id, lead.id.to_string()],
                )?;
            }
            threads::get(t, &th.id)
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

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct PostBody {
    text: String,
    /// The attached agent's status after the message (GitHub #28):
    /// `waiting_agent` or `tracking`.
    #[serde(default)]
    status: Option<String>,
}

/// A status of a body: `waiting_agent` or `tracking` for the agent.
fn agent_status(s: &str) -> ApiResult<ThreadStatus> {
    match ThreadStatus::parse(s) {
        Some(st @ (ThreadStatus::WaitingAgent | ThreadStatus::Tracking)) => Ok(st),
        _ => Err(ApiError::invalid(format!(
            "bad status `{s}`: the thread's agent sets waiting_agent or tracking"
        ))),
    }
}

async fn post_message(
    State(state): State<Shared>,
    actor: Actor,
    Path(id): Path<String>,
    ApiJson(b): ApiJson<PostBody>,
) -> ApiResult<Response> {
    let want = b.status.as_deref().map(agent_status).transpose()?;
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
            threads::post(t, &th.id, &author, &b.text, want).map(|th| threads::visible(&actor, th))
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
    crate::policy::operator_only(&actor, "responding to a thread")?;
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
    if b.confirm || matches!(actor, Actor::Operator) {
        // Refuse link provenance before looking up the thread, but keep the
        // existing not-found/conflict precedence for ordinary agent calls.
        crate::links::operator_only("confirming a thread close")?;
    }
    let by = actor.address();
    let label = actor.label();
    Ok(Json(
        state
            .db
            .write(label, move |t| {
                let th = readable(&actor, threads::get_bare(t, &id)?)?;
                threads::check_close(&th)?;
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
struct StatusBody {
    /// `waiting_agent` or `tracking`.
    status: String,
}

/// The attached agent or the operator sets the status of the thread
/// without a message (GitHub #28, TER-456): `waiting_agent` (the agent
/// works on) or `tracking` (as `/track`). An open question refuses it
/// (`409`).
async fn set_status(
    State(state): State<Shared>,
    actor: Actor,
    Path(id): Path<String>,
    ApiJson(b): ApiJson<StatusBody>,
) -> ApiResult<Json<Thread>> {
    let status = agent_status(&b.status)?;
    let by = actor.address();
    let label = actor.label();
    Ok(Json(
        state
            .db
            .write(label, move |t| {
                let th = readable(&actor, threads::get_bare(t, &id)?)?;
                match &actor {
                    Actor::Agent(a) if th.agent.as_deref() == Some(a.id.as_str()) => {
                        threads::set_by_agent(t, &th.id, &by, status)
                            .map(|th| threads::visible(&actor, th))
                    }
                    Actor::Operator => threads::set_by_agent(t, &th.id, &by, status)
                        .map(|th| threads::visible(&actor, th)),
                    _ => Err(ApiError::forbidden(format!(
                        "setting the status of thread {}: only its attached agent or the \
                         operator sets it",
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
            .write(label, move |t| {
                let th = threads::link_card(t, &actor, &id, &b.card)?;
                // A card linked after the agent's hand-off message moves
                // the thread into tracking (GitHub #28).
                threads::track_on_link(t, &th.id)?;
                Ok(threads::visible(&actor, threads::get(t, &th.id)?))
            })
            .await?,
    ))
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct RestartBody {
    reason: String,
    /// Atomically link a new related card when reopening.
    #[serde(default)]
    card: Option<String>,
    #[serde(default)]
    template: Option<String>,
}

async fn reopen(
    State(state): State<Shared>,
    actor: Actor,
    Path(id): Path<String>,
    ApiJson(b): ApiJson<RestartBody>,
) -> ApiResult<Json<Thread>> {
    if b.template.is_some() {
        return Err(ApiError::invalid("reopen uses a new global:request agent"));
    }
    restart(&state, actor, id, b, true).await.map(Json)
}

async fn swap(
    State(state): State<Shared>,
    actor: Actor,
    Path(id): Path<String>,
    ApiJson(b): ApiJson<RestartBody>,
) -> ApiResult<Json<Thread>> {
    restart(&state, actor, id, b, false).await.map(Json)
}

async fn restart(
    state: &Shared,
    actor: Actor,
    id: String,
    b: RestartBody,
    reopening: bool,
) -> ApiResult<Thread> {
    if !reopening && b.card.is_some() {
        return Err(ApiError::invalid("card is only accepted when reopening"));
    }
    if b.reason.trim().is_empty()
        || b.reason.chars().count() > threads::MAX_MESSAGE
        || b.reason.contains('\0')
    {
        return Err(ApiError::invalid(
            "reason is required and must fit a thread message",
        ));
    }
    let template = if reopening {
        "global:request".to_owned()
    } else {
        b.template
            .ok_or_else(|| ApiError::invalid("swap requires an architect template"))?
    };
    crate::template_files::refresh_quiet(state).await;
    let card = b.card.as_deref().map(crate::store::board::normalise_id);
    let (key, who, tp, linked) = (id.clone(), actor.clone(), template.clone(), card.clone());
    let (old, epoch) = state.db.read(move |c| {
        let th = threads::get(c, &key)?;
        if !th.request || (th.status == ThreadStatus::Closed) != reopening {
            return Err(ApiError::conflict("only a closed request reopens, and only an open request swaps"));
        }
        let new_card_project = linked.as_deref().map(|id| {
            let card = crate::store::board::get_card(c, id)?;
            policy::read_project(&who, &card.project)?;
            if card.archived { return Err(ApiError::invalid("an archived card cannot reopen a request")); }
            Ok(card.project)
        }).transpose()?;
        let allowed = match &who {
            Actor::Operator => true,
            Actor::Agent(a) => {
                a.roles.contains(&terrarium_core::Role::ProjectAdmin) &&
                (th.report_to.as_deref() == Some(a.id.as_str()) || (reopening &&
                    (new_card_project.as_deref() == Some(a.project.as_str()) || th.cards.as_deref().unwrap_or_default().iter().any(|card| card.project == a.project))))
            }
        };
        if !allowed { return Err(ApiError::forbidden("only the report-to lead or a lead with a linked card reopens a request; only the report-to lead swaps it")); }
        let project = crate::thread_agent::project_of(&th);
        let resolved = templates::resolve(c, project, &tp)?;
        if !reopening {
            let def = agent_template::to_def(&resolved.spec, &resolved.name, project).map_err(|e| ApiError::invalid(e.to_string()))?;
            crate::thread_agent::check_architect_template(&resolved.use_key(), &def)?;
        }
        let epoch = c.query_row("SELECT request_epoch FROM threads WHERE id = ?1", [&key], |r| r.get::<_, i64>(0))?;
        Ok((th.agent, epoch))
    }).await?;
    // Finish the previous bound spawn before making the new one. A delayed
    // close event must only close its original agent, never this replacement.
    if let Some(old) = old {
        let key = old.clone();
        let running = state
            .db
            .read(move |c| Ok(crate::store::spawns::get(c, &key).is_ok_and(|s| s.running())))
            .await?;
        if running {
            crate::spawn::close(
                state,
                &old,
                crate::store::spawns::DONE,
                Some("request agent replaced".into()),
                false,
            )
            .await?;
        }
    }
    let by = actor.address();
    let th = state
        .db
        .write(actor.label(), move |t| {
            threads::restart_request(
                t,
                &id,
                &by,
                &b.reason,
                &template,
                reopening,
                (epoch, card.as_deref()),
            )
        })
        .await?;
    crate::thread_agent::start(state, th, super::skills::library_opt(state))
        .await
        .map(|th| threads::visible(&actor, th))
}
