//! The column rules in the engine (TER-1113, kanban rework C2a, D2 of the
//! handoff in TER-1113, docs/api.md §19.13).
//!
//! A project with `board_flow = columns` runs only the rules of its board
//! columns (`board_column_rules`, TER-1120): no built-in pack and no
//! `rules.wcl`. Only the upgrade that converts the boards (C5) sets that
//! flow, so until then this code runs for no project.
//!
//! - **The rules.** [`read`] gives each enabled or disabled column rule as a
//!   legacy [`Rule`] named `<column>/<rule>` ([`engine_name`]), with its
//!   [`ColumnRuleMeta`] at the same index. So the run, the conditions, the
//!   verbs, the actor (`rule:<project>/<column>/<rule>`), the self-event
//!   guard, and the outbox stay those of the engine.
//! - **The triggers** ([`plan`]): `lands` (a card is created in the column
//!   or moves into it; with a column queue, when the card is admitted:
//!   the `admitted` event, TER-1114), `leaves` (a card moves out of it; a
//!   card that only waited in its queue never landed, so it does not
//!   leave, and its archive runs no `archived` rule), `archived` and
//!   `wait_met` (the column of the card in the event), `job_result` (a job
//!   or verdict event of a card that is in the column when the engine
//!   plans), `timer` (every `every_seconds`, [`fire_timers`]), and
//!   `count_reached` (a card lands and the column then holds exactly
//!   `count` cards: `data.to_count` of the event).
//! - **The order** (TER-1112 Q3): the `leaves` rules of the source column,
//!   then the `lands` rules of the target, then `count_reached`. With
//!   several cards (a batch job), the cards in board order (column, then
//!   position). In each column, the order of its rules.
//! - **The guards stay in the daemon** (D1): the chain depth, the self
//!   events, `max_per_hour` (default [`super::DEFAULT_MAX_PER_HOUR`]) and
//!   the per-card cap [`super::MAX_PER_CARD_PER_HOUR`]. A rule with an
//!   explicit `max_per_hour` has that number as its per-card cap too, never
//!   above [`rules_file::MAX_PER_HOUR`] (lead decision on TER-1113). The
//!   counts are by rule ID, so a rename does not reset them.
//! - **The start time.** An event older than the switch to `columns`
//!   (`board_flow_at`) starts no run. A change of the rules does not drop
//!   the events in flight.

use std::collections::HashMap;
use std::hash::{DefaultHasher, Hash, Hasher};

use chrono::{DateTime, Utc};
use rusqlite::{Connection, OptionalExtension, params};
use serde_json::{Value, json};
use terrarium_core::board_rules::{ColumnRule, Trigger};
use terrarium_core::rules_file::{self, Rule};
use tracing::warn;

use super::{
    MAX_PER_CARD_PER_HOUR, Planned, ProjectRules, TIMER_ACTOR, TIMER_KIND, actor_label,
    event_cards, event_job,
};
use crate::db::Db;
use crate::error::ApiResult;
use crate::events::{self, StoredEvent};
use crate::store::schedules::{self, MAX_CHAIN_DEPTH, event_view};
use crate::store::{board, jobs};

/// One column rule as the engine runs it: the facts that the legacy
/// [`Rule`] at the same index of [`ProjectRules::set`] does not have.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ColumnRuleMeta {
    /// The stable ID (`board_column_rules.id`).
    pub id: String,
    /// The column of the rule.
    pub column: String,
    /// The position of the column on the board.
    pub column_pos: i64,
    /// The position of the rule in its column.
    pub rule_pos: i64,
    pub trigger: Trigger,
}

/// The name of a column rule in the engine: `<column>/<rule>`. A rule name
/// has no `/` (`board_rules::check_rule`), so the name is unique in the
/// project, and the actor of its runs is `rule:<project>/<column>/<rule>`.
pub fn engine_name(column: &str, rule: &str) -> String {
    format!("{column}/{rule}")
}

