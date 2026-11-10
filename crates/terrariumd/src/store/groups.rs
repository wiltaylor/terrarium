//! Rule-owned board groups. Closed memberships remain as history but do not
//! prevent a card from joining the next group in its lifecycle.
use std::collections::HashMap;

use chrono::{DateTime, Duration, Utc};
use rusqlite::{Connection, OptionalExtension, params};
use serde::{Deserialize, Serialize};
use serde_json::json;

use crate::db::Tx;
use crate::error::{ApiError, ApiResult};
use crate::store::board;
use tracing::{debug, warn};

fn event(t: &mut Tx<'_>, id: &str, change: &str) -> ApiResult<()> {
    let project = board::project_of(t, id)?;
    t.emit(
        crate::events::EventDraft::new(
            "card",
            format!("{id} {change}"),
            json!({"change": change, "kind": "group"}),
        )
        .project(project)
        .card(id),
    )?;
    Ok(())
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct GroupStep {
    pub name: String,
    pub state: String,
    pub text: String,
    pub eta_at: Option<String>,
    pub started_at: Option<String>,
    pub finished_at: Option<String>,
    pub duration_secs: Option<i64>,
    pub estimated_secs: Option<i64>,
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
    pub steps: Vec<GroupStep>,
}

pub fn for_card(conn: &Connection, card: &str) -> ApiResult<Option<Group>> {
    let id: Option<String> = conn
        .query_row(
            "SELECT g.card FROM card_groups g WHERE g.card = ?1 OR
         (g.closed_at IS NULL AND EXISTS(SELECT 1 FROM group_members m
          WHERE m.group_card = g.card AND m.card = ?1)) LIMIT 1",
            [card],
            |r| r.get(0),
        )
        .optional()?;
    id.map(|id| get(conn, &id)).transpose()
}

pub fn get(conn: &Connection, id: &str) -> ApiResult<Group> {
    let (mut g, done_when, eta_jobs) = conn.query_row(
        "SELECT g.key, c.title, k.name, g.status, c.eta_at, g.closed_at,
                g.done_when, g.eta_jobs FROM card_groups g
         JOIN cards c ON c.id = g.card JOIN board_columns k ON k.id = c.column_id
         WHERE g.card = ?1",
        [id],
        |r| {
            Ok((
                Group {
                    id: id.into(),
                    key: r.get(0)?,
                    title: r.get(1)?,
                    column: r.get(2)?,
                    status: r.get(3)?,
                    eta_at: r.get(4)?,
                    closed_at: r.get(5)?,
                    done: 0,
                    total: 0,
                    members: Vec::new(),
                    steps: Vec::new(),
                },
                r.get::<_, String>(6)?,
                r.get::<_, bool>(7)?,
            ))
        },
    )?;
    let anchor: Option<i64> = conn
        .query_row(
            "SELECT k.position FROM board_columns k
        JOIN cards c ON c.project = k.project WHERE c.id = ?1 AND k.name = ?2",
            params![id, done_when],
            |r| r.get(0),
        )
        .optional()?;
    let mut stmt = conn.prepare(
        "SELECT c.id, k.position, k.role FROM group_members m
         JOIN cards c ON c.id = m.card JOIN board_columns k ON k.id = c.column_id
         WHERE m.group_card = ?1 ORDER BY m.position, c.id",
    )?;
    let rows = stmt.query_map([id], |r| {
        Ok((
            r.get::<_, String>(0)?,
            r.get::<_, i64>(1)?,
            r.get::<_, Option<String>>(2)?,
        ))
    })?;
    if eta_jobs {
        g.eta_at = None;
    }
    for row in rows {
        let (card, position, role) = row?;
        g.total += 1;
        // TER-147: at or beyond the anchor in board order, except blocked.
        if anchor.is_some_and(|anchor| position >= anchor) && role.as_deref() != Some("blocked") {
            g.done += 1;
        }
        if eta_jobs && let Some(job) = crate::store::jobs::open_of_card(conn, &card)? {
            let job_eta = job
                .started_at
                .as_deref()
                .and_then(|at| crate::store::jobs::started_eta(&job, at));
            if job_eta > g.eta_at {
                g.eta_at = job_eta;
            }
        }
        g.members.push(card);
    }
    let mut steps = conn.prepare(
        "SELECT name, state, text, eta_at, started_at, finished_at FROM group_steps
        WHERE group_card = ?1 ORDER BY position, name",
    )?;
    g.steps = steps
        .query_map([id], |r| {
            Ok(GroupStep {
                name: r.get(0)?,
                state: r.get(1)?,
                text: r.get(2)?,
                eta_at: r.get(3)?,
                started_at: r.get(4)?,
                finished_at: r.get(5)?,
                duration_secs: elapsed(r.get(4)?, r.get(5)?),
                estimated_secs: None,
            })
        })?
        .collect::<Result<_, _>>()?;
    let created: String =
        conn.query_row("SELECT created_at FROM cards WHERE id = ?1", [id], |r| {
            r.get(0)
        })?;
    let mut cursor = chrono::DateTime::parse_from_rfc3339(&created)
        .map(|v| v.with_timezone(&Utc))
        .unwrap_or_else(|_| Utc::now());
    let estimates = historical_secs(conn, id)?;
    for step in &mut g.steps {
        step.estimated_secs = estimates.get(&step.name).copied();
        if step.state == "done" || step.state == "failed" {
            step.eta_at = None;
            if let Some(at) = step
                .finished_at
                .as_deref()
                .and_then(|v| chrono::DateTime::parse_from_rfc3339(v).ok())
            {
                cursor = cursor.max(at.with_timezone(&Utc));
            }
            continue;
        }
        if step.eta_at.is_none() {
            let start = step
                .started_at
                .as_deref()
                .and_then(|v| chrono::DateTime::parse_from_rfc3339(v).ok())
                .map(|v| v.with_timezone(&Utc))
                .unwrap_or(cursor);
            step.eta_at = step
                .estimated_secs
                .and_then(|secs| start.checked_add_signed(Duration::seconds(secs)))
                .map(crate::time_param::stored);
        }
        if let Some(at) = step
            .eta_at
            .as_deref()
            .and_then(|v| chrono::DateTime::parse_from_rfc3339(v).ok())
        {
            cursor = cursor.max(at.with_timezone(&Utc));
        }
    }
    Ok(g)
}

fn elapsed(start: Option<String>, end: Option<String>) -> Option<i64> {
    let start = chrono::DateTime::parse_from_rfc3339(start.as_deref()?).ok()?;
    let end = chrono::DateTime::parse_from_rfc3339(end.as_deref()?).ok()?;
    Some((end - start).num_seconds().max(0))
}

/// Successful samples only, scoped to this project and group lifecycle rule.
fn historical_secs(
    conn: &Connection,
    id: &str,
) -> ApiResult<std::collections::BTreeMap<String, i64>> {
    let mut stmt = conn.prepare(
        "SELECT s.name, s.started_at, s.finished_at FROM group_steps s
        JOIN card_groups g ON g.card = s.group_card
        JOIN card_groups current ON current.card = ?1
        WHERE g.project = current.project AND g.rule = current.rule
        AND s.group_card <> ?1 AND s.state = 'done'
        AND s.started_at IS NOT NULL AND s.finished_at IS NOT NULL",
    )?;
    let mut samples = std::collections::BTreeMap::<String, Vec<i64>>::new();
    for row in stmt.query_map([id], |r| {
        Ok((r.get::<_, String>(0)?, elapsed(r.get(1)?, r.get(2)?)))
    })? {
        let (name, secs) = row?;
        if let Some(secs) = secs {
            samples.entry(name).or_default().push(secs);
        }
    }
    // Before phase timing existed, successful merge jobs still recorded queue
    // wait and total run time. Use run time as the initial e2e estimate only.
    if !samples.contains_key("queued") || !samples.contains_key("e2e") {
        let mut jobs = conn.prepare(
            "SELECT j.created_at, j.started_at, j.ended_at FROM jobs j
            JOIN card_groups g ON g.card = ?1 AND g.project = j.project
            WHERE g.key LIKE 'merge:%' AND j.kind = 'merge' AND j.state = 'passed'",
        )?;
        let mut queue = Vec::new();
        let mut run = Vec::new();
        for row in jobs.query_map([id], |r| {
            Ok((elapsed(r.get(0)?, r.get(1)?), elapsed(r.get(1)?, r.get(2)?)))
        })? {
            let (queued, running) = row?;
            queue.extend(queued);
            run.extend(running);
        }
        samples.entry("queued".into()).or_insert(queue);
        samples.entry("e2e".into()).or_insert(run);
    }
    Ok(samples
        .into_iter()
        .filter_map(|(name, mut values)| {
            values.sort_unstable();
            let n = values.len();
            if n == 0 {
                None
            } else {
                Some((
                    name,
                    values[(n - 1) / 2] + (values[n / 2] - values[(n - 1) / 2]) / 2,
                ))
            }
        })
        .collect())
}

pub fn by_key(conn: &Connection, project: &str, key: &str) -> ApiResult<Option<Group>> {
    let id: Option<String> = conn
        .query_row(
            "SELECT card FROM card_groups WHERE project = ?1 AND key = ?2",
            params![project, key],
            |r| r.get(0),
        )
        .optional()?;
    id.map(|id| get(conn, &id)).transpose()
}

fn deadline(value: &str) -> ApiResult<String> {
    let secs = crate::store::jobs::planned_secs(value)?;
    let at = Utc::now()
        .checked_add_signed(Duration::seconds(secs))
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
    if by_key(t, n.project, n.key)?.is_some() {
        return Ok(false);
    }
    let close_at = n.close_after.map(deadline).transpose()?;
    let eta_at = n.eta.filter(|v| *v != "jobs").map(deadline).transpose()?;
    let done_when = board::resolve_column(t, n.project, n.done_when)?;
    let column = board::resolve_column(t, n.project, n.column)?;
    let col: i64 = t.query_row(
        "SELECT id FROM board_columns WHERE project = ?1 AND name = ?2",
        params![n.project, column],
        |r| r.get(0),
    )?;
    t.execute(
        "UPDATE projects SET card_seq = card_seq + 1 WHERE name = ?1",
        [n.project],
    )?;
    let (prefix, seq): (String, i64) = t.query_row(
        "SELECT prefix, card_seq FROM projects WHERE name = ?1",
        [n.project],
        |r| Ok((r.get(0)?, r.get(1)?)),
    )?;
    let id = format!("{prefix}-{seq}");
    let at = crate::time_param::stored(Utc::now());
    t.execute(
        "INSERT INTO cards (id, project, number, column_id, position, title, body,
        labels, created_by, created_at, updated_at, kind, eta_at)
        VALUES (?1, ?2, ?3, ?4, 0, ?5, '', '[]', ?6, ?7, ?7, 'group', ?8)",
        params![
            id,
            n.project,
            seq,
            col,
            t.redact(n.title),
            t.actor,
            at,
            eta_at
        ],
    )?;
    t.execute(
        "INSERT INTO card_groups (card, project, key, rule, done_when, eta_jobs, close_at)
        VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)",
        params![
            id,
            n.project,
            n.key,
            n.rule,
            done_when,
            n.eta == Some("jobs"),
            close_at
        ],
    )?;
    t.audit("group.open", &id, Some(n.project), json!({"key": n.key}))?;
    event(t, &id, "group_opened")?;
    Ok(true)
}

