//! `/api/mail/...` and the project inboxes, `/api/projects/{name}/inbox/...`.

use axum::extract::{Path, State};
use axum::http::StatusCode;
use axum::response::Response;
use axum::routing::{get, post};
use axum::{Json, Router};
use serde::Deserialize;

use super::longpoll::{self, WaitQuery};
use super::{ApiJson, ApiQuery, Shared, paged};
use crate::actor::Actor;
use crate::db::Tx;
use crate::error::{ApiError, ApiResult};
use crate::policy;
use crate::store::Page;
use crate::store::board::may_read_message;
use crate::store::mail::{self, Address, MailFilter, Message, Outgoing};

pub fn routes() -> Router<Shared> {
    Router::new()
        .route("/mail", get(list).post(send))
        .route("/mail/{id}", get(show))
        .route("/mail/{id}/read", post(read))
        .route("/mail/{id}/reply", post(reply))
        .route("/projects/{name}/inbox", get(inbox_list).post(inbox_send))
        .route("/projects/{name}/inbox/{id}", get(inbox_show))
        .route("/projects/{name}/inbox/{id}/read", post(inbox_read))
        .route("/projects/{name}/inbox/{id}/reply", post(inbox_reply))
}

#[derive(Debug, Default, Clone, Deserialize)]
struct ListQuery {
    to: Option<String>,
    from: Option<String>,
    thread: Option<String>,
    state: Option<String>,
}

async fn list(
    State(state): State<Shared>,
    actor: Actor,
    ApiQuery(page): ApiQuery<Page>,
    ApiQuery(q): ApiQuery<ListQuery>,
    ApiQuery(wait): ApiQuery<WaitQuery>,
) -> ApiResult<Response> {
    let rows = longpoll::hold(&state, wait, &["mail"], || {
        let (actor, page, q) = (actor.clone(), page.clone(), q.clone());
        state.db.read(move |c| {
            let context = actor.project();
            let key = |s: &Option<String>| -> ApiResult<Option<String>> {
                s.as_deref()
                    .map(|s| mail::resolve_known(c, s, context).map(|a| a.key()))
                    .transpose()
            };
            let mut f = MailFilter {
                to: key(&q.to)?,
                from: key(&q.from)?,
                thread: q.thread.clone(),
                state: q.state.clone(),
                involving: None,
            };
            if let Actor::Agent(a) = &actor {
                match f.to.as_deref().and_then(|t| t.strip_prefix("project:")) {
                    Some(p) => policy::read_inbox(&actor, p)?,
                    None => f.involving = Some(a.id.clone()),
                }
            }
            mail::list(c, &f, &page)
        })
    })
    .await?;
    Ok(paged(rows))
}

/// The body of `POST /api/mail`.
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct NewMail {
    #[serde(default)]
    from: Option<String>,
    to: String,
    #[serde(default)]
    subject: Option<String>,
    body: String,
    #[serde(default)]
    thread_id: Option<String>,
}

/// The sender address for a new message from `actor`. The operator may
/// send as `operator` or as a project inbox. An agent sends as itself, or
/// as the inbox of a project it administers.
fn sender(t: &Tx<'_>, actor: &Actor, from: Option<&str>) -> ApiResult<Address> {
    let me = Address::from_actor(actor);
    let Some(from) = from.filter(|f| !f.is_empty()) else {
        return Ok(me);
    };
    let wanted = mail::resolve(t, from, actor.project())?;
    if wanted.key() == me.key() {
        return Ok(me);
    }
    match (&wanted, actor) {
        (Address::Project(p), _) if policy::is_admin_of(actor, p) => Ok(wanted),
        (Address::Project(p), _) => Err(ApiError::forbidden(format!(
            "sending as project:{p} needs project_admin of `{p}`, sysadmin, or the operator"
        ))),
        _ => Err(ApiError::forbidden(format!(
            "you cannot send as `{}`",
            wanted.display()
        ))),
    }
}

