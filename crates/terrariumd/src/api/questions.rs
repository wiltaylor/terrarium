//! Questions to the operator (PLAN §12e, docs/api.md §6.8).
//!
//! - `GET|POST /api/agents/{id}/questions`: the questions and notices of
//!   an agent; a leader asks a question, sends a notice (`kind:
//!   "notice"`), or asks a bundle of related questions (`kind: "bundle"`
//!   with `items`), with `wait: true` also a `question` wait on it.
//! - `GET /api/questions?status=&project=&agent=&kind=&card=`: the
//!   questions and notices that the caller may read.
//! - `GET|PATCH /api/questions/{id}`. A body with only `card` links the
//!   question to a card or unlinks it: the operator and a sysadmin at
//!   any time, the asking leader while it is open.
//! - `POST /api/questions/{id}/answer`: the operator, or a sysadmin agent
//!   for the question of another agent. A notice is `409`. A bundle takes
//!   `items`, with an answer for each item.
//! - `POST /api/questions/{id}/ack`: the same rights, for a notice. A
//!   question is `409`.
//! - `POST /api/questions/{id}/retract`: the asking leader or the
//!   operator.
//! - `POST /api/questions/{id}/to-thread`: the operator moves an open
//!   question or notice into a new Inbox thread with its agent (GitHub
//!   #30); the question becomes `in_thread`.

use axum::Json;
use axum::extract::{Path, State};
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use serde::{Deserialize, Deserializer, Serialize};
use serde_json::json;

use super::{ApiJson, ApiQuery, Shared};
use crate::actor::Actor;
use crate::error::{ApiError, ApiResult};
use crate::policy;
use crate::store::agents::{self, AgentView};
use crate::store::mail::Address;
use crate::store::questions::{
    self, Answer, BundleItem, Filter, ItemAnswer, NewQuestion, OptionRef, Priority, Question,
    QuestionKind, QuestionPatch, QuestionStatus,
};
use crate::store::waits::{self, NewWait, Wait, WaitKind};
use crate::waits as rt;

pub fn routes() -> super::link_registry::RegisteredRouter {
    super::link_registry::RegisteredRouter::new()
        .route(
            "/agents/{id}/questions",
            get(list_of).post(ask),
            &[
                (axum::http::Method::GET, super::link_registry::Access::Link),
                (axum::http::Method::POST, super::link_registry::Access::Link),
            ],
        )
        .route(
            "/questions",
            get(list_all),
            &[(axum::http::Method::GET, super::link_registry::Access::Link)],
        )
        .route(
            "/questions/{id}",
            get(show).patch(update),
            &[
                (axum::http::Method::GET, super::link_registry::Access::Link),
                (
                    axum::http::Method::PATCH,
                    super::link_registry::Access::Link,
                ),
            ],
        )
        .route(
            "/questions/{id}/answer",
            post(answer),
            &[(
                axum::http::Method::POST,
                super::link_registry::Access::Conditional,
            )],
        )
        .route(
            "/questions/{id}/ack",
            post(ack),
            &[(axum::http::Method::POST, super::link_registry::Access::Link)],
        )
        .route(
            "/questions/{id}/retract",
            post(retract),
            &[(axum::http::Method::POST, super::link_registry::Access::Link)],
        )
        .route(
            "/questions/{id}/to-thread",
            post(to_thread),
            &[(
                axum::http::Method::POST,
                super::link_registry::Access::OperatorOnly,
            )],
        )
}

async fn find_agent(state: &Shared, actor: &Actor, key: String) -> ApiResult<AgentView> {
    let context = actor.project().map(str::to_owned);
    state
        .db
        .read(move |c| agents::find(c, &key, context.as_deref()))
        .await
}

/// A question that the caller may read; else not found.
async fn find(state: &Shared, actor: &Actor, id: String) -> ApiResult<Question> {
    let q = state.db.read(move |c| questions::get(c, &id)).await?;
    policy::read_questions(actor, &q.agent, &q.project)
        .map_err(|_| ApiError::not_found(format!("no question `{}`", q.id)))?;
    Ok(q)
}

#[derive(Debug, Default, Deserialize)]
struct AgentListQuery {
    #[serde(default)]
    status: Option<String>,
    #[serde(default)]
    all: Option<bool>,
    #[serde(default)]
    kind: Option<String>,
}

/// The `kind` filter of a list: `question`, `notice`, or none (both).
fn kind_filter(kind: Option<&str>) -> ApiResult<Option<QuestionKind>> {
    kind.filter(|k| !k.trim().is_empty())
        .map(QuestionKind::parse)
        .transpose()
}

