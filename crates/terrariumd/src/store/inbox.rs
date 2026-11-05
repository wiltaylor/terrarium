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

/// The kind of an Inbox item.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum InboxKind {
    Question,
    Notice,
    Mail,
    System,
    Watch,
}

impl InboxKind {
    pub fn parse(s: &str) -> ApiResult<Self> {
        match s.trim() {
            "question" => Ok(Self::Question),
            "notice" => Ok(Self::Notice),
            "mail" => Ok(Self::Mail),
            "system" => Ok(Self::System),
            "watch" => Ok(Self::Watch),
            other => Err(ApiError::invalid(format!(
                "bad kind `{other}`: use question, notice, mail, system, or watch"
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
    /// `thread:<thread id>`, or `watch:<thread id>`.
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
}

/// The filters of the Inbox.
#[derive(Debug, Clone, Default)]
pub struct Filter {
    pub status: Status,
    /// One kind. `None`: every kind.
    pub kind: Option<InboxKind>,
    pub project: Option<String>,
}

/// The most items of one kind in a list.
const KIND_MAX: usize = 200;

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
        QuestionKind::Question | QuestionKind::Bundle => InboxKind::Question,
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
        "{WATCH_SELECT} ORDER BY w.created_at DESC, w.thread_id DESC LIMIT {}",
        KIND_MAX * 5
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
        questions::list(
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
            QuestionKind::Question | QuestionKind::Bundle => QuestionStatus::Answered,
            QuestionKind::Notice => QuestionStatus::Acknowledged,
        };
        out.extend(take(Some(done))?);
        out.extend(take(Some(QuestionStatus::Retracted))?);
    }
    Ok(out.into_iter().map(question_item).collect())
}

/// The Inbox: critical first, then urgent, then the newest first. The
/// open items come before the done ones in a list of both.
pub fn list(conn: &Connection, f: &Filter) -> ApiResult<Vec<InboxItem>> {
    let mut items = Vec::new();
    if wants(f, InboxKind::Question) {
        items.extend(question_items(conn, f, QuestionKind::Question)?);
        items.extend(question_items(conn, f, QuestionKind::Bundle)?);
    }
    if wants(f, InboxKind::Notice) {
        items.extend(question_items(conn, f, QuestionKind::Notice)?);
    }
    if wants(f, InboxKind::Mail) {
        let mut mail_rows = Vec::new();
        if f.status.open() {
            mail_rows.extend(
                mail::operator_mail(conn, true, KIND_MAX * 5)?
                    .into_iter()
                    .map(|m| mail_item(m, true)),
            );
        }
        if f.status.done() {
            mail_rows.extend(
                mail::operator_mail(conn, false, KIND_MAX * 5)?
                    .into_iter()
                    .map(|m| mail_item(m, false)),
            );
        }
        items.extend(
            mail_rows
                .into_iter()
                .filter(|i| in_project(f, i))
                .take(KIND_MAX * 2),
        );
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
    // A mail on a watched thread names the watch that it answers.
    if items.iter().any(|i| i.kind == InboxKind::Mail) {
        let by_thread: HashMap<String, Watch> = watches(conn)?
            .into_iter()
            .map(|w| (w.thread_id.clone(), w))
            .collect();
        for i in items.iter_mut().filter(|i| i.kind == InboxKind::Mail) {
            if let Some(m) = &i.message {
                i.watch = by_thread.get(&m.thread_id).cloned();
            }
        }
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
}

/// The counts of the open items, of every project or of one.
pub fn counts(conn: &Connection, project: Option<&str>) -> ApiResult<Counts> {
    let q = questions::open_total(conn, project, Some(QuestionKind::Question))?;
    let b = questions::open_total(conn, project, Some(QuestionKind::Bundle))?;
    let n = questions::open_total(conn, project, Some(QuestionKind::Notice))?;
    // A bundle counts once, as a question.
    let (question, notice) = (q.open + b.open, n.open);
    let f = Filter {
        status: Status::Open,
        kind: None,
        project: project.map(str::to_owned),
    };
    let mail = mail::operator_mail(conn, true, KIND_MAX * 5)?
        .into_iter()
        .map(|m| mail_item(m, true))
        .filter(|i| in_project(&f, i))
        .count();
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
    Ok(Counts {
        open: question + notice + mail + system,
        urgent: q.urgent + b.urgent + n.urgent,
        critical: q.critical + b.critical + n.critical,
        question,
        notice,
        mail,
        system,
        watch,
    })
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