pub fn add(t: &mut Tx<'_>, g: &Group, card: &str) -> ApiResult<bool> {
    if g.closed_at.is_some() {
        return Ok(false);
    }
    let member = board::get_card(t, card)?;
    let project = board::project_of(t, &g.id)?;
    if member.project != project || member.kind != "card" || member.archived {
        return Err(ApiError::invalid(
            "group members must be active ordinary cards of the same project",
        ));
    }
    if board::is_held(t, card)? {
        return Err(ApiError::forbidden("a held card cannot join a group"));
    }
    if let Some(other) = &member.group {
        if other.id == g.id {
            return Ok(false);
        }
        return Err(ApiError::conflict(format!(
            "{card} is already in open group {}",
            other.id
        )));
    }
    t.execute(
        "INSERT INTO group_members (group_card, card, position)
        SELECT ?1, ?2, coalesce(max(position), -1) + 1 FROM group_members WHERE group_card = ?1",
        params![g.id, card],
    )?;
    event(t, &g.id, "group_added")?;
    Ok(true)
}

pub fn remove(t: &mut Tx<'_>, g: &Group, card: &str) -> ApiResult<bool> {
    if g.closed_at.is_some() {
        return Ok(false);
    }
    if g.key.starts_with("merge:") {
        let member = board::get_card(t, card)?;
        // Repair cards deliberately stay with their batch while being worked.
        // Done has no role on the default board, so it is matched by name.
        if member.labels.iter().any(|l| l.starts_with("merge-fix:")) && member.column != "done" {
            return Ok(false);
        }
    }
    let changed = t.execute(
        "DELETE FROM group_members WHERE group_card = ?1 AND card = ?2",
        params![g.id, card],
    )? > 0;
    if changed {
        event(t, &g.id, "group_removed")?;
        close_finished_merge(t, &g.id)?;
    }
    Ok(changed)
}