/// `GET /api/agents/{id}/questions[?status=|all=true]`: no filter lists
/// the open questions and those answered in the last 24 h.
async fn list_of(
    State(state): State<Shared>,
    actor: Actor,
    Path(id): Path<String>,
    ApiQuery(q): ApiQuery<AgentListQuery>,
) -> ApiResult<Json<Vec<Question>>> {
    let a = find_agent(&state, &actor, id).await?;
    let key = a.agent.id.to_string();
    policy::read_questions(&actor, &key, &a.agent.project)?;
    let status = q
        .status
        .as_deref()
        .filter(|s| !s.is_empty() && *s != "all")
        .map(QuestionStatus::parse)
        .transpose()?;
    let all = q.all.unwrap_or(false) || q.status.as_deref() == Some("all");
    let f = Filter {
        agent: Some(key),
        kind: kind_filter(q.kind.as_deref())?,
        status,
        recent: status.is_none() && !all,
        ..Filter::default()
    };
    Ok(Json(state.db.read(move |c| questions::list(c, &f)).await?))
}

#[derive(Debug, Default, Deserialize)]
struct ListQuery {
    #[serde(default)]
    status: Option<String>,
    #[serde(default)]
    project: Option<String>,
    #[serde(default)]
    agent: Option<String>,
    #[serde(default)]
    kind: Option<String>,
    #[serde(default)]
    card: Option<String>,
}

/// `GET /api/questions?status=open&project=&agent=&card=`. The operator
/// and a sysadmin read all; a project_admin reads its project; a member
/// gets `403`. Default status: `open`; `all` lists every status. `card`:
/// only the questions linked to that card (an unknown card is `404`).
async fn list_all(
    State(state): State<Shared>,
    actor: Actor,
    ApiQuery(q): ApiQuery<ListQuery>,
) -> ApiResult<Response> {
    let project = match &actor {
        Actor::Operator => q.project.clone().filter(|p| !p.is_empty()),
        Actor::Agent(a) if a.reads_all() => q.project.clone().filter(|p| !p.is_empty()),
        Actor::Agent(a) if a.is_admin_of(&a.project) => {
            if let Some(p) = q.project.as_deref().filter(|p| !p.is_empty()) {
                policy::admin_project(&actor, p, "listing questions")?;
            }
            Some(a.project.clone())
        }
        Actor::Agent(_) => {
            return Err(ApiError::forbidden(
                "listing questions needs project_admin, sysadmin, or the operator; run \
                 `terrarium question ls` for your own",
            ));
        }
    };
    let status = match q.status.as_deref().map(str::trim) {
        None | Some("") => Some(QuestionStatus::Open),
        Some("all") => None,
        Some(s) => Some(QuestionStatus::parse(s)?),
    };
    let agent = match q.agent.as_deref().filter(|a| !a.is_empty()) {
        Some(key) => {
            let a = find_agent(&state, &actor, key.to_owned()).await?;
            let id = a.agent.id.to_string();
            policy::read_questions(&actor, &id, &a.agent.project)?;
            Some(id)
        }
        None => None,
    };
    let card = match q.card.as_deref().map(str::trim).filter(|c| !c.is_empty()) {
        Some(key) => {
            let key = key.to_owned();
            let c = state
                .db
                .read(move |c| crate::store::board::get_card(c, &key))
                .await?;
            // A project_admin sees only the cards of its project.
            let all =
                matches!(actor, Actor::Operator) || actor.agent().is_some_and(|a| a.reads_all());
            if let Some(p) = project.as_deref().filter(|p| !all && *p != c.project) {
                return Err(ApiError::not_found(format!(
                    "no card `{}` in project `{p}`",
                    c.id
                )));
            }
            Some(c.id)
        }
        None => None,
    };
    let kind = kind_filter(q.kind.as_deref())?;
    let f = Filter {
        agent,
        project: project.clone(),
        card,
        kind,
        status,
        recent: false,
    };
    let (rows, total) = state
        .db
        .read(move |c| {
            Ok((
                questions::list(c, &f)?,
                questions::open_total(c, project.as_deref(), kind)?,
            ))
        })
        .await?;
    let mut resp = Json(rows).into_response();
    for (k, v) in [
        ("x-open-questions", total.open),
        ("x-urgent-questions", total.urgent),
        ("x-critical-questions", total.critical),
    ] {
        if let Ok(v) = v.to_string().parse() {
            resp.headers_mut().insert(k, v);
        }
    }
    Ok(resp)
}

/// `GET /api/questions/{id}`.
async fn show(
    State(state): State<Shared>,
    actor: Actor,
    Path(id): Path<String>,
) -> ApiResult<Json<Question>> {
    Ok(Json(find(&state, &actor, id).await?))
}

