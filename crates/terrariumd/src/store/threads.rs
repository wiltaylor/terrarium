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
//! - `tracking` (TER-145, TER-144 D1, D9): the agent handed the work off
//!   and moved the thread into tracking ([`track`]). Its agent is parked,
//!   and the reminder and the stale close do not apply. `tracking_at`
//!   stays set while the thread tracks: an operator post wakes the agent
//!   (`waiting_agent`), and the agent's reply puts the thread back into
//!   `tracking`, not `waiting_operator`.
//! - Linked cards (TER-146, TER-144 D2, [`link_card`]): cards of any
//!   project that the thread tracks (`thread_cards`). The linker reads the
//!   thread and the card's project; a link gives no read access to the
//!   card or the thread. `GET /api/threads/{id}` and the list show them
//!   (`cards`, [`list_for`]), only those of a project the reader reads.
//! - Wake rules (TER-148, TER-144 D4, [`on_card_moved`]): in a thread that
//!   tracks, a linked card that goes to the blocked column, or a move that
//!   leaves all linked cards in review or later, wakes the agent (the
//!   `thread` event `cards`). A thread in `tracking` goes to
//!   `waiting_agent` and back to `tracking` on the agent's reply.
//!   Starting tracking with ready cards, or archiving the last unfinished
//!   card, also checks review readiness. Card events never interrupt an
//!   operator wait, and replies preserve open questions.
//! - The overall response can be sent again (TER-148, D10): the newest
//!   replaces `response`, and each stays in the conversation as a message
//!   of kind `response`.
//! - No register and no conflict check between threads (D36).
//! - Events `thread` {change, id, scope, status}, and an `inbox` event for
//!   the item `inbox-thread:<id>`.
//!
//! The rights are in the API layer ([`crate::api::threads`]).

use std::collections::HashMap;

use rusqlite::{Connection, OptionalExtension, Row, params};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

use crate::actor::Actor;
use crate::db::{Tx, now};
use crate::error::{ApiError, ApiResult};
use crate::events::EventDraft;
use crate::policy;
use crate::store::questions::{self, Question, QuestionStatus};
use crate::store::{agents, board, projects, spawns};

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
    /// The agent tracks handed-off work: parked, no stale close (TER-145).
    Tracking,
    Closed,
}

impl ThreadStatus {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::WaitingOperator => "waiting_operator",
            Self::WaitingAgent => "waiting_agent",
            Self::Tracking => "tracking",
            Self::Closed => "closed",
        }
    }

    pub fn parse(s: &str) -> Option<Self> {
        match s {
            "waiting_operator" => Some(Self::WaitingOperator),
            "waiting_agent" => Some(Self::WaitingAgent),
            "tracking" => Some(Self::Tracking),
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
    #[serde(flatten)]
    pub session: crate::store::session_refs::SessionField,
}

/// A card that the thread tracks (TER-146, [`link_card`]). Only the IDs:
/// a reader of the thread need not read the card's project.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ThreadCard {
    pub project: String,
    pub card: String,
    /// `operator` or an agent ULID.
    pub linked_by: String,
    pub linked_by_name: String,
    pub linked_at: String,
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
    /// Version of the agent replacement, carried through asynchronous spawn.
    #[serde(default)]
    pub request_epoch: i64,
    /// A request mediated by a request agent, rather than direct mail.
    #[serde(default)]
    pub request: bool,
    pub id: String,
    /// The outcome of an agent that ended while this thread was open.
    #[serde(default)]
    pub agent_end_outcome: Option<String>,
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
    /// The linked cards, the oldest link first (TER-146), also in a list
    /// (TER-147: the n/m chip); absent in the Inbox items.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cards: Option<Vec<ThreadCard>>,
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
    /// The close that the attached agent proposes (TER-95): the handoff
    /// that the operator confirms. `None`: no proposal.
    #[serde(default)]
    pub close_proposal: Option<Handoff>,
    /// Why a closed thread closed: [`CloseReason`].
    #[serde(default)]
    pub close_reason: Option<String>,
    /// The session of the thread agent, archived when the agent ends at
    /// the close: `/sessions/<id>` opens it.
    #[serde(default)]
    pub session_id: Option<String>,
    /// The last Inbox reminder of the untouched thread.
    #[serde(default)]
    pub reminded_at: Option<String>,
    /// When the agent moved the thread into tracking ([`track`], TER-145).
    /// Set while the thread tracks, also while an operator post wakes
    /// the agent. `None`: the thread does not track.
    #[serde(default)]
    pub tracking_at: Option<String>,
    /// Optional ops monitor, as `project:name`.
    #[serde(default)]
    pub monitor: Option<String>,
    /// The question or notice that the operator moved into this thread
    /// (GitHub #30, [`questions::to_thread`]). Its agent is not bound to
    /// the thread, so each message and response of the operator also goes
    /// to it as mail ([`mail_agent`]).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub from_question: Option<String>,
}

/// Why a thread closed.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CloseReason {
    /// The operator closed it.
    Operator,
    /// The operator confirmed the agent's proposal.
    Confirmed,
    /// Untouched while it waited on the operator (the stale close).
    Stale,
    /// Its agent did not start.
    FailedStart,
    /// A later operator message reached a thread whose agent ended.
    AgentEnded,
}

impl CloseReason {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Operator => "operator",
            Self::Confirmed => "confirmed",
            Self::Stale => "stale",
            Self::FailedStart => "failed_start",
            Self::AgentEnded => "agent_ended",
        }
    }
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
    row_named(r, |key| name_of(conn, key))
}

