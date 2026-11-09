//! The Inbox of the operator (PLAN §12e, docs/api.md §6.9): one read model
//! of the things that wait for the operator. It is a view, not a table:
//!
//! - `question` and `notice`: the rows of `questions` ([`questions`]). A
//!   bundle of questions is an item of the kind `question`, with the ID
//!   `bundle:<id>`;
//! - `mail`: the mail to `operator` that the operator did not acknowledge
//!   (state `queued` or `delivered`). The acknowledgement marks it read;
//!   a reply on its thread marks it replied ([`mail`]);
//! - `system`: the threads of the loop guard: a thread that is paused, or
//!   that has a held message that the operator did not release
//!   ([`mail::guard_threads`]);
//! - `watch`: a question that the operator asked an agent, or a question
//!   of the Inbox that the operator forwarded to one ([`ask`], the table
//!   `inbox_watches`). It is open while the newest message of its thread
//!   is the operator's: it waits for the agent, not for the operator, so
//!   the badge does not count it. The agent's answer is a `mail` item.
//!
//! No mail body is copied: an item holds the question or the message as
//! the stores give it. Each change of an item emits one `inbox` event
//! ([`emit`]), so the dashboard badge follows it.

use std::collections::HashMap;

use rusqlite::{Connection, OptionalExtension, params};
use serde::Serialize;
use serde_json::json;

use crate::db::{Tx, now};
use crate::error::{ApiError, ApiResult};
use crate::events::EventDraft;
use crate::store::mail::{self, Message};
use crate::store::questions::{self, Priority, Question, QuestionKind, QuestionStatus};
use crate::store::threads::{self, Thread, ThreadStatus};

/// The kind of an Inbox item.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum InboxKind {
    Question,
    Notice,
    Mail,
    System,
    Watch,
    /// An Inbox thread (TER-91).
    Thread,
}

impl InboxKind {
    pub fn parse(s: &str) -> ApiResult<Self> {
        match s.trim() {
            "question" => Ok(Self::Question),
            "notice" => Ok(Self::Notice),
            "mail" => Ok(Self::Mail),
            "system" => Ok(Self::System),
            "watch" => Ok(Self::Watch),
            "thread" => Ok(Self::Thread),
            other => Err(ApiError::invalid(format!(
                "bad kind `{other}`: use question, notice, mail, system, watch, or thread"
            ))),
        }
    }
}

/// The open items, the done items, or both.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Status {
    #[default]
    Open,
    Done,
    All,
}

impl Status {
    pub fn parse(s: &str) -> ApiResult<Self> {
        match s.trim() {
            "" | "open" => Ok(Self::Open),
            "done" => Ok(Self::Done),
            "all" => Ok(Self::All),
            other => Err(ApiError::invalid(format!(
                "bad status `{other}`: use open, done, or all"
            ))),
        }
    }

    fn open(self) -> bool {
        self != Self::Done
    }

    fn done(self) -> bool {
        self != Self::Open
    }
}

/// A thread of the loop guard, as a system item.
#[derive(Debug, Clone, Serialize)]
pub struct SystemItem {
    /// Always `paused_thread`.
    pub kind: &'static str,
    pub thread_id: String,
    pub subject: String,
    /// The loop guard pauses the thread now.
    pub paused: bool,
    /// The agent-to-agent messages since the last reset.
    pub agent_run: usize,
    /// `TERRARIUM_MAIL_LOOP_LIMIT`.
    pub limit: usize,
    /// The participants, in readable form.
    pub participants: Vec<String>,
    /// The held messages that the operator did not release.
    pub held: Vec<Message>,
}

/// A watched thread: a question that the operator asked an agent, or
/// forwarded to one.
#[derive(Debug, Clone, Serialize)]
pub struct Watch {
    pub thread_id: String,
    /// The agent that answers: its ULID.
    pub agent: String,
    /// `project/name`.
    pub agent_name: String,
    /// The forwarded question, if any.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub question_id: Option<String>,
    /// The first message: the ask.
    pub message_id: String,
    pub subject: String,
    pub created_at: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub cancelled_at: Option<String>,
    /// The newest message of the thread is the operator's: the watch waits
    /// for the agent.
    pub waiting: bool,
    /// The messages of the agent on the thread.
    pub replies: usize,
}

