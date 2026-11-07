//! The board rule engine (TER-162, TER-168, docs/api.md §19.9): it runs
//! the rules of each project's `rules.wcl` (`crate::store::rules`) on the
//! events of the log.
//!
//! - **Durable cursor.** The engine reads the event log after its own
//!   cursor (`rule_cursor`), not the live hub: a new event only wakes it.
//!   The first start sets the cursor to the end of the log, so old events
//!   start no run.
//! - **Plan, then run (the outbox).** [`plan_pass`] reads the events after
//!   the cursor, matches them against the enabled rules of their project
//!   in file order (`on`, `when`), and writes one `rule_runs` row for each
//!   (event, rule, card) in the same write that moves the cursor. A crash
//!   after that write loses no action: the next start runs the pending
//!   rows. A crash before it reads the events again.
//! - **Guards** at plan time: a rule does not see the events of its own
//!   runs; an event at chain depth [`MAX_CHAIN_DEPTH`] (`data.cause.depth`,
//!   counted over rule and trigger runs) starts no run; a rule runs at most
//!   `max_per_hour` times in an hour (default [`DEFAULT_MAX_PER_HOUR`]),
//!   and at most [`MAX_PER_CARD_PER_HOUR`] times for one card. A guard
//!   writes a `skipped` row with the reason.
//! - **Per-card order.** [`run_pending`] runs the pending rows in event
//!   order, then rule order. The rows of one card (its lane) run one after
//!   another; other lanes run beside them.
//! - **Conditions on the current state** (`if_card`, `if_job`) and the
//!   column check (each column that the rule names is still on the board)
//!   run in the write of the first action, so they see the state after the
//!   earlier runs of the same card. A condition that does not hold skips
//!   the run; a missing column fails it.
//! - **Ordered actions.** Each action is one write that also moves the
//!   row's step counter, so a restart goes on at the next action and never
//!   repeats a database action. The first failed action ends the run as
//!   `failed`; the actions after it do not run. An action that changes
//!   nothing is a no-op; a run of only no-ops ends as `noop`.
//! - **The rule actor.** Each action acts as `rule:<project>/<rule>`
//!   ([`rule_actor`]): an agent-shaped actor with `project_admin` of the
//!   rule's project only, never the operator. So the store's checks apply
//!   to it as to a lead: the project gate, the GitHub author gate (a held
//!   card), and the board invariants. Its events carry
//!   `data.cause = {rule, run, event, depth}`.
//! - **Dry run.** [`dry_run`] plans the runs for an event or a `seq` range
//!   with the guards, the conditions on the current state, and the
//!   rendered actions, and writes nothing.
//!
//! The verbs come with TER-169. Until then only `card.comment` runs; each
//! other verb fails its action with "not implemented yet".

use std::collections::{BTreeMap, HashMap};
use std::sync::Arc;
use std::time::Duration;

use chrono::{DateTime, Utc};
use rusqlite::{Connection, OptionalExtension, params};
use serde::Serialize;
use serde_json::{Map, Value, json};
use terrarium_core::Role;
use terrarium_core::config::Policy;
use terrarium_core::rules_file::{self, Action, BoardColumn, Context, Rule, RuleSet};
use terrarium_core::template::value_text;
use tokio::sync::broadcast::error::TryRecvError;
use tokio::task::JoinHandle;
use tracing::warn;

use crate::actor::{Actor, AgentActor};
use crate::api::Shared;
use crate::db::{Db, Tx, now};
use crate::error::{ApiError, ApiResult};
use crate::events::{self, EventFilter, StoredEvent};
use crate::store::schedules::{self, MAX_CHAIN_DEPTH, event_view};
use crate::store::{board, jobs, rules};

/// The most events that one [`plan_pass`] reads.
pub const BATCH: usize = 500;
/// The most pending rows that one [`run_pending`] takes.
pub const RUN_BATCH: usize = 200;
/// The runs of one rule in an hour without `max_per_hour`.
pub const DEFAULT_MAX_PER_HOUR: u32 = 30;
/// The most runs of one rule for one card in an hour.
pub const MAX_PER_CARD_PER_HOUR: i64 = 10;
/// The finished rows that each project keeps (the run log).
pub const KEEP_RUNS: i64 = 500;
/// The most events that one dry run reads.
pub const MAX_DRY_RUN_EVENTS: usize = 500;
/// The period of the engine without a wake-up.
const TICK: Duration = Duration::from_secs(10);

/// The label of rule `rule` of `project`: the actor of its writes.
pub fn actor_label(project: &str, rule: &str) -> String {
    format!("rule:{project}/{rule}")
}

/// The actor of rule `rule` of `project`: `project_admin` of that project
/// and nothing more. Not the operator, not a sysadmin.
pub fn rule_actor(project: &str, rule: &str) -> Actor {
    let label = actor_label(project, rule);
    Actor::Agent(AgentActor {
        id: label.clone(),
        name: label,
        project: project.to_owned(),
        roles: vec![Role::ProjectAdmin],
        policy: Policy::default(),
    })
}