fn close_finished_merge(t: &mut Tx<'_>, id: &str) -> ApiResult<()> {
    let g = get(t, id)?;
    if g.closed_at.is_none() && g.key.starts_with("merge:") {
        if g.total == 0 {
            close(t, &g, "Merge group abandoned: no members remain.")?;
        } else if g.done == g.total {
            close(t, &g, "All remaining members merged (accepted or later).")?;
        }
    }
    Ok(())
}

pub fn status(t: &mut Tx<'_>, g: &Group, text: Option<&str>, eta: Option<&str>) -> ApiResult<bool> {
    if g.closed_at.is_some() {
        return Ok(false);
    }
    let mut changed = false;
    if let Some(text) = text {
        let text = t.redact(text);
        changed |= t.execute(
            "UPDATE card_groups SET status = ?2 WHERE card = ?1 AND status <> ?2",
            params![g.id, text],
        )? > 0;
    }
    if let Some(eta) = eta {
        let at = if eta == "jobs" {
            None
        } else {
            Some(deadline(eta)?)
        };
        changed |= t.execute(
            "UPDATE card_groups SET eta_jobs = ?2 WHERE card = ?1 AND eta_jobs <> ?2",
            params![g.id, eta == "jobs"],
        )? > 0;
        changed |= t.execute(
            "UPDATE cards SET eta_at = ?2 WHERE id = ?1 AND eta_at IS NOT ?2",
            params![g.id, at],
        )? > 0;
    }
    if changed {
        event(t, &g.id, "group_status")?;
    }
    Ok(changed)
}