impl Watch {
    pub fn open(&self) -> bool {
        self.waiting && self.cancelled_at.is_none()
    }

    fn project(&self) -> Option<String> {
        self.agent_name.split_once('/').map(|(p, _)| p.to_owned())
    }
}

/// One item of the Inbox.
#[derive(Debug, Clone, Serialize)]
pub struct InboxItem {
    /// `question:<id>`, `bundle:<id>`, `notice:<id>`, `mail:<message id>`,
    /// `thread:<thread id>`, `watch:<thread id>`, or `inbox-thread:<id>`.
    pub id: String,
    pub kind: InboxKind,
    pub open: bool,
    /// Open, with the priority `urgent` or `critical`.
    pub urgent: bool,
    /// Open, with the priority `critical`: the operator must act now.
    pub critical: bool,
    pub project: Option<String>,
    /// When the item came in: the sort key.
    pub at: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub question: Option<Question>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub message: Option<Message>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub system: Option<SystemItem>,
    /// A `watch` item: the watch. A `mail` item on a watched thread: the
    /// watch that the mail answers.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub watch: Option<Watch>,
    /// A `thread` item: the thread as a list shows it (TER-91).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub thread: Option<Thread>,
}

/// The filters of the Inbox.
#[derive(Debug, Clone, Default)]
pub struct Filter {
    pub status: Status,
    /// One kind. `None`: every kind.
    pub kind: Option<InboxKind>,
    pub project: Option<String>,
}

/// The project of a message: the project of its sender.
fn sender_project(m: &Message) -> Option<String> {
    match m.from_kind.as_str() {
        "project" => m.from.strip_prefix("project:").map(str::to_owned),
        "agent" => m
            .from_name
            .split_once('/')
            .map(|(p, _)| p.to_owned())
            .filter(|p| !p.is_empty()),
        _ => None,
    }
}

fn question_item(q: Question) -> InboxItem {
    let kind = match q.kind {
        QuestionKind::Question | QuestionKind::Bundle | QuestionKind::ToolRun => {
            InboxKind::Question
        }
        QuestionKind::Notice => InboxKind::Notice,
    };
    InboxItem {
        id: q.item_id(),
        kind,
        open: q.status == QuestionStatus::Open,
        urgent: q.status == QuestionStatus::Open && q.priority.at_least_urgent(),
        critical: q.status == QuestionStatus::Open && q.priority == Priority::Critical,
        project: Some(q.project.clone()),
        at: q.created_at.clone(),
        question: Some(q),
        message: None,
        system: None,
        watch: None,
        thread: None,
    }
}

fn mail_item(m: Message, open: bool) -> InboxItem {
    InboxItem {
        id: format!("mail:{}", m.id),
        kind: InboxKind::Mail,
        open,
        urgent: false,
        critical: false,
        project: sender_project(&m),
        at: m.created_at.clone(),
        question: None,
        message: Some(m),
        system: None,
        watch: None,
        thread: None,
    }
}

/// The system item of `thread`.
fn system_item(conn: &Connection, thread: &str) -> ApiResult<InboxItem> {
    let held = mail::unreleased_held(conn, thread)?;
    let paused = mail::thread_paused(conn, thread)?;
    let (agent_run, limit) = mail::loop_state(conn, thread)?;
    let participants = mail::participants(conn, thread)?;
    let project = participants
        .iter()
        .find_map(|p| p.project().filter(|p| !p.is_empty()).map(str::to_owned));
    let first = conn.query_row(
        "SELECT coalesce(subject, ''), created_at FROM messages WHERE thread_id = ?1
         ORDER BY id LIMIT 1",
        [thread],
        |r| Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?)),
    )?;
    // The item came in with its first held message that is not released,
    // else with the last held message (a pause after releases).
    let at = match held.first() {
        Some(m) => m.created_at.clone(),
        None => conn
            .query_row(
                "SELECT max(created_at) FROM messages WHERE thread_id = ?1 AND held = 1",
                [thread],
                |r| r.get::<_, Option<String>>(0),
            )?
            .unwrap_or(first.1),
    };
    Ok(InboxItem {
        id: format!("thread:{thread}"),
        kind: InboxKind::System,
        open: true,
        urgent: false,
        critical: false,
        project,
        at,
        question: None,
        message: None,
        system: Some(SystemItem {
            kind: "paused_thread",
            thread_id: thread.to_owned(),
            subject: first.0,
            paused,
            agent_run,
            limit,
            participants: participants.iter().map(mail::Address::display).collect(),
            held,
        }),
        watch: None,
        thread: None,
    })
}

