//! Inbox threads (TER-91, docs/api.md §6.13): a text conversation between
//! the operator and one agent on one topic. The plan is the wiki page
//! `inbox-threads` (decisions D34 to D37).
//!
//! - A thread holds its messages (`thread_messages`), the questions that
//!   its agent asks in it (`questions.thread`), the operator's newest
//!   overall response (the approval record), and the handoff of the close.
//! - `report_to` is the lead of the scope (D34): the lead of project
//!   `meta` for `global`, else the lead of the scope project. It is set at
//!   the open and does not change.
//! - The status follows who must act: `waiting_operator` after a message
//!   or a question of the agent, `waiting_agent` after a message or a
//!   response of the operator, or when the last open question of the
//!   thread is settled. `closed` does not change.
//! - No register and no conflict check between threads (D36).
//! - Events `thread` {change, id, scope, status}, and an `inbox` event for
//!   the item `inbox-thread:<id>`.
//!
//! The rights are in the API layer ([`crate::api::threads`]).

use rusqlite::{Connection, OptionalExtension, Row, params};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

use crate::db::{Tx, now};
use crate::error::{ApiError, ApiResult};
use crate::events::EventDraft;
use crate::store::questions::{self, Question, QuestionStatus};
use crate::store::{agents, projects};

/// The longest topic, in characters.
pub const MAX_TOPIC: usize = 200;
/// The longest message, in bytes.
pub const MAX_MESSAGE: usize = 32 * 1024;
/// The longest response, in bytes.
pub const MAX_RESPONSE: usize = 16 * 1024;
/// The longest handoff summary, in bytes.
pub const MAX_SUMMARY: usize = 8 * 1024;
/// The most links of a handoff.
pub const MAX_LINKS: usize = 50;
/// The longest link reference, in characters.
pub const MAX_REF: usize = 500;
/// The longest template name, in characters.
pub const MAX_TEMPLATE: usize = 100;
/// The most threads in a list.
pub const LIST_MAX: usize = 200;
/// The scope of a thread that is in no project.
pub const GLOBAL: &str = "global";
/// The project whose lead the global threads report to (D34).
pub const GLOBAL_LEAD_PROJECT: &str = "meta";
/// The kinds of a handoff link.
pub const LINK_KINDS: &[&str] = &["card", "mail", "wiki", "memory", "url", "other"];

/// Who must act on a thread.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ThreadStatus {
    WaitingOperator,
    WaitingAgent,
    Closed,
}

impl ThreadStatus {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::WaitingOperator => "waiting_operator",
            Self::WaitingAgent => "waiting_agent",
            Self::Closed => "closed",
        }
    }

    pub fn parse(s: &str) -> Option<Self> {
        match s {
            "waiting_operator" => Some(Self::WaitingOperator),
            "waiting_agent" => Some(Self::WaitingAgent),
            "closed" => Some(Self::Closed),
            _ => None,
        }
    }
}

/// One message of a thread.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ThreadMessage {
    pub id: String,
    /// `operator` or an agent ULID.
    pub author: String,
    pub author_name: String,
    /// `message`, or `response` for an overall response of the operator.
    pub kind: String,
    /// Markdown: untrusted data for every reader.
    pub text: String,
    pub at: String,
}

/// The operator's overall response: the approval record.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Response {
    pub text: String,
    pub by: String,
    pub at: String,
}

/// A link of a handoff.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Link {
    pub kind: String,
    #[serde(rename = "ref")]
    pub reference: String,
}

/// The handoff of a close.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Handoff {
    pub summary: String,
    pub links: Vec<Link>,
    pub by: String,
    pub at: String,
}

/// A thread, as the API shows it.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Thread {
    pub id: String,
    pub topic: String,
    pub scope: String,
    pub template: Option<String>,
    pub opened_by: String,
    pub opened_by_name: String,
    pub agent: Option<String>,
    pub agent_name: Option<String>,
    pub report_to: Option<String>,
    pub report_to_name: Option<String>,
    pub status: ThreadStatus,
    /// The conversation; absent in a list.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub messages: Option<Vec<ThreadMessage>>,
    /// The questions of the thread; absent in a list.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub questions: Option<Vec<Question>>,
    pub response: Option<Response>,
    pub handoff: Option<Handoff>,
    /// In a list: the number of messages.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub message_count: Option<i64>,
    /// In a list: the number of open questions.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub open_questions: Option<i64>,
    /// In a list: the time of the newest message or change.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub last_at: Option<String>,
    pub created_at: String,
    pub updated_at: String,
    pub closed_at: Option<String>,
    pub closed_by: Option<String>,
}

