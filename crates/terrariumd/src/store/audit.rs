//! The audit log of privileged actions (PLAN §10). `Tx::audit` writes it.

use rusqlite::{Connection, Row};
use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::error::ApiResult;
use crate::store::{Page, Paged};

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct AuditEntry {
    pub id: String,
    /// The position in the log; the list cursor.
    pub seq: i64,
    pub at: String,
    /// `operator`, `agent:<ulid>`, or `system`.
    pub actor: String,
    /// For example `agent.start` or `card.reassign`.
    pub action: String,
    pub target: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub project: Option<String>,
    pub detail: Value,
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
    })
}

#[derive(Debug, Clone, Default, Deserialize)]
pub struct AuditFilter {
    /// Only rows at or after this RFC 3339 time.
    pub since: Option<String>,
    pub actor: Option<String>,
    pub action: Option<String>,
    pub project: Option<String>,
}

/// Rows oldest first. The cursor is a `seq`.
pub fn list(conn: &Connection, f: &AuditFilter, page: &Page) -> ApiResult<Paged<AuditEntry>> {
    let limit = page.limit();
    let after: i64 = page.cursor().and_then(|c| c.parse().ok()).unwrap_or(0);
    let mut stmt = conn.prepare(
        "SELECT * FROM audit WHERE seq > ?1
           AND (?2 IS NULL OR at >= ?2)
           AND (?3 IS NULL OR actor = ?3)
           AND (?4 IS NULL OR action = ?4)
           AND (?5 IS NULL OR project = ?5)
         ORDER BY seq LIMIT ?6",
    )?;
    let rows = stmt
        .query_map(
            rusqlite::params![
                after,
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