fn watch_item(w: Watch) -> InboxItem {
    InboxItem {
        id: format!("watch:{}", w.thread_id),
        kind: InboxKind::Watch,
        open: w.open(),
        urgent: false,
        critical: false,
        project: w.project(),
        at: w.created_at.clone(),
        question: None,
        message: None,
        system: None,
        watch: Some(w),
        thread: None,
    }
}

const WATCH_SELECT: &str = "SELECT w.thread_id, w.agent, w.question_id, w.message_id, w.created_at,
        w.cancelled_at,
        coalesce((SELECT subject FROM messages WHERE id = w.message_id), ''),
        (SELECT from_kind FROM messages WHERE thread_id = w.thread_id ORDER BY id DESC LIMIT 1),
        (SELECT count(*) FROM messages WHERE thread_id = w.thread_id AND from_agent = w.agent),
        coalesce((SELECT project || '/' || name FROM agents WHERE id = w.agent),
                 (SELECT project || '/' || name FROM agents_removed WHERE id = w.agent), w.agent)
     FROM inbox_watches w";

fn watch_row(r: &rusqlite::Row<'_>) -> rusqlite::Result<Watch> {
    let last: Option<String> = r.get(7)?;
    Ok(Watch {
        thread_id: r.get(0)?,
        agent: r.get(1)?,
        question_id: r.get(2)?,
        message_id: r.get(3)?,
        created_at: r.get(4)?,
        cancelled_at: r.get(5)?,
        subject: r.get(6)?,
        waiting: last.as_deref() == Some("operator"),
        replies: r.get::<_, i64>(8)? as usize,
        agent_name: r.get(9)?,
    })
}

/// Every watch, newest first.
pub fn watches(conn: &Connection) -> ApiResult<Vec<Watch>> {
    let mut st = conn.prepare(&format!(
        "{WATCH_SELECT} ORDER BY w.created_at DESC, w.thread_id DESC"
    ))?;
    let rows = st
        .query_map([], watch_row)?
        .collect::<Result<Vec<_>, _>>()?;
    Ok(rows)
}

/// The watch of `thread`, if it is watched.
pub fn watch_of(conn: &Connection, thread: &str) -> ApiResult<Option<Watch>> {
    Ok(conn
        .query_row(
            &format!("{WATCH_SELECT} WHERE w.thread_id = ?1"),
            [thread],
            watch_row,
        )
        .optional()?)
}

/// The most bytes of a forwarded context in the mail.
const FORWARD_CONTEXT_MAX: usize = 4096;

/// Cut `s` at `max` bytes, on a char boundary, with a note.
fn cut(s: &str, max: usize) -> String {
    if s.len() <= max {
        return s.to_owned();
    }
    let mut end = max;
    while !s.is_char_boundary(end) {
        end -= 1;
    }
    format!("{}\n[cut at {max} bytes]", &s[..end])
}

