//! Pinned conversations (docs/api.md §6.11, PLAN §19): the agents whose
//! conversation the dashboard keeps in its right-hand side bar. One list
//! for each owner (the operator: `operator`), in pin order. The foreign
//! key removes the pin of a deleted agent.

use rusqlite::{Connection, OptionalExtension, params};
use serde::Serialize;
use serde_json::json;

use crate::db::{Tx, now};
use crate::error::ApiResult;

/// One pin, with the agent's project and name for display.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct Pin {
    pub agent: String,
    pub project: String,
    pub name: String,
    pub pinned_at: String,
}

/// The pins of `owner`, oldest pin first.
pub fn list(conn: &Connection, owner: &str) -> ApiResult<Vec<Pin>> {
    let mut stmt = conn.prepare(
        "SELECT p.agent, a.project, a.name, p.pinned_at
           FROM pinned_agents p JOIN agents a ON a.id = p.agent
          WHERE p.owner = ?1
          ORDER BY p.position, p.pinned_at",
    )?;
    let rows = stmt
        .query_map([owner], |r| {
            Ok(Pin {
                agent: r.get(0)?,
                project: r.get(1)?,
                name: r.get(2)?,
                pinned_at: r.get(3)?,
            })
        })?
        .collect::<Result<Vec<_>, _>>()?;
    Ok(rows)
}

/// Pin `agent` (a ULID that exists) at the end of the list. A pin that
/// exists stays where it is.
pub fn pin(t: &mut Tx<'_>, owner: &str, agent: &str, project: &str) -> ApiResult<()> {
    let have: Option<i64> = t
        .query_row(
            "SELECT 1 FROM pinned_agents WHERE owner = ?1 AND agent = ?2",
            params![owner, agent],
            |r| r.get(0),
        )
        .optional()?;
    if have.is_some() {
        return Ok(());
    }
    t.execute(
        "INSERT INTO pinned_agents (owner, agent, position, pinned_at)
         VALUES (?1, ?2,
                 (SELECT COALESCE(MAX(position), 0) + 1 FROM pinned_agents WHERE owner = ?1),
                 ?3)",
        params![owner, agent, now()],
    )?;
    t.audit("agent.pin", agent, Some(project), json!({"owner": owner}))?;
    Ok(())
}

/// Unpin `agent`. No pin is not an error.
pub fn unpin(t: &mut Tx<'_>, owner: &str, agent: &str, project: Option<&str>) -> ApiResult<()> {
    let n = t.execute(
        "DELETE FROM pinned_agents WHERE owner = ?1 AND agent = ?2",
        params![owner, agent],
    )?;
    if n > 0 {
        t.audit("agent.unpin", agent, project, json!({"owner": owner}))?;
    }
    Ok(())
}