pub fn close(t: &mut Tx<'_>, g: &Group, comment: &str) -> ApiResult<bool> {
    if g.closed_at.is_some() {
        return Ok(false);
    }
    let at = crate::time_param::stored(Utc::now());
    t.execute(
        "UPDATE card_groups SET closed_at = ?2 WHERE card = ?1",
        params![g.id, at],
    )?;
    t.execute(
        "UPDATE cards SET archived = 1, updated_at = ?2 WHERE id = ?1",
        params![g.id, at],
    )?;
    board::system_comment(t, &g.id, comment, None)?;
    Ok(true)
}

/// The recipe boundary separates merge preparation from the test run.
pub fn merge_recipe_started(t: &mut Tx<'_>, card: &str) -> ApiResult<()> {
    if let Some(g) = for_card(t, card)?
        && g.key.starts_with("merge:")
    {
        step(t, &g, "merge job", Some("done"), None, None)?;
        step(t, &g, "e2e", Some("running"), None, None)?;
    }
    Ok(())
}

pub fn step(
    t: &mut Tx<'_>,
    g: &Group,
    name: &str,
    state: Option<&str>,
    text: Option<&str>,
    eta: Option<&str>,
) -> ApiResult<bool> {
    if g.closed_at.is_some() {
        return Ok(false);
    }
    if name.trim().is_empty() {
        return Err(ApiError::invalid("group step name must not be empty"));
    }
    if let Some(state) = state
        && !["pending", "running", "done", "failed"].contains(&state)
    {
        return Err(ApiError::invalid(
            "group step state must be pending, running, done or failed",
        ));
    }
    // Callers may update several steps using the same group snapshot.
    let fresh = t
        .query_row(
            "SELECT name, state, text, eta_at, started_at, finished_at
        FROM group_steps WHERE group_card = ?1 AND name = ?2",
            params![g.id, name],
            |r| {
                Ok(GroupStep {
                    name: r.get(0)?,
                    state: r.get(1)?,
                    text: r.get(2)?,
                    eta_at: r.get(3)?,
                    started_at: r.get(4)?,
                    finished_at: r.get(5)?,
                    duration_secs: None,
                    estimated_secs: None,
                })
            },
        )
        .optional()?;
    let existing = fresh.as_ref();
    let state = state
        .or_else(|| existing.map(|s| s.state.as_str()))
        .unwrap_or("pending");
    let text = t.redact(
        text.or_else(|| existing.map(|s| s.text.as_str()))
            .unwrap_or_default(),
    );
    let stored_eta = t
        .query_row(
            "SELECT eta_at FROM group_steps WHERE group_card = ?1 AND name = ?2",
            params![g.id, name],
            |r| r.get::<_, Option<String>>(0),
        )
        .optional()?
        .flatten();
    let eta_at = match eta {
        Some("jobs") => return Err(ApiError::invalid("a step ETA takes a duration, not jobs")),
        Some(value) => Some(deadline(value)?),
        None if state == "pending"
            || (state == "running"
                && existing.is_some_and(|s| s.state == "done" || s.state == "failed")) =>
        {
            None
        }
        None => stored_eta.clone(),
    };
    if existing.is_some_and(|s| s.state == state && s.text == text && stored_eta == eta_at) {
        return Ok(false);
    }
    let at = crate::time_param::stored(Utc::now());
    t.execute("INSERT INTO group_steps (group_card, name, position, state, text, eta_at, started_at, finished_at)
        SELECT ?1, ?2, coalesce(max(position), -1) + 1, ?3, ?4, ?5,
        CASE WHEN ?3 = 'running' THEN ?6 END,
        CASE WHEN ?3 IN ('done', 'failed') THEN ?6 END FROM group_steps WHERE group_card = ?1
        ON CONFLICT(group_card, name) DO UPDATE SET state = excluded.state,
        text = excluded.text, eta_at = excluded.eta_at,
        started_at = CASE WHEN excluded.state = 'pending' THEN NULL
            WHEN excluded.state = 'running' AND group_steps.state IN ('done', 'failed') THEN ?6
            ELSE coalesce(group_steps.started_at, excluded.started_at) END,
        finished_at = CASE WHEN excluded.state IN ('done', 'failed')
            THEN coalesce(group_steps.finished_at, ?6) ELSE NULL END",
        params![g.id, name, state, text, eta_at, at])?;
    event(t, &g.id, "group_step")?;
    Ok(true)
}