// ------------------------------------------------------------- the rules

/// The rules of one project as the engine runs them.
#[derive(Debug, Clone)]
pub struct ProjectRules {
    pub project: String,
    pub hash: String,
    /// When the rules were stored: an older event starts no run.
    pub updated_at: String,
    pub set: Arc<RuleSet>,
}

/// The parsed rules files, by project, with their hash: a file is parsed
/// again only when it changed.
#[derive(Debug, Default)]
pub struct Cache {
    sets: HashMap<String, (String, Arc<RuleSet>)>,
}

/// The rules of every project with a rules file. Each changed file is
/// parsed off the DB lock, without the column check (the engine checks the
/// columns when a run starts). A file that does not parse (a stored text
/// that a later schema refuses) runs nothing and logs a warning.
pub async fn load(db: &Db, cache: &mut Cache) -> ApiResult<Vec<ProjectRules>> {
    let rows: Vec<(String, String, String)> = db
        .read(|c| {
            let mut st =
                c.prepare("SELECT project, hash, updated_at FROM project_rules ORDER BY project")?;
            let rows = st
                .query_map([], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)))?
                .collect::<Result<Vec<_>, _>>()?;
            Ok(rows)
        })
        .await?;
    cache
        .sets
        .retain(|p, _| rows.iter().any(|(q, _, _)| q == p));
    let mut out = Vec::new();
    for (project, hash, updated_at) in rows {
        let cached = cache
            .sets
            .get(&project)
            .filter(|(h, _)| *h == hash)
            .map(|(_, s)| s.clone());
        let set = match cached {
            Some(s) => s,
            None => {
                let p = project.clone();
                let text: Option<String> = db
                    .read(move |c| {
                        Ok(c.query_row(
                            "SELECT text FROM project_rules WHERE project = ?1",
                            [p],
                            |r| r.get(0),
                        )
                        .optional()?)
                    })
                    .await?;
                let Some(text) = text else { continue };
                let ctx = Context {
                    columns: None,
                    max_rules: usize::MAX,
                };
                let set = match rules::parse_off(text, rules::file_path(&project), ctx).await? {
                    Ok(s) => Arc::new(s),
                    Err(e) => {
                        warn!(project = %project, "the stored rules do not parse; they run nothing: {e}");
                        Arc::new(RuleSet::default())
                    }
                };
                cache
                    .sets
                    .insert(project.clone(), (hash.clone(), set.clone()));
                set
            }
        };
        out.push(ProjectRules {
            project,
            hash,
            updated_at,
            set,
        });
    }
    Ok(out)
}

// ------------------------------------------------------------ the cursor

/// The last event `seq` that the engine read; `None` before its first
/// start.
pub fn cursor(conn: &Connection) -> ApiResult<Option<i64>> {
    Ok(conn
        .query_row("SELECT seq FROM rule_cursor WHERE id = 1", [], |r| r.get(0))
        .optional()?)
}

fn set_cursor(t: &Tx<'_>, seq: i64) -> ApiResult<()> {
    t.execute(
        "INSERT INTO rule_cursor (id, seq) VALUES (1, ?1)
         ON CONFLICT (id) DO UPDATE SET seq = excluded.seq",
        [seq],
    )?;
    Ok(())
}

/// At the first start, set the cursor to the end of the log.
pub async fn startup(db: &Db) -> ApiResult<()> {
    db.write("system", |t| {
        if cursor(t)?.is_none() {
            let last = events::last_seq(t)?;
            set_cursor(t, last)?;
        }
        Ok(())
    })
    .await
}

// ------------------------------------------------------------ the guards

/// The cards of an event: its `card`, `data.card`, and the texts of
/// `data.cards`, without repeats.
pub fn event_cards(e: &StoredEvent) -> Vec<String> {
    let mut out: Vec<String> = Vec::new();
    let mut add = |c: &str| {
        if !c.is_empty() && !out.iter().any(|o| o == c) {
            out.push(c.to_owned());
        }
    };
    if let Some(c) = &e.card {
        add(c);
    }
    if let Some(c) = e.data["card"].as_str() {
        add(c);
    }
    if let Some(list) = e.data["cards"].as_array() {
        for c in list.iter().filter_map(Value::as_str) {
            add(c);
        }
    }
    out
}

/// One run that the plan makes: for a card or none, and a skip reason
/// when a guard holds it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct Planned {
    pub card: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub skip: Option<String>,
}

fn count_since(
    conn: &Connection,
    project: &str,
    rule: &str,
    card: Option<&str>,
    since: &str,
) -> ApiResult<i64> {
    Ok(conn.query_row(
        "SELECT count(*) FROM rule_runs
          WHERE project = ?1 AND rule = ?2 AND created_at >= ?3 AND state != 'skipped'
            AND (?4 IS NULL OR card = ?4)",
        params![project, rule, since, card],
        |r| r.get(0),
    )?)
}

