//! The mailbox (PLAN §12): agent mailboxes, project inboxes, the operator.
//!
//! Each row has one sender and one recipient. A reply goes to every other
//! participant of the thread, one row each. The participants of a thread are
//! the addresses that sent or received a message on it.

use std::collections::{BTreeSet, HashMap};

use rusqlite::{Connection, OptionalExtension, Row, params};
use serde::{Deserialize, Serialize};
use serde_json::json;

use crate::db::{Tx, now};
use crate::error::{ApiError, ApiResult};
use crate::events::EventDraft;
use crate::store::{Page, Paged, agents, projects};

/// A mail address.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
pub enum Address {
    Operator,
    /// The daemon itself: quota alerts and board notices. It only sends.
    /// No caller can send as `system` or to it; a reply on its thread goes
    /// to the other participants.
    System,
    /// The inbox of a project: `project:<name>`.
    Project(String),
    Agent {
        id: String,
        name: String,
        project: String,
    },
}

impl Address {
    /// The stored form: `operator`, `project:<name>`, or the agent ULID.
    pub fn key(&self) -> String {
        match self {
            Self::Operator => "operator".into(),
            Self::System => "system".into(),
            Self::Project(p) => format!("project:{p}"),
            Self::Agent { id, .. } => id.clone(),
        }
    }

    pub fn kind(&self) -> &'static str {
        match self {
            Self::Operator => "operator",
            Self::System => "system",
            Self::Project(_) => "project",
            Self::Agent { .. } => "agent",
        }
    }

    /// A readable form: `operator`, `project:<name>`, `project/agent`. An
    /// agent key that never was an agent shows as the key alone.
    pub fn display(&self) -> String {
        match self {
            Self::Agent { name, project, .. } if project.is_empty() => name.clone(),
            Self::Agent { name, project, .. } => format!("{project}/{name}"),
            other => other.key(),
        }
    }

    /// The project the address belongs to, if any.
    pub fn project(&self) -> Option<&str> {
        match self {
            Self::Operator | Self::System => None,
            Self::Project(p) => Some(p),
            Self::Agent { project, .. } => Some(project),
        }
    }

    pub fn from_actor(actor: &crate::actor::Actor) -> Self {
        match actor {
            crate::actor::Actor::Operator => Self::Operator,
            crate::actor::Actor::Agent(a) => Self::Agent {
                id: a.id.clone(),
                name: a.name.clone(),
                project: a.project.clone(),
            },
        }
    }
}

/// Parse an address: `operator`, `system`, `project:<name>`, an agent
/// ULID, `project/agent`, or a bare agent name (looked up in `context`
/// first). [`send`] refuses mail to `system`, and the API refuses a sender
/// of `system`, so `system` is only useful as a list filter.
pub fn resolve(conn: &Connection, s: &str, context: Option<&str>) -> ApiResult<Address> {
    let s = s.trim();
    if s == "operator" {
        return Ok(Address::Operator);
    }
    if s == "system" {
        return Ok(Address::System);
    }
    if let Some(p) = s.strip_prefix("project:") {
        projects::get(conn, p)?;
        return Ok(Address::Project(p.to_owned()));
    }
    let a = agents::find(conn, s, context)?;
    Ok(Address::Agent {
        id: a.agent.id.to_string(),
        name: a.agent.name,
        project: a.agent.project,
    })
}

/// [`resolve`] for the read-only list filters: an agent key that no live
/// agent has falls back to the tombstone of a deleted agent (migration 12),
/// so the mail, cards, and memories of a removed agent stay listable. A
/// key that never was an agent is still not found. The write paths use
/// [`resolve`], so mail to a removed agent stays refused.
pub fn resolve_known(conn: &Connection, s: &str, context: Option<&str>) -> ApiResult<Address> {
    match resolve(conn, s, context) {
        Err(e) if e.code == crate::error::Code::NotFound && !s.trim().starts_with("project:") => {
            match agents::find_removed(conn, s.trim(), context)? {
                Some((id, project, name)) => Ok(Address::Agent { id, name, project }),
                None => Err(e),
            }
        }
        r => r,
    }
}

