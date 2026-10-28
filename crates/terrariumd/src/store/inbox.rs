//! The Inbox of the operator (PLAN §12e, docs/api.md §6.9): one read model
//! of the things that wait for the operator. It is a view, not a table:
//!
//! - `question` and `notice`: the rows of `questions` ([`questions`]);
//! - `mail`: the mail to `operator` that the operator did not acknowledge
//!   (state `queued` or `delivered`). The acknowledgement marks it read;
//!   a reply on its thread marks it replied ([`mail`]);
//! - `system`: the threads of the loop guard: a thread that is paused, or
//!   that has a held message that the operator did not release
//!   ([`mail::guard_threads`]).
//!
//! No mail body is copied: an item holds the question or the message as
//! the stores give it. Each change of an item emits one `inbox` event
//! ([`emit`]), so the dashboard badge follows it.

use rusqlite::Connection;
use serde::Serialize;
use serde_json::json;

use crate::db::Tx;
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
}

impl InboxKind {
    pub fn parse(s: &str) -> ApiResult<Self> {
        match s.trim() {
            "question" => Ok(Self::Question),
            "notice" => Ok(Self::Notice),
            "mail" => Ok(Self::Mail),
            "system" => Ok(Self::System),
            other => Err(ApiError::invalid(format!(
                "bad kind `{other}`: use question, notice, mail, or system"
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

/// One item of the Inbox.
#[derive(Debug, Clone, Serialize)]
pub struct InboxItem {
    /// `question:<id>`, `notice:<id>`, `mail:<message id>`, or
    /// `thread:<thread id>`.
    pub id: String,
    pub kind: InboxKind,
    pub open: bool,
    pub urgent: bool,
    pub project: Option<String>,
    /// When the item came in: the sort key.
    pub at: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub question: Option<Question>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub message: Option<Message>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub system: Option<SystemItem>,
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
        QuestionKind::Question => InboxKind::Question,
        QuestionKind::Notice => InboxKind::Notice,
    };
    InboxItem {
        id: format!("{}:{}", q.kind.as_str(), q.id),
        kind,
        open: q.status == QuestionStatus::Open,
        urgent: q.status == QuestionStatus::Open && q.priority == Priority::Urgent,
        project: Some(q.project.clone()),
        at: q.created_at.clone(),
        question: Some(q),
        message: None,
        system: None,
    }
}

fn mail_item(m: Message, open: bool) -> InboxItem {
    InboxItem {
        id: format!("mail:{}", m.id),
        kind: InboxKind::Mail,
        open,
        urgent: false,
        project: sender_project(&m),
        at: m.created_at.clone(),
        question: None,
        message: Some(m),
        system: None,
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
    })
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
            QuestionKind::Question => QuestionStatus::Answered,
            QuestionKind::Notice => QuestionStatus::Acknowledged,
        };
        out.extend(take(Some(done))?);
        out.extend(take(Some(QuestionStatus::Retracted))?);
    }
    Ok(out.into_iter().map(question_item).collect())
}

/// The Inbox: urgent first, then the newest first. The open items come
/// before the done ones in a list of both.
pub fn list(conn: &Connection, f: &Filter) -> ApiResult<Vec<InboxItem>> {
    let mut items = Vec::new();
    if wants(f, InboxKind::Question) {
        items.extend(question_items(conn, f, QuestionKind::Question)?);
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
    pub urgent: usize,
    pub question: usize,
    pub notice: usize,
    pub mail: usize,
    pub system: usize,
}

/// The counts of the open items, of every project or of one.
pub fn counts(conn: &Connection, project: Option<&str>) -> ApiResult<Counts> {
    let (question, uq) = questions::open_total(conn, project, Some(QuestionKind::Question))?;
    let (notice, un) = questions::open_total(conn, project, Some(QuestionKind::Notice))?;
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
    Ok(Counts {
        open: question + notice + mail + system,
        urgent: uq + un,
        question,
        notice,
        mail,
        system,
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