/// What rule `rule` of `pr` does with event `e`: no run (empty), or one
/// [`Planned`] for each card of the event (one without a card when the
/// event has none). Reads only. `after`: an event before this time starts
/// no run (the time the rules were stored; `None` in a dry run).
pub fn decide(
    conn: &Connection,
    project: &str,
    rule: &Rule,
    e: &StoredEvent,
    after: Option<&str>,
    now: DateTime<Utc>,
) -> ApiResult<Vec<Planned>> {
    if !rule.enabled || e.project.as_deref() != Some(project) {
        return Ok(Vec::new());
    }
    if e.actor.as_deref() == Some(actor_label(project, &rule.name).as_str()) {
        return Ok(Vec::new());
    }
    if after.is_some_and(|a| e.at.as_str() < a) {
        return Ok(Vec::new());
    }
    let view = event_view(e);
    if !rule
        .on
        .iter()
        .any(|k| terrarium_core::schedule::glob(k, &e.kind))
        || !rule.when.iter().all(|m| m.holds(&view))
    {
        return Ok(Vec::new());
    }
    if !rule.if_job.is_empty() && e.kind != "job" {
        return Ok(Vec::new());
    }
    let cards = event_cards(e);
    if !rule.if_card.is_empty() && cards.is_empty() {
        return Ok(Vec::new());
    }
    let cards: Vec<Option<String>> = if cards.is_empty() {
        vec![None]
    } else {
        cards.into_iter().map(Some).collect()
    };
    let depth = schedules::depth_of(e);
    if depth >= MAX_CHAIN_DEPTH {
        let skip = format!("chain depth {depth}: the event starts no run (loop guard)");
        return Ok(cards
            .into_iter()
            .map(|card| Planned {
                card,
                skip: Some(skip.clone()),
            })
            .collect());
    }
    let hour_ago = schedules::stamp(now - chrono::Duration::hours(1));
    let max = i64::from(rule.max_per_hour.unwrap_or(DEFAULT_MAX_PER_HOUR));
    let mut out = Vec::new();
    for card in cards {
        let skip = if count_since(conn, project, &rule.name, None, &hour_ago)? >= max {
            Some(format!("rate limit: {max} runs in an hour"))
        } else if let Some(c) = &card
            && count_since(conn, project, &rule.name, Some(c), &hour_ago)? >= MAX_PER_CARD_PER_HOUR
        {
            Some(format!(
                "card rate limit: {MAX_PER_CARD_PER_HOUR} runs for {c} in an hour"
            ))
        } else {
            None
        };
        out.push(Planned { card, skip });
    }
    Ok(out)
}

/// The `data.cause` of the events of a rule run.
fn cause(project: &str, rule: &str, run: &str, e: &StoredEvent) -> Value {
    json!({
        "rule": format!("{project}/{rule}"),
        "run": run,
        "event": e.seq,
        "depth": schedules::depth_of(e) + 1,
    })
}

// -------------------------------------------------------------- the plan

/// Read the events after the cursor (at most [`BATCH`]), write a
/// `rule_runs` row for each run that the rules plan, and move the cursor,
/// in one write. Returns the rows written (pending and skipped), and
/// whether more events wait.
pub async fn plan_pass(
    db: &Db,
    projects: Vec<ProjectRules>,
    now: DateTime<Utc>,
) -> ApiResult<(usize, bool)> {
    db.write("system", move |t| {
        let Some(cursor) = cursor(t)? else {
            let last = events::last_seq(t)?;
            set_cursor(t, last)?;
            return Ok((0, false));
        };
        let evs = events::list(t, &EventFilter::default(), cursor, BATCH)?;
        let Some(last) = evs.last().map(|e| e.seq) else {
            return Ok((0, false));
        };
        let by_project: HashMap<&str, &ProjectRules> =
            projects.iter().map(|p| (p.project.as_str(), p)).collect();
        let mut written = 0;
        let mut touched: Vec<String> = Vec::new();
        for e in &evs {
            let Some(pr) = e.project.as_deref().and_then(|p| by_project.get(p)) else {
                continue;
            };
            for (ord, rule) in pr.set.rules.iter().enumerate() {
                let planned = decide(t, &pr.project, rule, e, Some(&pr.updated_at), now)?;
                for p in planned {
                    insert_run(t, pr, ord, rule, e, &p, now)?;
                    written += 1;
                }
            }
            if !touched.contains(&pr.project) {
                touched.push(pr.project.clone());
            }
        }
        set_cursor(t, last)?;
        for p in &touched {
            prune(t, p)?;
        }
        Ok((written, evs.len() == BATCH))
    })
    .await
}