/// Turn a stored key back into an address.
fn from_key(conn: &Connection, key: &str, cache: &mut HashMap<String, Address>) -> Address {
    if let Some(a) = cache.get(key) {
        return a.clone();
    }
    let a = if key == "operator" {
        Address::Operator
    } else if key == "system" {
        Address::System
    } else if let Some(p) = key.strip_prefix("project:") {
        Address::Project(p.to_owned())
    } else {
        // The tombstone resolver, as the cards use: a live agent, else a
        // deleted one (migration 12), else the key alone.
        let known = agents::known_name(conn, key).ok().flatten();
        match known.as_deref().and_then(|n| n.split_once('/')) {
            Some((project, name)) => Address::Agent {
                id: key.to_owned(),
                name: name.to_owned(),
                project: project.to_owned(),
            },
            None => Address::Agent {
                id: key.to_owned(),
                name: key.to_owned(),
                project: String::new(),
            },
        }
    };
    cache.insert(key.to_owned(), a.clone());
    a
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Message {
    pub id: String,
    pub thread_id: String,
    /// `operator`, `project:<name>`, or an agent ULID.
    pub from: String,
    pub from_kind: String,
    /// `operator`, `project:<name>`, or `project/agent`.
    pub from_name: String,
    pub to: String,
    pub to_kind: String,
    pub to_name: String,
    pub subject: String,
    pub body: String,
    /// `queued`, `delivered`, `read`, or `replied`.
    pub state: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub in_reply_to: Option<String>,
    pub created_at: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub delivered_at: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub read_at: Option<String>,
    /// Who read the message first: `operator`, `system`, or an agent ULID
    /// (migration 12). A read before migration 12 has none.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub read_by: Option<String>,
    /// `read_by` in readable form: `operator` or `project/agent`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub read_by_name: Option<String>,
    /// The thread is paused by the loop guard: it has more than
    /// [`mail_loop_limit`] agent-to-agent messages since the last operator
    /// or project-inbox message, so new agent mail on it types no pointer.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub thread_paused: bool,
    /// The loop guard held this message when it was written: it typed no
    /// push pointer (migration 16). Older messages have `false`.
    #[serde(default)]
    pub held: bool,
    /// Why the loop guard held the message. Only for a held message.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub held_reason: Option<String>,
    /// When the operator released the held message: its recipient got the
    /// pointer then (migration 30).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub released_at: Option<String>,
    /// The actor label of the release: `operator`, or `system` for a
    /// message that migration 30 found on a thread with a later reset.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub released_by: Option<String>,
    /// `session_id` and `session_label`: the session of an agent sender at
    /// `created_at`, when the reader may read it. Only the lists that
    /// call [`attach_links`] fill it.
    #[serde(flatten)]
    pub link: crate::store::session_refs::SessionField,
}

struct RawMessage {
    id: String,
    thread_id: String,
    from: String,
    from_kind: String,
    to: String,
    to_kind: String,
    subject: Option<String>,
    body: String,
    state: String,
    in_reply_to: Option<String>,
    created_at: String,
    delivered_at: Option<String>,
    read_at: Option<String>,
    read_by: Option<String>,
    held: bool,
    held_reason: Option<String>,
    released_at: Option<String>,
    released_by: Option<String>,
}

fn raw_row(r: &Row<'_>) -> rusqlite::Result<RawMessage> {
    Ok(RawMessage {
        id: r.get("id")?,
        thread_id: r.get("thread_id")?,
        from: r.get("from_agent")?,
        from_kind: r.get("from_kind")?,
        to: r.get("to_agent")?,
        to_kind: r.get("to_kind")?,
        subject: r.get("subject")?,
        body: r.get("body")?,
        state: r.get("state")?,
        in_reply_to: r.get("in_reply_to")?,
        created_at: r.get("created_at")?,
        delivered_at: r.get("delivered_at")?,
        read_at: r.get("read_at")?,
        read_by: r.get("read_by")?,
        held: r.get("held")?,
        held_reason: r.get("held_reason")?,
        released_at: r.get("released_at")?,
        released_by: r.get("released_by")?,
    })
}

#[derive(Default)]
struct RenderCache {
    addresses: HashMap<String, Address>,
    paused: HashMap<String, bool>,
}

fn render(conn: &Connection, raw: RawMessage, cache: &mut RenderCache) -> Message {
    let from_name = from_key(conn, &raw.from, &mut cache.addresses).display();
    let to_name = from_key(conn, &raw.to, &mut cache.addresses).display();
    let read_by_name = raw
        .read_by
        .as_deref()
        .map(|k| from_key(conn, k, &mut cache.addresses).display());
    let thread_paused = match cache.paused.get(&raw.thread_id) {
        Some(p) => *p,
        None => {
            let p = thread_paused(conn, &raw.thread_id).unwrap_or(false);
            cache.paused.insert(raw.thread_id.clone(), p);
            p
        }
    };
    Message {
        id: raw.id,
        thread_id: raw.thread_id,
        from: raw.from,
        from_kind: raw.from_kind,
        from_name,
        to: raw.to,
        to_kind: raw.to_kind,
        to_name,
        subject: raw.subject.unwrap_or_default(),
        body: raw.body,
        state: raw.state,
        in_reply_to: raw.in_reply_to,
        created_at: raw.created_at,
        delivered_at: raw.delivered_at,
        read_at: raw.read_at,
        read_by: raw.read_by,
        read_by_name,
        thread_paused,
        held: raw.held,
        held_reason: raw.held_reason,
        released_at: raw.released_at,
        released_by: raw.released_by,
        link: Default::default(),
    }
}

/// Find the session of each message of a page that an agent sent: the
/// sender's session at `created_at` (`store::session_refs::links`). The
/// daemon does not record which turn of the recipient read a message, so
/// a message links to the sender's session only.
pub fn attach_links(conn: &Connection, rows: &mut [Message]) -> ApiResult<()> {
    use crate::store::session_refs::{self, LinkKey, SessionField};
    let keys: Vec<LinkKey<'_>> = rows
        .iter()
        .map(|m| LinkKey {
            session: None,
            agent: (m.from_kind == "agent")
                .then(|| session_refs::agent_of(&m.from))
                .flatten(),
            at: &m.created_at,
        })
        .collect();
    let links = session_refs::links(conn, &keys)?;
    for (m, l) in rows.iter_mut().zip(links) {
        m.link = SessionField::of(l);
    }
    Ok(())
}

pub fn get(conn: &Connection, id: &str) -> ApiResult<Message> {
    let raw = conn
        .query_row("SELECT * FROM messages WHERE id = ?1", [id], raw_row)
        .optional()?
        .ok_or_else(|| ApiError::not_found(format!("no message `{id}`")))?;
    Ok(render(conn, raw, &mut RenderCache::default()))
}

/// The filters of a mail list.
#[derive(Debug, Clone, Default)]
pub struct MailFilter {
    pub to: Option<String>,
    pub from: Option<String>,
    pub thread: Option<String>,
    /// One of the states, or `unread` for `queued` and `delivered`.
    pub state: Option<String>,
    /// Only messages to or from this address (an agent reader).
    pub involving: Option<String>,
    /// Only messages that touch this project: from or to its inbox, or
    /// from or to one of its agents (also a deleted one).
    pub project: Option<String>,
    /// Only messages at or after this time (RFC 3339).
    pub since: Option<String>,
    /// Only messages before this time (RFC 3339).
    pub until: Option<String>,
    /// Free text: a part of the subject or the body, case-insensitive.
    pub text: Option<String>,
}

/// The durable, non-ephemeral admin responsible for a project inbox.
/// Prefer the named lead, otherwise the oldest admin.
fn inbox_recipient(conn: &Connection, project: &str) -> ApiResult<Option<Address>> {
    Ok(conn
        .query_row(
            "SELECT id, name, project FROM agents a
         WHERE project = ?1 AND owner IS NULL
           AND EXISTS (SELECT 1 FROM roles r WHERE r.agent_id = a.id
                       AND r.role = 'project_admin' AND r.project = a.project)
         ORDER BY (name = 'lead') DESC, id LIMIT 1",
            [project],
            |r| {
                Ok(Address::Agent {
                    id: r.get(0)?,
                    name: r.get(1)?,
                    project: r.get(2)?,
                })
            },
        )
        .optional()?)
}

pub(crate) fn routed_inbox(conn: &Connection, id: &str) -> ApiResult<String> {
    let project: Option<String> = conn
        .query_row("SELECT project FROM agents WHERE id = ?1", [id], |r| {
            r.get(0)
        })
        .optional()?;
    let Some(project) = project else {
        return Ok(String::new());
    };
    Ok(
        if inbox_recipient(conn, &project)?.is_some_and(|a| a.key() == id) {
            format!("project:{project}")
        } else {
            String::new()
        },
    )
}

/// Recipients of queued project mail, even if its live event was missed.
pub(crate) fn queued_inbox_recipients(conn: &Connection) -> ApiResult<Vec<String>> {
    let mut stmt = conn.prepare(
        "SELECT DISTINCT substr(to_agent, 9) FROM messages
         WHERE to_kind = 'project' AND state = 'queued'",
    )?;
    let projects = stmt
        .query_map([], |r| r.get::<_, String>(0))?
        .collect::<Result<Vec<_>, _>>()?;
    let mut recipients = Vec::new();
    for project in projects {
        if let Some(to) = inbox_recipient(conn, &project)? {
            recipients.push(to.key());
        }
    }
    Ok(recipients)
}

/// Messages, oldest first. The cursor is a message ID.
pub fn list(conn: &Connection, f: &MailFilter, page: &Page) -> ApiResult<Paged<Message>> {
    let limit = page.limit();
    let mut sql = String::from("SELECT * FROM messages WHERE id > ?");
    let mut args: Vec<String> = vec![page.cursor().unwrap_or("").to_owned()];
    for (col, val) in [
        ("to_agent", &f.to),
        ("from_agent", &f.from),
        ("thread_id", &f.thread),
    ] {
        if let Some(v) = val {
            if col == "to_agent" {
                sql.push_str(" AND (to_agent = ? OR to_agent = ?)");
            } else {
                sql.push_str(&format!(" AND {col} = ?"));
            }
            args.push(v.clone());
            if col == "to_agent" {
                args.push(routed_inbox(conn, v)?);
            }
        }
    }
    match f.state.as_deref() {
        None | Some("") | Some("all") => {}
        Some("unread") => sql.push_str(" AND state IN ('queued', 'delivered')"),
        Some(s @ ("queued" | "delivered" | "read" | "replied")) => {
            sql.push_str(" AND state = ?");
            args.push(s.to_owned());
        }
        Some(other) => {
            return Err(ApiError::bad_request(format!(
                "bad state `{other}`: use queued, delivered, read, replied, or unread"
            )));
        }
    }
    if let Some(who) = &f.involving {
        sql.push_str(" AND (to_agent = ? OR from_agent = ? OR to_agent = ?)");
        args.push(who.clone());
        args.push(who.clone());
        args.push(routed_inbox(conn, who)?);
    }
    if let Some(p) = &f.project {
        sql.push_str(
            " AND (to_agent = ? OR from_agent = ?
                   OR to_agent IN (SELECT id FROM agents WHERE project = ?
                                   UNION SELECT id FROM agents_removed WHERE project = ?)
                   OR from_agent IN (SELECT id FROM agents WHERE project = ?
                                     UNION SELECT id FROM agents_removed WHERE project = ?))",
        );
        let inbox = format!("project:{p}");
        args.extend([inbox.clone(), inbox]);
        args.extend(std::iter::repeat_n(p.clone(), 4));
    }
    if let Some(at) = &f.since {
        sql.push_str(" AND created_at >= ?");
        args.push(at.clone());
    }
    if let Some(at) = &f.until {
        sql.push_str(" AND created_at < ?");
        args.push(at.clone());
    }
    if let Some(q) = f.text.as_deref().map(str::trim).filter(|q| !q.is_empty()) {
        sql.push_str(
            " AND (instr(lower(coalesce(subject, '')), ?) > 0 OR instr(lower(body), ?) > 0)",
        );
        args.push(q.to_lowercase());
        args.push(q.to_lowercase());
    }
    sql.push_str(&format!(" ORDER BY id LIMIT {}", limit + 1));
    let mut stmt = conn.prepare(&sql)?;
    let raws = stmt
        .query_map(rusqlite::params_from_iter(args), raw_row)?
        .collect::<Result<Vec<_>, _>>()?;
    let mut cache = RenderCache::default();
    let rows = raws
        .into_iter()
        .map(|r| render(conn, r, &mut cache))
        .collect();
    Ok(Paged::from_rows(rows, limit, |m: &Message| m.id.clone()))
}