/// One item of a bundle in an ask or an update.
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct ItemBody {
    #[serde(default)]
    multi_select: bool,
    text: String,
    #[serde(default)]
    options: Vec<String>,
    #[serde(default)]
    context: Option<String>,
}

impl From<ItemBody> for BundleItem {
    fn from(b: ItemBody) -> Self {
        BundleItem {
            multi_select: b.multi_select,
            text: b.text,
            options: b.options,
            context: b.context,
            ..BundleItem::default()
        }
    }
}

fn items_of(items: Vec<ItemBody>) -> Vec<BundleItem> {
    items.into_iter().map(BundleItem::from).collect()
}

/// The body of `POST /api/agents/{id}/questions`.
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct AskBody {
    #[serde(default)]
    test_request: bool,
    #[serde(default)]
    multi_select: bool,
    /// `question` (the default), `notice`, or `bundle`.
    #[serde(default)]
    kind: QuestionKind,
    /// The question, the notice, or the summary of a bundle.
    text: String,
    #[serde(default)]
    options: Vec<String>,
    /// The questions of a bundle: 2 to 20.
    #[serde(default)]
    items: Vec<ItemBody>,
    #[serde(default)]
    context: Option<String>,
    #[serde(default)]
    card: Option<String>,
    #[serde(default)]
    reply_to: Option<String>,
    #[serde(default)]
    priority: Option<Priority>,
    /// Also register a `question` wait on the new question.
    #[serde(default)]
    wait: bool,
    /// The timeout of the wait (a duration or a time). Default
    /// `TERRARIUM_WAIT_DEFAULT_TIMEOUT`.
    #[serde(default)]
    timeout: Option<String>,
    /// Ask in this Inbox thread (TER-91): the agent is its attached agent.
    #[serde(default)]
    thread: Option<String>,
}

/// The answer of `POST /api/agents/{id}/questions`: the question, and
/// the wait when `wait` was set.
#[derive(Debug, Serialize)]
struct Asked {
    #[serde(flatten)]
    question: Question,
    #[serde(skip_serializing_if = "Option::is_none")]
    wait: Option<Wait>,
}

/// The new `question` wait of `waiter` on `q`.
pub fn new_wait(waiter: &str, q: &Question, timeout_at: Option<String>) -> NewWait {
    NewWait {
        agent: waiter.to_owned(),
        kind: WaitKind::Question,
        spec: json!({"question": q.id}),
        summary: questions::wait_summary(q.kind, &q.text),
        target_agent: None,
        reason: None,
        timeout_at,
        due_at: None,
    }
}

/// Register a `question` wait of `waiter` on the question `id`, in one
/// transaction with the check that the question is the waiter's own and
/// still open.
pub fn create_wait(
    t: &mut crate::db::Tx<'_>,
    waiter: &str,
    id: &str,
    timeout_at: Option<String>,
    reason: Option<String>,
) -> ApiResult<Wait> {
    let q = questions::get(t, id)?;
    if q.agent != waiter {
        return Err(ApiError::not_found(format!(
            "no question `{id}` of this agent: an agent waits only on its own questions"
        )));
    }
    if q.status != QuestionStatus::Open {
        return Err(ApiError::conflict(format!(
            "{} {} is {} already; run `terrarium question show {}`",
            q.noun(),
            q.id,
            q.status.as_str(),
            q.id
        )));
    }
    let mut w = new_wait(waiter, &q, timeout_at);
    w.reason = reason;
    waits::create(t, w)
}