fn row_named(r: &Row<'_>, mut name: impl FnMut(&str) -> String) -> rusqlite::Result<Thread> {
    let status: String = r.get("status")?;
    let response: Option<String> = r.get("response")?;
    let handoff: Option<String> = r.get("handoff")?;
    let opened_by: String = r.get("opened_by")?;
    let agent: Option<String> = r.get("agent")?;
    let report_to: Option<String> = r.get("report_to")?;
    Ok(Thread {
        request_epoch: r.get("request_epoch")?,
        request: r.get("request")?,
        id: r.get("id")?,
        agent_end_outcome: r.get("agent_end_outcome")?,
        topic: r.get("topic")?,
        scope: r.get("scope")?,
        template: r.get("template")?,
        opened_by_name: name(&opened_by),
        opened_by,
        agent_name: agent.as_deref().map(&mut name),
        agent,
        report_to_name: report_to.as_deref().map(&mut name),
        report_to,
        status: ThreadStatus::parse(&status).unwrap_or(ThreadStatus::Closed),
        messages: None,
        questions: None,
        cards: None,
        response: response.and_then(|s| serde_json::from_str(&s).ok()),
        handoff: handoff.and_then(|s| serde_json::from_str(&s).ok()),
        message_count: None,
        open_questions: None,
        last_at: None,
        created_at: r.get("created_at")?,
        updated_at: r.get("updated_at")?,
        closed_at: r.get("closed_at")?,
        closed_by: r.get("closed_by")?,
        close_proposal: r
            .get::<_, Option<String>>("close_proposal")?
            .and_then(|s| serde_json::from_str(&s).ok()),
        close_reason: r.get("close_reason")?,
        session_id: r.get("session_id")?,
        reminded_at: r.get("reminded_at")?,
        tracking_at: r.get("tracking_at")?,
        monitor: r.get("monitor")?,
        from_question: r.get("from_question")?,
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
    th.cards = Some(cards_of(conn, &th.id)?);
    Ok(th)
}

/// A thread that `actor` may read; else `404`: the operator, an agent
/// that reads all (sysadmin, architect), and the opener, the attached agent
/// and the report-to lead. Its `cards` are only those that `actor` may
/// read ([`visible`]).
pub fn readable(actor: &Actor, th: Thread) -> ApiResult<Thread> {
    match actor {
        Actor::Operator => Ok(th),
        Actor::Agent(a) if a.reads_all() || th.involves(&a.id) => Ok(visible(actor, th)),
        Actor::Agent(_) => Err(ApiError::not_found(format!("no thread `{}`", th.id))),
    }
}

/// Thread `th` with only the linked cards whose project `actor` may read
/// (TER-146), or every linked card for its attached open request agent
/// (TER-910). Other thread readers gain no project access from a link.
pub fn visible(actor: &Actor, mut th: Thread) -> Thread {
    let own_request = matches!(actor, Actor::Agent(a) if th.request
        && th.status != ThreadStatus::Closed
        && th.agent.as_deref() == Some(&a.id));
    if th.monitor.as_ref().is_some_and(|m| {
        m.split_once(':')
            .is_none_or(|(p, _)| policy::read_project(actor, p).is_err())
    }) {
        th.monitor = None;
    }
    if let Some(cards) = th.cards.as_mut() {
        cards.retain(|c| policy::read_project(actor, &c.project).is_ok() || own_request);
    }
    th
}

/// The linked cards of thread `thread`, the oldest link first.
fn cards_of(conn: &Connection, thread: &str) -> ApiResult<Vec<ThreadCard>> {
    let mut st = conn.prepare(
        "SELECT project, card, linked_by, linked_at FROM thread_cards WHERE thread = ?1
         ORDER BY linked_at, rowid",
    )?;
    let rows = st
        .query_map([thread], |r| {
            let by: String = r.get(2)?;
            Ok(ThreadCard {
                project: r.get(0)?,
                card: r.get(1)?,
                linked_by_name: name_of(conn, &by),
                linked_by: by,
                linked_at: r.get(3)?,
            })
        })?
        .collect::<Result<Vec<_>, _>>()?;
    Ok(rows)
}

/// Card-only read access. An attached request agent may also read its
/// report-to lead's project while the request is open. For a link, `thread`
/// restricts that exception to the agent's own request. No project-wide
/// read or card-write capability is granted.
pub fn read_card_project(
    conn: &Connection,
    actor: &Actor,
    project: &str,
    thread: Option<&str>,
) -> ApiResult<()> {
    let denied = match policy::read_project(actor, project) {
        Ok(()) => return Ok(()),
        Err(e) => e,
    };
    if let Actor::Agent(a) = actor {
        let allowed: bool = conn.query_row(
            "SELECT EXISTS(SELECT 1 FROM threads th JOIN agents lead ON lead.id = th.report_to
             WHERE th.request = 1 AND th.agent = ?1 AND th.status != 'closed'
             AND lead.project = ?2 AND (?3 IS NULL OR th.id = ?3))",
            rusqlite::params![a.id, project, thread],
            |r| r.get(0),
        )?;
        if allowed {
            return Ok(());
        }
    }
    Err(denied)
}

/// Read-only TER-910 exception. Start at the indexed attached-agent lookup,
/// then the thread_cards primary key, never at a project's cards.
pub const REQUEST_CARDS_SQL: &str = "SELECT tc.card FROM threads th
 JOIN thread_cards tc ON tc.thread = th.id
 WHERE th.agent = ?1 AND th.request = 1 AND th.status != 'closed'
 AND tc.project = ?2";

pub fn read_linked_card(
    conn: &Connection,
    actor: &Actor,
    project: &str,
    card: Option<&str>,
) -> ApiResult<()> {
    let denied = match read_card_project(conn, actor, project, None) {
        Ok(()) => return Ok(()),
        Err(e) => e,
    };
    if let Actor::Agent(a) = actor {
        let sql = format!(
            "SELECT EXISTS(SELECT 1 FROM ({REQUEST_CARDS_SQL}) WHERE ?3 IS NULL OR card = ?3)"
        );
        let allowed: bool =
            conn.query_row(&sql, rusqlite::params![a.id, project, card], |r| r.get(0))?;
        if allowed {
            return Ok(());
        }
    }
    Err(denied)
}

/// Link `<project>:<card>` or `<card>` to a readable, open thread.
/// The card must be readable and unarchived. Cross-project request access
/// applies only to the attached agent's own thread.
pub fn link_card(t: &mut Tx<'_>, actor: &Actor, id: &str, card: &str) -> ApiResult<Thread> {
    let th = readable(actor, get_bare(t, id)?)?;
    not_closed(&th, "a card link")?;
    let (want, key) = match card.trim().split_once(':') {
        Some((p, c)) => (Some(p.trim()), c.trim()),
        None => (None, card.trim()),
    };
    if key.is_empty() || want == Some("") {
        return Err(ApiError::invalid(format!(
            "bad card `{card}`: use <project>:<card> or <card>, such as terrarium:TER-42"
        )));
    }
    // The project read right comes before the card is loaded, and its
    // refusal is the `404` of a missing card: a reader of the thread does
    // not learn that a card exists in a project it cannot read, nor which
    // project that is.
    let key = board::normalise_id(key);
    let missing = || ApiError::not_found(format!("no card `{key}`"));
    let (project, archived): (String, bool) = t
        .query_row(
            "SELECT project, archived FROM cards WHERE id = ?1",
            [&key],
            |r| Ok((r.get(0)?, r.get::<_, i64>(1)? != 0)),
        )
        .optional()?
        .ok_or_else(missing)?;
    read_card_project(t, actor, &project, Some(id)).map_err(|_| missing())?;
    if want.is_some_and(|p| p != project) {
        return Err(ApiError::not_found(format!(
            "no card `{key}` in project `{}`",
            want.unwrap_or_default()
        )));
    }
    if archived {
        return Err(ApiError::invalid(format!(
            "card {key} is archived: an archived card is not linked to a thread"
        )));
    }
    let c = board::get_card(t, &key)?;
    let at = now();
    let by = actor.address();
    let n = t.execute(
        "INSERT OR IGNORE INTO thread_cards (thread, project, card, linked_by, linked_at)
         VALUES (?1, ?2, ?3, ?4, ?5)",
        params![th.id, c.project, c.id, by, at],
    )?;
    if n > 0 {
        if !all_review_or_later(t, &th.id)? {
            t.execute(
                "UPDATE threads SET monitor_done = 0 WHERE id = ?1",
                [&th.id],
            )?;
        }
        // The row is in the card's project, not the thread's: a project
        // admin reads the audit rows of its own project (`audit_scope`),
        // and the admin of the thread's project may not read the card's
        // project. One row only, so that no row names the card where its
        // project is not readable.
        t.audit(
            "thread.card_link",
            &th.id,
            Some(c.project.as_str()),
            json!({"by": by, "card": c.id, "project": c.project}),
        )?;
        emit(t, &th, "card_linked")?;
    }
    Ok(visible(actor, get(t, &th.id)?))
}

/// The wake reason of a linked card that went to the blocked column
/// (TER-148, TER-144 D4).
pub const WAKE_BLOCKED: &str = "blocked";
/// The wake reason of a move that left all linked cards of a thread in
/// review or later (TER-148, TER-144 D4).
pub const WAKE_REVIEW: &str = "review";

/// The state of a linked card now, for the wake turn of the thread agent
/// (TER-148). Read from the card live.
#[derive(Debug, Clone, PartialEq)]
pub struct LinkedState {
    pub project: String,
    pub card: String,
    pub title: String,
    pub column: String,
    pub role: Option<String>,
    pub archived: bool,
    /// In review or later on its board ([`review_or_later`]).
    pub review_or_later: bool,
    /// The newest comment of the card, if any.
    pub last_comment: Option<String>,
}

/// Whether column `column` of the board `cols` (`(name, role)` in board
/// order) is in review or later: the column with the role `review` (else
/// the column named `review`) and every column after it, never the column
/// with the role `blocked`. A board with neither counts only `done`. The
/// same rule as the n/m chip of the dashboard (TER-147).
pub fn review_or_later(cols: &[(String, Option<String>)], column: &str) -> bool {
    let review = cols
        .iter()
        .position(|(_, r)| r.as_deref() == Some(board::REVIEW))
        .or_else(|| cols.iter().position(|(n, _)| n == board::REVIEW));
    let Some(at) = cols.iter().position(|(n, _)| n == column) else {
        return false;
    };
    if cols[at].1.as_deref() == Some(board::BLOCKED) {
        return false;
    }
    match review {
        Some(r) => at >= r,
        None => column == board::DONE,
    }
}

/// The linked cards of thread `thread` as they are now, the oldest link
/// first. A linked card that is gone is left out.
pub fn linked_states(conn: &Connection, thread: &str) -> ApiResult<Vec<LinkedState>> {
    let mut st = conn.prepare(
        "SELECT c.project, c.id, c.title, k.name, k.role, c.archived,
                (SELECT body FROM card_comments m WHERE m.card = c.id
                  ORDER BY m.created_at DESC, m.id DESC LIMIT 1)
         FROM thread_cards tc
         JOIN cards c ON c.id = tc.card
         JOIN board_columns k ON k.id = c.column_id
         WHERE tc.thread = ?1
         ORDER BY tc.linked_at, tc.rowid",
    )?;
    let rows = st
        .query_map([thread], |r| {
            Ok(LinkedState {
                project: r.get(0)?,
                card: r.get(1)?,
                title: r.get(2)?,
                column: r.get(3)?,
                role: r.get(4)?,
                archived: r.get::<_, i64>(5)? != 0,
                review_or_later: false,
                last_comment: r.get(6)?,
            })
        })?
        .collect::<Result<Vec<_>, _>>()?;
    let mut boards: std::collections::HashMap<String, Vec<(String, Option<String>)>> =
        std::collections::HashMap::new();
    let mut out = Vec::with_capacity(rows.len());
    for mut s in rows {
        if !boards.contains_key(&s.project) {
            let cols = board::column_roles(conn, &s.project)?;
            boards.insert(s.project.clone(), cols);
        }
        let cols = boards.get(&s.project).map(Vec::as_slice).unwrap_or(&[]);
        s.review_or_later = review_or_later(cols, &s.column);
        out.push(s);
    }
    Ok(out)
}

/// True when all live cards are in review or later and an optional monitor
/// is green. Requires a live card or a monitor, so monitor-only ops work too.
fn all_review_or_later(conn: &Connection, thread: &str) -> ApiResult<bool> {
    let monitor = get_bare(conn, thread)?.monitor;
    if !monitor_ready(conn, thread)? {
        return Ok(false);
    }
    let live: Vec<LinkedState> = linked_states(conn, thread)?
        .into_iter()
        .filter(|s| !s.archived)
        .collect();
    Ok((monitor.is_some() || !live.is_empty()) && live.iter().all(|s| s.review_or_later))
}

fn monitor_ready(conn: &Connection, thread: &str) -> ApiResult<bool> {
    let monitor = get_bare(conn, thread)?.monitor;
    if let Some(key) = &monitor {
        let (project, name) = key.split_once(':').unwrap_or(("", ""));
        let green: bool = conn.query_row(
            "SELECT EXISTS(SELECT 1 FROM monitors m WHERE m.project = ?1 AND m.name = ?2
             AND (SELECT status FROM monitor_results r WHERE r.monitor = m.id
                  ORDER BY r.pk DESC LIMIT 1) = 'ok')",
            params![project, name],
            |r| r.get(0),
        )?;
        if !green {
            return Ok(false);
        }
    }
    Ok(true)
}