/// The addresses that posted on or received a message on a thread.
pub fn participants(conn: &Connection, thread: &str) -> ApiResult<Vec<Address>> {
    let mut stmt = conn.prepare(
        "SELECT from_agent FROM messages WHERE thread_id = ?1
         UNION SELECT to_agent FROM messages WHERE thread_id = ?1",
    )?;
    let keys = stmt
        .query_map([thread], |r| r.get::<_, String>(0))?
        .collect::<Result<BTreeSet<_>, _>>()?;
    let mut cache = HashMap::new();
    Ok(keys.iter().map(|k| from_key(conn, k, &mut cache)).collect())
}

pub fn thread_exists(conn: &Connection, thread: &str) -> ApiResult<bool> {
    let n: i64 = conn.query_row(
        "SELECT count(*) FROM messages WHERE thread_id = ?1",
        [thread],
        |r| r.get(0),
    )?;
    Ok(n > 0)
}

/// Is `who` a participant of `thread`?
pub fn is_participant(conn: &Connection, thread: &str, who: &Address) -> ApiResult<bool> {
    Ok(participants(conn, thread)?
        .iter()
        .any(|p| p.key() == who.key()))
}

/// One message to write.
pub struct Outgoing<'a> {
    pub from: &'a Address,
    pub to: &'a Address,
    pub subject: &'a str,
    pub body: &'a str,
    pub thread: Option<&'a str>,
    pub in_reply_to: Option<&'a str>,
}

