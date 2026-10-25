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

    /// A readable form: `operator`, `project:<name>`, `project/agent`.
    pub fn display(&self) -> String {
        match self {
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

/// Parse an address: `operator`, `project:<name>`, an agent ULID,
/// `project/agent`, or a bare agent name (looked up in `context` first).
pub fn resolve(conn: &Connection, s: &str, context: Option<&str>) -> ApiResult<Address> {
    let s = s.trim();
    if s == "operator" {
        return Ok(Address::Operator);
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
        match agents::get(conn, key) {
            Ok(v) => Address::Agent {
                id: key.to_owned(),
                name: v.agent.name,
                project: v.agent.project,
            },
            // A deleted agent keeps its name (migration 12), else its ID.
            Err(_) => match agents::removed(conn, key).ok().flatten() {
                Some((project, name)) => Address::Agent {
                    id: key.to_owned(),
                    name,
                    project,
                },
                None => Address::Agent {
                    id: key.to_owned(),
                    name: key.to_owned(),
                    project: String::new(),
                },
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
    }
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
            sql.push_str(&format!(" AND {col} = ?"));
            args.push(v.clone());
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
        sql.push_str(" AND (to_agent = ? OR from_agent = ?)");
        args.push(who.clone());
        args.push(who.clone());
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
/// `queued` for the runner; mail to the operator or a project inbox is
/// `delivered` at once.
pub fn send(t: &mut Tx<'_>, m: Outgoing<'_>) -> ApiResult<Message> {
    if m.body.is_empty() {
        return Err(ApiError::invalid("the message body is empty"));
    }
    let id = ulid::Ulid::new().to_string();
    let thread = m
        .thread
        .map(str::to_owned)
        .unwrap_or_else(|| ulid::Ulid::new().to_string());
    let at = now();
    if *m.to == Address::System {
        return Err(ApiError::invalid("`system` only sends mail"));
    }
    // The loop guard: agent mail past the limit types no pointer.
    let held = matches!(
        (m.from, m.to),
        (Address::Agent { .. }, Address::Agent { .. })
    ) && agent_run(t, &thread)? >= mail_loop_limit();
    let (state, delivered) = match m.to {
        Address::Agent { .. } if !held => ("queued", None),
        _ => ("delivered", Some(at.clone())),
    };
    t.execute(
        "INSERT INTO messages (id, thread_id, from_agent, from_kind, to_agent, to_kind, subject,
                               body, state, created_at, delivered_at, in_reply_to)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12)",
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
            m.in_reply_to
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
    if held {
        data["held"] = json!(true);
        data["thread_state"] = json!("paused");
    }
    let mut draft = EventDraft::new("mail", summary, data);
    // The event names the recipient agent, also for a held message, so it
    // shows in the agent's events. The push listener types no pointer for
    // an event with `held: true`.
    if let Address::Agent { id, name, .. } = m.to {
        draft = draft.agent(id.clone(), name.clone());
    }
    if let Some(p) = m.to.project() {
        draft = draft.project(p);
    }
    t.emit(draft)?;
    if held {
        pause_notice(t, &thread)?;
    }
    get(t, &id)
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
/// one from the operator, or one from or to a project inbox. 0 when none.
fn last_reset(conn: &Connection, thread: &str) -> ApiResult<i64> {
    Ok(conn.query_row(
        "SELECT coalesce(max(rowid), 0) FROM messages WHERE thread_id = ?1
           AND (from_kind IN ('operator', 'project') OR to_kind = 'project')",
        [thread],
        |r| r.get(0),
    )?)
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
           AND subject LIKE 'Thread % needs a human'",
        params![thread, last_reset(t, thread)?],
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
                 The daemon types no more mail pointers for this thread. Reply on the \
                 thread to resume it."
            ),
            thread: Some(thread),
            in_reply_to: None,
        },
    )?;
    Ok(())
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
    let recipients: Vec<Address> = participants(t, &original.thread_id)?
        .into_iter()
        .filter(|p| p.key() != from.key() && *p != Address::System)
        .collect();
    if recipients.is_empty() {
        return Err(ApiError::invalid("the thread has no other participant"));
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