/// Check at most 64 tracked threads, using an indexed keyset cursor.
pub fn poll_test_ready(t: &mut Tx<'_>, after: &str) -> ApiResult<String> {
    let ids: Vec<String> = {
        let mut st = t.prepare("SELECT id FROM threads WHERE tracking_at IS NOT NULL AND status != 'closed' AND id > ?1 ORDER BY id LIMIT 64")?;
        st.query_map([after], |r| r.get(0))?
            .collect::<Result<_, _>>()?
    };
    for id in &ids {
        if monitor_ready(t, id)? {
            let (cards, reasons) =
                super::thread_test_ready::readiness(t, id, terrarium_version::VERSION)?;
            let previous: Option<String> = t.query_row(
                "SELECT test_ready_cards FROM threads WHERE id = ?1",
                [id],
                |r| r.get(0),
            )?;
            if reasons.is_empty() && previous.as_deref() != Some(&cards) {
                wake(t, id, WAKE_REVIEW)?;
            }
        }
    }
    Ok(if ids.len() == 64 {
        ids.last().cloned().unwrap_or_default()
    } else {
        String::new()
    })
}

/// Link only monitors in a project the caller can read. The API transaction
/// rolls back the link if tracking or another link fails.
pub fn link_monitor(t: &mut Tx<'_>, actor: &Actor, id: &str, key: &str) -> ApiResult<()> {
    // An explicit empty reference unlinks even a renamed/deleted monitor.
    if key.is_empty() {
        t.execute(
            "UPDATE threads SET monitor = NULL, monitor_done = 0 WHERE id = ?1",
            [id],
        )?;
        return Ok(());
    }
    let (project, name) = key
        .split_once(':')
        .filter(|(p, n)| !p.is_empty() && !n.is_empty())
        .ok_or_else(|| ApiError::invalid("monitor must be PROJECT:MONITOR"))?;
    policy::read_project(actor, project).map_err(|_| ApiError::not_found("no monitor"))?;
    crate::store::monitors::find(t, &format!("{project}/{name}"))?;
    t.execute(
        "UPDATE threads SET monitor = ?2, monitor_done = 0 WHERE id = ?1 AND monitor IS NOT ?2",
        params![id, key],
    )?;
    Ok(())
}

/// A renamed/deleted reference is unknown, not ready. Leave the reference
/// for diagnosis and wake its agent so it can replace or unlink it.
pub fn on_monitor_missing(t: &mut Tx<'_>, project: &str, name: &str) -> ApiResult<()> {
    let key = format!("{project}:{name}");
    let ids: Vec<String> = {
        let mut st =
            t.prepare("SELECT id FROM threads WHERE monitor = ?1 AND status != 'closed'")?;
        st.query_map([&key], |r| r.get(0))?
            .collect::<Result<Vec<_>, _>>()?
    };
    for id in ids {
        t.execute("UPDATE threads SET monitor_done = 0 WHERE id = ?1", [&id])?;
        insert_message(
            t,
            &id,
            "system",
            "message",
            "The linked monitor is missing (unknown). Replace it or unlink it with thread track --monitor ''.",
            &now(),
        )?;
        if get_bare(t, &id)?.tracking_at.is_some() {
            wake(t, &id, "monitor_missing")?;
        }
    }
    Ok(())
}

/// Monitor transitions reuse tracking wakes, never operator notices.
pub fn on_monitor_status(
    t: &mut Tx<'_>,
    project: &str,
    name: &str,
    status: crate::monitors::parse::Status,
) -> ApiResult<()> {
    use crate::monitors::parse::Status;
    let ids: Vec<(String, bool)> = {
        let mut st = t.prepare("SELECT id, monitor_done FROM threads WHERE monitor = ?1 AND tracking_at IS NOT NULL AND status != 'closed'")?;
        st.query_map([format!("{project}:{name}")], |r| {
            Ok((r.get(0)?, r.get(1)?))
        })?
        .collect::<Result<_, _>>()?
    };
    for (id, done) in ids {
        if status == Status::Ok && !done && all_review_or_later(t, &id)? {
            wake(t, &id, WAKE_REVIEW)?;
        } else if status == Status::Crit && done {
            t.execute("UPDATE threads SET monitor_done = 0 WHERE id = ?1", [&id])?;
            wake(t, &id, "monitor_red")?;
        }
    }
    Ok(())
}

/// Card `card` of `project` moved from column `from` to column `to` (role
/// `to_role`): wake the agent of each open thread that tracks
/// (`tracking_at`) and links the card (TER-148, TER-144 D4), in the
/// transaction of the move:
///
/// - a move into the column with the role `blocked` wakes it with the
///   reason [`WAKE_BLOCKED`];
/// - a move from a column before review into review or later
///   ([`review_or_later`]) that leaves every linked card that is not
///   archived in review or later wakes it with [`WAKE_REVIEW`]. A move
///   between two columns of review or later does not wake it again.
///
/// The wake ([`wake`]) emits the `thread` event `cards`; the thread agent
/// driver gives the agent a `[Thread:<id>]` turn with the linked cards
/// ([`crate::thread_agent`]).
pub fn on_card_moved(
    t: &mut Tx<'_>,
    card: &str,
    project: &str,
    _from: &str,
    to: &str,
    to_role: Option<&str>,
) -> ApiResult<()> {
    let ids: Vec<String> = {
        let mut st = t.prepare(
            "SELECT th.id FROM thread_cards tc JOIN threads th ON th.id = tc.thread
             WHERE tc.card = ?1 AND th.status != 'closed' AND th.tracking_at IS NOT NULL
             ORDER BY th.id",
        )?;
        st.query_map([card], |r| r.get(0))?
            .collect::<Result<Vec<_>, _>>()?
    };
    if ids.is_empty() {
        return Ok(());
    }
    let cols = board::column_roles(t, project)?;
    if !review_or_later(&cols, to) {
        for id in &ids {
            t.execute("UPDATE threads SET monitor_done = 0 WHERE id = ?1", [id])?;
        }
    }
    for id in ids {
        if to_role == Some(board::BLOCKED) {
            wake(t, &id, WAKE_BLOCKED)?;
        } else if all_review_or_later(t, &id)? {
            wake(t, &id, WAKE_REVIEW)?;
        }
    }
    Ok(())
}

