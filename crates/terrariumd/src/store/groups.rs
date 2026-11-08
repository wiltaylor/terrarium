//! Rule-owned board groups. Closed memberships remain as history but do not
//! prevent a card from joining the next group in its lifecycle.
use chrono::{Duration, Utc};
use rusqlite::{Connection, OptionalExtension, params};
use serde::{Deserialize, Serialize};
use serde_json::json;

use crate::db::Tx;
use crate::error::{ApiError, ApiResult};
use crate::store::board;

fn event(t: &mut Tx<'_>, id: &str, change: &str) -> ApiResult<()> {
    let project = board::project_of(t, id)?;
    t.emit(crate::events::EventDraft::new("card", format!("{id} {change}"),
        json!({"change": change, "kind": "group"})).project(project).card(id))?;
    Ok(())
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Group {
    pub id: String,
    pub key: String,
    pub title: String,
    pub column: String,
    pub status: String,
    pub done: i64,
    pub total: i64,
    pub members: Vec<String>,
    pub eta_at: Option<String>,
    pub closed_at: Option<String>,
}

pub fn for_card(conn: &Connection, card: &str) -> ApiResult<Option<Group>> {
    let id: Option<String> = conn.query_row(
        "SELECT g.card FROM card_groups g WHERE g.card = ?1 OR
         (g.closed_at IS NULL AND EXISTS(SELECT 1 FROM group_members m
          WHERE m.group_card = g.card AND m.card = ?1)) LIMIT 1",
        [card], |r| r.get(0),
    ).optional()?;
    id.map(|id| get(conn, &id)).transpose()
}

pub fn get(conn: &Connection, id: &str) -> ApiResult<Group> {
    let (mut g, done_when, eta_jobs) = conn.query_row(
        "SELECT g.key, c.title, k.name, g.status, c.eta_at, g.closed_at,
                g.done_when, g.eta_jobs FROM card_groups g
         JOIN cards c ON c.id = g.card JOIN board_columns k ON k.id = c.column_id
         WHERE g.card = ?1", [id], |r| Ok((Group {
            id: id.into(), key: r.get(0)?, title: r.get(1)?, column: r.get(2)?,
            status: r.get(3)?, eta_at: r.get(4)?, closed_at: r.get(5)?,
            done: 0, total: 0, members: Vec::new(),
        }, r.get::<_, String>(6)?, r.get::<_, bool>(7)?)),
    )?;
    let anchor: Option<i64> = conn.query_row("SELECT k.position FROM board_columns k
        JOIN cards c ON c.project = k.project WHERE c.id = ?1 AND k.name = ?2",
        params![id, done_when], |r| r.get(0)).optional()?;
    let mut stmt = conn.prepare(
        "SELECT c.id, k.position, k.role FROM group_members m
         JOIN cards c ON c.id = m.card JOIN board_columns k ON k.id = c.column_id
         WHERE m.group_card = ?1 ORDER BY m.position, c.id")?;
    let rows = stmt.query_map([id], |r| Ok((r.get::<_, String>(0)?,
        r.get::<_, i64>(1)?, r.get::<_, Option<String>>(2)?)))?;
    if eta_jobs { g.eta_at = None; }
    for row in rows {
        let (card, position, role) = row?;
        g.total += 1;
        // TER-147: at or beyond the anchor in board order, except blocked.
        if anchor.is_some_and(|anchor| position >= anchor) && role.as_deref() != Some("blocked") { g.done += 1; }
        if eta_jobs && let Some(job) = crate::store::jobs::open_of_card(conn, &card)? {
            let job_eta = job.started_at.as_deref().and_then(|at| crate::store::jobs::started_eta(&job, at));
            if job_eta > g.eta_at { g.eta_at = job_eta; }
        }
        g.members.push(card);
    }
    Ok(g)
}

pub fn by_key(conn: &Connection, project: &str, key: &str) -> ApiResult<Option<Group>> {
    let id: Option<String> = conn.query_row(
        "SELECT card FROM card_groups WHERE project = ?1 AND key = ?2",
        params![project, key], |r| r.get(0),
    ).optional()?;
    id.map(|id| get(conn, &id)).transpose()
}

fn deadline(value: &str) -> ApiResult<String> {
    let secs = crate::store::jobs::planned_secs(value)?;
    let at = Utc::now().checked_add_signed(Duration::seconds(secs))
        .ok_or_else(|| ApiError::invalid("group duration is too large"))?;
    Ok(crate::time_param::stored(at))
}

pub struct Open<'a> {
    pub project: &'a str,
    pub rule: &'a str,
    pub key: &'a str,
    pub title: &'a str,
    pub column: &'a str,
    pub done_when: &'a str,
    pub eta: Option<&'a str>,
    pub close_after: Option<&'a str>,
}