/// The event kinds of a trigger, for the readers of a [`Rule`] (the run
/// log, the lint). The engine matches the trigger in [`plan`], not these.
fn on_kinds(t: &Trigger) -> Vec<String> {
    match t {
        Trigger::JobResult { .. } => vec!["job".into(), "job.verdict".into()],
        Trigger::Timer { .. } => vec![TIMER_KIND.into()],
        Trigger::Lands
        | Trigger::Leaves
        | Trigger::CountReached { .. }
        | Trigger::WaitMet
        | Trigger::Archived => vec!["card".into()],
    }
}

/// The legacy [`Rule`] of column rule `r` of column `column`.
pub fn engine_rule(column: &str, r: &ColumnRule) -> Rule {
    Rule {
        name: engine_name(column, &r.name),
        description: r.description.clone(),
        enabled: r.enabled,
        on: on_kinds(&r.trigger),
        when: r.when.clone(),
        if_card: r.if_card.clone(),
        if_job: r.if_job.clone(),
        max_per_hour: r.max_per_hour,
        actions: r.actions.clone(),
        line: 0,
    }
}

/// The column rules of `project` in board order (column, then rule), with
/// a hash of what was read. A stored rule that no longer reads (a model
/// that a later daemon changed) is left out with a warning.
pub fn read(conn: &Connection, project: &str) -> ApiResult<(String, Vec<(ColumnRuleMeta, Rule)>)> {
    let mut st = conn.prepare(
        "SELECT r.id, r.name, r.definition_json, c.name, c.position, r.position
           FROM board_column_rules r JOIN board_columns c ON c.id = r.column_id
          WHERE c.project = ?1
          ORDER BY c.position, r.position",
    )?;
    let rows = st
        .query_map([project], |r| {
            Ok((
                r.get::<_, String>(0)?,
                r.get::<_, String>(1)?,
                r.get::<_, String>(2)?,
                r.get::<_, String>(3)?,
                r.get::<_, i64>(4)?,
                r.get::<_, i64>(5)?,
            ))
        })?
        .collect::<Result<Vec<_>, _>>()?;
    let mut h = DefaultHasher::new();
    rows.hash(&mut h);
    let hash = format!("columns+{:016x}", h.finish());
    let mut out = Vec::with_capacity(rows.len());
    for (id, name, def, column, column_pos, rule_pos) in rows {
        let parsed = serde_json::from_str::<Value>(&def).and_then(|mut v| {
            v["id"] = id.clone().into();
            v["name"] = name.clone().into();
            serde_json::from_value::<ColumnRule>(v)
        });
        match parsed {
            Ok(r) => out.push((
                ColumnRuleMeta {
                    id,
                    column: column.clone(),
                    column_pos,
                    rule_pos,
                    trigger: r.trigger.clone(),
                },
                engine_rule(&column, &r),
            )),
            Err(e) => {
                warn!(%project, rule = %id, "a stored column rule does not read; it runs nothing: {e}");
            }
        }
    }
    Ok((hash, out))
}

/// The runs of the rule with ID `rule_id` since `since`, up to `limit`.
fn count_since(
    conn: &Connection,
    rule_id: &str,
    card: Option<&str>,
    since: &str,
    limit: i64,
) -> ApiResult<i64> {
    Ok(conn.query_row(
        "SELECT count(*) FROM (SELECT 1 FROM rule_runs
          WHERE rule_id = ?1 AND created_at >= ?2 AND state != 'skipped'
            AND (?3 IS NULL OR card = ?3) LIMIT ?4)",
        params![rule_id, since, card, limit],
        |r| r.get(0),
    )?)
}

/// The per-card cap of a rule: its explicit `max_per_hour`, never above
/// [`rules_file::MAX_PER_HOUR`]; without one, [`MAX_PER_CARD_PER_HOUR`].
pub fn card_cap(rule: &Rule) -> i64 {
    rule.max_per_hour.map_or(MAX_PER_CARD_PER_HOUR, |n| {
        i64::from(n.min(rules_file::MAX_PER_HOUR))
    })
}

/// The cap of a rule in an hour: its `max_per_hour`, never above
/// [`rules_file::MAX_PER_HOUR`]; without one, the default.
fn rule_cap(rule: &Rule) -> i64 {
    i64::from(
        rule.max_per_hour
            .unwrap_or(super::DEFAULT_MAX_PER_HOUR)
            .min(rules_file::MAX_PER_HOUR),
    )
}