/// Wake the agent of the thread `id` that tracks, for `reason`
/// ([`WAKE_BLOCKED`], [`WAKE_REVIEW`]). A thread in `tracking` goes to
/// `waiting_agent`; `tracking_at` stays, so the agent's reply puts it back
/// into tracking ([`post`]). A thread that waits on the operator is left
/// parked. A thread waiting on the agent keeps its status. The `thread` event `cards`
/// carries only the reason, not the card: a reader of the thread's events
/// need not read the card's project (TER-146).
fn wake(t: &mut Tx<'_>, id: &str, reason: &str) -> ApiResult<()> {
    if (reason == WAKE_REVIEW || reason == "monitor_green")
        && !super::thread_test_ready::readiness(t, id, terrarium_version::VERSION)?
            .1
            .is_empty()
    {
        // Review is only a status event, never an invitation to test.
        let th = get_bare(t, id)?;
        emit_with(t, &th, "cards_status", json!({"reason": "review"}))?;
        return Ok(());
    }
    if reason == WAKE_REVIEW {
        t.execute(
            "UPDATE threads SET monitor_done = 1 WHERE id = ?1 AND monitor IS NOT NULL",
            [id],
        )?;
    }
    let th = get_bare(t, id)?;
    let reason = if reason == WAKE_REVIEW && th.monitor.is_some() {
        "monitor_green"
    } else {
        reason
    };
    let at = now();
    if th.status == ThreadStatus::WaitingOperator {
        if reason == "monitor_green" || reason == "monitor_red" {
            // Coalesce each monitor reason at its latest position, so the
            // last delivered state matches the latest monitor transition.
            t.execute(
                "DELETE FROM thread_pending_wakes WHERE thread = ?1 AND reason = ?2",
                params![id, reason],
            )?;
        }
        t.execute(
            "INSERT OR IGNORE INTO thread_pending_wakes(thread, reason) VALUES (?1, ?2)",
            params![id, reason],
        )?;
        return Ok(());
    }
    if reason == WAKE_REVIEW && !super::thread_test_ready::claim(t, id)? {
        return Ok(());
    }
    if reason == "monitor_green" {
        super::thread_test_ready::claim(t, id)?;
    }
    let status = if th.status == ThreadStatus::Tracking {
        ThreadStatus::WaitingAgent
    } else {
        th.status
    };
    set_status(t, &th.id, status, &at)?;
    let th = get_bare(t, &th.id)?;
    t.audit(
        "thread.card_wake",
        &th.id,
        th.project(),
        json!({"reason": reason}),
    )?;
    emit_with(t, &th, "cards", json!({"reason": reason}))
}

/// Deliver each held reason once, after the operator wait ends. Keep the
/// reason even if the card moved again: it is an event, not a status poll.
/// Rowid order preserves first arrival for blocked/review too, rather than
/// sorting those held TER-148 reasons by name. Monitor reasons are moved
/// to their latest position when coalesced, so the latest state is last.
fn flush_pending_wakes(t: &mut Tx<'_>, id: &str) -> ApiResult<()> {
    let th = get_bare(t, id)?;
    if th.status == ThreadStatus::WaitingOperator {
        return Ok(());
    }
    let reasons: Vec<String> = {
        let mut st =
            t.prepare("SELECT reason FROM thread_pending_wakes WHERE thread = ?1 ORDER BY rowid")?;
        st.query_map([id], |r| r.get(0))?
            .collect::<Result<Vec<_>, _>>()?
    };
    t.execute("DELETE FROM thread_pending_wakes WHERE thread = ?1", [id])?;
    if th.status != ThreadStatus::Closed {
        for reason in reasons {
            wake(t, id, &reason)?;
        }
    }
    Ok(())
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
                session: Default::default(),
            })
        })?
        .collect::<Result<Vec<_>, _>>()?;
    Ok(rows)
}

/// Resolve message sessions only for the read API. Ordinary `get` calls,
/// including those inside write transactions, do not need this lookup.
pub fn message_sessions(conn: &Connection, messages: &mut [ThreadMessage]) -> ApiResult<()> {
    let keys: Vec<_> = messages
        .iter()
        .map(|m| crate::store::session_refs::LinkKey {
            session: None,
            agent: crate::store::session_refs::agent_of(&m.author),
            at: &m.at,
        })
        .collect();
    let links = crate::store::session_refs::links(conn, &keys)?;
    for (message, link) in messages.iter_mut().zip(links) {
        message.session = crate::store::session_refs::SessionField::of(link);
    }
    Ok(())
}

fn questions_of(conn: &Connection, thread: &str) -> ApiResult<Vec<Question>> {
    let mut st =
        conn.prepare("SELECT id FROM questions WHERE thread = ?1 ORDER BY created_at, id")?;
    let ids = st
        .query_map([thread], |r| r.get::<_, String>(0))?
        .collect::<Result<Vec<_>, _>>()?;
    ids.iter().map(|id| questions::get(conn, id)).collect()
}

/// The unsettled questions of `thread`: `open`, and `in_thread` (a
/// question that the operator moved into this thread, TER-469). Both keep
/// the thread on the operator: the agent sets no other status until they
/// are settled (TER-495).
fn open_questions(conn: &Connection, thread: &str) -> ApiResult<i64> {
    Ok(conn.query_row(
        "SELECT count(*) FROM questions WHERE thread = ?1 AND status IN ('open', 'in_thread')",
        [thread],
        |r| r.get(0),
    )?)
}

/// Urgency for the Inbox list, in one query rather than one per thread.
pub fn inbox_urgency(conn: &Connection) -> ApiResult<HashMap<String, (bool, bool)>> {
    let mut st = conn.prepare(
        "SELECT thread, max(priority IN ('urgent', 'critical')), max(priority = 'critical')
         FROM questions WHERE status = 'open' AND thread IS NOT NULL GROUP BY thread",
    )?;
    Ok(st
        .query_map([], |r| Ok((r.get(0)?, (r.get(1)?, r.get(2)?))))?
        .collect::<Result<_, _>>()?)
}

/// Inbox badge counts need neither thread bodies nor per-thread name/summary queries.
/// A pending close proposal waits on the operator whatever the status
/// (the same rule as the Inbox thread item).
pub fn inbox_counts(conn: &Connection, project: Option<&str>) -> ApiResult<(usize, usize, usize)> {
    Ok(conn.query_row(
        "SELECT count(*), coalesce(sum(q.urgent > 0), 0), coalesce(sum(q.critical > 0), 0)
         FROM threads t
         LEFT JOIN (SELECT thread, count(*) AS open,
                           sum(priority IN ('urgent', 'critical')) AS urgent,
                           sum(priority = 'critical') AS critical
                    FROM questions WHERE status = 'open' AND thread IS NOT NULL
                    GROUP BY thread) q ON q.thread = t.id
         WHERE t.status != 'closed'
           AND (t.status = 'waiting_operator' OR t.close_proposal IS NOT NULL OR q.open > 0)
           AND (?1 IS NULL OR (t.scope != 'global' AND t.scope = ?1))",
        [project],
        |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
    )?)
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
        "SELECT *,
           (SELECT count(*) FROM thread_messages WHERE thread = threads.id) AS list_messages,
           (SELECT max(at) FROM thread_messages WHERE thread = threads.id) AS list_last,
           (SELECT count(*) FROM questions WHERE thread = threads.id AND status = 'open') AS list_questions
         FROM threads
         WHERE (?1 IS NULL OR status = ?1)
           AND (?1 IS NOT NULL OR ?2 OR status != 'closed')
           AND (?3 IS NULL OR scope = ?3)
           AND (?4 IS NULL OR agent = ?4 OR report_to = ?4 OR opened_by = ?4)",
    )?;
    let mut names = HashMap::new();
    let mut out = st
        .query_map(params![status, f.all, f.scope, f.involving], |r| {
            let mut th = row_named(r, |key| {
                names
                    .entry(key.to_owned())
                    .or_insert_with(|| name_of(conn, key))
                    .clone()
            })?;
            th.message_count = Some(r.get("list_messages")?);
            th.open_questions = Some(r.get("list_questions")?);
            let last: Option<String> = r.get("list_last")?;
            th.last_at = Some(
                last.filter(|l| l > &th.updated_at)
                    .unwrap_or_else(|| th.updated_at.clone()),
            );
            Ok(th)
        })?
        .collect::<Result<Vec<_>, _>>()?;
    out.sort_by(|a, b| {
        let w = |t: &Thread| t.status != ThreadStatus::WaitingOperator;
        w(a).cmp(&w(b)).then_with(|| b.last_at.cmp(&a.last_at))
    });
    Ok(out)
}

