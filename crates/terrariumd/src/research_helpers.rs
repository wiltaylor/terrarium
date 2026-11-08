//! Card research helpers. Admission runs in the spawn write transaction.
//! Operators tune the daemon's TERRARIUM_RESEARCH_HELPERS_PER_CARD and
//! TERRARIUM_RESEARCH_HELPERS_PER_DAY (rolling 24 hours), default 2 and 20.
//! A zero cap disables helpers. All projects have separate daily counts.

use rusqlite::{Connection, OptionalExtension, params};
use serde_json::json;

use crate::db::{Tx, now};
use crate::error::{ApiError, ApiResult};
use crate::events::EventDraft;
use crate::store::{board, spawns};

pub fn card(conn: &Connection, owner: &str) -> ApiResult<Option<String>> {
    Ok(spawns::find(conn, owner)?
        .and_then(|s| s.bound)
        .and_then(|b| b.strip_prefix("card:").map(str::to_owned)))
}

fn cap(name: &str, default: i64) -> i64 {
    std::env::var(name)
        .ok()
        .and_then(|s| s.parse::<i64>().ok())
        .filter(|n| *n >= 0)
        .unwrap_or(default)
}

/// None means admitted. Return a refusal as data so its event can commit.
pub fn admit(t: &mut Tx<'_>, owner: &str, project: &str, card: &str) -> ApiResult<Option<String>> {
    check(
        t,
        owner,
        project,
        card,
        cap("TERRARIUM_RESEARCH_HELPERS_PER_CARD", 2),
        cap("TERRARIUM_RESEARCH_HELPERS_PER_DAY", 20),
    )
}

fn check(
    t: &mut Tx<'_>,
    owner: &str,
    project: &str,
    card: &str,
    per_card: i64,
    per_day: i64,
) -> ApiResult<Option<String>> {
    let c = board::get_card(t, card)?;
    if c.project != project || c.assignee.as_deref() != Some(owner) || c.archived {
        return Err(ApiError::forbidden(
            "only the card's current assignee may spawn research helpers",
        ));
    }
    let live: i64 = t.query_row(
        "SELECT count(*) FROM research_helpers WHERE card = ?1 AND finished_at IS NULL",
        [card],
        |r| r.get(0),
    )?;
    if live >= per_card {
        let fresh = t.execute(
            "INSERT OR IGNORE INTO research_helper_alerts(card) VALUES (?1)",
            [card],
        )?;
        if fresh > 0 {
            // TER-79 consumes this durable, once-per-card trouble hook.
            t.emit(
                EventDraft::new(
                    "card.research_helper_limit",
                    "Card research helper limit reached",
                    json!({"card": card, "limit": per_card, "running": live}),
                )
                .project(project)
                .card(card),
            )?;
        }
        return Ok(Some(format!(
            "card {card} research helper limit is {per_card}"
        )));
    }
    let daily: i64 = t.query_row("SELECT count(*) FROM research_helpers WHERE project = ?1 AND julianday(spawned_at) >= julianday('now', '-24 hours')", [project], |r| r.get(0))?;
    if daily >= per_day {
        return Ok(Some(format!(
            "project {project} research helper daily limit is {per_day}"
        )));
    }
    let limit: Option<Option<i64>> = t
        .query_row(
            "SELECT wip_limit FROM board_columns WHERE project = ?1 AND lower(name) = 'research'",
            [project],
            |r| r.get(0),
        )
        .optional()?;
    let limit = limit.flatten().unwrap_or(2);
    let occupied: i64 = t.query_row("SELECT (SELECT count(*) FROM research_helpers WHERE project = ?1 AND finished_at IS NULL) + (SELECT count(*) FROM cards c JOIN board_columns k ON k.id = c.column_id WHERE c.project = ?1 AND lower(k.name) = 'research' AND c.archived = 0 AND c.assignee IS NOT NULL)", [project], |r| r.get(0))?;
    if occupied >= limit {
        return Ok(Some(format!(
            "project {project} research WIP limit is {limit}"
        )));
    }
    Ok(None)
}

pub fn record(t: &mut Tx<'_>, id: &str, project: &str, card: &str) -> ApiResult<()> {
    t.execute(
        "INSERT INTO research_helpers(id, project, card, spawned_at) VALUES (?1, ?2, ?3, ?4)",
        params![id, project, card, now()],
    )?;
    Ok(())
}

pub fn finish(t: &mut Tx<'_>, sp: &spawns::Spawn) -> ApiResult<()> {
    let card: Option<String> = t
        .query_row(
            "SELECT card FROM research_helpers WHERE id = ?1 AND finished_at IS NULL",
            [&sp.id],
            |r| r.get(0),
        )
        .optional()?;
    if let Some(card) = card {
        t.execute(
            "UPDATE research_helpers SET finished_at = ?2 WHERE id = ?1",
            params![sp.id, now()],
        )?;
        board::system_comment(
            t,
            &card,
            &format!(
                "Research helper {} ({})\n\n{}",
                sp.name,
                sp.outcome.as_deref().unwrap_or("finished"),
                sp.result.as_deref().unwrap_or("No result supplied.")
            ),
            sp.session_id.as_deref(),
        )?;
    }
    Ok(())
}