/// One run that [`plan`] makes: the index of the rule in
/// [`ProjectRules::set`], the run, and its order in the event.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ColumnPlan {
    pub index: usize,
    pub planned: Planned,
    pub ord: usize,
}

/// The order of a run in its event: phase (0 leaves, 1 lands or job
/// result, 2 count), the card's place on the board, the rule's place.
fn order(phase: usize, card_rank: usize, m: &ColumnRuleMeta) -> usize {
    let col = usize::try_from(m.column_pos).unwrap_or(0).min(999);
    let pos = usize::try_from(m.rule_pos).unwrap_or(0).min(999);
    ((phase * 1000 + card_rank.min(999)) * 1000 + col) * 1000 + pos
}

/// The runs that event `e` starts in the column rules of `pr`, in order.
/// Reads only. `after`: an event before this time starts no run (the
/// switch to `columns`; `None` in a dry run).
pub fn plan(
    conn: &Connection,
    pr: &ProjectRules,
    e: &StoredEvent,
    after: Option<&str>,
    now: DateTime<Utc>,
) -> ApiResult<Vec<ColumnPlan>> {
    let Some(meta) = pr.columns.as_deref() else {
        return Ok(Vec::new());
    };
    if e.project.as_deref() != Some(pr.project.as_str()) || after.is_some_and(|a| e.at.as_str() < a)
    {
        return Ok(Vec::new());
    }
    // (index, card, phase, card rank)
    let mut hits: Vec<(usize, Option<String>, usize, usize)> = Vec::new();
    let in_column = |column: &str, want: &dyn Fn(&Trigger) -> bool| -> Vec<usize> {
        meta.iter()
            .enumerate()
            .filter(|(_, m)| m.column == column && want(&m.trigger))
            .map(|(i, _)| i)
            .collect()
    };
    match e.kind.as_str() {
        TIMER_KIND => {
            // A timer names its rule; only the engine writes one.
            if e.actor.as_deref() == Some(TIMER_ACTOR)
                && let Some(name) = e.data["rule"].as_str()
                && let Some(i) = pr.set.rules.iter().position(|r| r.name == name)
            {
                hits.push((i, None, 1, 0));
            }
        }
        "card" => {
            let card = event_cards(e).into_iter().next();
            let column = e.data["column"].as_str().unwrap_or_default();
            let count = e.data["to_count"].as_u64();
            let counted = |at: &str, hits: &mut Vec<_>| {
                if let Some(n) = count {
                    for i in in_column(
                        at,
                        &|t| matches!(t, Trigger::CountReached { count } if u64::from(*count) == n),
                    ) {
                        hits.push((i, None, 2, 0));
                    }
                }
            };
            // The column queues (TER-1114): a card that moves into a queue
            // lands when it is admitted, and a card that leaves a queue
            // never landed, so it does not leave either.
            let flag = |k: &str| e.data[k].as_bool().unwrap_or(false);
            match e.data["change"].as_str().unwrap_or_default() {
                "moved" => {
                    if let Some(from) = e.data["from"].as_str().filter(|_| !flag("from_queued")) {
                        for i in in_column(from, &|t| *t == Trigger::Leaves) {
                            hits.push((i, card.clone(), 0, 0));
                        }
                    }
                    if let Some(to) = e.data["to"].as_str().filter(|_| !flag("queued")) {
                        for i in in_column(to, &|t| *t == Trigger::Lands) {
                            hits.push((i, card.clone(), 1, 0));
                        }
                        counted(to, &mut hits);
                    }
                }
                "admitted" => {
                    for i in in_column(column, &|t| *t == Trigger::Lands) {
                        hits.push((i, card.clone(), 1, 0));
                    }
                    counted(column, &mut hits);
                }
                "created" => {
                    for i in in_column(column, &|t| *t == Trigger::Lands) {
                        hits.push((i, card.clone(), 1, 0));
                    }
                    counted(column, &mut hits);
                }
                "archived" if !flag("queued") => {
                    for i in in_column(column, &|t| *t == Trigger::Archived) {
                        hits.push((i, card.clone(), 1, 0));
                    }
                }
                "wait_met" => {
                    for i in in_column(column, &|t| *t == Trigger::WaitMet) {
                        hits.push((i, card.clone(), 1, 0));
                    }
                }
                _ => {}
            }
        }
        "job" | "job.verdict" => {
            let change = if e.kind == "job.verdict" {
                "verdict"
            } else {
                e.data["change"].as_str().unwrap_or_default()
            };
            let kind = match event_job(e) {
                Some(id) => jobs::get(conn, id).ok().map(|j| j.kind.as_str()),
                None => None,
            }
            .or_else(|| {
                e.data["kind"]
                    .as_str()
                    .and_then(jobs::JobKind::parse)
                    .map(|k| k.as_str())
            });
            // Each card of the job, in the column that it is in now.
            let mut cards: Vec<(i64, i64, String, String)> = Vec::new();
            for id in event_cards(e) {
                if let Ok(c) = board::get_card(conn, &id)
                    && !c.archived
                {
                    let col = meta
                        .iter()
                        .find(|m| m.column == c.column)
                        .map_or(i64::MAX, |m| m.column_pos);
                    cards.push((col, c.position, c.column, id));
                }
            }
            cards.sort();
            for (rank, (_, _, column, id)) in cards.into_iter().enumerate() {
                let want = |t: &Trigger| match t {
                    Trigger::JobResult { events, job_kinds } => {
                        events.iter().any(|x| x == change)
                            && (job_kinds.is_empty()
                                || kind.is_some_and(|k| job_kinds.iter().any(|x| x == k)))
                    }
                    _ => false,
                };
                for i in in_column(&column, &want) {
                    hits.push((i, Some(id.clone()), 1, rank));
                }
            }
        }
        _ => {}
    }
    let depth = schedules::depth_of(e);
    let hour_ago = schedules::stamp(now - chrono::Duration::hours(1));
    let view = event_view(e);
    let mut out = Vec::new();
    for (index, card, phase, rank) in hits {
        let rule = &pr.set.rules[index];
        let m = &meta[index];
        if !rule.enabled
            || e.actor.as_deref() == Some(actor_label(&pr.project, &rule.name).as_str())
        {
            continue;
        }
        if e.kind != TIMER_KIND && !rule.when.iter().all(|w| w.holds(&view)) {
            continue;
        }
        if !rule.if_card.is_empty() && card.is_none() {
            continue;
        }
        let skip = if depth >= MAX_CHAIN_DEPTH {
            Some(format!(
                "chain depth {depth}: the event starts no run (loop guard)"
            ))
        } else {
            let max = rule_cap(rule);
            let card_max = card_cap(rule);
            if count_since(conn, &m.id, None, &hour_ago, max)? >= max {
                Some(format!("rate limit: {max} runs in an hour"))
            } else if let Some(c) = &card
                && count_since(conn, &m.id, Some(c), &hour_ago, card_max)? >= card_max
            {
                Some(format!(
                    "card rate limit: {card_max} runs for {c} in an hour"
                ))
            } else {
                None
            }
        };
        out.push(ColumnPlan {
            index,
            planned: Planned { card, skip },
            ord: order(phase, rank, m),
        });
    }
    out.sort_by_key(|p| p.ord);
    Ok(out)
}