/// [`list`] for `actor`: each thread with its linked cards whose project
/// `actor` may read ([`visible`]), for the n/m chip of the Threads list
/// (TER-147). A list never shows a card of a project that `actor` cannot
/// read.
pub fn list_for(conn: &Connection, f: &Filter, actor: &Actor) -> ApiResult<Vec<Thread>> {
    list(conn, f)?
        .into_iter()
        .map(|mut th| {
            th.cards = Some(cards_of(conn, &th.id)?);
            Ok(visible(actor, th))
        })
        .collect()
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
    let status = if template.as_deref() == Some("versions") {
        let exists: bool = t.query_row(
            "SELECT EXISTS(SELECT 1 FROM threads WHERE scope = ?1
             AND template = 'versions' AND status <> 'closed')",
            [&scope],
            |r| r.get(0),
        )?;
        if exists {
            return Err(ApiError::conflict(
                "the project already has a Versions thread",
            ));
        }
        ThreadStatus::Tracking
    } else if n.opened_by == "operator" && text.is_some() {
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

/// One standing project thread for quiet version updates. Posting in tracking
/// keeps the update off the Threads badge, without hiding its history.
pub fn version_post(
    t: &mut Tx<'_>,
    project: &str,
    version: &str,
    changed: &str,
    test: &str,
) -> ApiResult<Thread> {
    let version = check_text("version", version, 128)?;
    if version.chars().any(char::is_control) {
        return Err(ApiError::invalid("version must be one line"));
    }
    let changed = check_text("changed", changed, MAX_MESSAGE)?;
    let test = check_text("test", test, MAX_MESSAGE)?;
    let heading = format!("Pre-release {version}\n\n");
    let text = format!("{heading}What changed:\n{changed}\n\nHow to test:\n{test}");
    check_text("text", &text, MAX_MESSAGE)?;
    let lead = lead_of(t, project)?
        .ok_or_else(|| ApiError::conflict("a version thread needs a project lead"))?;
    let existing: Option<String> = t
        .query_row(
            "SELECT id FROM threads WHERE scope = ?1 AND template = 'versions'
             AND status <> 'closed' ORDER BY created_at, id LIMIT 1",
            [project],
            |r| r.get(0),
        )
        .optional()?;
    let id = match existing {
        Some(id) => id,
        None => {
            open(
                t,
                NewThread {
                    topic: "Versions".to_owned(),
                    scope: project.to_owned(),
                    template: Some("versions".to_owned()),
                    opened_by: lead.clone(),
                    agent: Some(lead.clone()),
                    text: None,
                },
            )?
            .id
        }
    };
    let duplicate: bool = t.query_row(
        "SELECT EXISTS(SELECT 1 FROM thread_messages WHERE thread = ?1
         AND substr(text, 1, length(?2)) = ?2)",
        params![id, heading],
        |r| r.get(0),
    )?;
    if duplicate {
        return Err(ApiError::conflict(
            "this pre-release already has a version post",
        ));
    }
    post(t, &id, &lead, &text, Some(ThreadStatus::Tracking))
}

fn insert_message(
    t: &mut Tx<'_>,
    thread: &str,
    author: &str,
    kind: &str,
    text: &str,
    at: &str,
) -> ApiResult<String> {
    let id = ulid::Ulid::new().to_string();
    t.execute(
        "INSERT INTO thread_messages (id, thread, author, kind, text, at)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
        params![id, thread, author, kind, text, at],
    )?;
    Ok(id)
}

/// Replace an ended request agent. Keep every message and linked card.
pub fn restart_request(
    t: &mut Tx<'_>,
    id: &str,
    by: &str,
    reason: &str,
    template: &str,
    reopening: bool,
    expected: (i64, Option<&str>),
) -> ApiResult<Thread> {
    let (epoch, card) = expected;
    let th = get_bare(t, id)?;
    if !th.request || (th.status == ThreadStatus::Closed) != reopening {
        return Err(ApiError::conflict("the request status changed"));
    }
    let current: i64 = t.query_row(
        "SELECT request_epoch FROM threads WHERE id = ?1",
        [id],
        |r| r.get(0),
    )?;
    if current != epoch {
        return Err(ApiError::conflict("another request agent replacement won"));
    }
    let reason = check_text("reason", reason, MAX_MESSAGE)?;
    let reason = t.redact_string(reason);
    let at = now();
    let verb = if reopening {
        "Reopened"
    } else {
        "Agent replaced"
    };
    if let Some(handoff) = &th.handoff {
        insert_message(
            t,
            id,
            "system",
            "message",
            &format!(
                "Previous close handoff (quoted data):\n{}",
                serde_json::to_string(handoff).map_err(|e| ApiError::invalid(e.to_string()))?
            ),
            &at,
        )?;
    }
    insert_message(t, id, by, "message", &format!("{verb}: {reason}"), &at)?;
    t.execute(
        "UPDATE threads SET status = 'waiting_agent', template = ?2, agent = NULL,
         agent_end_outcome = NULL, close_proposal = NULL, handoff = NULL, closed_at = NULL,
         closed_by = NULL, close_reason = NULL, reminded_at = NULL, updated_at = ?3,
         request_epoch = request_epoch + 1 WHERE id = ?1",
        params![id, template, at],
    )?;
    if let Some(card) = card {
        let c = board::get_card(t, card)?;
        if c.archived {
            return Err(ApiError::invalid(
                "an archived card cannot reopen a request",
            ));
        }
        t.execute("INSERT OR IGNORE INTO thread_cards (thread, project, card, linked_by, linked_at) VALUES (?1, ?2, ?3, ?4, ?5)", params![id, c.project, c.id, by, at])?;
        t.audit(
            "thread.card_link",
            id,
            Some(&c.project),
            json!({"card": c.id, "by": by}),
        )?;
    }
    let th = get(t, id)?;
    t.audit(
        "thread.restart",
        id,
        th.project(),
        json!({"by": by, "reopening": reopening, "template": template}),
    )?;
    emit(t, &th, "restarted")?;
    Ok(th)
}

/// One message of thread `thread` by its ID.
pub fn message(conn: &Connection, thread: &str, id: &str) -> ApiResult<ThreadMessage> {
    messages(conn, thread)?
        .into_iter()
        .find(|m| m.id == id)
        .ok_or_else(|| ApiError::not_found(format!("thread {thread} has no message `{id}`")))
}