pub fn open(t: &mut Tx<'_>, n: Open<'_>) -> ApiResult<bool> {
    board::check_title(n.title)?;
    if n.key.trim().is_empty() || n.title.trim().is_empty() {
        return Err(ApiError::invalid("group key and title must not be empty"));
    }
    if by_key(t, n.project, n.key)?.is_some() { return Ok(false); }
    let close_at = n.close_after.map(deadline).transpose()?;
    let eta_at = n.eta.filter(|v| *v != "jobs").map(deadline).transpose()?;
    let done_when = board::resolve_column(t, n.project, n.done_when)?;
    let column = board::resolve_column(t, n.project, n.column)?;
    let col: i64 = t.query_row("SELECT id FROM board_columns WHERE project = ?1 AND name = ?2",
        params![n.project, column], |r| r.get(0))?;
    t.execute("UPDATE projects SET card_seq = card_seq + 1 WHERE name = ?1", [n.project])?;
    let (prefix, seq): (String, i64) = t.query_row(
        "SELECT card_prefix, card_seq FROM projects WHERE name = ?1", [n.project],
        |r| Ok((r.get(0)?, r.get(1)?)))?;
    let id = format!("{prefix}-{seq}");
    let at = crate::time_param::stored(Utc::now());
    t.execute("INSERT INTO cards (id, project, number, column_id, position, title, body,
        labels, created_by, created_at, updated_at, kind, eta_at)
        VALUES (?1, ?2, ?3, ?4, 0, ?5, '', '[]', ?6, ?7, ?7, 'group', ?8)",
        params![id, n.project, seq, col, t.redact(n.title), t.actor, at, eta_at])?;
    t.execute("INSERT INTO card_groups (card, project, key, rule, done_when, eta_jobs, close_at)
        VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)",
        params![id, n.project, n.key, n.rule, done_when, n.eta == Some("jobs"), close_at])?;
    t.audit("group.open", &id, Some(n.project), json!({"key": n.key}))?;
    event(t, &id, "group_opened")?;
    Ok(true)
}

pub fn add(t: &mut Tx<'_>, g: &Group, card: &str) -> ApiResult<bool> {
    if g.closed_at.is_some() { return Ok(false); }
    let member = board::get_card(t, card)?;
    let project = board::project_of(t, &g.id)?;
    if member.project != project || member.kind != "card" || member.archived {
        return Err(ApiError::invalid("group members must be active ordinary cards of the same project"));
    }
    if board::is_held(t, card)? { return Err(ApiError::forbidden("a held card cannot join a group")); }
    if let Some(other) = &member.group {
        if other.id == g.id { return Ok(false); }
        return Err(ApiError::conflict(format!("{card} is already in open group {}", other.id)));
    }
    t.execute("INSERT INTO group_members (group_card, card, position)
        SELECT ?1, ?2, coalesce(max(position), -1) + 1 FROM group_members WHERE group_card = ?1",
        params![g.id, card])?;
    event(t, &g.id, "group_added")?;
    Ok(true)
}

pub fn remove(t: &mut Tx<'_>, g: &Group, card: &str) -> ApiResult<bool> {
    if g.closed_at.is_some() { return Ok(false); }
    let changed = t.execute("DELETE FROM group_members WHERE group_card = ?1 AND card = ?2",
        params![g.id, card])? > 0;
    if changed { event(t, &g.id, "group_removed")?; }
    Ok(changed)
}

pub fn status(t: &mut Tx<'_>, g: &Group, text: Option<&str>, eta: Option<&str>) -> ApiResult<bool> {
    if g.closed_at.is_some() { return Ok(false); }
    let mut changed = false;
    if let Some(text) = text {
        let text = t.redact(text);
        changed |= t.execute("UPDATE card_groups SET status = ?2 WHERE card = ?1 AND status <> ?2",
            params![g.id, text])? > 0;
    }
    if let Some(eta) = eta {
        let at = if eta == "jobs" { None } else { Some(deadline(eta)?) };
        changed |= t.execute("UPDATE card_groups SET eta_jobs = ?2 WHERE card = ?1 AND eta_jobs <> ?2",
            params![g.id, eta == "jobs"])? > 0;
        changed |= t.execute("UPDATE cards SET eta_at = ?2 WHERE id = ?1 AND eta_at IS NOT ?2",
            params![g.id, at])? > 0;
    }
    if changed { event(t, &g.id, "group_status")?; }
    Ok(changed)
}

pub fn close(t: &mut Tx<'_>, g: &Group, comment: &str) -> ApiResult<bool> {
    if g.closed_at.is_some() { return Ok(false); }
    let at = crate::time_param::stored(Utc::now());
    t.execute("UPDATE card_groups SET closed_at = ?2 WHERE card = ?1", params![g.id, at])?;
    t.execute("UPDATE cards SET archived = 1, updated_at = ?2 WHERE id = ?1", params![g.id, at])?;
    board::system_comment(t, &g.id, comment, None)?;
    Ok(true)
}

/// The safeguard is independent of whether the creating rule still exists.
/// Its comment and rule-log row commit together with the close.
pub fn expire(t: &mut Tx<'_>) -> ApiResult<()> {
    let at = crate::time_param::stored(Utc::now());
    let due: Vec<(String, String, String)> = {
        let mut stmt = t.prepare("SELECT card, project, rule FROM card_groups
            WHERE closed_at IS NULL AND close_at <= ?1")?;
        stmt.query_map([&at], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)))?
            .collect::<Result<_, _>>()?
    };
    for (id, project, rule) in due {
        let g = get(t, &id)?;
        close(t, &g, "Group closed by the close_after safeguard.")?;
        let run = ulid::Ulid::new().to_string();
        let steps = json!([{"verb":"group.close", "ok":true, "changed":true,
            "detail":{"group":id, "reason":"close_after"}}]);
        t.execute("INSERT INTO rule_runs (id, project, rule, rule_hash, ord, event_seq,
            card, lane, rule_json, cause, state, step, steps, reason, created_at, updated_at)
            VALUES (?1, ?2, ?3, 'close_after', 0, 0, ?4, ?4, '{}', '{}', 'done', 1,
            ?5, 'close_after safeguard', ?6, ?6)",
            params![run, project, rule, id, steps.to_string(), at])?;
    }
    Ok(())
}
