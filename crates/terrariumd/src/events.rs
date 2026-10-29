//! The append-only event log and the live event hub.
//!
//! Every change that a client may want to watch is one row in `events`:
//! agent status, hook events, mail, and board changes. `seq` orders the log
//! and is the `?since=` cursor of the event streams.

use rusqlite::{Connection, OptionalExtension, Row, params_from_iter};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use tokio::sync::broadcast;

use crate::db::now;
use crate::error::ApiResult;

/// One row of the event log, as the API shows it.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct StoredEvent {
    /// The position in the log. Use it as `?since=`.
    pub seq: i64,
    pub id: String,
    pub at: String,
    /// For example `status`, `turn_start`, `mail`, `card`.
    pub kind: String,
    /// `operator`, `agent:<ulid>`, or `system`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub actor: Option<String>,
    /// The agent name. `data.agent_id` and `agent_id` carry the ULID.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub agent: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub agent_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub project: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub card: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub session: Option<String>,
    /// A short line for lists.
    #[serde(default)]
    pub summary: String,
    pub data: Value,
}

/// An event before it has a place in the log.
#[derive(Debug, Clone)]
pub struct EventDraft {
    pub kind: String,
    pub summary: String,
    pub agent_id: Option<String>,
    pub agent_name: Option<String>,
    pub project: Option<String>,
    pub card: Option<String>,
    pub session: Option<String>,
    pub data: Value,
}

impl EventDraft {
    pub fn new(kind: impl Into<String>, summary: impl Into<String>, data: Value) -> Self {
        Self {
            kind: kind.into(),
            summary: summary.into(),
            agent_id: None,
            agent_name: None,
            project: None,
            card: None,
            session: None,
            data,
        }
    }
    /// The agent the event is about: its ULID and its name.
    pub fn agent(mut self, id: impl Into<String>, name: impl Into<String>) -> Self {
        self.agent_id = Some(id.into());
        self.agent_name = Some(name.into());
        self
    }
    pub fn project(mut self, project: impl Into<String>) -> Self {
        self.project = Some(project.into());
        self
    }
    pub fn card(mut self, card: impl Into<String>) -> Self {
        self.card = Some(card.into());
        self
    }
    pub fn session(mut self, session: Option<String>) -> Self {
        self.session = session;
        self
    }
}

/// The event kinds that name the agent that did them when the draft names
/// no agent, so they show in `GET /api/agents/{id}/events`. Mail and card
/// events name their subject (the recipient, the assignee) on their own:
/// the push listener reads that as the agent to point at.
const ACTOR_ATTRIBUTED: &[&str] = &["wiki.", "artifact.", "memory.", "version."];

/// Name the acting agent on a draft of an [`ACTOR_ATTRIBUTED`] kind.
fn attribute_actor(conn: &Connection, actor: &str, d: &mut EventDraft) -> ApiResult<()> {
    let Some(agent) = actor.strip_prefix("agent:") else {
        return Ok(());
    };
    if d.agent_id.is_some() || !ACTOR_ATTRIBUTED.iter().any(|p| d.kind.starts_with(p)) {
        return Ok(());
    }
    let name: Option<String> = conn
        .query_row("SELECT name FROM agents WHERE id = ?1", [agent], |r| {
            r.get(0)
        })
        .optional()?;
    if let Some(name) = name {
        d.agent_id = Some(agent.to_owned());
        d.agent_name = Some(name);
    }
    Ok(())
}

/// Append one event.
pub fn insert(conn: &Connection, actor: &str, mut d: EventDraft) -> ApiResult<StoredEvent> {
    attribute_actor(conn, actor, &mut d)?;
    let id = ulid::Ulid::new().to_string();
    let at = now();
    if let Value::Object(map) = &mut d.data {
        if let Some(agent) = &d.agent_id {
            map.entry("agent_id")
                .or_insert_with(|| Value::String(agent.clone()));
        }
        if let Some(card) = &d.card {
            map.entry("card_id")
                .or_insert_with(|| Value::String(card.clone()));
        }
    }
    conn.execute(
        "INSERT INTO events (id, agent_id, agent_name, project, card, session_id, actor, kind,
                             summary, payload, at)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11)",
        rusqlite::params![
            id,
            d.agent_id,
            d.agent_name,
            d.project,
            d.card,
            d.session,
            actor,
            d.kind,
            d.summary,
            d.data.to_string(),
            at
        ],
    )?;
    let seq = conn.last_insert_rowid();
    Ok(StoredEvent {
        seq,
        id,
        at,
        kind: d.kind,
        actor: Some(actor.to_owned()),
        agent: d.agent_name,
        agent_id: d.agent_id,
        project: d.project,
        card: d.card,
        session: d.session,
        summary: d.summary,
        data: d.data,
    })
}

/// Which events a reader wants and may see.
#[derive(Debug, Clone, Default)]
pub struct EventFilter {
    pub agent: Option<String>,
    pub project: Option<String>,
    pub card: Option<String>,
    pub kind: Option<String>,
    /// For an agent reader: only events of this project or of this agent.
    pub visible_to: Option<(String, String)>,
}

impl EventFilter {
    pub fn matches(&self, e: &StoredEvent) -> bool {
        let eq = |want: &Option<String>, have: &Option<String>| {
            want.as_ref().is_none_or(|w| have.as_ref() == Some(w))
        };
        eq(&self.agent, &e.agent_id)
            && eq(&self.project, &e.project)
            && eq(&self.card, &e.card)
            && self.kind.as_ref().is_none_or(|k| *k == e.kind)
            && self.visible_to.as_ref().is_none_or(|(project, agent)| {
                e.project.as_deref() == Some(project) || e.agent_id.as_deref() == Some(agent)
            })
    }
}