/// The subject and the body of the mail of an ask: `text`, and the quoted
/// question `q` of a forward.
fn ask_mail(text: &str, q: Option<&Question>) -> (String, String) {
    let first = |s: &str| -> String {
        let line = s
            .lines()
            .find(|l| !l.trim().is_empty())
            .unwrap_or("")
            .trim();
        if line.chars().count() > 80 {
            line.chars().take(79).collect::<String>() + "…"
        } else {
            line.to_owned()
        }
    };
    let mut body = String::new();
    let subject = match q {
        None => {
            body.push_str(text.trim());
            body.push_str("\n\n");
            format!("Question from the operator: {}", first(text))
        }
        Some(q) => {
            if !text.trim().is_empty() {
                body.push_str(text.trim());
                body.push_str("\n\n");
            }
            let what = match q.kind {
                QuestionKind::Notice => "notice",
                QuestionKind::Bundle => "bundle of questions",
                QuestionKind::Question => "question",
                QuestionKind::ToolRun => "tool run approval",
            };
            body.push_str(&format!(
                "The operator forwards this {what} of {} to you",
                q.agent_name
            ));
            if let Some(c) = &q.card {
                body.push_str(&format!(" (card {c})"));
            }
            body.push_str(":\n\n");
            for l in q.text.lines() {
                body.push_str(&format!("> {l}\n"));
            }
            for (i, o) in q.options.iter().enumerate() {
                body.push_str(&format!("> {}. {o}\n", i + 1));
            }
            for (i, it) in q.items.iter().enumerate() {
                body.push_str(&format!("> Item {}: {}\n", i + 1, it.text));
                for (j, o) in it.options.iter().enumerate() {
                    body.push_str(&format!(">   {}. {o}\n", j + 1));
                }
            }
            if let Some(c) = q.context.as_deref().filter(|c| !c.trim().is_empty()) {
                body.push_str(">\n> Context:\n");
                for l in cut(c, FORWARD_CONTEXT_MAX).lines() {
                    body.push_str(&format!("> {l}\n"));
                }
            }
            body.push('\n');
            format!("Forwarded {what} from {}: {}", q.agent_name, first(&q.text))
        }
    };
    body.push_str(
        "Answer the operator with `terrarium mail reply <this message>`: an \
         acknowledgement, the answer, or your questions.\n",
    );
    (subject, body)
}

/// Ask agent `to` (a key that `mail::resolve` takes) a question as the
/// operator, or forward question `question` to it, and watch the thread.
/// `text` is the question, or the operator's note on the forward. The
/// mail is a new thread from `operator`.
pub fn ask(
    t: &mut Tx<'_>,
    to: &str,
    text: &str,
    question: Option<&str>,
) -> ApiResult<(Message, Watch)> {
    let addr = mail::resolve(t, to, None)?;
    let mail::Address::Agent { id: agent, .. } = &addr else {
        return Err(ApiError::invalid(format!(
            "`{to}` is not an agent: ask an agent"
        )));
    };
    let agent = agent.clone();
    let q = question.map(|id| questions::get(t, id)).transpose()?;
    if q.is_none() && text.trim().is_empty() {
        return Err(ApiError::invalid("the question text is empty"));
    }
    let (subject, body) = ask_mail(text, q.as_ref());
    let m = mail::send(
        t,
        mail::Outgoing {
            from: &mail::Address::Operator,
            to: &addr,
            subject: &subject,
            body: &body,
            thread: None,
            in_reply_to: None,
        },
    )?;
    t.execute(
        "INSERT INTO inbox_watches (thread_id, agent, question_id, message_id, created_at)
         VALUES (?1, ?2, ?3, ?4, ?5)",
        params![
            m.thread_id,
            agent,
            q.as_ref().map(|q| q.id.clone()),
            m.id,
            now()
        ],
    )?;
    let w = watch_of(t, &m.thread_id)?
        .ok_or_else(|| ApiError::internal("the new watch is not in the store"))?;
    emit(
        t,
        &format!("watch:{}", w.thread_id),
        "watch",
        "opened",
        w.project().as_deref(),
    )?;
    Ok((m, w))
}

/// Stop the watch of `thread`. An unknown thread is `404`; a cancelled
/// one `409`.
pub fn cancel_watch(t: &mut Tx<'_>, thread: &str) -> ApiResult<Watch> {
    let w = watch_of(t, thread)?
        .ok_or_else(|| ApiError::not_found(format!("no watch of thread `{thread}`")))?;
    if w.cancelled_at.is_some() {
        return Err(ApiError::conflict(format!(
            "the watch of thread {thread} is cancelled already"
        )));
    }
    t.execute(
        "UPDATE inbox_watches SET cancelled_at = ?2 WHERE thread_id = ?1",
        params![thread, now()],
    )?;
    if w.open() {
        emit(
            t,
            &format!("watch:{thread}"),
            "watch",
            "closed",
            w.project().as_deref(),
        )?;
    }
    watch_of(t, thread)?.ok_or_else(|| ApiError::internal("the watch is gone"))
}