fn insert_run(
    t: &Tx<'_>,
    pr: &ProjectRules,
    ord: usize,
    rule: &Rule,
    e: &StoredEvent,
    p: &Planned,
    now: DateTime<Utc>,
) -> ApiResult<()> {
    let id = ulid::Ulid::new().to_string();
    let lane = p
        .card
        .clone()
        .unwrap_or_else(|| format!("project:{}", pr.project));
    let (state, reason) = match &p.skip {
        Some(r) => ("skipped", Some(r.as_str())),
        None => ("pending", None),
    };
    let at = schedules::stamp(now);
    t.execute(
        "INSERT INTO rule_runs (id, project, rule, rule_hash, ord, event_seq, card, lane,
                                rule_json, cause, state, reason, created_at, updated_at)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?13)
         ON CONFLICT (project, rule, event_seq, lane) DO NOTHING",
        params![
            id,
            pr.project,
            rule.name,
            pr.hash,
            ord as i64,
            e.seq,
            p.card,
            lane,
            serde_json::to_string(rule)?,
            cause(&pr.project, &rule.name, &id, e).to_string(),
            state,
            reason,
            at,
        ],
    )?;
    Ok(())
}

/// Keep the newest [`KEEP_RUNS`] finished rows of `project`.
fn prune(t: &Tx<'_>, project: &str) -> ApiResult<()> {
    t.execute(
        "DELETE FROM rule_runs WHERE project = ?1 AND state != 'pending' AND id NOT IN (
             SELECT id FROM rule_runs WHERE project = ?1 AND state != 'pending'
              ORDER BY id DESC LIMIT ?2)",
        params![project, KEEP_RUNS],
    )?;
    Ok(())
}

// --------------------------------------------------------------- the run

/// One `rule_runs` row.
#[derive(Debug, Clone, Serialize)]
pub struct RunRow {
    pub id: String,
    pub project: String,
    pub rule: String,
    pub rule_hash: String,
    pub event_seq: i64,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub card: Option<String>,
    /// `pending`, `done`, `noop`, `failed`, or `skipped`.
    pub state: String,
    /// The next action (pending), or the actions run.
    pub step: i64,
    /// One result for each action run: `{verb, changed, detail}`.
    pub steps: Vec<Value>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub reason: Option<String>,
    pub created_at: String,
    pub updated_at: String,
    #[serde(skip)]
    pub rule_def: Option<Rule>,
    #[serde(skip)]
    pub cause: Value,
}

const RUN_COLS: &str = "id, project, rule, rule_hash, event_seq, card, state, step, steps,
                        reason, created_at, updated_at, rule_json, cause";

fn run_row(r: &rusqlite::Row<'_>) -> rusqlite::Result<RunRow> {
    let steps: String = r.get(8)?;
    let rule_json: String = r.get(12)?;
    let cause: String = r.get(13)?;
    Ok(RunRow {
        id: r.get(0)?,
        project: r.get(1)?,
        rule: r.get(2)?,
        rule_hash: r.get(3)?,
        event_seq: r.get(4)?,
        card: r.get(5)?,
        state: r.get(6)?,
        step: r.get(7)?,
        steps: serde_json::from_str(&steps).unwrap_or_default(),
        reason: r.get(9)?,
        created_at: r.get(10)?,
        updated_at: r.get(11)?,
        rule_def: serde_json::from_str(&rule_json).ok(),
        cause: serde_json::from_str(&cause).unwrap_or(Value::Null),
    })
}

fn get_run(conn: &Connection, id: &str) -> ApiResult<RunRow> {
    conn.query_row(
        &format!("SELECT {RUN_COLS} FROM rule_runs WHERE id = ?1"),
        [id],
        run_row,
    )
    .optional()?
    .ok_or_else(|| ApiError::not_found(format!("no rule run `{id}`")))
}

/// The run log of `project`, newest first; `rule` filters by rule.
pub fn list_runs(
    conn: &Connection,
    project: &str,
    rule: Option<&str>,
    limit: usize,
) -> ApiResult<Vec<RunRow>> {
    let mut st = conn.prepare(&format!(
        "SELECT {RUN_COLS} FROM rule_runs WHERE project = ?1 AND (?2 IS NULL OR rule = ?2)
          ORDER BY id DESC LIMIT ?3"
    ))?;
    let rows = st
        .query_map(params![project, rule, limit as i64], run_row)?
        .collect::<Result<Vec<_>, _>>()?;
    Ok(rows)
}