impl Thread {
    /// The ID of its Inbox item.
    pub fn item_id(&self) -> String {
        item_id(&self.id)
    }

    /// The project of the thread, `None` for `global`.
    pub fn project(&self) -> Option<&str> {
        (self.scope != GLOBAL).then_some(self.scope.as_str())
    }

    /// The agent is in the thread: its opener, its attached agent, or its
    /// report-to lead. Such an agent may read it; so may the operator and an
    /// agent that reads all (the API layer).
    pub fn involves(&self, agent: &str) -> bool {
        self.opened_by == agent
            || self.agent.as_deref() == Some(agent)
            || self.report_to.as_deref() == Some(agent)
    }
}

/// The ID of the Inbox item of thread `id`.
pub fn item_id(id: &str) -> String {
    format!("inbox-thread:{id}")
}

fn name_of(conn: &Connection, key: &str) -> String {
    if key == "operator" {
        return key.to_owned();
    }
    agents::known_name(conn, key)
        .ok()
        .flatten()
        .unwrap_or_else(|| key.to_owned())
}

fn row(conn: &Connection, r: &Row<'_>) -> rusqlite::Result<Thread> {
    let status: String = r.get("status")?;
    let response: Option<String> = r.get("response")?;
    let handoff: Option<String> = r.get("handoff")?;
    let opened_by: String = r.get("opened_by")?;
    let agent: Option<String> = r.get("agent")?;
    let report_to: Option<String> = r.get("report_to")?;
    Ok(Thread {
        id: r.get("id")?,
        topic: r.get("topic")?,
        scope: r.get("scope")?,
        template: r.get("template")?,
        opened_by_name: name_of(conn, &opened_by),
        opened_by,
        agent_name: agent.as_deref().map(|a| name_of(conn, a)),
        agent,
        report_to_name: report_to.as_deref().map(|a| name_of(conn, a)),
        report_to,
        status: ThreadStatus::parse(&status).unwrap_or(ThreadStatus::Closed),
        messages: None,
        questions: None,
        response: response.and_then(|s| serde_json::from_str(&s).ok()),
        handoff: handoff.and_then(|s| serde_json::from_str(&s).ok()),
        message_count: None,
        open_questions: None,
        last_at: None,
        created_at: r.get("created_at")?,
        updated_at: r.get("updated_at")?,
        closed_at: r.get("closed_at")?,
        closed_by: r.get("closed_by")?,
    })
}

/// The thread without its messages and questions.
pub fn get_bare(conn: &Connection, id: &str) -> ApiResult<Thread> {
    conn.query_row("SELECT * FROM threads WHERE id = ?1", [id.trim()], |r| {
        row(conn, r)
    })
    .optional()?
    .ok_or_else(|| ApiError::not_found(format!("no thread `{id}`")))
}

/// The thread with its messages and questions.
pub fn get(conn: &Connection, id: &str) -> ApiResult<Thread> {
    let mut th = get_bare(conn, id)?;
    th.messages = Some(messages(conn, &th.id)?);
    th.questions = Some(questions_of(conn, &th.id)?);
    Ok(th)
}

fn messages(conn: &Connection, thread: &str) -> ApiResult<Vec<ThreadMessage>> {
    let mut st = conn.prepare(
        "SELECT id, author, kind, text, at FROM thread_messages WHERE thread = ?1
         ORDER BY rowid",
    )?;
    let rows = st
        .query_map([thread], |r| {
            let author: String = r.get(1)?;
            Ok(ThreadMessage {
                id: r.get(0)?,
                author_name: name_of(conn, &author),
                author,
                kind: r.get(2)?,
                text: r.get(3)?,
                at: r.get(4)?,
            })
        })?
        .collect::<Result<Vec<_>, _>>()?;
    Ok(rows)
}