/// A message on `thread` from `from` to `to` was stored: a watch of the
/// thread closes with the answer of its agent and opens again with the
/// operator's next message. `mail::send` calls it.
pub fn on_thread_mail(
    t: &mut Tx<'_>,
    thread: &str,
    from: &mail::Address,
    to: &mail::Address,
) -> ApiResult<()> {
    let Some(w) = watch_of(t, thread)? else {
        return Ok(());
    };
    if w.cancelled_at.is_some() {
        return Ok(());
    }
    let change = match (from, to) {
        (mail::Address::Operator, mail::Address::Agent { id, .. }) if *id == w.agent => "opened",
        (mail::Address::Agent { id, .. }, mail::Address::Operator) if *id == w.agent => "closed",
        _ => return Ok(()),
    };
    emit(
        t,
        &format!("watch:{thread}"),
        "watch",
        change,
        w.project().as_deref(),
    )
}

fn wants(f: &Filter, kind: InboxKind) -> bool {
    f.kind.is_none_or(|k| k == kind)
}

fn in_project(f: &Filter, item: &InboxItem) -> bool {
    match &f.project {
        Some(p) => item.project.as_deref() == Some(p.as_str()),
        None => true,
    }
}

/// The questions and notices of the filter.
fn question_items(conn: &Connection, f: &Filter, kind: QuestionKind) -> ApiResult<Vec<InboxItem>> {
    let mut out = Vec::new();
    let take = |status: Option<QuestionStatus>| -> ApiResult<Vec<Question>> {
        questions::list_inbox(
            conn,
            &questions::Filter {
                project: f.project.clone(),
                kind: Some(kind),
                status,
                ..questions::Filter::default()
            },
        )
    };
    if f.status.open() {
        out.extend(take(Some(QuestionStatus::Open))?);
    }
    if f.status.done() {
        let done = match kind {
            QuestionKind::Question | QuestionKind::Bundle | QuestionKind::ToolRun => {
                QuestionStatus::Answered
            }
            QuestionKind::Notice => QuestionStatus::Acknowledged,
        };
        out.extend(take(Some(done))?);
        out.extend(take(Some(QuestionStatus::Retracted))?);
    }
    // A question of a thread shows in its thread item, not on its own.
    Ok(out
        .into_iter()
        .filter(|q| q.thread.is_none())
        .map(question_item)
        .collect())
}

/// The item of an Inbox thread: open while it waits for the operator or
/// has an open question (its questions are no items of their own); urgent
/// or critical by its open questions.
fn thread_item(th: Thread, urgency: (bool, bool)) -> InboxItem {
    let open = th.status == ThreadStatus::WaitingOperator
        || (th.status != ThreadStatus::Closed && th.open_questions.unwrap_or(0) > 0);
    let (urgent, critical) = if open { urgency } else { (false, false) };
    InboxItem {
        id: th.item_id(),
        kind: InboxKind::Thread,
        open,
        urgent,
        critical,
        project: th.project().map(str::to_owned),
        at: th.last_at.clone().unwrap_or_else(|| th.updated_at.clone()),
        question: None,
        message: None,
        system: None,
        watch: None,
        thread: Some(th),
    }
}

/// The thread items of the filter.
fn thread_items(conn: &Connection, f: &Filter) -> ApiResult<Vec<InboxItem>> {
    let mut out = Vec::new();
    let urgency = threads::inbox_urgency(conn)?;
    for th in threads::list_all(conn, f.status.done())? {
        let flags = urgency.get(&th.id).copied().unwrap_or_default();
        let item = thread_item(th, flags);
        if ((item.open && f.status.open()) || (!item.open && f.status.done()))
            && in_project(f, &item)
        {
            out.push(item);
        }
    }
    Ok(out)
}