/// Write a message after the §12 scope checks.
pub(super) fn send_checked(
    t: &mut Tx<'_>,
    actor: &Actor,
    from: &Address,
    to: &Address,
    subject: &str,
    body: &str,
    thread: Option<&str>,
) -> ApiResult<Message> {
    if let Some(th) = thread.filter(|th| mail::thread_exists(t, th).unwrap_or(false)) {
        // An existing thread: only its participants post on it.
        if !actor.is_operator() && !mail::is_participant(t, th, from)? {
            return Err(ApiError::forbidden(format!(
                "you are not a participant of thread {th}"
            )));
        }
    }
    if let (Actor::Agent(a), Address::Agent { .. }) = (actor, from) {
        // A reply to a thread participant is allowed; threads span projects.
        let in_thread = match thread {
            Some(th) => mail::is_participant(t, th, to)?,
            None => false,
        };
        if !in_thread {
            policy::mail_to(a, to)?;
        }
    }
    mail::send(
        t,
        Outgoing {
            from,
            to,
            subject,
            body,
            thread,
            in_reply_to: None,
        },
    )
}

async fn send(
    State(state): State<Shared>,
    actor: Actor,
    ApiJson(m): ApiJson<NewMail>,
) -> ApiResult<(StatusCode, Json<Message>)> {
    let msg = state
        .db
        .write(actor.label(), move |t| {
            let from = sender(t, &actor, m.from.as_deref())?;
            let context = from.project().map(str::to_owned);
            let to = mail::resolve(t, &m.to, context.as_deref())?;
            send_checked(
                t,
                &actor,
                &from,
                &to,
                m.subject.as_deref().unwrap_or_default(),
                &m.body,
                m.thread_id.as_deref(),
            )
        })
        .await?;
    Ok((StatusCode::CREATED, Json(msg)))
}

async fn show(
    State(state): State<Shared>,
    actor: Actor,
    Path(id): Path<String>,
) -> ApiResult<Json<Message>> {
    let m = state.db.read(move |c| mail::get(c, &id)).await?;
    if !may_read_message(&actor, &m) {
        return Err(ApiError::not_found(format!("no message `{}`", m.id)));
    }
    Ok(Json(m))
}

/// Is the actor the recipient of `m`: the agent itself, the operator, or an
/// admin of the project inbox?
fn is_recipient(actor: &Actor, m: &Message) -> bool {
    match actor {
        Actor::Operator => true,
        Actor::Agent(a) => {
            m.to == a.id
                || m.to
                    .strip_prefix("project:")
                    .is_some_and(|p| a.is_admin_of(p))
        }
    }
}