/// Run the pending rows (at most [`RUN_BATCH`]): the rows of one lane (a
/// card) in order, the lanes beside each other. Returns whether more rows
/// wait.
pub async fn run_pending(state: &Shared) -> ApiResult<bool> {
    let pending: Vec<(String, String)> = state
        .db
        .read(|c| {
            let mut st = c.prepare(
                "SELECT id, lane FROM rule_runs WHERE state = 'pending'
                  ORDER BY event_seq, ord, id LIMIT ?1",
            )?;
            let rows = st
                .query_map([RUN_BATCH as i64], |r| Ok((r.get(0)?, r.get(1)?)))?
                .collect::<Result<Vec<_>, _>>()?;
            Ok(rows)
        })
        .await?;
    let more = pending.len() == RUN_BATCH;
    let mut lanes: BTreeMap<String, Vec<String>> = BTreeMap::new();
    for (id, lane) in pending {
        lanes.entry(lane).or_default().push(id);
    }
    let tasks: Vec<JoinHandle<()>> = lanes
        .into_values()
        .map(|ids| {
            let st = state.clone();
            tokio::spawn(async move {
                for id in ids {
                    if let Err(e) = run_one(&st, &id).await {
                        warn!(run = %id, "a rule run failed to record: {e}");
                    }
                }
            })
        })
        .collect();
    for t in tasks {
        if let Err(e) = t.await {
            warn!("a rule lane task ended: {e}");
        }
    }
    Ok(more)
}

/// What one write of a run did.
enum Step {
    /// The run ended (or was not pending).
    Ended,
    /// An action ran; more may follow.
    Next,
    /// The action failed: its write rolled back. Record the failure.
    Failed {
        step: i64,
        verb: String,
        error: String,
    },
}

/// Run one pending row to its end: one write for each action.
pub async fn run_one(state: &Shared, id: &str) -> ApiResult<()> {
    let row = {
        let id = id.to_owned();
        state.db.read(move |c| get_run(c, &id)).await?
    };
    if row.state != "pending" {
        return Ok(());
    }
    let label = actor_label(&row.project, &row.rule);
    loop {
        let rid = row.id.clone();
        let cause = row.cause.clone();
        let step = state
            .db
            .write(label.clone(), move |t| {
                t.cause = Some(cause);
                step_once(t, &rid)
            })
            .await;
        let step = match step {
            Ok(s) => s,
            Err(e) => match action_failure(&e) {
                Some(f) => f,
                // An error outside an action (the database): the row
                // stays pending for the next pass.
                None => return Err(e),
            },
        };
        match step {
            Step::Ended => return Ok(()),
            Step::Next => {}
            Step::Failed { step, verb, error } => {
                let rid = row.id.clone();
                let cause = row.cause.clone();
                return state
                    .db
                    .write(label, move |t| {
                        t.cause = Some(cause);
                        fail_run(t, &rid, step, &verb, &error)
                    })
                    .await;
            }
        }
    }
}

/// The marker of an action that failed in [`step_once`]: its write
/// returns this error so that the action's changes roll back.
const ACTION_FAILED: &str = "rule action failed";

fn action_failure(e: &ApiError) -> Option<Step> {
    if e.message != ACTION_FAILED {
        return None;
    }
    let d = e.details.as_ref()?;
    Some(Step::Failed {
        step: d["rule_step"].as_i64()?,
        verb: d["verb"].as_str()?.to_owned(),
        error: d["error"].as_str()?.to_owned(),
    })
}

fn finish(t: &Tx<'_>, id: &str, state: &str, reason: Option<&str>) -> ApiResult<()> {
    t.execute(
        "UPDATE rule_runs SET state = ?2, reason = ?3, updated_at = ?4 WHERE id = ?1",
        params![id, state, reason, now()],
    )?;
    Ok(())
}

fn fail_run(t: &Tx<'_>, id: &str, step: i64, verb: &str, error: &str) -> ApiResult<()> {
    let row = get_run(t, id)?;
    if row.state != "pending" || row.step != step {
        return Ok(());
    }
    let mut steps = row.steps;
    steps.push(json!({"verb": verb, "ok": false, "error": error}));
    t.execute(
        "UPDATE rule_runs SET steps = ?2 WHERE id = ?1",
        params![id, Value::Array(steps).to_string()],
    )?;
    let reason = format!("action {} ({verb}) failed: {error}", step + 1);
    finish(t, id, "failed", Some(&reason))
}

/// The state that a run reads: the card, the board, the event, the job.
struct RunCtx {
    project: String,
    rule: String,
    event: Option<StoredEvent>,
    card: Option<board::Card>,
    columns: Vec<BoardColumn>,
    job: Option<jobs::Job>,
}

fn event_at(conn: &Connection, seq: i64) -> ApiResult<Option<StoredEvent>> {
    Ok(events::list(conn, &EventFilter::default(), seq - 1, 1)?
        .into_iter()
        .find(|e| e.seq == seq))
}