fn questions_of(conn: &Connection, thread: &str) -> ApiResult<Vec<Question>> {
    let mut st =
        conn.prepare("SELECT id FROM questions WHERE thread = ?1 ORDER BY created_at, id")?;
    let ids = st
        .query_map([thread], |r| r.get::<_, String>(0))?
        .collect::<Result<Vec<_>, _>>()?;
    ids.iter().map(|id| questions::get(conn, id)).collect()
}

fn open_questions(conn: &Connection, thread: &str) -> ApiResult<i64> {
    Ok(conn.query_row(
        "SELECT count(*) FROM questions WHERE thread = ?1 AND status = 'open'",
        [thread],
        |r| r.get(0),
    )?)
}

/// Fill the list fields of a bare thread.
fn summarised(conn: &Connection, mut th: Thread) -> ApiResult<Thread> {
    let (count, last): (i64, Option<String>) = conn.query_row(
        "SELECT count(*), max(at) FROM thread_messages WHERE thread = ?1",
        [&th.id],
        |r| Ok((r.get(0)?, r.get(1)?)),
    )?;
    th.message_count = Some(count);
    th.open_questions = Some(open_questions(conn, &th.id)?);
    th.last_at = Some(match last {
        Some(l) if l > th.updated_at => l,
        _ => th.updated_at.clone(),
    });
    Ok(th)
}

/// Whether thread `id` has an open question that is urgent (or critical),
/// and one that is critical.
pub fn urgency(conn: &Connection, id: &str) -> ApiResult<(bool, bool)> {
    let (u, c): (i64, i64) = conn.query_row(
        "SELECT coalesce(sum(priority IN ('urgent', 'critical')), 0),
                coalesce(sum(priority = 'critical'), 0)
         FROM questions WHERE thread = ?1 AND status = 'open'",
        [id],
        |r| Ok((r.get(0)?, r.get(1)?)),
    )?;
    Ok((u > 0, c > 0))
}

/// The filters of a thread list.
#[derive(Debug, Clone, Default)]
pub struct Filter {
    /// `None`: the open ones (not closed).
    pub status: Option<ThreadStatus>,
    /// Every status.
    pub all: bool,
    pub scope: Option<String>,
    /// Only the threads of this agent: opener, attached, or report-to.
    pub involving: Option<String>,
}

/// The threads of a filter, without messages: the ones that wait for the
/// operator first, then the newest activity first. At most [`LIST_MAX`].
pub fn list(conn: &Connection, f: &Filter) -> ApiResult<Vec<Thread>> {
    let mut out = list_unbounded(conn, f)?;
    out.truncate(LIST_MAX);
    Ok(out)
}

fn list_unbounded(conn: &Connection, f: &Filter) -> ApiResult<Vec<Thread>> {
    let status = f.status.map(ThreadStatus::as_str);
    let mut st = conn.prepare(
        "SELECT * FROM threads
         WHERE (?1 IS NULL OR status = ?1)
           AND (?1 IS NOT NULL OR ?2 OR status != 'closed')
           AND (?3 IS NULL OR scope = ?3)
           AND (?4 IS NULL OR agent = ?4 OR report_to = ?4 OR opened_by = ?4)",
    )?;
    let rows = st
        .query_map(params![status, f.all, f.scope, f.involving], |r| {
            row(conn, r)
        })?
        .collect::<Result<Vec<_>, _>>()?;
    let mut out = rows
        .into_iter()
        .map(|t| summarised(conn, t))
        .collect::<ApiResult<Vec<_>>>()?;
    out.sort_by(|a, b| {
        let w = |t: &Thread| t.status != ThreadStatus::WaitingOperator;
        w(a).cmp(&w(b)).then_with(|| b.last_at.cmp(&a.last_at))
    });
    Ok(out)
}

/// Every thread that is not closed, and the closed ones with `closed_too`,
/// with no limit (for the Inbox).
pub fn list_all(conn: &Connection, closed_too: bool) -> ApiResult<Vec<Thread>> {
    list_unbounded(
        conn,
        &Filter {
            all: closed_too,
            ..Filter::default()
        },
    )
}

