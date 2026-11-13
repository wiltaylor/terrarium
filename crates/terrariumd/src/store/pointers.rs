//! The push pointers that wait for the next start of an agent that does
//! not run (`pending_pointers`, migration 15). The supervisor keeps them in
//! memory too; this table lets them live through a daemon restart.

use rusqlite::{Connection, params};

use crate::db::Tx;
use crate::error::ApiResult;

/// One waiting pointer item.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PendingPointer {
    /// Higher for a newer item.
    pub id: i64,
    pub agent: String,
    /// `card`, `lead`, `lease`, or `reservation`.
    pub kind: String,
    /// One item for each agent, kind, and key.
    pub key: String,
    pub line: String,
    /// The item as JSON.
    pub data: String,
}

/// Write `p`. A row for the same agent, kind, and key is replaced only
/// when `p.id` is higher, so an older write that comes late changes
/// nothing.
pub fn put(t: &mut Tx, p: &PendingPointer) -> ApiResult<()> {
    t.execute(
        "INSERT INTO pending_pointers (id, agent, kind, key, line, data, created_at)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)
         ON CONFLICT (agent, kind, key) DO UPDATE SET
             id = excluded.id, line = excluded.line, data = excluded.data,
             created_at = excluded.created_at
         WHERE excluded.id > pending_pointers.id",
        params![
            p.id,
            p.agent,
            p.kind,
            p.key,
            p.line,
            p.data,
            crate::db::now()
        ],
    )?;
    Ok(())
}

/// Delete the rows of `agent` for `items` (kind, key) that are not newer
/// than `upto`: a newer item for the same key stays.
pub fn delete(t: &mut Tx, agent: &str, items: &[(String, String)], upto: i64) -> ApiResult<()> {
    for (kind, key) in items {
        t.execute(
            "DELETE FROM pending_pointers
             WHERE agent = ?1 AND kind = ?2 AND key = ?3 AND id <= ?4",
            params![agent, kind, key, upto],
        )?;
    }
    Ok(())
}

/// Every row, oldest first. The rows of agents that are gone are deleted
/// first.
pub fn load(c: &Connection) -> ApiResult<Vec<PendingPointer>> {
    c.execute(
        "DELETE FROM pending_pointers WHERE agent NOT IN (SELECT id FROM agents)",
        [],
    )?;
    let mut stmt =
        c.prepare("SELECT id, agent, kind, key, line, data FROM pending_pointers ORDER BY id")?;
    let rows = stmt.query_map([], row)?;
    Ok(rows.collect::<Result<_, _>>()?)
}

/// The rows of `agent`, oldest first (TER-1103: the reload of a running
/// agent's pointer).
pub fn for_agent(c: &Connection, agent: &str) -> ApiResult<Vec<PendingPointer>> {
    let mut stmt = c.prepare(
        "SELECT id, agent, kind, key, line, data FROM pending_pointers
         WHERE agent = ?1 ORDER BY id",
    )?;
    let rows = stmt.query_map([agent], row)?;
    Ok(rows.collect::<Result<_, _>>()?)
}

fn row(r: &rusqlite::Row<'_>) -> rusqlite::Result<PendingPointer> {
    Ok(PendingPointer {
        id: r.get(0)?,
        agent: r.get(1)?,
        kind: r.get(2)?,
        key: r.get(3)?,
        line: r.get(4)?,
        data: r.get(5)?,
    })
}