/// `POST /api/agents/{id}/questions`: a leader asks the operator, or
/// sends a notice (`kind: "notice"`).
async fn ask(
    State(state): State<Shared>,
    actor: Actor,
    Path(id): Path<String>,
    ApiJson(b): ApiJson<AskBody>,
) -> ApiResult<Response> {
    let a = find_agent(&state, &actor, id).await?;
    let key = a.agent.id.to_string();
    let what = match b.kind {
        QuestionKind::Question | QuestionKind::Bundle => "asking the operator",
        QuestionKind::Notice => "sending a notice to the operator",
        QuestionKind::ToolRun => {
            return Err(ApiError::invalid(
                "a tool_run question comes only from a tool run (`terrarium tool run`)",
            ));
        }
    };
    // In its own thread, the attached agent asks also without a leader
    // role (the store checks that the thread is its own and open).
    let thread = b
        .thread
        .as_deref()
        .map(str::trim)
        .filter(|t| !t.is_empty())
        .map(str::to_owned);
    if thread.is_some() {
        policy::ask_in_thread(&actor, &key, what)?;
    } else {
        policy::ask_question(&actor, &key, &a.agent.project, what)?;
    }
    let tuning = state.waits.tuning();
    let timeout_at = match (b.wait, b.timeout.as_deref()) {
        (false, Some(_)) => {
            return Err(ApiError::invalid(
                "`timeout` is for a wait: set `wait: true`",
            ));
        }
        (false, None) => None,
        (true, Some(t)) => Some(rt::from_now(crate::api::waits::parse_span(
            "timeout",
            t,
            tuning.max_timeout,
        )?)),
        (true, None) => Some(rt::from_now(tuning.default_timeout)),
    };
    let new = NewQuestion {
        multi_select: b.multi_select,
        agent: key.clone(),
        kind: b.kind,
        text: b.text,
        options: b.options,
        items: items_of(b.items),
        context: b.context,
        card: b.card,
        priority: b.priority.unwrap_or_default(),
        thread,
        hold_card: None,
        reply_to: b.reply_to,
    };
    let wait = b.wait;
    let (question, wait) = state
        .db
        .write(actor.label(), move |t| {
            if b.test_request {
                let thread = new.thread.as_deref().ok_or_else(|| ApiError::invalid("test_request requires a thread"))?;
                crate::store::threads::check_ask(t, thread, &new.agent)?;
                crate::store::thread_test_ready::check(t, thread)?;
            }
            let q = questions::ask(t, new)?;
            let w = if wait {
                Some(create_wait(t, &key, &q.id, timeout_at, None)?)
            } else {
                None
            };
            Ok((q, w))
        })
        .await?;
    Ok((StatusCode::CREATED, Json(Asked { question, wait })).into_response())
}

/// `null` in a JSON body: `Some(None)`; a missing field: `None`.
fn nullable<'de, D, T>(d: D) -> Result<Option<Option<T>>, D::Error>
where
    D: Deserializer<'de>,
    T: Deserialize<'de>,
{
    Option::<T>::deserialize(d).map(Some)
}

/// The body of `PATCH /api/questions/{id}`.
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct PatchBody {
    #[serde(default)]
    text: Option<String>,
    /// `[]` or `null` clears the options.
    #[serde(default, deserialize_with = "nullable")]
    options: Option<Option<Vec<String>>>,
    /// The new items of a bundle, all of them.
    #[serde(default)]
    items: Option<Vec<ItemBody>>,
    #[serde(default, deserialize_with = "nullable")]
    context: Option<Option<String>>,
    #[serde(default, deserialize_with = "nullable")]
    card: Option<Option<String>>,
    #[serde(default)]
    priority: Option<Priority>,
}

/// `PATCH /api/questions/{id}`: the asking leader changes its open
/// question. Each change adds 1 to `revision`. A question that is not
/// open is `409`. A body with only `card` links or unlinks the card and
/// keeps the revision (see [`policy::link_question`] for who may).
async fn update(
    State(state): State<Shared>,
    actor: Actor,
    Path(id): Path<String>,
    ApiJson(b): ApiJson<PatchBody>,
) -> ApiResult<Json<Question>> {
    let q = find(&state, &actor, id).await?;
    if let (Some(card), None, None, None, None, None) = (
        &b.card,
        &b.text,
        &b.options,
        &b.items,
        &b.context,
        &b.priority,
    ) {
        let any_status = policy::link_question(&actor, &q.agent, &q.project)?;
        let (qid, card) = (q.id.clone(), card.clone());
        return Ok(Json(
            state
                .db
                .write(actor.label(), move |t| {
                    questions::set_card(t, &qid, card.as_deref(), any_status)
                })
                .await?,
        ));
    }
    policy::ask_question(&actor, &q.agent, &q.project, "updating a question")?;
    let patch = QuestionPatch {
        text: b.text,
        options: b.options.map(Option::unwrap_or_default),
        items: b.items.map(items_of),
        context: b.context,
        card: b.card,
        priority: b.priority,
    };
    let qid = q.id.clone();
    Ok(Json(
        state
            .db
            .write(actor.label(), move |t| questions::update(t, &qid, patch))
            .await?,
    ))
}

/// The body of `POST /api/questions/{id}/answer`.
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct AnswerBody {
    #[serde(default)]
    options: Vec<OptionRef>,
    #[serde(default)]
    answer: Option<String>,
    /// A label, or a 0-based index.
    #[serde(default)]
    option: Option<OptionRef>,
    /// A bundle: one entry for each item, in order.
    #[serde(default)]
    items: Option<Vec<ItemAnswer>>,
    /// The revision that the answerer saw: another one is `409`.
    #[serde(default)]
    revision: Option<i64>,
}