/// The merge group repair, then the `close_after` safeguard, in one write.
/// The rule engine runs the two parts on its own ([`reconcile_merge_groups`]
/// in its planning write, [`close_expired`] at its tick).
pub fn expire(t: &mut Tx<'_>) -> ApiResult<()> {
    reconcile_merge_groups(t, &HashMap::new(), Utc::now())?;
    close_expired(t)
}

/// The safeguard is independent of whether the creating rule still exists.
/// Its comment and rule-log row commit together with the close.
pub fn close_expired(t: &mut Tx<'_>) -> ApiResult<()> {
    let at = crate::time_param::stored(Utc::now());
    let due: Vec<(String, String, String)> = {
        let mut stmt = t.prepare(
            "SELECT card, project, rule FROM card_groups
            WHERE closed_at IS NULL AND close_at <= ?1",
        )?;
        stmt.query_map([&at], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)))?
            .collect::<Result<_, _>>()?
    };
    for (id, project, rule) in due {
        let g = get(t, &id)?;
        close(t, &g, "Group closed by the close_after safeguard.")?;
        let run = ulid::Ulid::new().to_string();
        let steps = json!([{"verb":"group.close", "ok":true, "changed":true,
            "detail":{"group":id, "reason":"close_after"}}]);
        t.execute(
            "INSERT INTO rule_runs (id, project, rule, rule_hash, ord, event_seq,
            card, lane, rule_json, cause, state, step, steps, reason, created_at, updated_at)
            VALUES (?1, ?2, ?3, 'close_after', 0, 0, ?4, ?4, '{}', '{}', 'done', 1,
            ?5, 'close_after safeguard', ?6, ?6)",
            params![run, project, rule, id, steps.to_string(), at],
        )?;
    }
    Ok(())
}