// ------------------------------------------------------------- the timers

/// The key of a column timer in [`super::Cache::fired`]: the rule ID with a
/// prefix that a rule name cannot have.
fn timer_key(rule_id: &str) -> String {
    format!("\0column:{rule_id}")
}

/// Whether `key` of [`super::Cache::fired`] is the timer of a column rule
/// of `projects` that is still there.
pub fn live_timer(projects: &[ProjectRules], project: &str, key: &str) -> bool {
    projects.iter().filter(|p| p.project == project).any(|p| {
        p.columns.as_deref().is_some_and(|meta| {
            meta.iter()
                .any(|m| matches!(m.trigger, Trigger::Timer { .. }) && timer_key(&m.id) == key)
        })
    })
}

/// Write a `rule.timer` event for each enabled `timer` rule of a column
/// whose period started: one in each slot of `every_seconds` (UTC epoch
/// slots), also across restarts (the event log is checked). No catch-up
/// after downtime: only the current slot fires. Returns the events written.
pub async fn fire_timers(
    db: &Db,
    projects: &[ProjectRules],
    fired: &mut HashMap<(String, String), String>,
    now: DateTime<Utc>,
) -> usize {
    let mut written = 0;
    for p in projects {
        let Some(meta) = p.columns.as_deref() else {
            continue;
        };
        for (m, rule) in meta.iter().zip(&p.set.rules) {
            let Trigger::Timer { every_seconds } = m.trigger else {
                continue;
            };
            if !rule.enabled || every_seconds == 0 {
                continue;
            }
            let every = i64::try_from(every_seconds).unwrap_or(i64::MAX);
            let slot = now.timestamp().div_euclid(every).to_string();
            let key = (p.project.clone(), timer_key(&m.id));
            if fired.get(&key) == Some(&slot) {
                continue;
            }
            let (project, rule_id, name, column, s) = (
                p.project.clone(),
                m.id.clone(),
                rule.name.clone(),
                m.column.clone(),
                slot.clone(),
            );
            let result = db
                .write(TIMER_ACTOR, move |tx| {
                    let exists: Option<i64> = tx
                        .query_row(
                            "SELECT 1 FROM events WHERE project = ?1 AND kind = ?2 AND actor = 'system'
                               AND json_extract(payload, '$.rule_id') = ?3
                               AND json_extract(payload, '$.slot') = ?4 LIMIT 1",
                            params![project, TIMER_KIND, rule_id, s],
                            |r| r.get(0),
                        )
                        .optional()?;
                    if exists.is_some() {
                        return Ok(0);
                    }
                    tx.emit(
                        events::EventDraft::new(
                            TIMER_KIND,
                            format!("the timer of column rule {name}"),
                            json!({"rule": name, "rule_id": rule_id, "column": column,
                                   "slot": s, "every_seconds": every_seconds}),
                        )
                        .project(project),
                    )?;
                    Ok(1)
                })
                .await;
            match result {
                Ok(n) => {
                    written += n;
                    fired.insert(key, slot);
                }
                Err(e) => warn!(
                    "the rule engine cannot write the timer of column rule {} of project {}: {e}",
                    rule.name, p.project
                ),
            }
        }
    }
    written
}

