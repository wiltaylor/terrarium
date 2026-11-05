//! The Inbox of the operator (PLAN §12e, docs/api.md §6.9).
//!
//! - `GET /api/inbox?status=open|done|all&kind=&project=`: the questions,
//!   the notices, the mail to the operator, and the system items in one
//!   list, critical first, then urgent, then the newest first.
//!   `x-inbox-open`, `x-inbox-urgent`, and `x-inbox-critical` give the
//!   open counts.
//! - `GET /api/inbox/counts?project=`: the open counts of each kind.
//! - `POST /api/inbox/mail/{id}/ack`: acknowledge a mail to the operator,
//!   with an optional reply on its thread.
//! - `POST /api/inbox/asks`: ask an agent a question, or forward a question
//!   of the Inbox to one, as the operator; the thread is watched.
//! - `DELETE /api/inbox/watches/{thread}`: stop a watch.
//!
//! The questions and the notices keep their own verbs (`/api/questions/
//! {id}/answer|ack|retract`), and the system items use
//! `POST /api/mail/{id}/release` and `POST /api/mail/threads/{id}/resume`.
//! Only the operator reads the Inbox: it holds the operator's mail.

use axum::extract::{Path, State};
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use axum::routing::{delete, get, post};
use axum::{Json, Router};
use serde::{Deserialize, Serialize};
use serde_json::json;

use super::{ApiJson, ApiQuery, Shared};
use crate::actor::Actor;
use crate::error::{ApiError, ApiResult};
use crate::policy;
use crate::store::inbox::{self, Counts, Filter, InboxKind, Status, Watch};
use crate::store::mail::{self, Address, Message};

pub fn routes() -> Router<Shared> {
    Router::new()
        .route("/inbox", get(list))
        .route("/inbox/counts", get(counts))
        .route("/inbox/mail/{id}/ack", post(ack_mail))
        .route("/inbox/asks", post(ask))
        .route("/inbox/watches/{thread}", delete(cancel_watch))
}

fn operator(actor: &Actor) -> ApiResult<()> {
    policy::operator_only(actor, "the Inbox").map_err(|_| {
        ApiError::forbidden(
            "the Inbox is the operator's; an agent lists its own questions and notices with \
             `terrarium question ls`",
        )
    })
}

#[derive(Debug, Default, Deserialize)]
struct ListQuery {
    #[serde(default)]
    status: Option<String>,
    #[serde(default)]
    kind: Option<String>,
    #[serde(default)]
    project: Option<String>,
}

/// `GET /api/inbox`.
async fn list(
    State(state): State<Shared>,
    actor: Actor,
    ApiQuery(q): ApiQuery<ListQuery>,
) -> ApiResult<Response> {
    operator(&actor)?;
    let f = Filter {
        status: Status::parse(q.status.as_deref().unwrap_or_default())?,
        kind: q
            .kind
            .as_deref()
            .filter(|k| !k.trim().is_empty())
            .map(InboxKind::parse)
            .transpose()?,
        project: q.project.filter(|p| !p.trim().is_empty()),
    };
    let project = f.project.clone();
    let (rows, c) = state
        .db
        .read(move |c| Ok((inbox::list(c, &f)?, inbox::counts(c, project.as_deref())?)))
        .await?;
    let mut resp = Json(rows).into_response();
    for (k, v) in [
        ("x-inbox-open", c.open),
        ("x-inbox-urgent", c.urgent),
        ("x-inbox-critical", c.critical),
    ] {
        if let Ok(v) = v.to_string().parse() {
            resp.headers_mut().insert(k, v);
        }
    }
    Ok(resp)
}

#[derive(Debug, Default, Deserialize)]
struct CountsQuery {
    #[serde(default)]
    project: Option<String>,
}

/// `GET /api/inbox/counts`.
async fn counts(
    State(state): State<Shared>,
    actor: Actor,
    ApiQuery(q): ApiQuery<CountsQuery>,
) -> ApiResult<Json<Counts>> {
    operator(&actor)?;
    let project = q.project.filter(|p| !p.trim().is_empty());
    Ok(Json(
        state
            .db
            .read(move |c| inbox::counts(c, project.as_deref()))
            .await?,
    ))
}

/// The body of `POST /api/inbox/mail/{id}/ack`.
#[derive(Debug, Default, Deserialize)]
#[serde(deny_unknown_fields)]
struct AckBody {
    /// A reply, as `operator`, on the thread of the mail.
    #[serde(default)]
    reply: Option<String>,
}

/// The answer of `POST /api/inbox/mail/{id}/ack`.
#[derive(Debug, Serialize)]
struct Acked {
    message: Message,
    replies: Vec<Message>,
}

