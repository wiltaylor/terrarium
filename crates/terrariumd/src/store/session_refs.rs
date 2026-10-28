//! The sessions behind board work (PLAN §13, §18). A card comment of an
//! agent links to the agent's current session, and a card event of an
//! agent carries it as `data.actor_session`. A session keeps its row
//! after `agent rm` (migration 37): the row then names the harness, and
//! `archive` holds the copy of its session file (`crate::session_archive`).

use rusqlite::{Connection, OptionalExtension, Row, params};
use serde::{Deserialize, Serialize};

use crate::db::{Tx, now};
use crate::error::ApiResult;

/// A session, with enough of its agent to render a link.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct SessionRef {
    pub id: String,
    pub agent_id: String,
    /// The agent name, without the project.
    pub agent: String,
    pub project: String,
    pub harness: String,
    pub started_at: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub ended_at: Option<String>,
    /// The agent is deleted. The transcript comes from the archive.
    pub removed: bool,
}

/// The internal row: a [`SessionRef`] and its archive path.
#[derive(Debug, Clone)]
pub struct SessionRow {
    pub session: SessionRef,
    pub harness_session_id: Option<String>,
    pub current: bool,
    /// The archived session file, relative to the state dir.
    pub archive: Option<String>,
}

const SELECT: &str = "SELECT s.id, s.agent_id, s.harness_session_id, s.current, s.started_at,
            s.ended_at, s.archive,
            coalesce(a.name, r.name, s.agent_id) AS agent_name,
            coalesce(a.project, r.project, '') AS project,
            coalesce(a.harness, s.harness, '') AS harness,
            a.id IS NULL AS removed
     FROM sessions s
     LEFT JOIN agents a ON a.id = s.agent_id
     LEFT JOIN agents_removed r ON r.id = s.agent_id";

fn row(r: &Row<'_>) -> rusqlite::Result<SessionRow> {
    Ok(SessionRow {
        session: SessionRef {
            id: r.get("id")?,
            agent_id: r.get("agent_id")?,
            agent: r.get("agent_name")?,
            project: r.get("project")?,
            harness: r.get("harness")?,
            started_at: r.get("started_at")?,
            ended_at: r.get("ended_at")?,
            removed: r.get::<_, i64>("removed")? != 0,
        },
        harness_session_id: r.get("harness_session_id")?,
        current: r.get::<_, i64>("current")? != 0,
        archive: r.get("archive")?,
    })
}

/// One session by its ID, of a live or a deleted agent.
pub fn get(conn: &Connection, id: &str) -> ApiResult<Option<SessionRow>> {
    Ok(conn
        .query_row(&format!("{SELECT} WHERE s.id = ?1"), [id], row)
        .optional()?)
}

/// The current session of `author` when it is an agent; `None` for the
/// operator, a project, or an agent with no session.
pub fn current_of(conn: &Connection, author: &str) -> ApiResult<Option<String>> {
    let id = author.strip_prefix("agent:").unwrap_or(author);
    if id == "operator" || id == "system" || id.starts_with("project:") {
        return Ok(None);
    }
    Ok(conn
        .query_row(
            "SELECT id FROM sessions WHERE agent_id = ?1 AND current = 1",
            [id],
            |r| r.get(0),
        )
        .optional()?)
}

/// The distinct sessions that touched `card`: the sessions of its
/// comments and of its card events (`data.actor_session`), oldest first.
pub fn of_card(conn: &Connection, card: &str) -> ApiResult<Vec<SessionRef>> {
    let mut stmt = conn.prepare(&format!(
        "{SELECT} WHERE s.id IN (
             SELECT session_id FROM card_comments WHERE card = ?1 AND session_id IS NOT NULL
             UNION
             SELECT json_extract(payload, '$.actor_session') FROM events
             WHERE card = ?1 AND kind = 'card'
               AND json_extract(payload, '$.actor_session') IS NOT NULL)
         ORDER BY s.started_at, s.id"
    ))?;
    let rows = stmt
        .query_map([card], row)?
        .map(|r| r.map(|x| x.session))
        .collect::<Result<Vec<_>, _>>()?;
    Ok(rows)
}

/// The end of an agent delete (`store::agents::delete`): its sessions
/// stay, end, and keep the harness of the agent for the transcript
/// parser.
pub fn on_agent_deleted(t: &mut Tx<'_>, agent: &str, harness: &str) -> ApiResult<()> {
    t.execute(
        "UPDATE sessions SET harness = ?2,
                current = 0,
                ended_at = coalesce(ended_at, ?3),
                end_reason = coalesce(end_reason, 'removed')
         WHERE agent_id = ?1",
        params![agent, harness, now()],
    )?;
    Ok(())
}

/// The sessions of a deleted agent whose file is not archived yet.
pub fn unarchived(conn: &Connection, agent: &str) -> ApiResult<Vec<SessionRow>> {
    let mut stmt = conn.prepare(&format!(
        "{SELECT} WHERE s.agent_id = ?1 AND s.archive IS NULL ORDER BY s.id"
    ))?;
    let rows = stmt
        .query_map([agent], row)?
        .collect::<Result<Vec<_>, _>>()?;
    Ok(rows)
}

/// Record the archived file of a session.
pub fn set_archive(t: &mut Tx<'_>, session: &str, rel: &str) -> ApiResult<()> {
    t.execute(
        "UPDATE sessions SET archive = ?2 WHERE id = ?1",
        params![session, rel],
    )?;
    Ok(())
}