/// The lead of a project (D34): its project_admin agent named `lead`, else
/// its first project_admin by name. `None` when it has none.
pub fn lead_of(conn: &Connection, project: &str) -> ApiResult<Option<String>> {
    Ok(conn
        .query_row(
            "SELECT a.id FROM agents a JOIN roles r ON r.agent_id = a.id
             WHERE a.project = ?1 AND r.role = 'project_admin'
             ORDER BY a.name != 'lead', a.name LIMIT 1",
            [project],
            |r| r.get(0),
        )
        .optional()?)
}

fn check_text(what: &str, text: &str, max: usize) -> ApiResult<String> {
    let text = text.trim();
    if text.is_empty() {
        return Err(ApiError::invalid(format!("`{what}` is empty")));
    }
    if text.len() > max {
        return Err(ApiError::invalid(format!(
            "`{what}` has {} bytes; the most is {max}",
            text.len()
        )));
    }
    if text.contains('\0') {
        return Err(ApiError::invalid(format!("`{what}` holds a NUL byte")));
    }
    Ok(text.to_owned())
}

/// A new thread. The API layer checks the rights.
#[derive(Debug, Clone, Default)]
pub struct NewThread {
    pub topic: String,
    /// `global` or a project name.
    pub scope: String,
    pub template: Option<String>,
    /// `operator` or an agent ULID.
    pub opened_by: String,
    /// The attached agent (a ULID), if any.
    pub agent: Option<String>,
    /// The first message, by the opener.
    pub text: Option<String>,
}

/// Open a thread: check the fields, find the report-to lead (D34), write
/// the first message, emit the events, write the audit row.
pub fn open(t: &mut Tx<'_>, n: NewThread) -> ApiResult<Thread> {
    let topic = n.topic.trim().to_owned();
    if topic.is_empty() {
        return Err(ApiError::invalid("`topic` is empty"));
    }
    if topic.chars().count() > MAX_TOPIC || topic.contains(['\n', '\r', '\0']) {
        return Err(ApiError::invalid(format!(
            "`topic` is one line of at most {MAX_TOPIC} characters"
        )));
    }
    let scope = match n.scope.trim() {
        "" => GLOBAL.to_owned(),
        s => s.to_owned(),
    };
    let lead_project = if scope == GLOBAL {
        GLOBAL_LEAD_PROJECT.to_owned()
    } else {
        projects::get(t, &scope)?;
        scope.clone()
    };
    let template = n
        .template
        .map(|s| s.trim().to_owned())
        .filter(|s| !s.is_empty());
    if let Some(tp) = &template
        && (tp.chars().count() > MAX_TEMPLATE || tp.contains(['\n', '\r', '\0']))
    {
        return Err(ApiError::invalid(format!(
            "`template` is one line of at most {MAX_TEMPLATE} characters"
        )));
    }
    if let Some(a) = &n.agent {
        let v = agents::get(t, a)?;
        if scope != GLOBAL && v.agent.project != scope {
            return Err(ApiError::invalid(format!(
                "agent {}/{} is not in project `{scope}`: a project thread's agent is of its \
                 project",
                v.agent.project, v.agent.name
            )));
        }
    }
    let report_to = lead_of(t, &lead_project)?.ok_or_else(|| {
        ApiError::conflict(format!(
            "project `{lead_project}` has no lead (a project_admin agent), so a `{scope}` \
             thread has nobody to report to (D34)"
        ))
    })?;
    let text = n
        .text
        .as_deref()
        .map(|s| check_text("text", s, MAX_MESSAGE))
        .transpose()?
        .map(|s| t.redact_string(s));
    let topic = t.redact_string(topic);
    let status = if n.opened_by == "operator" && text.is_some() {
        ThreadStatus::WaitingAgent
    } else {
        ThreadStatus::WaitingOperator
    };
    let id = ulid::Ulid::new().to_string();
    let at = now();
    t.execute(
        "INSERT INTO threads (id, topic, scope, template, opened_by, agent, report_to, status,
                              created_at, updated_at)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?9)",
        params![
            id,
            topic,
            scope,
            template,
            n.opened_by,
            n.agent,
            report_to,
            status.as_str(),
            at
        ],
    )?;
    if let Some(text) = &text {
        insert_message(t, &id, &n.opened_by, "message", text, &at)?;
    }
    let th = get(t, &id)?;
    t.audit(
        "thread.open",
        &id,
        th.project(),
        json!({"topic": questions::short(&th.topic, 200), "scope": th.scope,
               "template": th.template, "agent": th.agent, "report_to": th.report_to}),
    )?;
    emit(t, &th, "opened")?;
    Ok(th)
}

