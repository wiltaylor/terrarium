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

/// Append one event.
pub fn insert(conn: &Connection, actor: &str, mut d: EventDraft) -> ApiResult<StoredEvent> {
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
    let mut sql = String::from("SELECT * FROM events WHERE seq > ?");
    let mut args: Vec<String> = vec![since.to_string()];
    for (col, val) in [
        ("agent_id", &filter.agent),
        ("project", &filter.project),
        ("card", &filter.card),
        ("kind", &filter.kind),
    ] {
        if let Some(v) = val {
            sql.push_str(&format!(" AND {col} = ?"));
            args.push(v.clone());
        }
    }
    if let Some((project, agent)) = &filter.visible_to {
        sql.push_str(" AND (project = ? OR agent_id = ?)");
        args.push(project.clone());
        args.push(agent.clone());
    }
    sql.push_str(&format!(" ORDER BY seq LIMIT {limit}"));
    let mut stmt = conn.prepare(&sql)?;
    // `seq > ?` compares with an integer; bind it as one.
    let mut values: Vec<rusqlite::types::Value> = Vec::with_capacity(args.len());
    values.push(rusqlite::types::Value::Integer(since));
    values.extend(args.into_iter().skip(1).map(rusqlite::types::Value::Text));
    let rows = stmt.query_map(params_from_iter(values), from_row)?;
    Ok(rows.collect::<Result<_, _>>()?)
}

/// Turn a `?since=` value into a `seq`: a number is a `seq`; anything else
/// is an event ID, and the result is that event's `seq`.
pub fn resolve_since(conn: &Connection, since: Option<&str>) -> ApiResult<i64> {
    let Some(since) = since.filter(|s| !s.is_empty()) else {
        return Ok(0);
    };
    if let Ok(n) = since.parse::<i64>() {
        return Ok(n);
    }
    conn.query_row("SELECT seq FROM events WHERE id = ?1", [since], |r| {
        r.get(0)
    })
    .optional()?
    .ok_or_else(|| crate::error::ApiError::bad_request(format!("no event `{since}` for since")))
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