/// Write one message and its `mail` event. Mail to an agent waits in
/// `queued` for the runner. Project inbox mail queues for its lead (or
/// oldest admin), keeping the project address and thread intact.
pub fn send(t: &mut Tx<'_>, m: Outgoing<'_>) -> ApiResult<Message> {
    if m.body.is_empty() {
        return Err(ApiError::invalid("the message body is empty"));
    }
    // Each message (a reply, a resume note) comes here: redact it once
    // (PLAN §16b.6).
    let (subject, body) = (t.redact(m.subject), t.redact(m.body));
    let m = Outgoing {
        subject: &subject,
        body: &body,
        ..m
    };
    let id = ulid::Ulid::new().to_string();
    let thread = m
        .thread
        .map(str::to_owned)
        .unwrap_or_else(|| ulid::Ulid::new().to_string());
    let at = now();
    if *m.to == Address::System {
        return Err(ApiError::invalid("`system` only sends mail"));
    }
    let recipient = match m.to {
        Address::Project(p) => inbox_recipient(t, p)?.ok_or_else(|| ApiError::invalid(
            format!("project:{p} has no project_admin to receive mail; ask for a project admin before sending")
        ))?,
        other => other.clone(),
    };
    // The one delivery point of every message (TER-61 review round 5, F9):
    // a write of a link request delivers nothing to an agent that holds
    // sysadmin, whatever route it came through. The error fails the whole
    // transaction, so the question, thread, or reply change rolls back.
    if t.link.is_some()
        && let Address::Agent { id, .. } = &recipient
        && agents::get(t, id).is_ok_and(|v| v.agent.roles.contains(&terrarium_core::Role::Sysadmin))
    {
        return Err(ApiError::new(
            crate::error::Code::LinkForbidden,
            "a link key may not deliver a message to an agent that holds sysadmin \
             (docs/api.md §36.4)",
        ));
    }
    // The loop guard: agent mail past the limit types no pointer.
    let limit = mail_loop_limit();
    let held_reason = if matches!(
        (m.from, m.to),
        (Address::Agent { .. }, Address::Agent { .. })
    ) {
        let run = agent_run(t, &thread)?;
        (run >= limit).then(|| {
            format!(
                "loop guard: message {} of agent-to-agent mail on thread {thread} since the \
                 last operator or project-inbox message (limit {limit})",
                run + 1
            )
        })
    } else {
        None
    };
    let held = held_reason.is_some();
    let (state, delivered) = match &recipient {
        Address::Agent { .. } if !held => ("queued", None),
        _ => ("delivered", Some(at.clone())),
    };
    t.execute(
        "INSERT INTO messages (id, thread_id, from_agent, from_kind, to_agent, to_kind, subject,
                               body, state, created_at, delivered_at, in_reply_to, held,
                               held_reason)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?14)",
        params![
            id,
            thread,
            m.from.key(),
            m.from.kind(),
            m.to.key(),
            m.to.kind(),
            m.subject,
            m.body,
            state,
            at,
            delivered,
            m.in_reply_to,
            held,
            held_reason
        ],
    )?;
    let summary = format!(
        "mail from {} to {}: {}",
        m.from.display(),
        m.to.display(),
        m.subject
    );
    let mut data = json!({
        "message_id": id,
        "thread_id": thread,
        "from": m.from.key(),
        "from_name": m.from.display(),
        "to": m.to.key(),
        "to_name": m.to.display(),
        "subject": m.subject,
    });
    if let Some(reason) = &held_reason {
        data["held"] = json!(true);
        data["held_reason"] = json!(reason);
        data["thread_state"] = json!("paused");
    }
    let mut draft = EventDraft::new("mail", summary, data);
    // The event names the recipient agent, also for a held message, so it
    // shows in the agent's events. The push listener types no pointer for
    // an event with `held: true`.
    if let Address::Agent { id, name, .. } = &recipient {
        draft = draft.agent(id.clone(), name.clone());
    }
    if let Some(p) = m.to.project() {
        draft = draft.project(p);
    }
    t.emit(draft)?;
    if *m.to == Address::Operator && !is_pause_notice(m.from, m.subject) {
        crate::store::inbox::emit(t, &format!("mail:{id}"), "mail", "opened", m.from.project())?;
    }
    crate::store::inbox::on_thread_mail(t, &thread, m.from, m.to)?;
    if held {
        pause_notice(t, &thread)?;
        crate::store::inbox::emit(
            t,
            &format!("thread:{thread}"),
            "system",
            "updated",
            m.to.project(),
        )?;
    }
    get(t, &id)
}

/// The subject of the notice of a pause, from `system` to the operator.
pub(super) const PAUSE_SUBJECT_LIKE: &str = "Thread % needs a human";

/// Is this the notice of a pause? The Inbox shows the paused thread as a
/// system item, not this mail.
pub fn is_pause_notice(from: &Address, subject: &str) -> bool {
    *from == Address::System
        && subject.starts_with("Thread ")
        && subject.ends_with(" needs a human")
}

/// [`is_pause_notice`] on a stored message: `from` is the address key of
/// the row. A pause notice opens no mail item ([`send`] skips it), so its
/// close emits none either (`inbox::mail_closed`).
pub fn is_pause_notice_key(from: &str, subject: &str) -> bool {
    from == "system" && is_pause_notice(&Address::System, subject)
}

/// `TERRARIUM_MAIL_LOOP_LIMIT`: the agent-to-agent messages a thread may
/// have since the last operator or project-inbox message before the loop
/// guard pauses it (PLAN §12).
pub const ENV_MAIL_LOOP_LIMIT: &str = "TERRARIUM_MAIL_LOOP_LIMIT";
/// The default of [`ENV_MAIL_LOOP_LIMIT`].
pub const DEFAULT_MAIL_LOOP_LIMIT: usize = 6;