/// The Inbox: critical first, then urgent, then the newest first. The
/// open items come before the done ones in a list of both.
pub fn list(conn: &Connection, f: &Filter) -> ApiResult<Vec<InboxItem>> {
    page(conn, f, None, usize::MAX)
}

type Cursor = (bool, bool, bool, String, String);

fn key(i: &InboxItem) -> Cursor {
    (i.open, i.critical, i.urgent, i.at.clone(), i.id.clone())
}

/// Keyset pagination, independent of whether the previous item still exists.
/// Only mail in the selected page loads bodies and resolves thread state.
pub fn page(
    conn: &Connection,
    f: &Filter,
    cursor: Option<&str>,
    limit: usize,
) -> ApiResult<Vec<InboxItem>> {
    let cursor: Option<Cursor> = cursor
        .map(|s| serde_json::from_str(s).map_err(|_| ApiError::invalid("invalid Inbox cursor")))
        .transpose()?;
    let mut items = Vec::new();
    if wants(f, InboxKind::Question) {
        items.extend(question_items(conn, f, QuestionKind::Question)?);
        items.extend(question_items(conn, f, QuestionKind::Bundle)?);
        items.extend(question_items(conn, f, QuestionKind::ToolRun)?);
    }
    if wants(f, InboxKind::Notice) {
        items.extend(question_items(conn, f, QuestionKind::Notice)?);
    }
    if wants(f, InboxKind::Mail) {
        let mut st = conn.prepare(
            "SELECT m.id, m.created_at, m.state IN ('queued','delivered'),
                    CASE m.from_kind WHEN 'project' THEN substr(m.from_agent,9)
                         WHEN 'agent' THEN coalesce(a.project, old.project) END AS project
             FROM messages m
             LEFT JOIN agents a ON m.from_kind = 'agent' AND a.id = m.from_agent
             LEFT JOIN agents_removed old ON m.from_kind = 'agent' AND old.id = m.from_agent
             WHERE m.to_kind = 'operator' AND m.state IN ('queued','delivered','read','replied')
               AND NOT (m.from_kind = 'system' AND coalesce(m.subject,'') LIKE ?1)
               AND (?2 IS NULL OR
                    (m.from_kind = 'project' AND m.from_agent = 'project:' || ?2) OR
                    (m.from_kind = 'agent' AND coalesce(a.project, old.project) = ?2))",
        )?;
        let rows = st.query_map(params![mail::PAUSE_SUBJECT_LIKE, f.project], |r| {
            Ok(InboxItem {
                id: format!("mail:{}", r.get::<_, String>(0)?),
                at: r.get(1)?,
                open: r.get(2)?,
                project: r.get(3)?,
                kind: InboxKind::Mail,
                urgent: false,
                critical: false,
                question: None,
                message: None,
                system: None,
                watch: None,
                thread: None,
            })
        })?;
        for row in rows {
            let item = row?;
            if (item.open && f.status.open()) || (!item.open && f.status.done()) {
                items.push(item);
            }
        }
    }
    if wants(f, InboxKind::Watch) {
        for w in watches(conn)? {
            let item = watch_item(w);
            if ((item.open && f.status.open()) || (!item.open && f.status.done()))
                && in_project(f, &item)
            {
                items.push(item);
            }
        }
    }
    if wants(f, InboxKind::Thread) {
        items.extend(thread_items(conn, f)?);
    }
    if wants(f, InboxKind::System) && f.status.open() {
        for thread in mail::guard_threads(conn)? {
            let item = system_item(conn, &thread)?;
            if in_project(f, &item) {
                items.push(item);
            }
        }
    }
    items.sort_by(|a, b| {
        b.open
            .cmp(&a.open)
            .then(b.critical.cmp(&a.critical))
            .then(b.urgent.cmp(&a.urgent))
            .then(b.at.cmp(&a.at))
            .then(b.id.cmp(&a.id))
    });
    items.retain(|i| cursor.as_ref().is_none_or(|c| key(i) < *c));
    items.truncate(limit);
    if items.iter().any(|i| i.kind == InboxKind::Mail) {
        let by_thread: HashMap<String, Watch> = watches(conn)?
            .into_iter()
            .map(|w| (w.thread_id.clone(), w))
            .collect();
        for i in items.iter_mut().filter(|i| i.kind == InboxKind::Mail) {
            let m = mail::get(conn, &i.id[5..])?;
            let watch = by_thread.get(&m.thread_id).cloned();
            *i = mail_item(m, i.open);
            i.watch = watch;
        }
    }
    Ok(items)
}