fn load_ctx(conn: &Connection, row: &RunRow) -> ApiResult<RunCtx> {
    let event = event_at(conn, row.event_seq)?;
    let card = match &row.card {
        Some(c) => board::get_card(conn, c).ok(),
        None => None,
    };
    let columns = board::column_roles(conn, &row.project)?
        .into_iter()
        .map(|(name, role)| BoardColumn { name, role })
        .collect();
    let job = match &event {
        Some(e) if e.kind == "job" => e.data["id"]
            .as_str()
            .and_then(|id| jobs::get(conn, id).ok()),
        _ => None,
    };
    Ok(RunCtx {
        project: row.project.clone(),
        rule: row.rule.clone(),
        event,
        card,
        columns,
        job,
    })
}

/// Why the run may not start: `Err(reason)` with `failed` true for a
/// missing column, false for a condition that does not hold.
fn check_start(rule: &Rule, row: &RunRow, ctx: &RunCtx) -> Result<(), (bool, String)> {
    for c in rules_file::column_refs(rule) {
        if rules_file::resolve_column(&ctx.columns, &c).is_none() {
            return Err((
                true,
                format!(
                    "column `{c}` is not on the board of project `{}` (the board changed after the rules check)",
                    ctx.project
                ),
            ));
        }
    }
    if let Some(card) = &row.card
        && ctx.card.is_none()
    {
        return Err((false, format!("card {card} is gone")));
    }
    if let Some(card) = &ctx.card {
        for (i, cond) in rule.if_card.iter().enumerate() {
            if let Some(why) = card_condition_fails(cond, card, &ctx.columns) {
                return Err((false, format!("if_card {} does not hold: {why}", i + 1)));
            }
        }
    }
    for (i, cond) in rule.if_job.iter().enumerate() {
        let Some(job) = &ctx.job else {
            return Err((false, format!("if_job {}: the event has no job", i + 1)));
        };
        let state = job.state.as_str();
        if !cond.state.is_empty() && !cond.state.iter().any(|s| s == state) {
            return Err((
                false,
                format!("if_job {} does not hold: the job is {state}", i + 1),
            ));
        }
        let outcome = job.outcome.as_deref().unwrap_or("none");
        if !cond.outcome.is_empty() && !cond.outcome.iter().any(|o| o == outcome) {
            return Err((
                false,
                format!("if_job {} does not hold: the outcome is {outcome}", i + 1),
            ));
        }
    }
    Ok(())
}

/// The first part of `cond` that does not hold for `card`, or `None`.
pub fn card_condition_fails(
    cond: &rules_file::CardCondition,
    card: &board::Card,
    columns: &[BoardColumn],
) -> Option<String> {
    let is_in = |refs: &[String]| {
        refs.iter()
            .any(|r| rules_file::resolve_column(columns, r).is_some_and(|c| c.name == card.column))
    };
    if !cond.column.is_empty() && !is_in(&cond.column) {
        return Some(format!("{} is in {}", card.id, card.column));
    }
    if is_in(&cond.not_column) {
        return Some(format!("{} is in {}", card.id, card.column));
    }
    if let Some(l) = cond.has_labels.iter().find(|l| !card.labels.contains(l)) {
        return Some(format!("{} has no label {l}", card.id));
    }
    if let Some(l) = cond.lacks_labels.iter().find(|l| card.labels.contains(l)) {
        return Some(format!("{} has the label {l}", card.id));
    }
    if let Some(a) = cond.assigned
        && a != card.assignee.is_some()
    {
        return Some(format!(
            "{} is {}assigned",
            card.id,
            if a { "not " } else { "" }
        ));
    }
    if let Some(a) = cond.archived
        && a != card.archived
    {
        return Some(format!(
            "{} is {}archived",
            card.id,
            if a { "not " } else { "" }
        ));
    }
    None
}

/// The value of a placeholder (`event.*`, `card.*`, `job.*`, `rule.name`).
fn lookup(ctx: &RunCtx, name: &str) -> Option<String> {
    if let Some(path) = name.strip_prefix("event.") {
        return ctx.event.as_ref().map(|e| event_view(e).text(path));
    }
    if let Some(f) = name.strip_prefix("card.") {
        let c = ctx.card.as_ref()?;
        return Some(match f {
            "id" => c.id.clone(),
            "title" => c.title.clone(),
            "column" => c.column.clone(),
            "assignee" => c.assignee_name.clone().or(c.assignee.clone())?,
            "labels" => c.labels.join(", "),
            "project" => c.project.clone(),
            _ => return None,
        });
    }
    if let Some(f) = name.strip_prefix("job.") {
        // The event's job, then the event's own fields (TER-167 adds the
        // verdict fields to the event).
        if let Some(j) = &ctx.job {
            let v = match f {
                "id" => Some(j.id.clone()),
                "kind" => Some(j.kind.as_str().to_owned()),
                "state" => Some(j.state.as_str().to_owned()),
                "outcome" => j.outcome.clone(),
                "attempt" => Some(j.attempt.to_string()),
                "repo" => Some(j.repo.clone()),
                "commit" => j.commit.clone(),
                _ => None,
            };
            if v.is_some() {
                return v;
            }
        }
        let e = ctx.event.as_ref()?;
        return e.data.get(f).filter(|v| !v.is_null()).map(value_text);
    }
    (name == "rule.name").then(|| ctx.rule.clone())
}

