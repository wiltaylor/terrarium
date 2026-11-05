//! Ephemeral agents (PLAN §18, "Ephemeral agents"): one `spawns` row for
//! each spawn (migration 51).
//!
//! An ephemeral agent is an agent row with an `owner`: the ULID of the
//! agent that spawned it, or `operator`. The row lives while the agent
//! runs. The finish (`crate::spawn`) deletes the agent row as `agent rm`
//! does (the name stays in `agents_removed`, the session files go to the
//! archive), and the `spawns` row keeps the history: the task, the
//! outcome, the result, and the last session for the session link.

use rusqlite::{Connection, OptionalExtension, Row, params};
use serde::Serialize;

use crate::db::{Tx, now};
use crate::error::{ApiError, ApiResult};
use crate::store::{Page, Paged, valid_name};

/// The owner of an ephemeral agent that the operator spawned.
pub const OPERATOR: &str = "operator";
/// The longest result that the row keeps.
pub const MAX_RESULT: usize = 16 * 1024;
/// The longest task.
pub const MAX_TASK: usize = 64 * 1024;

/// The outcomes of a finish.
pub const DONE: &str = "done";
pub const FAILED: &str = "failed";
pub const TIMEOUT: &str = "timeout";
pub const OWNER_GONE: &str = "owner_gone";
pub const CANCELLED: &str = "cancelled";

/// One spawn, as the API shows it (`GET /api/agents/{id}/spawned`).
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct Spawn {
    /// The ULID of the ephemeral agent.
    pub id: String,
    /// The owner: an agent ULID, or `operator`.
    pub owner: String,
    pub owner_name: String,
    pub project: String,
    pub name: String,
    /// The template, as the spawn named it.
    pub template: String,
    pub harness: String,
    /// The first turn.
    pub task: String,
    /// The first line of the task, at most 120 characters.
    pub summary: String,
    pub spawned_by: String,
    pub spawned_at: String,
    /// The end of the lifetime.
    pub deadline_at: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub wait_id: Option<String>,
    /// The live status of the agent, or `finished`.
    pub status: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub finished_at: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub outcome: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub result: Option<String>,
    /// The session of the agent: the current one while it runs, the last
    /// one after the finish. The dashboard links `/sessions/<id>`.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub session_id: Option<String>,
    /// From the spawn to the finish, or to now.
    pub duration_secs: i64,
}

impl Spawn {
    pub fn running(&self) -> bool {
        self.finished_at.is_none()
    }
}

/// The spawn facts that an ephemeral agent shows (`AgentView::spawn`).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, serde::Deserialize)]
pub struct SpawnInfo {
    pub owner_name: String,
    pub template: String,
    pub task: String,
    pub spawned_at: String,
    pub deadline_at: String,
}

/// A new spawn row.
#[derive(Debug, Clone)]
pub struct NewSpawn<'a> {
    pub id: &'a str,
    pub owner: &'a str,
    pub owner_name: &'a str,
    pub project: &'a str,
    pub name: &'a str,
    pub template: &'a str,
    pub harness: &'a str,
    pub task: &'a str,
    pub deadline_at: &'a str,
}

/// The first line of `text`, at most `n` characters.
pub fn first_line(text: &str, n: usize) -> String {
    let line = text.lines().find(|l| !l.trim().is_empty()).unwrap_or("");
    let line = line.trim();
    if line.chars().count() > n {
        line.chars().take(n.saturating_sub(1)).collect::<String>() + "…"
    } else {
        line.to_owned()
    }
}

/// `text` cut to [`MAX_RESULT`] bytes at a character boundary.
pub fn clip_result(text: &str) -> String {
    let text = text.trim();
    if text.len() <= MAX_RESULT {
        return text.to_owned();
    }
    let mut end = MAX_RESULT - "\n[cut]".len();
    while !text.is_char_boundary(end) {
        end -= 1;
    }
    format!("{}\n[cut]", &text[..end])
}

fn secs_between(from: &str, to: Option<&str>) -> i64 {
    let parse = |s: &str| chrono::DateTime::parse_from_rfc3339(s).ok();
    let Some(a) = parse(from) else { return 0 };
    let b = to
        .and_then(parse)
        .map(|t| t.with_timezone(&chrono::Utc))
        .unwrap_or_else(chrono::Utc::now);
    (b - a.with_timezone(&chrono::Utc)).num_seconds().max(0)
}