/// The open items of each kind, for the badge.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize)]
pub struct Counts {
    pub open: usize,
    /// The open urgent and critical items.
    pub urgent: usize,
    /// The open critical items only.
    pub critical: usize,
    pub question: usize,
    pub notice: usize,
    pub mail: usize,
    pub system: usize,
    /// The open watches: they wait for an agent, so `open` does not count
    /// them.
    pub watch: usize,
    /// The open thread items: waiting for the operator, or with an open
    /// question.
    pub thread: usize,
}

/// The counts of the open items, of every project or of one.
pub fn counts(conn: &Connection, project: Option<&str>) -> ApiResult<Counts> {
    let q = questions::inbox_open_total(conn, project, Some(QuestionKind::Question))?;
    let b = questions::inbox_open_total(conn, project, Some(QuestionKind::Bundle))?;
    let r = questions::inbox_open_total(conn, project, Some(QuestionKind::ToolRun))?;
    let n = questions::inbox_open_total(conn, project, Some(QuestionKind::Notice))?;
    // A bundle counts once, as a question; so does a tool run approval.
    let (question, notice) = (q.open + b.open + r.open, n.open);
    let f = Filter {
        status: Status::Open,
        kind: None,
        project: project.map(str::to_owned),
    };
    let mail = mail::operator_mail_count(conn, project)?;
    let mut system = 0;
    for thread in mail::guard_threads(conn)? {
        if in_project(&f, &system_item(conn, &thread)?) {
            system += 1;
        }
    }
    let watch = watches(conn)?
        .into_iter()
        .map(watch_item)
        .filter(|i| i.open && in_project(&f, i))
        .count();
    let (thread, thread_urgent, thread_critical) = threads::inbox_counts(conn, project)?;
    Ok(Counts {
        open: question + notice + mail + system + thread,
        urgent: q.urgent + b.urgent + r.urgent + n.urgent + thread_urgent,
        critical: q.critical + b.critical + r.critical + n.critical + thread_critical,
        question,
        notice,
        mail,
        system,
        watch,
        thread,
    })
}

/// An operator mail left the open list: its `mail` item of the Inbox
/// closes. `mail::mark_read`, `mail::mark_read_in_turn`, and `mail::reply`
/// call it with the message as it was before (GitHub #25): only a mail to
/// the operator that was `queued` or `delivered` was an open item, so only
/// that change closes one. The ack reaches the same stores, and mail to an
/// agent is no Inbox item.
pub fn mail_closed(t: &mut Tx<'_>, before: &Message) -> ApiResult<()> {
    if before.to_kind != "operator" || !matches!(before.state.as_str(), "queued" | "delivered") {
        return Ok(());
    }
    emit(
        t,
        &format!("mail:{}", before.id),
        "mail",
        "closed",
        sender_project(before).as_deref(),
    )
}

/// Emit an `inbox` event: `item` (`notice:<id>`, ...) of `kind` was
/// `opened`, `updated`, or `closed`.
pub fn emit(
    t: &mut Tx<'_>,
    item: &str,
    kind: &str,
    change: &str,
    project: Option<&str>,
) -> ApiResult<()> {
    let mut data = json!({"item": item, "kind": kind, "change": change});
    let mut draft_project = None;
    if let Some(p) = project.filter(|p| !p.is_empty()) {
        data["project"] = json!(p);
        draft_project = Some(p.to_owned());
    }
    let mut draft = EventDraft::new("inbox", format!("inbox: {kind} {item} {change}"), data);
    if let Some(p) = draft_project {
        draft = draft.project(p);
    }
    t.emit(draft)?;
    Ok(())
}