#[cfg(test)]
#[allow(clippy::expect_used)]
mod tests {
    use super::*;

    #[test]
    fn the_card_cap_follows_an_explicit_max_up_to_the_ceiling() {
        let mut r = engine_rule(
            "review",
            &serde_json::from_value(json!({
                "name": "r", "trigger": {"kind": "lands"},
                "actions": [{"verb": "card.comment", "args": {"text": "x"}}]
            }))
            .expect("a rule"),
        );
        assert_eq!(r.name, "review/r");
        assert_eq!(card_cap(&r), MAX_PER_CARD_PER_HOUR);
        assert_eq!(rule_cap(&r), i64::from(super::super::DEFAULT_MAX_PER_HOUR));
        r.max_per_hour = Some(600);
        assert_eq!(card_cap(&r), 600);
        r.max_per_hour = Some(5000);
        assert_eq!(card_cap(&r), i64::from(rules_file::MAX_PER_HOUR));
        assert_eq!(rule_cap(&r), i64::from(rules_file::MAX_PER_HOUR));
        r.max_per_hour = Some(3);
        assert_eq!(card_cap(&r), 3);
    }

    #[test]
    fn leaves_runs_before_lands_and_cards_in_board_order() {
        let m = |c, p| ColumnRuleMeta {
            id: String::new(),
            column: String::new(),
            column_pos: c,
            rule_pos: p,
            trigger: Trigger::Lands,
        };
        assert!(order(0, 0, &m(5, 9)) < order(1, 0, &m(0, 0)));
        assert!(order(1, 0, &m(5, 9)) < order(1, 1, &m(0, 0)));
        assert!(order(1, 0, &m(1, 9)) < order(1, 0, &m(2, 0)));
        assert!(order(1, 0, &m(1, 0)) < order(1, 0, &m(1, 1)));
        assert!(order(1, 999, &m(999, 999)) < order(2, 0, &m(0, 0)));
    }
}