/// Attach the agent `agent` (a ULID) to thread `id`: the agent that the
/// daemon spawned for it at the open (TER-92, [`crate::thread_agent`]).
pub fn set_agent(t: &mut Tx<'_>, id: &str, agent: &str) -> ApiResult<Thread> {
    let at = now();
    t.execute(
        "UPDATE threads SET agent = ?2, updated_at = ?3 WHERE id = ?1",
        params![id, agent, at],
    )?;
    let th = get(t, id)?;
    t.audit(
        "thread.agent",
        &th.id,
        th.project(),
        json!({"agent": agent, "template": th.template}),
    )?;
    emit(t, &th, "agent")?;
    Ok(th)
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

fn close_after_agent_end(t: &mut Tx<'_>, th: Thread) -> ApiResult<Thread> {
    let summary = "Your message was saved, but the thread agent has ended. Nobody will answer in this thread.";
    insert_message(t, &th.id, "system", "message", summary, &now())?;
    close(
        t,
        &th.id,
        "system",
        CloseReason::AgentEnded,
        Some(summary),
        vec![],
    )
}

/// Post a message: by the operator (then the agent must act) or by the
/// attached agent (then the operator must act). In a thread that tracks
/// (`tracking_at`), the agent's message puts it back into `tracking`
/// (TER-145, D9).
///
/// `want` (GitHub #28): the attached agent's message sets this status
/// instead, `waiting_agent` (it works on) or `tracking` (as [`track`]).
/// The operator sets none, and an open question refuses it (`409`): a
/// question keeps the thread on the operator.
pub fn post(
    t: &mut Tx<'_>,
    id: &str,
    author: &str,
    text: &str,
    want: Option<ThreadStatus>,
) -> ApiResult<Thread> {
    let th = get_bare(t, id)?;
    not_closed(&th, "a message")?;
    if let Some(w) = want {
        agent_status(w)?;
        if author == "operator" {
            return Err(ApiError::invalid(
                "`status` is for the thread's agent: an operator message sets waiting_agent",
            ));
        }
        no_open_question(t, &th.id, w)?;
    }
    let text = t.redact_string(check_text("text", text, MAX_MESSAGE)?);
    let at = now();
    let msg = insert_message(t, &th.id, author, "message", &text, &at)?;
    if author == "operator" && th.agent_end_outcome.is_some() {
        emit_with(t, &th, "message", json!({"message": msg, "author": author}))?;
        return close_after_agent_end(t, th);
    }
    let status = if author == "operator" {
        ThreadStatus::WaitingAgent
    } else if let Some(w) = want {
        w
    } else if open_questions(t, &th.id)? > 0 {
        ThreadStatus::WaitingOperator
    } else if th.tracking_at.is_some() {
        ThreadStatus::Tracking
    } else {
        ThreadStatus::WaitingOperator
    };
    set_status(t, &th.id, status, &at)?;
    if author == "operator" || want.is_some() {
        // The operator writes on: a close proposal of the agent is
        // declined (TER-95). The agent that works on or tracks withdraws
        // its own proposal, as [`track`] does.
        clear_proposal(t, &th.id)?;
    }
    let th = get(t, &th.id)?;
    emit_with(t, &th, "message", json!({"message": msg, "author": author}))?;
    if author == "operator" {
        mail_agent(t, &th, "message", &text)?;
    }
    if want == Some(ThreadStatus::Tracking) {
        return track(t, id, author);
    }
    flush_pending_wakes(t, id)?;
    Ok(th)
}

/// A status that the attached agent may set (GitHub #28): `waiting_agent`
/// or `tracking`. `waiting_operator` follows from a message or a question,
/// and `closed` from the close.
fn agent_status(s: ThreadStatus) -> ApiResult<()> {
    match s {
        ThreadStatus::WaitingAgent | ThreadStatus::Tracking => Ok(()),
        other => Err(ApiError::invalid(format!(
            "the thread's agent sets waiting_agent or tracking, not {}: a message or a \
             question sets waiting_operator, and `thread close` closes",
            other.as_str()
        ))),
    }
}

/// An open question keeps the thread on the operator: the agent sets no
/// other status until it is settled (`409`).
fn no_open_question(t: &Tx<'_>, id: &str, want: ThreadStatus) -> ApiResult<()> {
    let open = open_questions(t, id)?;
    if open > 0 {
        return Err(ApiError::conflict(format!(
            "thread {id} has {open} open question(s): the operator settles them first, or you \
             retract them; then set {}",
            want.as_str()
        )));
    }
    Ok(())
}

/// An `open` question of thread `id` refuses the operator's `want`
/// (`409`). An `in_thread` question does not: the operator moved it into
/// the thread (TER-514).
fn operator_no_open_question(t: &Tx<'_>, id: &str, want: ThreadStatus) -> ApiResult<()> {
    let open: i64 = t.query_row(
        "SELECT count(*) FROM questions WHERE thread = ?1 AND status = 'open'",
        [id],
        |r| r.get(0),
    )?;
    if open > 0 {
        return Err(ApiError::conflict(format!(
            "thread {id} has {open} open question(s) in the Inbox: answer them first; then set {}",
            want.as_str()
        )));
    }
    Ok(())
}

/// The attached agent or the operator `by` sets the status of thread
/// `id` without a message (GitHub #28, `thread status`, TER-456):
/// `tracking` as [`track`], or
/// `waiting_agent`: the agent works on, and the thread does not wait on
/// the operator (no Inbox badge, no reminder, no stale close). An open
/// question refuses it (`409`); a close proposal is withdrawn. The
/// operator's `waiting_agent` passes an `in_thread` question: the operator
/// moved it into this thread and decides when the agent works on (TER-514).
pub fn set_by_agent(t: &mut Tx<'_>, id: &str, by: &str, status: ThreadStatus) -> ApiResult<Thread> {
    agent_status(status)?;
    if status == ThreadStatus::Tracking {
        return track(t, id, by);
    }
    let th = get_bare(t, id)?;
    not_closed(&th, "a status change")?;
    if by == "operator" {
        operator_no_open_question(t, &th.id, status)?;
    } else {
        no_open_question(t, &th.id, status)?;
    }
    let at = now();
    t.execute(
        "UPDATE threads SET status = ?2, updated_at = ?3, close_proposal = NULL WHERE id = ?1",
        params![th.id, status.as_str(), at],
    )?;
    let th = get(t, &th.id)?;
    t.audit(
        "thread.status",
        &th.id,
        th.project(),
        json!({"by": by, "status": status.as_str()}),
    )?;
    emit_with(t, &th, "status", json!({"author": by}))?;
    Ok(th)
}

/// Move thread `id` into tracking when a card is linked to it while it
/// waits on the operator only because of the attached agent's last
/// message: the agent handed off, and the new card is the work (GitHub
/// #28). Nothing changes with an open question, a close proposal, an
/// ended agent, or when the last message is not the agent's.
pub fn track_on_link(t: &mut Tx<'_>, id: &str) -> ApiResult<()> {
    let th = get_bare(t, id)?;
    let Some(agent) = th.agent.as_deref() else {
        return Ok(());
    };
    if th.status != ThreadStatus::WaitingOperator
        || th.close_proposal.is_some()
        || th.agent_end_outcome.is_some()
        || open_questions(t, &th.id)? > 0
    {
        return Ok(());
    }
    let last: Option<String> = t
        .query_row(
            "SELECT author FROM thread_messages WHERE thread = ?1
             ORDER BY at DESC, id DESC LIMIT 1",
            [&th.id],
            |r| r.get(0),
        )
        .optional()?;
    if last.as_deref() == Some(agent) {
        track(t, &th.id, agent)?;
    }
    Ok(())
}

/// Mark an open thread whose bound agent ended, without an Inbox notice.
/// Called in the spawn's finish transaction, so retries cannot duplicate it.
pub fn agent_ended(t: &mut Tx<'_>, id: &str, agent: &str, outcome: &str) -> ApiResult<()> {
    let th = get_bare(t, id)?;
    if th.status == ThreadStatus::Closed || th.agent.as_deref() != Some(agent) {
        return Ok(());
    }
    let text = format!(
        "Thread agent ended: {outcome}. Its open questions were retracted because it can no longer receive answers. The thread remains open and its report-to lead has been mailed to review the linked cards."
    );
    let at = now();
    insert_message(t, id, "system", "message", &text, &at)?;
    t.execute(
        "UPDATE threads SET updated_at = ?2, agent_end_outcome = ?3,
         status = CASE WHEN status = 'waiting_agent' THEN 'waiting_operator' ELSE status END
         WHERE id = ?1",
        params![id, at, outcome],
    )?;
    let open: Vec<String> = {
        let mut st = t.prepare(
            "SELECT id FROM questions WHERE thread = ?1 AND agent = ?2
               AND status IN ('open', 'in_thread')",
        )?;
        st.query_map(params![id, agent], |r| r.get(0))?
            .collect::<Result<Vec<_>, _>>()?
    };
    for question in open {
        questions::retract(
            t,
            &question,
            true,
            Some(&format!(
                "Thread agent ended: {outcome}; it can no longer receive answers."
            )),
        )?;
    }
    if let Some(lead) = &th.report_to {
        let to = match super::mail::resolve(t, lead, None) {
            Ok(to) => to,
            Err(e) => {
                tracing::warn!(thread = %id, report_to = %lead, "no agent-end mail: {}", e.message);
                emit(t, &get_bare(t, id)?, "agent_ended")?;
                return Ok(());
            }
        };
        super::mail::send(
            t,
            super::mail::Outgoing {
                from: &super::mail::Address::System,
                to: &to,
                subject: &format!("Thread {id}: agent ended"),
                body: &format!(
                    "{text}\n\nThread: {id}\nAgent: {agent}\nRead `terrarium thread show {id}` and review its linked cards with `terrarium card show <id>`. Use your board tools to arrange any follow-up work. Only the operator can close this thread now."
                ),
                thread: None,
                in_reply_to: None,
            },
        )?;
    }
    emit(t, &get_bare(t, id)?, "agent_ended")?;
    Ok(())
}

/// The operator's overall response: kept as `response` (the approval
/// record) and as a message of kind `response`.
pub fn respond(t: &mut Tx<'_>, id: &str, text: &str) -> ApiResult<Thread> {
    let th = get_bare(t, id)?;
    not_closed(&th, "a response")?;
    let text = t.redact_string(check_text("text", text, MAX_RESPONSE)?);
    let at = now();
    let msg = insert_message(t, &th.id, "operator", "response", &text, &at)?;
    let response = Response {
        text: text.clone(),
        by: "operator".into(),
        at: at.clone(),
    };
    t.execute(
        "UPDATE threads SET response = ?2, status = 'waiting_agent', updated_at = ?3,
                            close_proposal = NULL
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
    emit_with(
        t,
        &th,
        "response",
        json!({"message": msg, "author": "operator"}),
    )?;
    if th.agent_end_outcome.is_some() {
        return close_after_agent_end(t, th);
    }
    mail_agent(t, &th, "response", &text)?;
    flush_pending_wakes(t, id)?;
    Ok(th)
}

/// The mail of an operator `message` or `response` in thread `th` to its
/// attached agent, when the thread was opened from a question
/// (`from_question`, GitHub #30): that agent is not bound to the thread,
/// so no `[Thread:<id>]` turn reaches it. The mail goes from the operator
/// on the mail thread of the question, and wakes the agent as all mail
/// does. Any other thread: nothing.
fn mail_agent(t: &mut Tx<'_>, th: &Thread, kind: &str, text: &str) -> ApiResult<()> {
    let (Some(qid), Some(agent)) = (th.from_question.as_deref(), th.agent.as_deref()) else {
        return Ok(());
    };
    let Ok(q) = questions::get(t, qid) else {
        return Ok(());
    };
    let Ok(to) = super::mail::resolve(t, agent, None) else {
        return Ok(());
    };
    let what = if kind == "response" {
        "sent the overall response"
    } else {
        "wrote"
    };
    let mut body = format!(
        "The operator {what} in the Inbox thread {} (from your {} {}):\n\n",
        th.id,
        q.noun(),
        q.id
    );
    for line in text.lines() {
        body.push_str("> ");
        body.push_str(line);
        body.push('\n');
    }
    body.push_str(&format!(
        "\nRead the thread with `terrarium thread show {id}` and reply there with \
         `terrarium thread post {id} -m \"...\"`, not by mail.\n",
        id = th.id
    ));
    super::mail::send(
        t,
        super::mail::Outgoing {
            from: &super::mail::Address::Operator,
            to: &to,
            subject: &format!("Thread: {}", questions::short(&th.topic, 60)),
            body: &body,
            thread: Some(&q.thread_id),
            in_reply_to: None,
        },
    )?;
    Ok(())
}

/// The attached agent `by` moves thread `id` into tracking after its
/// handoff (TER-145, TER-144 D1): the agent parks, and the reminder and
/// the stale close do not apply. A close proposal is withdrawn. A thread
/// with an open question waits on the operator: `409 conflict`.
/// On the first handoff, cards already in review or later wake it at once.
pub fn track(t: &mut Tx<'_>, id: &str, by: &str) -> ApiResult<Thread> {
    let th = get_bare(t, id)?;
    let first = th.tracking_at.is_none();
    not_closed(&th, "tracking")?;
    let open = open_questions(t, &th.id)?;
    if open > 0 {
        return Err(ApiError::conflict(format!(
            "thread {} has {open} open question(s): the operator settles them first, or you \
             retract them; then track it",
            th.id
        )));
    }
    let at = now();
    t.execute(
        "UPDATE threads SET status = 'tracking', tracking_at = ?2, updated_at = ?2,
                            close_proposal = NULL
         WHERE id = ?1",
        params![th.id, at],
    )?;
    let th = get(t, &th.id)?;
    t.audit("thread.track", &th.id, th.project(), json!({"by": by}))?;
    emit_with(t, &th, "tracking", json!({"author": by}))?;
    flush_pending_wakes(t, id)?;
    let monitor_pending: bool = t.query_row(
        "SELECT monitor IS NOT NULL AND monitor_done = 0 FROM threads WHERE id = ?1",
        [id],
        |r| r.get(0),
    )?;
    if (first || monitor_pending) && all_review_or_later(t, id)? {
        wake(t, id, WAKE_REVIEW)?;
    }
    get(t, id)
}

/// Archiving an unfinished card can leave all remaining live cards ready.
pub fn on_card_archived(t: &mut Tx<'_>, card: &str) -> ApiResult<()> {
    let ids: Vec<String> = {
        let mut st = t.prepare(
            "SELECT th.id FROM thread_cards tc JOIN threads th ON th.id = tc.thread
             WHERE tc.card = ?1 AND th.status != 'closed' AND th.tracking_at IS NOT NULL",
        )?;
        st.query_map([card], |r| r.get(0))?
            .collect::<Result<Vec<_>, _>>()?
    };
    for id in ids {
        let states = linked_states(t, &id)?;
        if states.iter().any(|s| s.card == card && !s.review_or_later)
            && all_review_or_later(t, &id)?
        {
            wake(t, &id, WAKE_REVIEW)?;
        }
    }
    Ok(())
}

fn clear_proposal(t: &mut Tx<'_>, id: &str) -> ApiResult<()> {
    t.execute(
        "UPDATE threads SET close_proposal = NULL WHERE id = ?1",
        [id],
    )?;
    Ok(())
}

/// Check a handoff: the summary (empty when absent) and the links.
fn checked_handoff(
    t: &mut Tx<'_>,
    summary: Option<&str>,
    links: Vec<Link>,
) -> ApiResult<(String, Vec<Link>)> {
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
    Ok((summary, checked))
}

/// The attached agent proposes the close (TER-95, wiki `inbox-threads`
/// §5.2): the handoff waits in `close_proposal` until the operator
/// confirms it ([`confirm_close`]) or writes on (a message or a response
/// declines it). The thread waits on the operator, so the agent parks. A
/// new proposal replaces the old one.
pub fn propose_close(
    t: &mut Tx<'_>,
    id: &str,
    by: &str,
    summary: Option<&str>,
    links: Vec<Link>,
) -> ApiResult<Thread> {
    let th = get_bare(t, id)?;
    check_close(&th)?;
    not_closed(&th, "a close proposal")?;
    let (summary, links) = checked_handoff(t, summary, links)?;
    let at = now();
    let proposal = Handoff {
        summary,
        links,
        by: by.to_owned(),
        at: at.clone(),
    };
    t.execute(
        "UPDATE threads SET close_proposal = ?2, status = 'waiting_operator', updated_at = ?3
         WHERE id = ?1",
        params![th.id, json!(proposal).to_string(), at],
    )?;
    let th = get(t, &th.id)?;
    t.audit(
        "thread.propose_close",
        &th.id,
        th.project(),
        json!({"by": by, "links": proposal.links.len()}),
    )?;
    emit_with(t, &th, "close_proposed", json!({"author": by}))?;
    Ok(th)
}

/// The operator confirms the agent's close proposal: the thread closes
/// with the proposed handoff. No proposal is `409 conflict`.
pub fn confirm_close(t: &mut Tx<'_>, id: &str) -> ApiResult<Thread> {
    confirm_close_by(t, id, "operator")
}

/// Confirm a proposal, retaining who actually confirmed it in the audit.
pub fn confirm_close_by(t: &mut Tx<'_>, id: &str, by: &str) -> ApiResult<Thread> {
    let th = get_bare(t, id)?;
    check_close(&th)?;
    not_closed(&th, "a confirm")?;
    let Some(p) = th.close_proposal else {
        return Err(ApiError::conflict(format!(
            "thread {} has no close proposal to confirm: close it with a handoff of your own",
            th.id
        )));
    };
    // The handoff stays the agent's: it wrote it.
    close_as(
        t,
        &th.id,
        by,
        &p.by,
        CloseReason::Confirmed,
        Some(&p.summary),
        p.links,
    )
}

/// The session of the agent of thread `th` that the close archives: the
/// current session of its bound (spawned) agent. `None` for a named agent
/// (it lives on) or an agent with no session yet.
fn agent_session(conn: &Connection, th: &Thread) -> ApiResult<Option<String>> {
    let Some(sp) = spawns::open_bound(conn, &crate::thread_agent::bound(&th.id))? else {
        return Ok(None);
    };
    Ok(agents::current_session(conn, &sp.id)?.or(sp.session_id))
}

/// The project Versions thread stays open across pre-releases, for every caller.
pub fn check_close(th: &Thread) -> ApiResult<()> {
    if th.template.as_deref() == Some("versions") {
        return Err(ApiError::conflict(format!(
            "standing Versions thread {} cannot be closed",
            th.id
        )));
    }
    Ok(())
}

/// Close a thread with a handoff (summary and links), for `reason`. The
/// open questions of the thread are retracted first, so that none stays
/// open in a closed thread and the waits on them end. The session of the
/// thread agent is kept in `session_id`, and each handoff card gets a
/// comment that links to it (TER-95); the agent ends after the commit
/// ([`crate::thread_agent`]), and its session is archived then.
pub fn close(
    t: &mut Tx<'_>,
    id: &str,
    by: &str,
    reason: CloseReason,
    summary: Option<&str>,
    links: Vec<Link>,
) -> ApiResult<Thread> {
    close_as(t, id, by, by, reason, summary, links)
}

/// [`close`] by `by`, with a handoff written by `author`.
fn close_as(
    t: &mut Tx<'_>,
    id: &str,
    by: &str,
    author: &str,
    reason: CloseReason,
    summary: Option<&str>,
    links: Vec<Link>,
) -> ApiResult<Thread> {
    let th = get_bare(t, id)?;
    check_close(&th)?;
    not_closed(&th, "a close")?;
    let (summary, checked) = checked_handoff(t, summary, links)?;
    let open: Vec<String> = {
        let mut st = t.prepare(
            "SELECT id FROM questions WHERE thread = ?1 AND status IN ('open', 'in_thread')
             ORDER BY created_at, id",
        )?;
        st.query_map([&th.id], |r| r.get(0))?
            .collect::<Result<Vec<_>, _>>()?
    };
    let by_owner = th.agent.as_deref() == Some(by);
    let why = format!("thread {} was closed", th.id);
    for q in &open {
        questions::retract(t, q, by_owner, Some(&why))?;
    }
    let session = agent_session(t, &th)?;
    let at = now();
    let handoff = Handoff {
        summary,
        links: checked,
        by: author.to_owned(),
        at: at.clone(),
    };
    t.execute(
        "UPDATE threads SET status = 'closed', handoff = ?2, closed_at = ?3, closed_by = ?4,
                            updated_at = ?3, close_reason = ?5, close_proposal = NULL,
                            session_id = coalesce(?6, session_id)
         WHERE id = ?1",
        params![
            th.id,
            json!(handoff).to_string(),
            at,
            by,
            reason.as_str(),
            session
        ],
    )?;
    let th = get(t, &th.id)?;
    let (cards, skipped) = link_cards(t, &th)?;
    t.audit(
        "thread.close",
        &th.id,
        th.project(),
        json!({"by": by, "reason": reason.as_str(),
               "links": th.handoff.as_ref().map_or(0, |h| h.links.len()),
               "retracted": open, "session": th.session_id, "cards": cards,
               "skipped_cards": skipped}),
    )?;
    emit(t, &th, "closed")?;
    Ok(th)
}

/// Comment on each card of the handoff of the closed thread `th`, with a
/// link to the archived session of its agent (the comment's session). Only
/// a card that the reader of the handoff may read gets the comment: the
/// agent that wrote the handoff, else (an operator or a stale close) the
/// report-to lead of the thread. A card that does not resolve or that the
/// reader may not read is skipped, with the reason. `(commented, skipped)`.
fn link_cards(t: &mut Tx<'_>, th: &Thread) -> ApiResult<(Vec<String>, Vec<Value>)> {
    let Some(h) = &th.handoff else {
        return Ok((Vec::new(), Vec::new()));
    };
    let reader_key = if h.by != "operator" && h.by != "system" {
        Some(h.by.as_str())
    } else {
        th.report_to.as_deref()
    };
    let reader = match reader_key {
        Some(k) => agents::actor_for(t, k)?,
        None => None,
    };
    let mut body = format!(
        "Inbox thread {} was closed ({}).",
        th.id,
        th.close_reason.as_deref().unwrap_or("closed")
    );
    match &th.session_id {
        Some(s) => body.push_str(&format!(
            " The session of its agent {} is archived: /sessions/{s}",
            th.agent_name.as_deref().unwrap_or("-")
        )),
        None => body.push_str(" Its agent has no archived session."),
    }
    let mut out = Vec::new();
    let mut skipped = Vec::new();
    for l in h.links.iter().filter(|l| l.kind == "card") {
        let res = match &reader {
            None => Err(ApiError::forbidden(
                "the handoff has no reader whose rights can be checked",
            )),
            Some(a) => board::card_project(t, &l.reference).and_then(|p| {
                policy::read_project(&Actor::Agent(a.clone()), &p)?;
                board::system_comment(t, &l.reference, &body, th.session_id.as_deref())
            }),
        };
        match res {
            Ok(card) => out.push(card),
            Err(e) => {
                tracing::warn!(thread = %th.id, card = %l.reference, "no session link on a handoff card: {}", e.message);
                skipped.push(json!({"card": l.reference, "reason": e.message}));
            }
        }
    }
    Ok((out, skipped))
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
        Some(s) => {
            set_status(t, &th.id, s, &at)?;
            // The operator answers the last open question: it writes on,
            // so a close proposal of the agent is declined, as in [`post`]
            // and [`respond`] (TER-95).
            if s == ThreadStatus::WaitingAgent
                && matches!(
                    q.status,
                    QuestionStatus::Answered | QuestionStatus::Acknowledged
                )
            {
                clear_proposal(t, &th.id)?;
            }
        }
        None => {
            t.execute(
                "UPDATE threads SET updated_at = ?2 WHERE id = ?1",
                params![th.id, at],
            )?;
        }
    }
    let th = get_bare(t, &th.id)?;
    emit(t, &th, "question")?;
    flush_pending_wakes(t, id)
}

/// The `thread` and `inbox` events of a change.
pub fn emit(t: &mut Tx<'_>, th: &Thread, change: &str) -> ApiResult<()> {
    emit_with(t, th, change, json!({}))
}

/// [`emit`] with more fields in the `thread` event: `message` (the ID of
/// the new message) and `author`, which the thread agent driver reads
/// ([`crate::thread_agent`]).
fn emit_with(t: &mut Tx<'_>, th: &Thread, change: &str, extra: Value) -> ApiResult<()> {
    let mut data: Value = json!({"change": change, "id": th.id, "scope": th.scope, "agent": th.agent,
                                 "status": th.status});
    if let (Some(d), Value::Object(x)) = (data.as_object_mut(), extra) {
        d.extend(x);
    }
    // The summary of a project thread has no topic: the project's readers
    // of the events need not see it. A global thread's events are in no
    // project.
    let summary = match th.project() {
        Some(_) => format!("thread {} {change} ({})", th.id, th.status.as_str()),
        None => format!(
            "thread {} {change}: {}",
            th.id,
            questions::short(&th.topic, 80)
        ),
    };
    let mut draft = EventDraft::new("thread", summary, data);
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

#[cfg(test)]
mod tests {
    use super::*;

    fn cols(spec: &[(&str, Option<&str>)]) -> Vec<(String, Option<String>)> {
        spec.iter()
            .map(|(n, r)| ((*n).to_owned(), r.map(str::to_owned)))
            .collect()
    }

    #[test]
    fn review_or_later_follows_the_review_column() {
        let d = cols(&board::DEFAULT_COLUMNS);
        for c in ["review", "merge_queue", "accepted", "done"] {
            assert!(review_or_later(&d, c), "{c}");
        }
        // The blocked column is after review on the default board, and
        // never counts.
        for c in [
            "backlog",
            "ready",
            "in_progress",
            "validating",
            "blocked",
            "nope",
        ] {
            assert!(!review_or_later(&d, c), "{c}");
        }
        // A column named review with no role.
        let named = cols(&[
            ("backlog", None),
            ("doing", Some("working")),
            ("review", None),
            ("done", None),
        ]);
        assert!(review_or_later(&named, "review"));
        assert!(!review_or_later(&named, "doing"));
        // The role wins over the name.
        let role = cols(&[
            ("backlog", None),
            ("doing", Some("working")),
            ("check", Some("review")),
            ("review", None),
            ("done", None),
        ]);
        assert!(review_or_later(&role, "check"));
        // No review column: only done.
        let none = cols(&[
            ("backlog", None),
            ("doing", Some("working")),
            ("done", None),
        ]);
        assert!(review_or_later(&none, "done"));
        assert!(!review_or_later(&none, "doing"));
    }
}