/// `POST /api/inbox/mail/{id}/ack`: the mail leaves the open list. With a
/// reply, the reply goes as `operator` to the other participants of the
/// thread. A mail that is not to the operator is not found; one that is
/// acknowledged already is `409`.
async fn ack_mail(
    State(state): State<Shared>,
    actor: Actor,
    Path(id): Path<String>,
    body: axum::body::Bytes,
) -> ApiResult<Json<Acked>> {
    operator(&actor)?;
    let b: AckBody = if body.iter().all(u8::is_ascii_whitespace) {
        AckBody::default()
    } else {
        serde_json::from_slice(&body).map_err(|e| ApiError::bad_request(e.to_string()))?
    };
    let reply = b
        .reply
        .map(|r| r.trim().to_owned())
        .filter(|r| !r.is_empty());
    let out = state
        .db
        .write(actor.label(), move |t| {
            let m = mail::get(t, &id)?;
            if m.to_kind != "operator" {
                return Err(ApiError::not_found(format!(
                    "no mail `{id}` to the operator"
                )));
            }
            if !matches!(m.state.as_str(), "queued" | "delivered") {
                return Err(ApiError::conflict(format!(
                    "mail {id} is acknowledged already ({})",
                    m.state
                ))
                .with_details(json!({"state": m.state})));
            }
            let replies = match &reply {
                Some(body) => mail::reply(t, &m, &Address::Operator, None, body)?,
                None => {
                    mail::mark_read(t, &m.id)?;
                    Vec::new()
                }
            };
            let project = m
                .from_name
                .split_once('/')
                .map(|(p, _)| p.to_owned())
                .or_else(|| m.from.strip_prefix("project:").map(str::to_owned));
            t.audit(
                "inbox.ack",
                &m.id,
                project.as_deref(),
                json!({"from": m.from, "thread": m.thread_id, "reply": replies.len()}),
            )?;
            inbox::emit(
                t,
                &format!("mail:{}", m.id),
                "mail",
                "closed",
                project.as_deref(),
            )?;
            Ok(Acked {
                message: mail::get(t, &m.id)?,
                replies,
            })
        })
        .await?;
    Ok(Json(out))
}

/// The body of `POST /api/inbox/asks`.
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct AskBody {
    /// The agent: a ULID, `project/name`, or a name.
    to: String,
    /// The question; with `question`, an optional note.
    #[serde(default)]
    text: String,
    /// The question, bundle, or notice to forward.
    #[serde(default)]
    question: Option<String>,
}

/// The answer of `POST /api/inbox/asks`.
#[derive(Debug, Serialize)]
struct Asked {
    message: Message,
    watch: Watch,
}

/// The most characters of an ask or a note.
const ASK_MAX: usize = 4000;

/// `POST /api/inbox/asks`: one mail from `operator` to the agent, on a new
/// thread that the Inbox watches until the agent answers.
async fn ask(
    State(state): State<Shared>,
    actor: Actor,
    ApiJson(b): ApiJson<AskBody>,
) -> ApiResult<(StatusCode, Json<Asked>)> {
    operator(&actor)?;
    if b.text.chars().count() > ASK_MAX {
        return Err(ApiError::invalid(format!(
            "the text has more than {ASK_MAX} characters"
        )));
    }
    let question = b
        .question
        .map(|q| q.trim().to_owned())
        .filter(|q| !q.is_empty());
    let out = state
        .db
        .write(actor.label(), move |t| {
            let (message, watch) = inbox::ask(t, &b.to, &b.text, question.as_deref())?;
            t.audit(
                "inbox.ask",
                &watch.thread_id,
                watch.agent_name.split_once('/').map(|(p, _)| p),
                json!({"to": watch.agent, "question": watch.question_id, "message": message.id}),
            )?;
            Ok(Asked { message, watch })
        })
        .await?;
    Ok((StatusCode::CREATED, Json(out)))
}

/// `DELETE /api/inbox/watches/{thread}`: the Inbox stops waiting for the
/// agent's answer. The mail stays.
async fn cancel_watch(
    State(state): State<Shared>,
    actor: Actor,
    Path(thread): Path<String>,
) -> ApiResult<Json<Watch>> {
    operator(&actor)?;
    let w = state
        .db
        .write(actor.label(), move |t| {
            let w = inbox::cancel_watch(t, &thread)?;
            t.audit(
                "inbox.watch_cancel",
                &w.thread_id,
                w.agent_name.split_once('/').map(|(p, _)| p),
                json!({"agent": w.agent}),
            )?;
            Ok(w)
        })
        .await?;
    Ok(Json(w))
}