fn insert_message(
    t: &mut Tx<'_>,
    thread: &str,
    author: &str,
    kind: &str,
    text: &str,
    at: &str,
) -> ApiResult<()> {
    t.execute(
        "INSERT INTO thread_messages (id, thread, author, kind, text, at)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
        params![
            ulid::Ulid::new().to_string(),
            thread,
            author,
            kind,
            text,
            at
        ],
    )?;
    Ok(())
}

fn not_closed(th: &Thread, what: &str) -> ApiResult<()> {
    if th.status == ThreadStatus::Closed {
        return Err(ApiError::conflict(format!(
            "thread {} is closed: {what} is not possible",
            th.id
        )));
    }
    Ok(())
}

fn set_status(t: &mut Tx<'_>, id: &str, status: ThreadStatus, at: &str) -> ApiResult<()> {
    t.execute(
        "UPDATE threads SET status = ?2, updated_at = ?3 WHERE id = ?1",
        params![id, status.as_str(), at],
    )?;
    Ok(())
}

/// Post a message: by the operator (then the agent must act) or by the
/// attached agent (then the operator must act).
pub fn post(t: &mut Tx<'_>, id: &str, author: &str, text: &str) -> ApiResult<Thread> {
    let th = get_bare(t, id)?;
    not_closed(&th, "a message")?;
    let text = t.redact_string(check_text("text", text, MAX_MESSAGE)?);
    let at = now();
    insert_message(t, &th.id, author, "message", &text, &at)?;
    let status = if author == "operator" {
        ThreadStatus::WaitingAgent
    } else {
        ThreadStatus::WaitingOperator
    };
    set_status(t, &th.id, status, &at)?;
    let th = get(t, &th.id)?;
    emit(t, &th, "message")?;
    Ok(th)
}

/// The operator's overall response: kept as `response` (the approval
/// record) and as a message of kind `response`.
pub fn respond(t: &mut Tx<'_>, id: &str, text: &str) -> ApiResult<Thread> {
    let th = get_bare(t, id)?;
    not_closed(&th, "a response")?;
    let text = t.redact_string(check_text("text", text, MAX_RESPONSE)?);
    let at = now();
    insert_message(t, &th.id, "operator", "response", &text, &at)?;
    let response = Response {
        text: text.clone(),
        by: "operator".into(),
        at: at.clone(),
    };
    t.execute(
        "UPDATE threads SET response = ?2, status = 'waiting_agent', updated_at = ?3
         WHERE id = ?1",
        params![th.id, json!(response).to_string(), at],
    )?;
    let th = get(t, &th.id)?;
    t.audit(
        "thread.respond",
        &th.id,
        th.project(),
        json!({"text": questions::short(&text, 200)}),
    )?;
    emit(t, &th, "response")?;
    Ok(th)
}