/// The loop limit: [`ENV_MAIL_LOOP_LIMIT`], else
/// [`DEFAULT_MAIL_LOOP_LIMIT`]. Read once.
pub fn mail_loop_limit() -> usize {
    static LIMIT: std::sync::OnceLock<usize> = std::sync::OnceLock::new();
    *LIMIT.get_or_init(|| {
        std::env::var(ENV_MAIL_LOOP_LIMIT)
            .ok()
            .and_then(|v| v.trim().parse().ok())
            .unwrap_or(DEFAULT_MAIL_LOOP_LIMIT)
    })
}

/// The rowid of the last message on the thread that resets the loop guard:
/// one from the operator, or one from or to a project inbox; or the last
/// message at a resume with no mail (`thread_resumes`). 0 when none.
fn last_reset(conn: &Connection, thread: &str) -> ApiResult<i64> {
    Ok(conn.query_row(
        "SELECT max(
            (SELECT coalesce(max(rowid), 0) FROM messages WHERE thread_id = ?1
               AND (from_kind IN ('operator', 'project') OR to_kind = 'project')),
            (SELECT coalesce(max(after_rowid), 0) FROM thread_resumes WHERE thread_id = ?1))",
        [thread],
        |r| r.get(0),
    )?)
}

/// The agent-to-agent messages on the thread since the last reset, and the
/// limit. For the system items of the Inbox.
pub fn loop_state(conn: &Connection, thread: &str) -> ApiResult<(usize, usize)> {
    Ok((agent_run(conn, thread)?, mail_loop_limit()))
}

/// The agent-to-agent messages on the thread since the last reset.
fn agent_run(conn: &Connection, thread: &str) -> ApiResult<usize> {
    let n: i64 = conn.query_row(
        "SELECT count(*) FROM messages WHERE thread_id = ?1
           AND from_kind = 'agent' AND to_kind = 'agent' AND rowid > ?2",
        params![thread, last_reset(conn, thread)?],
        |r| r.get(0),
    )?;
    Ok(usize::try_from(n).unwrap_or(0))
}

/// Is the thread paused by the loop guard?
pub fn thread_paused(conn: &Connection, thread: &str) -> ApiResult<bool> {
    Ok(agent_run(conn, thread)? > mail_loop_limit())
}

/// Mail the operator once for each pause of a thread. The notice goes on
/// the thread, so the operator can answer there and reset the guard.
fn pause_notice(t: &mut Tx<'_>, thread: &str) -> ApiResult<()> {
    let sent: i64 = t.query_row(
        "SELECT count(*) FROM messages WHERE thread_id = ?1
           AND from_kind = 'system' AND to_kind = 'operator' AND rowid > ?2
           AND subject LIKE ?3",
        params![thread, last_reset(t, thread)?, PAUSE_SUBJECT_LIKE],
        |r| r.get(0),
    )?;
    if sent > 0 {
        return Ok(());
    }
    let n = agent_run(t, thread)?;
    send(
        t,
        Outgoing {
            from: &Address::System,
            to: &Address::Operator,
            subject: &format!("Thread {thread} needs a human"),
            body: &format!(
                "thread {thread} needs a human: {n} agent replies without operator input. \
                 The daemon types no more mail pointers for this thread. Resume it in the \
                 Inbox (or `terrarium admin inbox resume {thread}`), or reply on the thread."
            ),
            thread: Some(thread),
            in_reply_to: None,
        },
    )?;
    Ok(())
}

/// The mail to the operator for the Inbox, newest first, at most `limit`:
/// the open ones (`queued` or `delivered`: not acknowledged) or the done
/// ones (`read` or `replied`). The notices of a pause are not there: the
/// Inbox shows the paused thread as a system item.
pub fn operator_mail(conn: &Connection, open: bool, limit: usize) -> ApiResult<Vec<Message>> {
    let states = if open {
        "('queued', 'delivered')"
    } else {
        "('read', 'replied')"
    };
    let mut st = conn.prepare(&format!(
        "SELECT * FROM messages WHERE to_kind = 'operator' AND state IN {states}
           AND NOT (from_kind = 'system' AND coalesce(subject, '') LIKE ?1)
         ORDER BY id DESC LIMIT {limit}"
    ))?;
    let raws = st
        .query_map([PAUSE_SUBJECT_LIKE], raw_row)?
        .collect::<Result<Vec<_>, _>>()?;
    let mut cache = RenderCache::default();
    Ok(raws
        .into_iter()
        .map(|r| render(conn, r, &mut cache))
        .collect())
}

/// Count without loading bodies or resolving the loop state of every mail thread.
pub fn operator_mail_count(conn: &Connection, project: Option<&str>) -> ApiResult<usize> {
    Ok(conn.query_row(
        "SELECT count(*) FROM messages m
         LEFT JOIN agents a ON m.from_kind = 'agent' AND a.id = m.from_agent
         LEFT JOIN agents_removed old ON m.from_kind = 'agent' AND old.id = m.from_agent
         WHERE m.to_kind = 'operator' AND m.state IN ('queued', 'delivered')
           AND NOT (m.from_kind = 'system' AND coalesce(m.subject, '') LIKE ?1)
           AND (?2 IS NULL OR
                (m.from_kind = 'project' AND m.from_agent = 'project:' || ?2) OR
                (m.from_kind = 'agent' AND coalesce(a.project, old.project) = ?2))",
        params![PAUSE_SUBJECT_LIKE, project],
        |r| r.get(0),
    )?)
}

/// The held messages on `thread` that the operator did not release yet,
/// oldest first.
pub fn unreleased_held(conn: &Connection, thread: &str) -> ApiResult<Vec<Message>> {
    let mut st = conn.prepare(
        "SELECT * FROM messages WHERE thread_id = ?1 AND held = 1 AND released_at IS NULL
         ORDER BY id",
    )?;
    let raws = st
        .query_map([thread], raw_row)?
        .collect::<Result<Vec<_>, _>>()?;
    let mut cache = RenderCache::default();
    Ok(raws
        .into_iter()
        .map(|r| render(conn, r, &mut cache))
        .collect())
}