async fn read(
    State(state): State<Shared>,
    actor: Actor,
    Path(id): Path<String>,
) -> ApiResult<Json<Message>> {
    let m = state
        .db
        .write(actor.label(), move |t| {
            let m = mail::get(t, &id)?;
            if !may_read_message(&actor, &m) {
                return Err(ApiError::not_found(format!("no message `{id}`")));
            }
            if !is_recipient(&actor, &m) {
                return Err(ApiError::forbidden(
                    "only the recipient marks a message read",
                ));
            }
            mail::mark_read(t, &id)
        })
        .await?;
    Ok(Json(m))
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct ReplyBody {
    body: String,
    #[serde(default)]
    subject: Option<String>,
}

/// Reply on the thread of `m`. The reply goes out from the address the
/// message went to when the actor answers for it, else from the actor.
fn reply_from(
    t: &mut Tx<'_>,
    actor: &Actor,
    m: &Message,
    r: &ReplyBody,
) -> ApiResult<Vec<Message>> {
    if !may_read_message(actor, m) {
        return Err(ApiError::not_found(format!("no message `{}`", m.id)));
    }
    let from = if is_recipient(actor, m) {
        mail::resolve(t, &m.to, None)?
    } else {
        Address::from_actor(actor)
    };
    mail::reply(t, m, &from, r.subject.as_deref(), &r.body)
}

async fn reply(
    State(state): State<Shared>,
    actor: Actor,
    Path(id): Path<String>,
    ApiJson(r): ApiJson<ReplyBody>,
) -> ApiResult<(StatusCode, Json<Vec<Message>>)> {
    let out = state
        .db
        .write(actor.label(), move |t| {
            let m = mail::get(t, &id)?;
            reply_from(t, &actor, &m, &r)
        })
        .await?;
    Ok((StatusCode::CREATED, Json(out)))
}

#[derive(Debug, Default, Deserialize)]
struct InboxQuery {
    state: Option<String>,
    #[serde(default)]
    unread: bool,
    thread: Option<String>,
}

async fn inbox_list(
    State(state): State<Shared>,
    actor: Actor,
    Path(name): Path<String>,
    ApiQuery(page): ApiQuery<Page>,
    ApiQuery(q): ApiQuery<InboxQuery>,
    ApiQuery(wait): ApiQuery<WaitQuery>,
) -> ApiResult<Response> {
    policy::read_inbox(&actor, &name)?;
    let rows = longpoll::hold(&state, wait, &["mail"], || {
        let (name, page, unread, st, thread) = (
            name.clone(),
            page.clone(),
            q.unread,
            q.state.clone(),
            q.thread.clone(),
        );
        state.db.read(move |c| {
            crate::store::projects::get(c, &name)?;
            let f = MailFilter {
                to: Some(format!("project:{name}")),
                state: if unread { Some("unread".into()) } else { st },
                thread,
                ..MailFilter::default()
            };
            mail::list(c, &f, &page)
        })
    })
    .await?;
    Ok(paged(rows))
}

/// The body of `POST /api/projects/{name}/inbox`.
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct NewInboxMail {
    #[serde(default)]
    from: Option<String>,
    #[serde(default)]
    subject: Option<String>,
    body: String,
    #[serde(default)]
    thread_id: Option<String>,
}

async fn inbox_send(
    State(state): State<Shared>,
    actor: Actor,
    Path(name): Path<String>,
    ApiJson(m): ApiJson<NewInboxMail>,
) -> ApiResult<(StatusCode, Json<Message>)> {
    let msg = state
        .db
        .write(actor.label(), move |t| {
            crate::store::projects::get(t, &name)?;
            let from = sender(t, &actor, m.from.as_deref())?;
            let to = Address::Project(name.clone());
            send_checked(
                t,
                &actor,
                &from,
                &to,
                m.subject.as_deref().unwrap_or_default(),
                &m.body,
                m.thread_id.as_deref(),
            )
        })
        .await?;
    Ok((StatusCode::CREATED, Json(msg)))
}

fn inbox_message(t: &Tx<'_>, name: &str, id: &str) -> ApiResult<Message> {
    let m = mail::get(t, id)?;
    if m.to != format!("project:{name}") {
        return Err(ApiError::not_found(format!(
            "no message `{id}` in the inbox of `{name}`"
        )));
    }
    Ok(m)
}

async fn inbox_show(
    State(state): State<Shared>,
    actor: Actor,
    Path((name, id)): Path<(String, String)>,
) -> ApiResult<Json<Message>> {
    policy::read_inbox(&actor, &name)?;
    let m = state
        .db
        .read(move |c| {
            let m = mail::get(c, &id)?;
            if m.to != format!("project:{name}") {
                return Err(ApiError::not_found(format!(
                    "no message `{id}` in the inbox of `{name}`"
                )));
            }
            Ok(m)
        })
        .await?;
    Ok(Json(m))
}

async fn inbox_read(
    State(state): State<Shared>,
    actor: Actor,
    Path((name, id)): Path<(String, String)>,
) -> ApiResult<Json<Message>> {
    policy::read_inbox(&actor, &name)?;
    let m = state
        .db
        .write(actor.label(), move |t| {
            inbox_message(t, &name, &id)?;
            mail::mark_read(t, &id)
        })
        .await?;
    Ok(Json(m))
}

async fn inbox_reply(
    State(state): State<Shared>,
    actor: Actor,
    Path((name, id)): Path<(String, String)>,
    ApiJson(r): ApiJson<ReplyBody>,
) -> ApiResult<(StatusCode, Json<Vec<Message>>)> {
    policy::read_inbox(&actor, &name)?;
    let out = state
        .db
        .write(actor.label(), move |t| {
            let m = inbox_message(t, &name, &id)?;
            let from = Address::Project(name.clone());
            mail::reply(t, &m, &from, r.subject.as_deref(), &r.body)
        })
        .await?;
    Ok((StatusCode::CREATED, Json(out)))
}