fn row(r: &Row<'_>) -> rusqlite::Result<Spawn> {
    let task: String = r.get("task")?;
    let spawned_at: String = r.get("spawned_at")?;
    let finished_at: Option<String> = r.get("finished_at")?;
    let live: Option<String> = r.get("live_status")?;
    let live_session: Option<String> = r.get("live_session")?;
    let stored_session: Option<String> = r.get("session_id")?;
    Ok(Spawn {
        id: r.get("id")?,
        owner: r.get("owner")?,
        owner_name: r.get("owner_name")?,
        project: r.get("project")?,
        name: r.get("name")?,
        template: r.get("template")?,
        harness: r.get("harness")?,
        summary: first_line(&task, 120),
        task,
        spawned_by: r.get("spawned_by")?,
        duration_secs: secs_between(&spawned_at, finished_at.as_deref()),
        spawned_at,
        deadline_at: r.get("deadline_at")?,
        wait_id: r.get("wait_id")?,
        status: match (&finished_at, live) {
            (Some(_), _) => "finished".into(),
            (None, Some(s)) => s,
            (None, None) => "stopped".into(),
        },
        finished_at,
        outcome: r.get("outcome")?,
        result: r.get("result")?,
        session_id: stored_session.or(live_session),
    })
}

/// The select of [`row`]: the spawn, and the live status and session of
/// its agent while the agent row exists.
const SELECT: &str = "SELECT s.*, a.status AS live_status,
        (SELECT x.id FROM sessions x WHERE x.agent_id = s.id
         ORDER BY x.current DESC, x.id DESC LIMIT 1) AS live_session
     FROM spawns s LEFT JOIN agents a ON a.id = s.id";

/// Write a new spawn row.
pub fn insert(t: &mut Tx<'_>, n: &NewSpawn<'_>) -> ApiResult<()> {
    t.execute(
        "INSERT INTO spawns (id, owner, owner_name, project, name, template, harness, task,
                             spawned_by, spawned_at, deadline_at)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11)",
        params![
            n.id,
            n.owner,
            n.owner_name,
            n.project,
            n.name,
            n.template,
            n.harness,
            n.task,
            t.actor,
            now(),
            n.deadline_at
        ],
    )?;
    t.execute(
        "UPDATE agents SET owner = ?2 WHERE id = ?1",
        params![n.id, n.owner],
    )?;
    Ok(())
}

/// Note the wait of the owner on the finish.
pub fn set_wait(t: &mut Tx<'_>, id: &str, wait: &str) -> ApiResult<()> {
    t.execute(
        "UPDATE spawns SET wait_id = ?2 WHERE id = ?1",
        params![id, wait],
    )?;
    Ok(())
}

pub fn find(conn: &Connection, id: &str) -> ApiResult<Option<Spawn>> {
    Ok(conn
        .query_row(&format!("{SELECT} WHERE s.id = ?1"), [id], row)
        .optional()?)
}

pub fn get(conn: &Connection, id: &str) -> ApiResult<Spawn> {
    find(conn, id)?.ok_or_else(|| ApiError::not_found(format!("no spawn `{id}`")))
}

/// The spawn facts of the ephemeral agent `id`, while it runs.
pub fn info(conn: &Connection, id: &str) -> ApiResult<Option<SpawnInfo>> {
    Ok(conn
        .query_row(
            "SELECT owner_name, template, task, spawned_at, deadline_at FROM spawns
             WHERE id = ?1",
            [id],
            |r| {
                Ok(SpawnInfo {
                    owner_name: r.get(0)?,
                    template: r.get(1)?,
                    task: r.get(2)?,
                    spawned_at: r.get(3)?,
                    deadline_at: r.get(4)?,
                })
            },
        )
        .optional()?)
}

/// The ephemeral agents of `owner` that have not finished.
pub fn running_count(conn: &Connection, owner: &str) -> ApiResult<usize> {
    let n: i64 = conn.query_row(
        "SELECT count(*) FROM spawns WHERE owner = ?1 AND finished_at IS NULL",
        [owner],
        |r| r.get(0),
    )?;
    Ok(usize::try_from(n).unwrap_or(0))
}

/// The spawns of `owner` since `since` (RFC 3339).
pub fn count_since(conn: &Connection, owner: &str, since: &str) -> ApiResult<usize> {
    let n: i64 = conn.query_row(
        "SELECT count(*) FROM spawns WHERE owner = ?1 AND spawned_at >= ?2",
        params![owner, since],
        |r| r.get(0),
    )?;
    Ok(usize::try_from(n).unwrap_or(0))
}

/// The spawns of `owner` (an agent ULID or `operator`), newest first. For
/// `operator`, `project` limits the list to one project.
pub fn list_of(
    conn: &Connection,
    owner: &str,
    project: Option<&str>,
    page: &Page,
) -> ApiResult<Paged<Spawn>> {
    let limit = page.limit();
    let mut stmt = conn.prepare(&format!(
        "{SELECT} WHERE s.owner = ?1 AND (?2 = '' OR s.id < ?2)
           AND (?3 IS NULL OR s.project = ?3)
         ORDER BY s.id DESC LIMIT ?4"
    ))?;
    let rows = stmt
        .query_map(
            params![
                owner,
                page.cursor().unwrap_or(""),
                project,
                (limit + 1) as i64
            ],
            row,
        )?
        .collect::<Result<Vec<_>, _>>()?;
    Ok(Paged::from_rows(rows, limit, |s| s.id.clone()))
}

