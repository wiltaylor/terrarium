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
            Self::Project(p) => format!("project:{p}"),
            Self::Agent { id, .. } => id.clone(),
        }
    }

    pub fn kind(&self) -> &'static str {
        match self {
            Self::Operator => "operator",
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
            Self::Operator => None,
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

/// Turn a stored key back into an address. A deleted agent keeps its ID.
fn from_key(conn: &Connection, key: &str, cache: &mut HashMap<String, Address>) -> Address {
    if let Some(a) = cache.get(key) {
        return a.clone();
    }
    let a = if key == "operator" {
        Address::Operator
    } else if let Some(p) = key.strip_prefix("project:") {
        Address::Project(p.to_owned())
    } else {
        match agents::get(conn, key) {
            Ok(v) => Address::Agent {
                id: key.to_owned(),
                name: v.agent.name,
                project: v.agent.project,
            },
            Err(_) => Address::Agent {
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
    })
}

fn render(conn: &Connection, raw: RawMessage, cache: &mut HashMap<String, Address>) -> Message {
    let from_name = from_key(conn, &raw.from, cache).display();
    let to_name = from_key(conn, &raw.to, cache).display();
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
    }
}

pub fn get(conn: &Connection, id: &str) -> ApiResult<Message> {
    let raw = conn
        .query_row("SELECT * FROM messages WHERE id = ?1", [id], raw_row)
        .optional()?
        .ok_or_else(|| ApiError::not_found(format!("no message `{id}`")))?;
    Ok(render(conn, raw, &mut HashMap::new()))
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
    let mut cache = HashMap::new();
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
    let (state, delivered) = match m.to {
        Address::Agent { .. } => ("queued", None),
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
    let data = json!({
        "message_id": id,
        "thread_id": thread,
        "from": m.from.key(),
        "from_name": m.from.display(),
        "to": m.to.key(),
        "to_name": m.to.display(),
        "subject": m.subject,
    });
    let mut draft = EventDraft::new("mail", summary, data);
    if let Address::Agent { id, name, .. } = m.to {
        draft = draft.agent(id.clone(), name.clone());
    }
    if let Some(p) = m.to.project() {
        draft = draft.project(p);
    }
    t.emit(draft)?;
    get(t, &id)
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
        .filter(|p| p.key() != from.key())
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
    t.execute(
        "UPDATE messages SET state = 'replied', read_at = coalesce(read_at, ?2) WHERE id = ?1",
        params![original.id, now()],
    )?;
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

/// Mark a message `read`. A `replied` message stays `replied`.
pub fn mark_read(t: &mut Tx<'_>, id: &str) -> ApiResult<Message> {
    get(t, id)?;
    t.execute(
        "UPDATE messages SET state = CASE WHEN state = 'replied' THEN state ELSE 'read' END,
                read_at = coalesce(read_at, ?2),
                delivered_at = coalesce(delivered_at, ?2)
         WHERE id = ?1",
        params![id, now()],
    )?;
    get(t, id)
}