/// `POST /api/questions/{id}/answer`.
async fn answer(
    State(state): State<Shared>,
    actor: Actor,
    Path(id): Path<String>,
    ApiJson(b): ApiJson<AnswerBody>,
) -> ApiResult<Json<Question>> {
    let q = find(&state, &actor, id).await?;
    policy::answer_question(&actor, &q.agent)?;
    let by = Address::from_actor(&actor);
    let qid = q.id.clone();
    let a = Answer {
        options: b.options,
        text: b.answer,
        option: b.option,
        items: b.items,
        revision: b.revision,
    };
    Ok(Json(
        state
            .db
            .write(actor.label(), move |t| questions::answer(t, &qid, &by, &a))
            .await?,
    ))
}

/// The body of `POST /api/questions/{id}/ack`.
#[derive(Debug, Default, Deserialize)]
#[serde(deny_unknown_fields)]
struct AckBody {
    /// An optional reply to the agent.
    #[serde(default)]
    text: Option<String>,
}

/// `POST /api/questions/{id}/ack`: acknowledge a notice. The body is
/// optional. The agent always gets one mail on the thread of the notice.
async fn ack(
    State(state): State<Shared>,
    actor: Actor,
    Path(id): Path<String>,
    body: axum::body::Bytes,
) -> ApiResult<Json<Question>> {
    let b: AckBody = optional_body(&body)?;
    let q = find(&state, &actor, id).await?;
    policy::answer_question(&actor, &q.agent)?;
    let by = Address::from_actor(&actor);
    let qid = q.id.clone();
    Ok(Json(
        state
            .db
            .write(actor.label(), move |t| {
                questions::acknowledge(t, &qid, &by, b.text.as_deref())
            })
            .await?,
    ))
}

/// A JSON body that may be absent: no body is the default.
fn optional_body<T: Default + serde::de::DeserializeOwned>(body: &[u8]) -> ApiResult<T> {
    if body.iter().all(u8::is_ascii_whitespace) {
        Ok(T::default())
    } else {
        serde_json::from_slice(body).map_err(|e| ApiError::bad_request(e.to_string()))
    }
}

/// The body of `POST /api/questions/{id}/retract`.
#[derive(Debug, Default, Deserialize)]
#[serde(deny_unknown_fields)]
struct RetractBody {
    #[serde(default)]
    reason: Option<String>,
}

/// `POST /api/questions/{id}/retract`.
async fn retract(
    State(state): State<Shared>,
    actor: Actor,
    Path(id): Path<String>,
    body: axum::body::Bytes,
) -> ApiResult<Json<Question>> {
    // The body is optional: no body is no reason.
    let b: RetractBody = optional_body(&body)?;
    let q = find(&state, &actor, id).await?;
    let by_owner = actor.agent().is_some_and(|a| a.id == q.agent);
    // The agent that waits for a tool run approval may give it up (its
    // CLI does on Ctrl-C), whatever its role.
    if !(by_owner && (q.kind == QuestionKind::ToolRun || q.thread.is_some())) {
        policy::retract_question(&actor, &q.agent, &q.project)?;
    }
    let qid = q.id.clone();
    Ok(Json(
        state
            .db
            .write(actor.label(), move |t| {
                questions::retract(t, &qid, by_owner, b.reason.as_deref())
            })
            .await?,
    ))
}

/// The body of `POST /api/questions/{id}/to-thread`.
#[derive(Debug, Default, Deserialize)]
#[serde(deny_unknown_fields)]
struct ToThreadBody {
    /// The operator's message after the question in the first message.
    #[serde(default)]
    message: Option<String>,
}

/// `POST /api/questions/{id}/to-thread` (GitHub #30): the operator only.
/// The body is optional. `201` `{question, thread}`.
async fn to_thread(
    State(state): State<Shared>,
    actor: Actor,
    Path(id): Path<String>,
    body: axum::body::Bytes,
) -> ApiResult<Response> {
    let b: ToThreadBody = optional_body(&body)?;
    crate::links::operator_only("moving a question into a thread")?;
    let q = find(&state, &actor, id).await?;
    crate::policy::operator_only(&actor, "moving a question into a thread")?;
    let qid = q.id.clone();
    let (q, th) = state
        .db
        .write(actor.label(), move |t| {
            questions::to_thread(t, &qid, b.message.as_deref())
        })
        .await?;
    Ok((
        StatusCode::CREATED,
        Json(json!({"question": q, "thread": th})),
    )
        .into_response())
}