/// Close a thread with a handoff (summary and links). The open questions
/// of the thread are retracted first, so that none stays open in a closed
/// thread and the waits on them end.
pub fn close(
    t: &mut Tx<'_>,
    id: &str,
    by: &str,
    summary: Option<&str>,
    links: Vec<Link>,
) -> ApiResult<Thread> {
    let th = get_bare(t, id)?;
    not_closed(&th, "a close")?;
    let summary = match summary.map(str::trim).filter(|s| !s.is_empty()) {
        Some(s) => t.redact_string(check_text("summary", s, MAX_SUMMARY)?),
        None => String::new(),
    };
    if links.len() > MAX_LINKS {
        return Err(ApiError::invalid(format!(
            "a handoff has at most {MAX_LINKS} links; it has {}",
            links.len()
        )));
    }
    let mut checked = Vec::with_capacity(links.len());
    for (n, l) in links.into_iter().enumerate() {
        let kind = l.kind.trim().to_owned();
        if !LINK_KINDS.contains(&kind.as_str()) {
            return Err(ApiError::invalid(format!(
                "link {}: bad kind `{kind}`: use {}",
                n + 1,
                LINK_KINDS.join(", ")
            )));
        }
        let reference = l.reference.trim().to_owned();
        if reference.is_empty()
            || reference.chars().count() > MAX_REF
            || reference.contains(['\n', '\r', '\0'])
        {
            return Err(ApiError::invalid(format!(
                "link {}: `ref` is one line of 1 to {MAX_REF} characters",
                n + 1
            )));
        }
        checked.push(Link {
            kind,
            reference: t.redact_string(reference),
        });
    }
    let open: Vec<String> = {
        let mut st = t.prepare(
            "SELECT id FROM questions WHERE thread = ?1 AND status = 'open'
             ORDER BY created_at, id",
        )?;
        st.query_map([&th.id], |r| r.get(0))?
            .collect::<Result<Vec<_>, _>>()?
    };
    let by_owner = th.agent.as_deref() == Some(by);
    let reason = format!("thread {} was closed", th.id);
    for q in &open {
        questions::retract(t, q, by_owner, Some(&reason))?;
    }
    let at = now();
    let handoff = Handoff {
        summary,
        links: checked,
        by: by.to_owned(),
        at: at.clone(),
    };
    t.execute(
        "UPDATE threads SET status = 'closed', handoff = ?2, closed_at = ?3, closed_by = ?4,
                            updated_at = ?3
         WHERE id = ?1",
        params![th.id, json!(handoff).to_string(), at, by],
    )?;
    let th = get(t, &th.id)?;
    t.audit(
        "thread.close",
        &th.id,
        th.project(),
        json!({"by": by, "links": th.handoff.as_ref().map_or(0, |h| h.links.len()),
               "retracted": open}),
    )?;
    emit(t, &th, "closed")?;
    Ok(th)
}

/// Check that `agent` may ask a question in thread `id`: it is the
/// attached agent, and the thread is open.
pub fn check_ask(conn: &Connection, id: &str, agent: &str) -> ApiResult<Thread> {
    let th = get_bare(conn, id)?;
    if th.agent.as_deref() != Some(agent) {
        return Err(ApiError::conflict(format!(
            "thread {} is not this agent's thread: only its attached agent asks in it",
            th.id
        )));
    }
    if th.status == ThreadStatus::Closed {
        return Err(ApiError::conflict(format!(
            "thread {} is closed: ask in an open thread",
            th.id
        )));
    }
    Ok(th)
}

/// A question of thread `id` changed (`change` as in the `question`
/// events): an ask makes the thread wait for the operator; the settling of
/// its last open question makes it wait for the agent. A closed thread does
/// not change.
pub fn on_question(t: &mut Tx<'_>, id: &str, q: &Question, change: &str) -> ApiResult<()> {
    let Ok(th) = get_bare(t, id) else {
        return Ok(());
    };
    if th.status == ThreadStatus::Closed {
        return Ok(());
    }
    let at = now();
    let status = if change == "asked" {
        Some(ThreadStatus::WaitingOperator)
    } else if q.status != QuestionStatus::Open && open_questions(t, &th.id)? == 0 {
        Some(ThreadStatus::WaitingAgent)
    } else {
        None
    };
    match status {
        Some(s) => set_status(t, &th.id, s, &at)?,
        None => {
            t.execute(
                "UPDATE threads SET updated_at = ?2 WHERE id = ?1",
                params![th.id, at],
            )?;
        }
    }
    let th = get_bare(t, &th.id)?;
    emit(t, &th, "question")
}

/// The `thread` and `inbox` events of a change.
fn emit(t: &mut Tx<'_>, th: &Thread, change: &str) -> ApiResult<()> {
    let data: Value = json!({"change": change, "id": th.id, "scope": th.scope,
                             "status": th.status});
    let mut draft = EventDraft::new(
        "thread",
        format!(
            "thread {} {change}: {}",
            th.id,
            questions::short(&th.topic, 80)
        ),
        data,
    );
    if let Some(p) = th.project() {
        draft = draft.project(p.to_owned());
    }
    t.emit(draft)?;
    let inbox_change = match (change, th.status) {
        ("opened", _) => "opened",
        (_, ThreadStatus::Closed) => "closed",
        _ => "updated",
    };
    crate::store::inbox::emit(t, &th.item_id(), "thread", inbox_change, th.project())
}