/// How long the repair of one merge group waits for its pending rule runs.
/// After that it repairs anyway: a stopped lane keeps its row pending.
pub const RECONCILE_WAIT: Duration = Duration::minutes(10);

/// A pending rule run that can still change the merge group `id`: a run of
/// the group card or of a member, or, while the group has no member, a run
/// of the rule that opened it in its project (the run between its
/// `group_open` and its first `group_add`). Only pending rows are read
/// (`rule_runs_pending`).
fn pending_run(conn: &Connection, id: &str) -> ApiResult<Option<String>> {
    Ok(conn
        .query_row(
            "SELECT r.id FROM card_groups g JOIN rule_runs r ON r.state = 'pending'
             WHERE g.card = ?1 AND (
                r.card = g.card
                OR r.card IN (SELECT card FROM group_members WHERE group_card = g.card)
                OR (NOT EXISTS (SELECT 1 FROM group_members WHERE group_card = g.card)
                    AND r.project = g.project AND r.rule = g.rule))
             LIMIT 1",
            [id],
            |r| r.get(0),
        )
        .optional()?)
}

/// Repair memberships left by older rule packs. Review is also a valid
/// member state: queued jobs may create their group before moving the cards.
///
/// A group with a pending rule run ([`pending_run`]) waits (TER-471). A run
/// writes each action on its own, so a repair between `group_open` and
/// `group_add` found an empty group and closed it as abandoned, and a
/// repair between a card move and its `merge-group-returned` run removed
/// the card before the rule did. The engine calls this in its planning
/// write, only after a `plan_events` that read the log to its end, so each
/// event before that write has its runs.
///
/// `waiting` holds when each waiting group was first skipped. A group that
/// waited [`RECONCILE_WAIT`] is repaired anyway. Returns the groups that
/// wait now, with that time.
pub fn reconcile_merge_groups(
    t: &mut Tx<'_>,
    waiting: &HashMap<String, DateTime<Utc>>,
    now: DateTime<Utc>,
) -> ApiResult<HashMap<String, DateTime<Utc>>> {
    let mut still = HashMap::new();
    let ids: Vec<String> = {
        let mut stmt = t.prepare(
            "SELECT card FROM card_groups WHERE closed_at IS NULL AND key LIKE 'merge:%'",
        )?;
        stmt.query_map([], |r| r.get(0))?
            .collect::<Result<_, _>>()?
    };
    for id in ids {
        if let Some(run) = pending_run(t, &id)? {
            let since = waiting.get(&id).copied().unwrap_or(now);
            if now - since < RECONCILE_WAIT {
                debug!(group = %id, run = %run, "merge group repair waits for a pending rule run");
                still.insert(id, since);
                continue;
            }
            warn!(group = %id, run = %run, since = %since,
                "merge group repair runs anyway: a rule run stayed pending for 10 minutes");
        }
        let g = get(t, &id)?;
        for member in &g.members {
            let card = board::get_card(t, member)?;
            let role: Option<String> = t.query_row(
                "SELECT k.role FROM cards c JOIN board_columns k ON k.id = c.column_id WHERE c.id = ?1",
                [member],
                |r| r.get(0),
            )?;
            // Ready, backlog and done have no roles on the default board.
            // Working and blocked use roles so renamed columns work too.
            let repair = card.labels.iter().any(|l| l.starts_with("merge-fix:"));
            if (repair && card.column == "done")
                || (!repair
                    && (card.archived
                        || matches!(role.as_deref(), Some("working" | "blocked"))
                        || (role.is_none() && matches!(card.column.as_str(), "ready" | "backlog"))))
            {
                remove(t, &g, member)?;
            }
        }
        close_finished_merge(t, &id)?;
    }
    Ok(still)
}