/// What an action did.
#[derive(Debug, Clone)]
pub struct Done {
    /// False: the action changed nothing (a no-op).
    pub changed: bool,
    pub detail: Value,
}

/// The verbs that run today. TER-169 adds the others.
pub const IMPLEMENTED: &[&str] = &["card.comment"];

/// Run one action as `actor`, in the run's write.
fn perform(t: &mut Tx<'_>, actor: &Actor, ctx: &RunCtx, action: &Action) -> ApiResult<Done> {
    let text = |key: &str| {
        action
            .args
            .get(key)
            .and_then(Value::as_str)
            .map(|s| rules_file::render(s, &|n| lookup(ctx, n), true))
    };
    match action.verb.as_str() {
        "card.comment" => {
            let card = ctx
                .card
                .as_ref()
                .ok_or_else(|| ApiError::invalid("card.comment needs an event with a card"))?;
            let body = text("text").unwrap_or_default();
            board::add_comment(t, actor, &card.id, &body)?;
            Ok(Done {
                changed: true,
                detail: json!({"card": card.id, "text": body}),
            })
        }
        other => Err(ApiError::invalid(format!(
            "verb `{other}` is not implemented yet (TER-169)"
        ))),
    }
}

/// One write of a run: at the first action, the start checks; then the
/// next action, with the step counter in the same write.
fn step_once(t: &mut Tx<'_>, id: &str) -> ApiResult<Step> {
    let row = get_run(t, id)?;
    if row.state != "pending" {
        return Ok(Step::Ended);
    }
    let Some(rule) = row.rule_def.clone() else {
        finish(t, id, "failed", Some("the planned rule does not read"))?;
        return Ok(Step::Ended);
    };
    let ctx = load_ctx(t, &row)?;
    if row.step == 0
        && let Err((failed, why)) = check_start(&rule, &row, &ctx)
    {
        finish(t, id, if failed { "failed" } else { "skipped" }, Some(&why))?;
        return Ok(Step::Ended);
    }
    let Some(action) = usize::try_from(row.step)
        .ok()
        .and_then(|i| rule.actions.get(i))
    else {
        end_run(t, &row)?;
        return Ok(Step::Ended);
    };
    let actor = rule_actor(&row.project, &row.rule);
    match perform(t, &actor, &ctx, action) {
        Ok(done) => {
            let mut steps = row.steps.clone();
            steps.push(
                json!({"verb": action.verb, "ok": true, "changed": done.changed,
                              "detail": done.detail}),
            );
            t.execute(
                "UPDATE rule_runs SET step = step + 1, steps = ?2, updated_at = ?3 WHERE id = ?1",
                params![id, Value::Array(steps).to_string(), now()],
            )?;
            let row = get_run(t, id)?;
            if usize::try_from(row.step).unwrap_or(usize::MAX) >= rule.actions.len() {
                end_run(t, &row)?;
                return Ok(Step::Ended);
            }
            Ok(Step::Next)
        }
        // Roll the action's write back; the caller records the failure.
        Err(e) => Err(ApiError::internal(ACTION_FAILED).with_details(json!({
            "rule_step": row.step,
            "verb": action.verb,
            "error": e.to_string(),
        }))),
    }
}

/// The end of a run whose actions all ran: `done`, or `noop` when none
/// changed anything.
fn end_run(t: &Tx<'_>, row: &RunRow) -> ApiResult<()> {
    let changed = row
        .steps
        .iter()
        .any(|s| s["changed"].as_bool().unwrap_or(true));
    finish(t, &row.id, if changed { "done" } else { "noop" }, None)
}

// ------------------------------------------------------------ the engine

/// One round: the pending rows of before, then a plan of the new events
/// and their runs, until the log after the cursor is read.
pub async fn pass(state: &Shared, cache: &mut Cache) {
    loop {
        if let Err(e) = drain_runs(state).await {
            warn!("the rule engine cannot run its outbox: {e}");
            return;
        }
        let projects = match load(&state.db, cache).await {
            Ok(p) => p,
            Err(e) => {
                warn!("the rule engine cannot read the rules: {e}");
                return;
            }
        };
        match plan_pass(&state.db, projects, Utc::now()).await {
            Ok((_, true)) => {}
            Ok((_, false)) => {
                if let Err(e) = drain_runs(state).await {
                    warn!("the rule engine cannot run its outbox: {e}");
                }
                return;
            }
            Err(e) => {
                warn!("the rule engine cannot read the event log: {e}");
                return;
            }
        }
    }
}

async fn drain_runs(state: &Shared) -> ApiResult<()> {
    while run_pending(state).await? {}
    Ok(())
}

