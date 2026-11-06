//! Inbox threads (TER-91, docs/api.md §6.13, [`crate::store::threads`]):
//!
//! - `POST /api/threads` (the operator; agents with `thread:open` come
//!   with TER-93): open a thread.
//! - `GET /api/threads[?status=&scope=]`: the operator and an agent that
//!   reads all (sysadmin, architect) see all; another agent sees the
//!   threads it opened, is attached to, or is the report-to lead of.
//! - `GET /api/threads/{id}`: the thread with its messages and questions.
//! - `POST /api/threads/{id}/messages` (the operator, the attached agent).
//! - `POST /api/threads/{id}/respond` (the operator): the overall response.
//! - `POST /api/threads/{id}/close` (the operator, the attached agent).

use axum::extract::{Path, State};
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use axum::{Json, Router};
use serde::Deserialize;

use super::{ApiJson, ApiQuery, Shared};
use crate::actor::Actor;
use crate::error::{ApiError, ApiResult};
use crate::store::agents;
use crate::store::threads::{self, Filter, Link, NewThread, Thread, ThreadStatus};

pub fn routes() -> Router<Shared> {
    Router::new()
        .route("/threads", get(list).post(open))
        .route("/threads/{id}", get(show))
        .route("/threads/{id}/messages", post(post_message))
        .route("/threads/{id}/respond", post(respond))
        .route("/threads/{id}/close", post(close))
}

/// A thread that the caller may read; else `404`: the operator, an agent
/// that reads all (sysadmin, architect), and the opener, the attached agent
/// and the report-to lead.
fn readable(actor: &Actor, th: Thread) -> ApiResult<Thread> {
    match actor {
        Actor::Operator => Ok(th),
        Actor::Agent(a) if a.reads_all() || th.involves(&a.id) => Ok(th),
        Actor::Agent(_) => Err(ApiError::not_found(format!("no thread `{}`", th.id))),
    }
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
    if !actor.is_operator() {
        return Err(ApiError::forbidden(
            "opening a thread: the operator opens threads; agents get the right `thread:open` \
             with TER-93",
        ));
    }
    let th = state
        .db
        .write(actor.label(), move |t| {
            let scope = b.scope.unwrap_or_default();
            let agent = match b.agent.as_deref().map(str::trim).filter(|a| !a.is_empty()) {
                Some(key) => {
                    let context =
                        (scope != threads::GLOBAL && !scope.is_empty()).then_some(scope.as_str());
                    Some(agents::find(t, key, context)?.agent.id.to_string())
                }
                None => None,
            };
            threads::open(
                t,
                NewThread {
                    topic: b.topic,
                    scope,
                    template: b.template,
                    opened_by: "operator".into(),
                    agent,
                    text: b.text,
                },
            )
        })
        .await?;
    Ok((StatusCode::CREATED, Json(th)).into_response())
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
                    "bad status `{s}`: use waiting_operator, waiting_agent, closed, open, or all"
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
    Ok(Json(state.db.read(move |c| threads::list(c, &f)).await?))
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
            threads::post(t, &th.id, &author, &b.text)
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
        serde_json::from_slice(&body).map_err(|e| ApiError::invalid(e.to_string()))?
    };
    let by = actor.address();
    let label = actor.label();
    Ok(Json(
        state
            .db
            .write(label, move |t| {
                let th = readable(&actor, threads::get_bare(t, &id)?)?;
                if let Actor::Agent(a) = &actor
                    && th.agent.as_deref() != Some(a.id.as_str())
                {
                    return Err(ApiError::forbidden(format!(
                        "closing thread {}: only its attached agent and the operator close it",
                        th.id
                    )));
                }
                threads::close(t, &th.id, &by, b.summary.as_deref(), b.links)
            })
            .await?,
    ))
}