/// The threads that need the operator because of the loop guard: a held
/// message that is not released, or a pause now. These are the system
/// items of the Inbox. A paused thread has a held message since its last
/// reset, so the threads with a held message are the candidates.
pub fn guard_threads(conn: &Connection) -> ApiResult<Vec<String>> {
    let mut st = conn.prepare(
        "SELECT thread_id, max(released_at IS NULL) FROM messages WHERE held = 1
         GROUP BY thread_id ORDER BY min(id)",
    )?;
    let rows = st
        .query_map([], |r| Ok((r.get::<_, String>(0)?, r.get::<_, bool>(1)?)))?
        .collect::<Result<Vec<_>, _>>()?;
    let mut out = Vec::new();
    for (thread, unreleased) in rows {
        if unreleased || thread_paused(conn, &thread)? {
            out.push(thread);
        }
    }
    Ok(out)
}

/// Release one held message (the operator). A message to an agent becomes
/// `queued` again, and its `mail` event (with `released: true` and no
/// `held`) makes the push pointer for the recipient. The thread stays
/// paused. A message that is not held, or that is released already, is
/// `409`.
pub fn release(t: &mut Tx<'_>, id: &str) -> ApiResult<Message> {
    let m = get(t, id)?;
    if !m.held {
        return Err(ApiError::conflict(format!(
            "message {id} is not held: the loop guard did not stop it"
        )));
    }
    if let Some(at) = &m.released_at {
        return Err(ApiError::conflict(format!(
            "message {id} was released at {at}"
        )));
    }
    let to_agent = m.to_kind == "agent";
    t.execute(
        "UPDATE messages SET released_at = ?2, released_by = ?3,
                state = CASE WHEN ?4 AND state = 'delivered' THEN 'queued' ELSE state END,
                delivered_at = CASE WHEN ?4 AND state = 'delivered' THEN NULL
                                    ELSE delivered_at END
         WHERE id = ?1",
        params![id, now(), t.actor, to_agent],
    )?;
    let m = get(t, id)?;
    let to = from_key(t, &m.to, &mut HashMap::new());
    if to_agent {
        let data = json!({
            "message_id": m.id,
            "thread_id": m.thread_id,
            "from": m.from,
            "from_name": m.from_name,
            "to": m.to,
            "to_name": m.to_name,
            "subject": m.subject,
            "released": true,
        });
        let mut draft = EventDraft::new(
            "mail",
            format!(
                "mail from {} to {} released: {}",
                m.from_name, m.to_name, m.subject
            ),
            data,
        );
        if let Address::Agent { id, name, .. } = &to {
            draft = draft.agent(id.clone(), name.clone());
        }
        if let Some(p) = to.project().filter(|p| !p.is_empty()) {
            draft = draft.project(p);
        }
        t.emit(draft)?;
    }
    let project = to.project().filter(|p| !p.is_empty()).map(str::to_owned);
    t.audit(
        "mail.release",
        &m.id,
        project.as_deref(),
        json!({"thread": m.thread_id, "to": m.to}),
    )?;
    let change = if guard_threads(t)?.contains(&m.thread_id) {
        "updated"
    } else {
        "closed"
    };
    crate::store::inbox::emit(
        t,
        &format!("thread:{}", m.thread_id),
        "system",
        change,
        project.as_deref(),
    )?;
    Ok(m)
}

/// The outcome of [`resume`].
#[derive(Debug, Clone, Serialize)]
pub struct Resumed {
    pub thread_id: String,
    /// The held messages that the resume released.
    pub released: Vec<String>,
    /// The note of the operator: one message to each other participant.
    pub note: Vec<Message>,
}

/// Resume a thread (the operator): end the pause of the loop guard, and
/// release each held message on it. With a note, the note goes from the
/// operator to the other participants of the thread. With no note, nobody
/// gets mail. A `thread_resumes` row resets the guard in both cases. The
/// pause notices on the thread become read. A thread that is not paused
/// and has no held message is `409`.
pub fn resume(t: &mut Tx<'_>, thread: &str, note: Option<&str>) -> ApiResult<Resumed> {
    if !thread_exists(t, thread)? {
        return Err(ApiError::not_found(format!("no thread `{thread}`")));
    }
    let held = unreleased_held(t, thread)?;
    if held.is_empty() && !thread_paused(t, thread)? {
        return Err(ApiError::conflict(format!(
            "thread {thread} is not paused and has no held message"
        )));
    }
    let max: i64 = t.query_row(
        "SELECT coalesce(max(rowid), 0) FROM messages WHERE thread_id = ?1",
        [thread],
        |r| r.get(0),
    )?;
    t.execute(
        "INSERT INTO thread_resumes (thread_id, after_rowid, by, at) VALUES (?1, ?2, ?3, ?4)",
        params![thread, max, t.actor, now()],
    )?;
    let mut released = Vec::new();
    for m in &held {
        release(t, &m.id)?;
        released.push(m.id.clone());
    }
    let note = note.map(str::trim).filter(|n| !n.is_empty());
    let mut sent = Vec::new();
    if let Some(body) = note {
        let subject: String = t
            .query_row(
                "SELECT subject FROM messages WHERE thread_id = ?1
                   AND coalesce(subject, '') <> '' ORDER BY id LIMIT 1",
                [thread],
                |r| r.get(0),
            )
            .optional()?
            .unwrap_or_default();
        let subject = if subject.is_empty() || subject.starts_with("Re: ") {
            subject
        } else {
            format!("Re: {subject}")
        };
        for to in participants(t, thread)? {
            if matches!(to, Address::Operator | Address::System) {
                continue;
            }
            sent.push(send(
                t,
                Outgoing {
                    from: &Address::Operator,
                    to: &to,
                    subject: &subject,
                    body,
                    thread: Some(thread),
                    in_reply_to: None,
                },
            )?);
        }
    }
    let at = now();
    t.execute(
        "UPDATE messages SET state = 'read', read_at = coalesce(read_at, ?2),
                read_by = coalesce(read_by, 'operator')
         WHERE thread_id = ?1 AND from_kind = 'system' AND to_kind = 'operator'
           AND subject LIKE ?3 AND state IN ('queued', 'delivered')",
        params![thread, at, PAUSE_SUBJECT_LIKE],
    )?;
    t.audit(
        "mail.resume",
        thread,
        None,
        json!({"released": released, "note": note.is_some()}),
    )?;
    crate::store::inbox::emit(t, &format!("thread:{thread}"), "system", "closed", None)?;
    Ok(Resumed {
        thread_id: thread.to_owned(),
        released,
        note: sent,
    })
}