/// The engine task: [`startup`], then a [`pass`] at each wake-up (a new
/// event on the hub, or every [`TICK`]).
pub fn spawn(state: Shared) -> JoinHandle<()> {
    let mut rx = state.db.hub().subscribe();
    tokio::spawn(async move {
        if let Err(e) = startup(&state.db).await {
            warn!("the rule engine cannot set its cursor: {e}");
        }
        let mut cache = Cache::default();
        loop {
            pass(&state, &mut cache).await;
            tokio::select! {
                () = tokio::time::sleep(TICK) => {}
                r = rx.recv() => {
                    if matches!(r, Err(tokio::sync::broadcast::error::RecvError::Closed)) {
                        rx = state.db.hub().subscribe();
                    }
                }
            }
            while let Ok(_) | Err(TryRecvError::Lagged(_)) = rx.try_recv() {}
        }
    })
}

// ----------------------------------------------------------- the dry run

/// What a dry run plans for one (event, rule, card).
#[derive(Debug, Clone, Serialize)]
pub struct DryPlan {
    pub event: i64,
    pub kind: String,
    pub rule: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub card: Option<String>,
    /// `run`, `skipped` (a guard or a condition), or `failed` (a column).
    pub outcome: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub reason: Option<String>,
    /// The actions with their arguments filled from the current state.
    /// Later actions see the state before the earlier ones run.
    pub actions: Vec<Value>,
    /// The actor of the actions.
    pub actor: String,
}

/// Plan the runs of `set` (the rules of `project`) for the events of
/// `project` with `seq` in `from..=to` (at most [`MAX_DRY_RUN_EVENTS`]),
/// on the current state. Reads only.
pub fn dry_run(
    conn: &Connection,
    project: &str,
    set: &RuleSet,
    from: i64,
    to: i64,
    now: DateTime<Utc>,
) -> ApiResult<(usize, Vec<DryPlan>)> {
    let filter = EventFilter {
        project: Some(project.to_owned()),
        ..Default::default()
    };
    let evs: Vec<StoredEvent> = events::list(conn, &filter, from - 1, MAX_DRY_RUN_EVENTS)?
        .into_iter()
        .filter(|e| e.seq <= to)
        .collect();
    let mut out = Vec::new();
    for e in &evs {
        for rule in &set.rules {
            for p in decide(conn, project, rule, e, None, now)? {
                let mut plan = DryPlan {
                    event: e.seq,
                    kind: e.kind.clone(),
                    rule: rule.name.clone(),
                    card: p.card.clone(),
                    outcome: "run".into(),
                    reason: None,
                    actions: Vec::new(),
                    actor: actor_label(project, &rule.name),
                };
                if let Some(skip) = p.skip {
                    plan.outcome = "skipped".into();
                    plan.reason = Some(skip);
                    out.push(plan);
                    continue;
                }
                let row = RunRow {
                    id: String::new(),
                    project: project.to_owned(),
                    rule: rule.name.clone(),
                    rule_hash: String::new(),
                    event_seq: e.seq,
                    card: p.card,
                    state: "pending".into(),
                    step: 0,
                    steps: Vec::new(),
                    reason: None,
                    created_at: String::new(),
                    updated_at: String::new(),
                    rule_def: None,
                    cause: Value::Null,
                };
                let ctx = load_ctx(conn, &row)?;
                if let Err((failed, why)) = check_start(rule, &row, &ctx) {
                    plan.outcome = if failed { "failed" } else { "skipped" }.into();
                    plan.reason = Some(why);
                    out.push(plan);
                    continue;
                }
                plan.actions = rule
                    .actions
                    .iter()
                    .map(|a| planned_action(a, &ctx))
                    .collect();
                out.push(plan);
            }
        }
    }
    Ok((evs.len(), out))
}

/// The arguments whose text an agent reads: filled as quoted data.
const QUOTED_ARGS: &[&str] = &["text", "comment", "subject", "body", "turn"];

fn planned_action(a: &Action, ctx: &RunCtx) -> Value {
    let fill = |key: &str, v: &Value| -> Value {
        let quoted = QUOTED_ARGS.contains(&key);
        let one = |s: &str| Value::String(rules_file::render(s, &|n| lookup(ctx, n), quoted));
        match v {
            Value::String(s) => one(s),
            Value::Array(items) => Value::Array(
                items
                    .iter()
                    .map(|i| i.as_str().map_or_else(|| i.clone(), one))
                    .collect(),
            ),
            other => other.clone(),
        }
    };
    let args: Map<String, Value> = a
        .args
        .iter()
        .map(|(k, v)| (k.clone(), fill(k, v)))
        .collect();
    json!({
        "verb": a.verb,
        "args": args,
        "implemented": IMPLEMENTED.contains(&a.verb.as_str()),
    })
}