/// The ephemeral agents that have not finished, oldest first.
pub fn open(conn: &Connection) -> ApiResult<Vec<Spawn>> {
    let mut stmt = conn.prepare(&format!(
        "{SELECT} WHERE s.finished_at IS NULL ORDER BY s.id"
    ))?;
    let rows = stmt.query_map([], row)?.collect::<Result<Vec<_>, _>>()?;
    Ok(rows)
}

/// The IDs of every spawn of `owner`, open or finished.
pub fn ids_of(conn: &Connection, owner: &str) -> ApiResult<Vec<String>> {
    let mut stmt = conn.prepare("SELECT id FROM spawns WHERE owner = ?1 ORDER BY id")?;
    let rows = stmt
        .query_map([owner], |r| r.get(0))?
        .collect::<Result<Vec<_>, _>>()?;
    Ok(rows)
}

/// The open spawns of `owner`.
pub fn open_of(conn: &Connection, owner: &str) -> ApiResult<Vec<Spawn>> {
    let mut stmt = conn.prepare(&format!(
        "{SELECT} WHERE s.owner = ?1 AND s.finished_at IS NULL ORDER BY s.id"
    ))?;
    let rows = stmt
        .query_map([owner], row)?
        .collect::<Result<Vec<_>, _>>()?;
    Ok(rows)
}

/// Keep the result that `agent done` gives, before the finish.
pub fn set_result(t: &mut Tx<'_>, id: &str, result: &str) -> ApiResult<()> {
    let r = t.redact_string(clip_result(result));
    t.execute(
        "UPDATE spawns SET result = ?2 WHERE id = ?1 AND finished_at IS NULL",
        params![id, r],
    )?;
    Ok(())
}

/// Mark the spawn finished. `result`: `None` keeps the result that is
/// there (from `agent done`). Returns false when it was finished already.
pub fn mark_finished(
    t: &mut Tx<'_>,
    id: &str,
    outcome: &str,
    result: Option<&str>,
    session: Option<&str>,
) -> ApiResult<bool> {
    let result = result.map(|r| t.redact_string(clip_result(r)));
    let n = t.execute(
        "UPDATE spawns SET finished_at = ?2, outcome = ?3,
                result = coalesce(?4, result), session_id = coalesce(?5, session_id)
         WHERE id = ?1 AND finished_at IS NULL",
        params![id, now(), outcome, result, session],
    )?;
    Ok(n > 0)
}

/// The name of the next ephemeral agent of `owner_name` from
/// `template` in `project`: `<owner>-<template>-<n>`, with the lowest `n`
/// that no agent and no spawn of the project has. Parts that are not
/// valid in a name become `-`, and a long name is cut.
pub fn next_name(
    conn: &Connection,
    project: &str,
    owner_name: &str,
    template: &str,
) -> ApiResult<String> {
    let clean = |s: &str| -> String {
        let mut out: String = s
            .to_ascii_lowercase()
            .chars()
            .map(|c| {
                if c.is_ascii_alphanumeric() || c == '-' || c == '_' {
                    c
                } else {
                    '-'
                }
            })
            .collect();
        out = out.trim_matches(['-', '_']).to_owned();
        out
    };
    let mut base = format!("{}-{}", clean(owner_name), clean(template));
    if base.len() > 54 {
        base.truncate(54);
        base = base.trim_end_matches(['-', '_']).to_owned();
    }
    let taken = |name: &str| -> ApiResult<bool> {
        let n: i64 = conn.query_row(
            "SELECT (SELECT count(*) FROM agents WHERE project = ?1 AND name = ?2)
                  + (SELECT count(*) FROM spawns WHERE project = ?1 AND name = ?2)",
            params![project, name],
            |r| r.get(0),
        )?;
        Ok(n > 0)
    };
    for n in 1..100_000 {
        let name = format!("{base}-{n}");
        if valid_name(&name) && !taken(&name)? {
            return Ok(name);
        }
    }
    Err(ApiError::conflict(format!(
        "no free name for an ephemeral agent `{base}-<n>`; give one with `name`"
    )))
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::*;

    #[test]
    fn first_line_and_clip() {
        assert_eq!(first_line("\n  Find the bug \nmore", 120), "Find the bug");
        assert_eq!(first_line("abcdef", 4), "abc…");
        let long = "é".repeat(MAX_RESULT);
        let c = clip_result(&long);
        assert!(c.len() <= MAX_RESULT);
        assert!(c.ends_with("[cut]"));
        assert_eq!(clip_result("  ok  "), "ok");
    }

    #[test]
    fn durations() {
        assert_eq!(
            secs_between("2026-01-01T00:00:00.000Z", Some("2026-01-01T00:01:30.000Z")),
            90
        );
        assert_eq!(secs_between("bad", None), 0);
    }
}