/// Reply on the thread of `original`: one message to every other
/// participant. The original becomes `replied`.
pub fn reply(
    t: &mut Tx<'_>,
    original: &Message,
    from: &Address,
    subject: Option<&str>,
    body: &str,
) -> ApiResult<Vec<Message>> {
    let subject = match subject {
        Some(s) => s.to_owned(),
        None => {
            // The subject of the message, else the first subject on the thread.
            let base = if original.subject.is_empty() {
                t.query_row(
                    "SELECT subject FROM messages WHERE thread_id = ?1 AND coalesce(subject, '') <> ''
                     ORDER BY id LIMIT 1",
                    [&original.thread_id],
                    |r| r.get::<_, String>(0),
                )
                .optional()?
                .unwrap_or_default()
            } else {
                original.subject.clone()
            };
            if base.is_empty() || base.starts_with("Re: ") {
                base
            } else {
                format!("Re: {base}")
            }
        }
    };
    let all = participants(t, &original.thread_id)?;
    let system_thread = all.contains(&Address::System);
    let recipients: Vec<Address> = all
        .into_iter()
        .filter(|p| p.key() != from.key() && *p != Address::System)
        .collect();
    if recipients.is_empty() {
        // A daemon notice (a wait result, a watch) has nobody to answer.
        return Err(ApiError::invalid(if system_thread {
            "this is a system mail: it needs no reply. Act on it, or mail the agent \
             or the operator with `terrarium mail send`"
        } else {
            "the thread has no other participant"
        }));
    }
    let mut out = Vec::new();
    for to in &recipients {
        out.push(send(
            t,
            Outgoing {
                from,
                to,
                subject: &subject,
                body,
                thread: Some(&original.thread_id),
                in_reply_to: Some(&original.id),
            },
        )?);
    }
    let first = original.read_at.is_none();
    t.execute(
        "UPDATE messages SET state = 'replied', read_at = coalesce(read_at, ?2),
                read_by = CASE WHEN read_at IS NULL THEN ?3 ELSE read_by END
         WHERE id = ?1",
        params![original.id, now(), reader(t)],
    )?;
    if first {
        audit_read(t, original, "reply")?;
    }
    crate::store::inbox::mail_closed(t, original)?;
    Ok(out)
}

/// Mark queued messages `delivered`: the runner typed the push pointer for
/// them and the harness took the turn (PLAN §12). Other states stay.
pub fn mark_delivered(t: &mut Tx<'_>, ids: &[String]) -> ApiResult<usize> {
    let at = now();
    let mut n = 0;
    for id in ids {
        n += t.execute(
            "UPDATE messages SET state = 'delivered', delivered_at = coalesce(delivered_at, ?2)
             WHERE id = ?1 AND state = 'queued'",
            params![id, at],
        )?;
    }
    Ok(n)
}

/// Mark messages `read` by `reader` (an agent ULID) that a submitted turn
/// held whole (PLAN §8.5): the agent saw the subject and the body. Only a
/// `queued` or `delivered` message changes. Each one writes a `mail.read`
/// audit row with `via = "turn"`.
pub fn mark_read_in_turn(t: &mut Tx<'_>, ids: &[String], reader: &str) -> ApiResult<usize> {
    let at = now();
    let mut n = 0;
    for id in ids {
        let before = get(t, id)?;
        let changed = t.execute(
            "UPDATE messages SET state = 'read', read_at = coalesce(read_at, ?2),
                    read_by = coalesce(read_by, ?3),
                    delivered_at = coalesce(delivered_at, ?2)
             WHERE id = ?1 AND state IN ('queued', 'delivered')",
            params![id, at, reader],
        )?;
        if changed > 0 && before.read_at.is_none() {
            audit_read(t, &before, "turn")?;
        }
        if changed > 0 {
            crate::store::inbox::mail_closed(t, &before)?;
        }
        n += changed;
    }
    Ok(n)
}

/// Mark a message `read`. A `replied` message stays `replied`. The first
/// read records the reader (`read_by`) and writes a `mail.read` audit row;
/// a read again changes nothing.
pub fn mark_read(t: &mut Tx<'_>, id: &str) -> ApiResult<Message> {
    let before = get(t, id)?;
    t.execute(
        "UPDATE messages SET state = CASE WHEN state = 'replied' THEN state ELSE 'read' END,
                read_by = CASE WHEN read_at IS NULL THEN ?3 ELSE read_by END,
                read_at = coalesce(read_at, ?2),
                delivered_at = coalesce(delivered_at, ?2)
         WHERE id = ?1",
        params![id, now(), reader(t)],
    )?;
    if before.read_at.is_none() {
        audit_read(t, &before, "read")?;
    }
    crate::store::inbox::mail_closed(t, &before)?;
    get(t, id)
}

/// The key of the reader of a transaction: `operator`, `system`, or an
/// agent ULID.
fn reader(t: &Tx<'_>) -> String {
    t.actor
        .strip_prefix("agent:")
        .unwrap_or(&t.actor)
        .to_owned()
}

