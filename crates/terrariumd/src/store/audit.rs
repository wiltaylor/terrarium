//! The audit log of privileged actions (PLAN §10). `Tx::audit` writes it.

use rusqlite::{Connection, Row};
use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::error::ApiResult;
use crate::store::session_refs::{self, LinkKey, SessionField};
use crate::store::{Page, Paged};

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct AuditEntry {
    pub id: String,
    /// The position in the log; the list cursor.
    pub seq: i64,
    pub at: String,
    /// `operator`, `agent:<ulid>`, `link:<ulid>` (a link key, TER-61), or
    /// `system`.
    pub actor: String,
    /// For example `agent.start` or `card.reassign`.
    pub action: String,
    pub target: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub project: Option<String>,
    pub detail: Value,
    /// `session_id` and `session_label`: the session of an agent actor at
    /// `at`, when the reader may read it.
    #[serde(flatten)]
    pub link: SessionField,
    /// The label of the link of a `link:<ulid>` actor.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub link_label: Option<String>,
}

fn row(r: &Row<'_>) -> rusqlite::Result<AuditEntry> {
    let detail: String = r.get("detail")?;
    Ok(AuditEntry {
        id: r.get("id")?,
        seq: r.get("seq")?,
        at: r.get("at")?,
        actor: r.get("actor")?,
        action: r.get("action")?,
        target: r.get("target")?,
        project: r.get("project")?,
        detail: serde_json::from_str(&detail).unwrap_or(Value::String(detail)),
        link: SessionField::default(),
        link_label: None,
    })
}

#[derive(Debug, Clone, Default, Deserialize)]
pub struct AuditFilter {
    /// Only rows at or after this time. The API normalises it with
    /// `time_param::parse_time_param` to the stored form first.
    pub since: Option<String>,
    pub actor: Option<String>,
    pub action: Option<String>,
    pub project: Option<String>,
    /// `asc` (the default) or `desc`. The API checks the value.
    pub order: Option<String>,
}

impl AuditFilter {
    /// True when the row passes the filter (`since` aside). The live
    /// `audit` frames of the event streams use it.
    pub fn matches(&self, e: &AuditEntry) -> bool {
        self.actor.as_ref().is_none_or(|a| *a == e.actor)
            && self.action.as_ref().is_none_or(|a| *a == e.action)
            && self
                .project
                .as_ref()
                .is_none_or(|p| e.project.as_ref() == Some(p))
    }
}

/// Rows oldest first. The cursor is a `seq`. With `order=desc`, newest
/// first, and the cursor goes on to older rows.
pub fn list(conn: &Connection, f: &AuditFilter, page: &Page) -> ApiResult<Paged<AuditEntry>> {
    let limit = page.limit();
    let desc = f.order.as_deref() == Some("desc");
    let cursor: Option<i64> = page.cursor().and_then(|c| c.parse().ok());
    let sql = if desc {
        "SELECT * FROM audit WHERE seq < ?1
           AND (?2 IS NULL OR at >= ?2)
           AND (?3 IS NULL OR actor = ?3)
           AND (?4 IS NULL OR action = ?4)
           AND (?5 IS NULL OR project = ?5)
         ORDER BY seq DESC LIMIT ?6"
    } else {
        "SELECT * FROM audit WHERE seq > ?1
           AND (?2 IS NULL OR at >= ?2)
           AND (?3 IS NULL OR actor = ?3)
           AND (?4 IS NULL OR action = ?4)
           AND (?5 IS NULL OR project = ?5)
         ORDER BY seq LIMIT ?6"
    };
    let bound = cursor.unwrap_or(if desc { i64::MAX } else { 0 });
    let mut stmt = conn.prepare(sql)?;
    let rows = stmt
        .query_map(
            rusqlite::params![
                bound,
                f.since,
                f.actor,
                f.action,
                f.project,
                (limit + 1) as i64
            ],
            row,
        )?
        .collect::<Result<Vec<_>, _>>()?;
    Ok(Paged::from_rows(rows, limit, |e| e.seq.to_string()))
}

/// Find the session of each row of a page whose actor is an agent: the
/// agent's session at the row's time (`store::session_refs::links`).
pub fn attach_links(conn: &Connection, rows: &mut [AuditEntry]) -> ApiResult<()> {
    let keys: Vec<LinkKey<'_>> = rows
        .iter()
        .map(|e| LinkKey {
            session: None,
            agent: e
                .actor
                .strip_prefix("agent:")
                .and_then(session_refs::agent_of),
            at: &e.at,
        })
        .collect();
    let links = session_refs::links(conn, &keys)?;
    for (e, l) in rows.iter_mut().zip(links) {
        e.link = SessionField::of(l);
    }
    // The label of each link actor (`store::links`).
    let ids: Vec<&str> = rows
        .iter()
        .filter_map(|e| e.actor.strip_prefix("link:"))
        .collect();
    if !ids.is_empty() {
        let mut labels = crate::store::links::labels(conn, &ids)?.into_iter();
        for e in rows.iter_mut().filter(|e| e.actor.starts_with("link:")) {
            e.link_label = labels.next().flatten();
        }
    }
    Ok(())
}

/// One row by its ID.
pub fn get(conn: &Connection, id: &str) -> ApiResult<Option<AuditEntry>> {
    use rusqlite::OptionalExtension;
    Ok(conn
        .query_row("SELECT * FROM audit WHERE id = ?1", [id], row)
        .optional()?)
}