fn from_row(r: &Row<'_>) -> rusqlite::Result<StoredEvent> {
    let payload: String = r.get("payload")?;
    Ok(StoredEvent {
        seq: r.get("seq")?,
        id: r.get("id")?,
        at: r.get("at")?,
        kind: r.get("kind")?,
        actor: r.get("actor")?,
        agent: r.get("agent_name")?,
        agent_id: r.get("agent_id")?,
        project: r.get("project")?,
        card: r.get("card")?,
        session: r.get("session_id")?,
        summary: r.get("summary")?,
        data: serde_json::from_str(&payload).unwrap_or(Value::String(payload)),
    })
}

/// Events after `since`, oldest first, at most `limit`.
pub fn list(
    conn: &Connection,
    filter: &EventFilter,
    since: i64,
    limit: usize,
) -> ApiResult<Vec<StoredEvent>> {
    select(conn, filter, since, None, limit)
}

/// Events after `since` and before `before` (all when `None`), newest
/// first, at most `limit`. The pages of `?order=desc`.
pub fn list_desc(
    conn: &Connection,
    filter: &EventFilter,
    since: i64,
    before: Option<i64>,
    limit: usize,
) -> ApiResult<Vec<StoredEvent>> {
    select(conn, filter, since, Some(before.unwrap_or(i64::MAX)), limit)
}

/// The query of `list` (`before` is `None`: oldest first) and of
/// `list_desc` (`before` is set: newest first). The `seq` key of the
/// table and the `(agent_id, seq)`, `(project, seq)`, and `(card, seq)`
/// indexes serve both directions.
fn select(
    conn: &Connection,
    filter: &EventFilter,
    since: i64,
    before: Option<i64>,
    limit: usize,
) -> ApiResult<Vec<StoredEvent>> {
    use rusqlite::types::Value as Sql;
    let mut sql = String::from("SELECT * FROM events WHERE seq > ?");
    // `seq` compares with an integer; bind it as one.
    let mut values: Vec<Sql> = vec![Sql::Integer(since)];
    if let Some(before) = before {
        sql.push_str(" AND seq < ?");
        values.push(Sql::Integer(before));
    }
    for (col, val) in [
        ("agent_id", &filter.agent),
        ("project", &filter.project),
        ("card", &filter.card),
        ("kind", &filter.kind),
    ] {
        if let Some(v) = val {
            sql.push_str(&format!(" AND {col} = ?"));
            values.push(Sql::Text(v.clone()));
        }
    }
    if let Some((project, agent)) = &filter.visible_to {
        sql.push_str(" AND (project = ? OR agent_id = ?)");
        values.push(Sql::Text(project.clone()));
        values.push(Sql::Text(agent.clone()));
    }
    let order = if before.is_some() { "DESC" } else { "ASC" };
    sql.push_str(&format!(" ORDER BY seq {order} LIMIT {limit}"));
    let mut stmt = conn.prepare(&sql)?;
    let rows = stmt.query_map(params_from_iter(values), from_row)?;
    Ok(rows.collect::<Result<_, _>>()?)
}

/// Turn a `?since=` value into a `seq`: a number is a `seq`; a time
/// (`time_param`: RFC 3339 with any offset, or a duration such as `30m`)
/// gives the events at or after that time; anything else is an event ID,
/// and the result is that event's `seq`. `latest` is the caller's to handle.
pub fn resolve_since(conn: &Connection, since: Option<&str>) -> ApiResult<i64> {
    let Some(since) = since.map(str::trim).filter(|s| !s.is_empty()) else {
        return Ok(0);
    };
    if let Ok(n) = since.parse::<i64>() {
        return Ok(n);
    }
    if let Some(t) = crate::time_param::parse_time(since) {
        let first: Option<i64> = conn.query_row(
            "SELECT min(seq) FROM events WHERE at >= ?1",
            [crate::time_param::stored(t)],
            |r| r.get(0),
        )?;
        return match first {
            Some(seq) => Ok(seq - 1),
            None => last_seq(conn),
        };
    }
    conn.query_row("SELECT seq FROM events WHERE id = ?1", [since], |r| {
        r.get(0)
    })
    .optional()?
    .ok_or_else(|| {
        crate::error::ApiError::bad_request(format!(
            "bad `since` `{since}`: no such event; use a seq, an event ID, `latest` (streams), \
             or {}",
            crate::time_param::FORMS
        ))
    })
}

/// The newest `seq`, or 0 for an empty log.
pub fn last_seq(conn: &Connection) -> ApiResult<i64> {
    Ok(conn
        .query_row("SELECT max(seq) FROM events", [], |r| {
            r.get::<_, Option<i64>>(0)
        })
        .optional()?
        .flatten()
        .unwrap_or(0))
}

/// The live feed of new events. Each subscriber gets every event that
/// commits after it subscribed.
#[derive(Clone)]
pub struct EventHub {
    tx: broadcast::Sender<StoredEvent>,
}

impl Default for EventHub {
    fn default() -> Self {
        let (tx, _) = broadcast::channel(4096);
        Self { tx }
    }
}

impl EventHub {
    pub fn publish(&self, event: StoredEvent) {
        // No subscriber is not an error.
        let _ = self.tx.send(event);
    }

    pub fn subscribe(&self) -> broadcast::Receiver<StoredEvent> {
        self.tx.subscribe()
    }
}