/// The `mail.read` audit row of the first read of `m`. `via` is `read`
/// (`POST .../read`: the CLI, the dashboard) or `reply`.
fn audit_read(t: &Tx<'_>, m: &Message, via: &str) -> ApiResult<()> {
    let to = from_key(t, &m.to, &mut HashMap::new());
    let project = to.project().filter(|p| !p.is_empty());
    t.audit(
        "mail.read",
        &m.id,
        project,
        json!({"thread": m.thread_id, "from": m.from, "to": m.to, "via": via}),
    )
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;

    /// The `inbox` events of one item, as the daemon streams them.
    fn inbox_events(t: &Tx<'_>, item: &str) -> Vec<serde_json::Value> {
        let mut st = t
            .prepare("SELECT payload FROM events WHERE kind = 'inbox'")
            .unwrap();
        st.query_map([], |r| r.get::<_, String>(0))
            .unwrap()
            .filter_map(|p| serde_json::from_str::<serde_json::Value>(&p.ok()?).ok())
            .filter(|d| d["item"] == item)
            .collect()
    }

    /// The `inbox` closed events of one item, as the daemon streams them.
    fn closed_events(t: &Tx<'_>, item: &str) -> Vec<serde_json::Value> {
        inbox_events(t, item)
            .into_iter()
            .filter(|d| d["change"] == "closed")
            .collect()
    }

    /// GitHub #25: every path that moves a mail to the operator out of
    /// `queued` or `delivered` emits the `inbox` closed event of its item,
    /// so the Inbox closes it when the operator reads or replies outside
    /// it. Mail to an agent emits nothing, and a second read changes
    /// nothing.
    #[test]
    fn operator_mail_closes_the_inbox_item_on_every_path() {
        let mut conn = Connection::open_in_memory().unwrap();
        crate::db::migrate(&mut conn).unwrap();
        let agent_id = ulid::Ulid::new().to_string();
        conn.execute(
            "INSERT INTO agents (id, name, project, harness, workspace, created_at, updated_at)
             VALUES (?1, 'w', 'p', 'claude', '/w', 't', 't')",
            [&agent_id],
        )
        .unwrap();
        let worker = Address::Agent {
            id: agent_id.clone(),
            name: "w".into(),
            project: "p".into(),
        };
        let mut t = Tx::for_test(conn.transaction().unwrap(), "operator");
        let to_operator = |t: &mut Tx<'_>, from: &Address, subject: &str| {
            send(
                t,
                Outgoing {
                    from,
                    to: &Address::Operator,
                    subject,
                    body: "body",
                    thread: None,
                    in_reply_to: None,
                },
            )
        };

        // A read in a turn (the daemon typed the mail into a turn) closes it.
        let m = to_operator(&mut t, &Address::System, "One").unwrap();
        assert_eq!(
            mark_read_in_turn(&mut t, std::slice::from_ref(&m.id), "operator").unwrap(),
            1
        );
        let closed = closed_events(&t, &format!("mail:{}", m.id));
        assert_eq!(closed.len(), 1, "{closed:?}");
        assert_eq!(closed[0]["kind"], "mail");
        // Again: nothing changed, no second event.
        assert_eq!(
            mark_read_in_turn(&mut t, std::slice::from_ref(&m.id), "operator").unwrap(),
            0
        );
        assert_eq!(closed_events(&t, &format!("mail:{}", m.id)).len(), 1);

        // A read outside the Inbox (the Mail page, the CLI).
        let m = to_operator(&mut t, &Address::System, "Two").unwrap();
        assert_eq!(mark_read(&mut t, &m.id).unwrap().state, "read");
        assert_eq!(mark_read(&mut t, &m.id).unwrap().state, "read");
        assert_eq!(closed_events(&t, &format!("mail:{}", m.id)).len(), 1);

        // A reply outside the Inbox closes it, with the project of the sender.
        let m = to_operator(&mut t, &worker, "Three").unwrap();
        assert_eq!(m.from_name, "p/w");
        reply(&mut t, &m, &Address::Operator, None, "answer").unwrap();
        let closed = closed_events(&t, &format!("mail:{}", m.id));
        assert_eq!(closed.len(), 1, "{closed:?}");
        assert_eq!(closed[0]["project"], "p");

        // Mail to an agent is no Inbox item: reading it emits nothing.
        let to_agent = send(
            &mut t,
            Outgoing {
                from: &Address::Operator,
                to: &worker,
                subject: "Four",
                body: "body",
                thread: None,
                in_reply_to: None,
            },
        )
        .unwrap();
        assert_eq!(to_agent.state, "queued");
        mark_read(&mut t, &to_agent.id).unwrap();
        mark_read_in_turn(&mut t, std::slice::from_ref(&to_agent.id), &agent_id).unwrap();
        reply(&mut t, &to_agent, &worker, None, "answer").unwrap();
        assert!(closed_events(&t, &format!("mail:{}", to_agent.id)).is_empty());

        // A pause notice opened no mail item (`send` skips it), so reading
        // it closes none: no event at all, on any path.
        let notice = to_operator(&mut t, &Address::System, "Thread 01J needs a human").unwrap();
        assert!(inbox_events(&t, &format!("mail:{}", notice.id)).is_empty());
        mark_read(&mut t, &notice.id).unwrap();
        mark_read_in_turn(&mut t, std::slice::from_ref(&notice.id), "operator").unwrap();
        assert!(inbox_events(&t, &format!("mail:{}", notice.id)).is_empty());
    }

    /// The name reader goes through the tombstone resolver: a deleted agent
    /// shows as `project/name`, and a key that never was an agent shows as
    /// the key alone, never as `/<ULID>`.
    #[test]
    fn names_resolve_through_the_tombstone() {
        let mut conn = Connection::open_in_memory().unwrap();
        crate::db::migrate(&mut conn).unwrap();
        let gone = ulid::Ulid::new().to_string();
        conn.execute(
            "INSERT INTO agents_removed (id, project, name, removed_at) VALUES (?1, 'p', 'w', 't')",
            [&gone],
        )
        .unwrap();
        let mut cache = HashMap::new();
        assert_eq!(from_key(&conn, &gone, &mut cache).display(), "p/w");
        let never = ulid::Ulid::new().to_string();
        assert_eq!(from_key(&conn, &never, &mut cache).display(), never);
        assert_eq!(from_key(&conn, "system", &mut cache).display(), "system");
        assert_eq!(resolve(&conn, "system", None).unwrap(), Address::System);
    }
}
