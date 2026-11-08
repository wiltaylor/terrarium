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
//!   and at most [`MAX_PER_CARD_PER_HOUR`] times for one card unless
//!   `max_per_hour` explicitly sets both limits. A guard
//!   writes a `skipped` row with the reason.
//! - **Per-card order.** [`run_pending`] runs the pending rows in event
//!   order, then rule order. The rows of one card (its lane) run one after
//!   another; other lanes run beside them. A row that cannot be recorded
//!   stops its lane until the next pass. A row of a rule that is disabled
//!   or gone since the plan is skipped when it has not started.
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
//! The verbs are in [`crate::rules_verbs`] (TER-169). What a verb cannot
//! do in its write (the queue tick, a kill, a wake, the close of a bound
//! agent) runs after the action's commit.

use std::collections::{BTreeMap, HashMap, HashSet};
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
use crate::rules_verbs::{self, Effect};
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
/// The kind of the event that the engine writes when the wait of a batch
/// rule ends (`job_queue` with `max_wait_minutes`, TER-172).
pub const TIMER_KIND: &str = "rule.timer";
/// The actor of a timer event: the engine.
const TIMER_ACTOR: &str = "system";

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
    /// The timers written ([`fire_timers`]): the due time of each
    /// (project, rule), so one wait writes one timer.
    fired: HashMap<(String, String), String>,
    /// When [`fire_timers`] last ran.
    timers_at: Option<std::time::Instant>,
}

/// One project for [`load`]: its rules file (hash, time) and its flow.
struct LoadRow {
    project: String,
    file: Option<(String, String)>,
    rules_flow: bool,
    flow_at: Option<String>,
}

/// The rules of every project with a rules file or with `board_flow =
/// rules` (TER-170). A project with `rules` runs the built-in pack with its
/// own rules ([`rules_file::with_pack`]); its runs start with the events
/// after the switch. Each changed file is parsed off the DB lock, without
/// the column check (the engine checks the columns when a run starts). A
/// file that does not parse (a stored text that a later schema refuses)
/// runs nothing of its own and logs a warning.
pub async fn load(db: &Db, cache: &mut Cache) -> ApiResult<Vec<ProjectRules>> {
    let rows: Vec<LoadRow> = db
        .read(|c| {
            let mut st = c.prepare(
                "SELECT p.name, r.hash, r.updated_at, p.board_flow, p.board_flow_at
                   FROM projects p LEFT JOIN project_rules r ON r.project = p.name
                  WHERE r.project IS NOT NULL OR p.board_flow = 'rules'
                  ORDER BY p.name",
            )?;
            let rows = st
                .query_map([], |r| {
                    let hash: Option<String> = r.get(1)?;
                    let at: Option<String> = r.get(2)?;
                    let flow: Option<String> = r.get(3)?;
                    Ok(LoadRow {
                        project: r.get(0)?,
                        file: hash.zip(at),
                        rules_flow: flow.as_deref() == Some("rules"),
                        flow_at: r.get(4)?,
                    })
                })?
                .collect::<Result<Vec<_>, _>>()?;
            Ok(rows)
        })
        .await?;
    cache
        .sets
        .retain(|p, _| rows.iter().any(|r| r.project == *p));
    let mut out = Vec::new();
    for row in rows {
        let LoadRow {
            project,
            file,
            rules_flow,
            flow_at,
        } = row;
        let file_hash = file.as_ref().map_or("-", |(h, _)| h.as_str());
        let hash = if rules_flow {
            format!("pack+{file_hash}")
        } else {
            file_hash.to_owned()
        };
        // An event older than the rules (or than the switch to the
        // rules) starts no run.
        let updated_at = [
            file.as_ref().map(|(_, at)| at.clone()),
            flow_at.filter(|_| rules_flow),
        ]
        .into_iter()
        .flatten()
        .max()
        .unwrap_or_default();
        let cached = cache
            .sets
            .get(&project)
            .filter(|(h, _)| *h == hash)
            .map(|(_, s)| s.clone());
        let set = match cached {
            Some(s) => s,
            None if file.is_none() => {
                let set = Arc::new(rules_file::with_pack(
                    rules_file::default_pack(),
                    &RuleSet::default(),
                ));
                cache
                    .sets
                    .insert(project.clone(), (hash.clone(), set.clone()));
                set
            }
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
                let own = match rules::parse_off(text, rules::file_path(&project), ctx).await? {
                    Ok(s) => s,
                    Err(e) => {
                        warn!(project = %project, "the stored rules do not parse; they run nothing: {e}");
                        RuleSet::default()
                    }
                };
                let set = Arc::new(if rules_flow {
                    rules_file::with_pack(rules_file::default_pack(), &own)
                } else {
                    own
                });
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
    if e.kind == TIMER_KIND {
        // A timer is for the rule that it names only, and only the engine
        // writes one; `on` and `when` do not apply to it (TER-172).
        if e.data["rule"].as_str() != Some(rule.name.as_str())
            || e.actor.as_deref() != Some(TIMER_ACTOR)
        {
            return Ok(Vec::new());
        }
    } else {
        let view = event_view(e);
        if !rule
            .on
            .iter()
            .any(|k| terrarium_core::schedule::glob(k, &e.kind))
            || !rule.when.iter().all(|m| m.holds(&view))
        {
            return Ok(Vec::new());
        }
    }
    if !rule.if_job.is_empty() && event_job(e).is_none() {
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
    let card_max = rule.max_per_hour.map_or(MAX_PER_CARD_PER_HOUR, i64::from);
    let mut out = Vec::new();
    for card in cards {
        let skip = if count_since(conn, project, &rule.name, None, &hour_ago)? >= max {
            Some(format!("rate limit: {max} runs in an hour"))
        } else if let Some(c) = &card
            && count_since(conn, project, &rule.name, Some(c), &hour_ago)? >= card_max
        {
            Some(format!(
                "card rate limit: {card_max} runs for {c} in an hour"
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
            prune(t, p, now)?;
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

/// Keep the newest [`KEEP_RUNS`] finished rows of `project`, and every row
/// of the last hour: the rate limits of [`decide`] count those.
fn prune(t: &Tx<'_>, project: &str, now: DateTime<Utc>) -> ApiResult<()> {
    let hour_ago = schedules::stamp(now - chrono::Duration::hours(1));
    t.execute(
        "DELETE FROM rule_runs WHERE project = ?1 AND state != 'pending' AND created_at < ?3
            AND id NOT IN (
             SELECT id FROM rule_runs WHERE project = ?1 AND state != 'pending'
              ORDER BY id DESC LIMIT ?2)",
        params![project, KEEP_RUNS, hour_ago],
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

/// The enabled rules of `projects`, as (project, rule): a pending run of
/// any other rule (disabled or gone since its plan) does not start.
pub fn live_rules(projects: &[ProjectRules]) -> HashSet<(String, String)> {
    projects
        .iter()
        .flat_map(|p| {
            p.set
                .rules
                .iter()
                .filter(|r| r.enabled)
                .map(|r| (p.project.clone(), r.name.clone()))
        })
        .collect()
}

/// Run the pending rows (at most [`RUN_BATCH`]): the rows of one lane (a
/// card) in order, the lanes beside each other. A row that cannot be
/// recorded (a database error) stops its lane, so the later rows of its
/// card wait for it. `live`: the enabled rules ([`live_rules`]). Returns
/// whether more rows wait and every lane went through (no error), so a
/// caller that loops does not spin on a failing row.
pub async fn run_pending(state: &Shared, live: Arc<HashSet<(String, String)>>) -> ApiResult<bool> {
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
    let tasks: Vec<JoinHandle<bool>> = lanes
        .into_values()
        .map(|ids| {
            let st = state.clone();
            let live = live.clone();
            tokio::spawn(async move {
                for id in ids {
                    if let Err(e) = run_one(&st, &id, &live).await {
                        // Keep the card's order: its later rows wait for
                        // this one (the next pass).
                        warn!(run = %id, "a rule run failed to record; its lane waits: {e}");
                        return false;
                    }
                }
                true
            })
        })
        .collect();
    let mut clean = true;
    for t in tasks {
        match t.await {
            Ok(ok) => clean &= ok,
            Err(e) => {
                warn!("a rule lane task ended: {e}");
                clean = false;
            }
        }
    }
    Ok(more && clean)
}

/// What one write of a run did.
enum Step {
    /// The run ended (or was not pending), with the effects of its last
    /// action.
    Ended(Vec<Effect>),
    /// An action ran, with its effects; more may follow.
    Next(Vec<Effect>),
    /// The action failed: its write rolled back. Record the failure.
    Failed {
        step: i64,
        verb: String,
        error: String,
    },
}

/// Run one pending row to its end: one write for each action.
pub async fn run_one(state: &Shared, id: &str, live: &HashSet<(String, String)>) -> ApiResult<()> {
    let row = {
        let id = id.to_owned();
        state.db.read(move |c| get_run(c, &id)).await?
    };
    if row.state != "pending" {
        return Ok(());
    }
    let label = actor_label(&row.project, &row.rule);
    let rule_live = live.contains(&(row.project.clone(), row.rule.clone()));
    loop {
        let rid = row.id.clone();
        let cause = row.cause.clone();
        let step = state
            .db
            .write(label.clone(), move |t| {
                t.cause = Some(cause);
                step_once(t, &rid, rule_live)
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
            Step::Ended(fx) => {
                rules_verbs::apply(state, fx).await;
                return Ok(());
            }
            Step::Next(fx) => rules_verbs::apply(state, fx).await,
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
/// Loaded again for each action, so it sees the earlier actions.
#[derive(Debug, Clone)]
pub struct RunCtx {
    pub project: String,
    pub rule: String,
    pub event: Option<StoredEvent>,
    pub card: Option<board::Card>,
    pub columns: Vec<BoardColumn>,
    /// The job of a `job` event (`data.id`) or a `job.verdict` event
    /// (`data.job`), as the store has it now.
    pub job: Option<jobs::Job>,
    /// The rerun of the job, when one is queued or ran.
    pub rerun: Option<jobs::Job>,
    /// The halves of the job, when it was split (left first).
    pub halves: Vec<jobs::Job>,
    pub merge_group_key: Option<String>,
}

impl RunCtx {
    /// The job as the run's card sees it: the card's branch, worker, and
    /// conflict in a batch ([`crate::jobs::card_view`]).
    pub fn job_view(&self) -> Option<jobs::Job> {
        let j = self.job.as_ref()?;
        let card = self.card.as_ref().map(|c| c.id.as_str());
        match j.batch.iter().find(|c| Some(c.card.as_str()) == card) {
            Some(c) => Some(crate::jobs::card_view(j, c)),
            None => Some(j.clone()),
        }
    }
}

fn event_at(conn: &Connection, seq: i64) -> ApiResult<Option<StoredEvent>> {
    Ok(
        events::list(conn, &EventFilter::default(), seq.saturating_sub(1), 1)?
            .into_iter()
            .find(|e| e.seq == seq),
    )
}

/// The job ID of a `job` event (`data.id`) or a `job.verdict` event
/// (`data.job`).
fn event_job(e: &StoredEvent) -> Option<&str> {
    match e.kind.as_str() {
        "job" => e.data["id"].as_str(),
        "job.verdict" => e.data["job"].as_str(),
        _ => None,
    }
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
    let job = event
        .as_ref()
        .and_then(event_job)
        .and_then(|id| jobs::get(conn, id).ok());
    let (rerun, halves) = match &job {
        Some(j) => (
            jobs::rerun_of(conn, j)?.and_then(|id| jobs::get(conn, &id).ok()),
            jobs::halves_of(conn, &j.id)?,
        ),
        None => (None, Vec::new()),
    };
    let merge_group_key = match &job {
        Some(j) if j.kind == jobs::JobKind::Merge => conn
            .query_row(
                "SELECT g.key FROM card_groups g JOIN group_members m ON m.group_card = g.card
             WHERE m.card IN (SELECT value FROM json_each(?1))
             AND g.project = ?2 AND g.key LIKE 'merge:%'
             AND (g.closed_at IS NULL OR g.closed_at >= ?3)
             ORDER BY g.rowid DESC LIMIT 1",
                rusqlite::params![
                    serde_json::to_string(&j.cards().iter().map(|c| &c.card).collect::<Vec<_>>())?,
                    j.project,
                    j.created_at
                ],
                |r| r.get(0),
            )
            .optional()?,
        _ => None,
    };
    Ok(RunCtx {
        project: row.project.clone(),
        rule: row.rule.clone(),
        event,
        card,
        columns,
        job,
        rerun,
        halves,
        merge_group_key,
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
        let merged = job
            .cards()
            .iter()
            .filter(|c| c.outcome.as_deref() != Some("conflict"))
            .count();
        let flags = [
            (
                "split",
                cond.split,
                job.parent.is_some(),
                "a half of a split batch",
            ),
            (
                "batch",
                cond.batch,
                merged >= 2,
                "a batch of two or more merged cards",
            ),
            ("rerun", cond.rerun, ctx.rerun.is_some(), "run again"),
            (
                "halves",
                cond.halves,
                !ctx.halves.is_empty(),
                "split in halves",
            ),
        ];
        for (name, want, have, what) in flags {
            if let Some(want) = want
                && want != have
            {
                return Err((
                    false,
                    format!(
                        "if_job {} does not hold: {name} = {want}, but the job {} {what}",
                        i + 1,
                        if have { "is" } else { "is not" }
                    ),
                ));
            }
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
pub(crate) fn lookup(ctx: &RunCtx, name: &str) -> Option<String> {
    if let Some(f) = name.strip_prefix("group.") {
        let g = ctx.card.as_ref()?.group.as_ref()?;
        return Some(match f {
            "id" => g.id.clone(),
            "key" => g.key.clone(),
            "title" => g.title.clone(),
            "column" => g.column.clone(),
            "status" => g.status.clone(),
            "done" => g.done.to_string(),
            "total" => g.total.to_string(),
            "eta_at" => g.eta_at.clone()?,
            _ => return None,
        });
    }
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
        // A `job.verdict` event says what the job was for its card (the
        // outcome of a card that conflicted in a batch, its worker): its
        // own fields first. Then the event's job, then the event's fields.
        // A raw field (rules_file::RAW_FIELDS, not quoted) comes from the
        // stored job only: any event source can write a `job.verdict`.
        let raw = rules_file::RAW_FIELDS.contains(&name);
        if let Some(e) = ctx
            .event
            .as_ref()
            .filter(|e| e.kind == "job.verdict" && !raw)
        {
            let key = if f == "id" { "job" } else { f };
            if let Some(v) = e.data.get(key).filter(|v| !v.is_null()) {
                return Some(value_text(v));
            }
        }
        if let (Some(j), Some(v)) = (&ctx.job, ctx.job_view()) {
            let next = || -> Option<String> {
                match (&ctx.rerun, ctx.halves.as_slice()) {
                    (Some(r), _) => Some(r.id.clone()),
                    (None, []) => None,
                    (None, h) => Some(
                        h.iter()
                            .map(|x| x.id.as_str())
                            .collect::<Vec<_>>()
                            .join(" "),
                    ),
                }
            };
            // The texts of the coded flow (TER-170), from its own code.
            let out = match f {
                "group_key" => Some(
                    ctx.merge_group_key
                        .clone()
                        .unwrap_or_else(|| format!("merge:{}", j.id)),
                ),
                "id" => Some(j.id.clone()),
                "kind" => Some(j.kind.as_str().to_owned()),
                "state" => Some(j.state.as_str().to_owned()),
                "outcome" => v.outcome.clone(),
                "attempt" => Some(j.attempt.to_string()),
                "repo" => Some(j.repo.clone()),
                "commit" => j.commit.clone(),
                "branch" => Some(v.branch.clone()),
                "by_agent" => v.by_agent.clone(),
                "next_jobs" => next(),
                "queue_comment" => Some(crate::jobs::queue_comment(j, &v.card)),
                "rerun_comment" => ctx
                    .rerun
                    .as_ref()
                    .map(|r| crate::jobs::rerun_comment(&v, r, !j.batch.is_empty())),
                "split_comment" => match ctx.halves.as_slice() {
                    [l, r] => Some(crate::jobs::split_comment(&v, l, r)),
                    _ => None,
                },
                "fail_comment" => Some(crate::jobs::fail_comment(j, &v)),
                "fail_mail" => Some(crate::jobs::fail_mail(&v)),
                "end_comment" => Some(crate::jobs::verdict_comment(&v)),
                "failure_block" => crate::jobs::verdict_failure(&v),
                _ => None,
            };
            if out.is_some() {
                return out;
            }
        }
        if raw {
            return None;
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

/// Whether `verb` runs: each verb of the schema does since TER-169.
pub fn implemented(verb: &str) -> bool {
    rules_file::VERBS.iter().any(|v| v.name == verb)
}

/// Run `action` of rule `rule` of `project` as `actor`, on the state of
/// event `event_seq` and `card`, in the caller's write, as the engine
/// runs one action, without the effects after the commit. For tests
/// that run a verb as another actor.
pub fn try_verb(
    t: &mut Tx<'_>,
    actor: &Actor,
    project: &str,
    rule: &str,
    event_seq: i64,
    card: Option<&str>,
    action: &Action,
) -> ApiResult<Done> {
    let row = RunRow {
        id: String::new(),
        project: project.to_owned(),
        rule: rule.to_owned(),
        rule_hash: String::new(),
        event_seq,
        card: card.map(str::to_owned),
        state: "pending".into(),
        step: 0,
        steps: Vec::new(),
        reason: None,
        created_at: String::new(),
        updated_at: String::new(),
        rule_def: None,
        cause: Value::Null,
    };
    let ctx = load_ctx(t, &row)?;
    Ok(rules_verbs::perform(t, actor, &ctx, action)?.0)
}

/// One write of a run: at the first action, the start checks; then the
/// next action, with the step counter in the same write. `rule_live`:
/// the rule is still stored and enabled; a run that has not started is
/// skipped when it is not (a run that started goes to its end).
fn step_once(t: &mut Tx<'_>, id: &str, rule_live: bool) -> ApiResult<Step> {
    let row = get_run(t, id)?;
    if row.state != "pending" {
        return Ok(Step::Ended(Vec::new()));
    }
    if row.step == 0 && !rule_live {
        finish(
            t,
            id,
            "skipped",
            Some("the rule is disabled or gone since the plan"),
        )?;
        return Ok(Step::Ended(Vec::new()));
    }
    let Some(rule) = row.rule_def.clone() else {
        finish(t, id, "failed", Some("the planned rule does not read"))?;
        return Ok(Step::Ended(Vec::new()));
    };
    let ctx = load_ctx(t, &row)?;
    if row.step == 0
        && let Err((failed, why)) = check_start(&rule, &row, &ctx)
    {
        finish(t, id, if failed { "failed" } else { "skipped" }, Some(&why))?;
        return Ok(Step::Ended(Vec::new()));
    }
    let Some(action) = usize::try_from(row.step)
        .ok()
        .and_then(|i| rule.actions.get(i))
    else {
        end_run(t, &row)?;
        return Ok(Step::Ended(Vec::new()));
    };
    let actor = rule_actor(&row.project, &row.rule);
    match rules_verbs::perform(t, &actor, &ctx, action) {
        Ok((done, effects)) => {
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
                return Ok(Step::Ended(effects));
            }
            Ok(Step::Next(effects))
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

// ------------------------------------------------------------ the timers

/// One batch rule that waits for a time: `job_queue` of kind `batch` with
/// `min_cards` and `max_wait_minutes`.
struct TimerRule {
    project: String,
    rule: String,
    from: String,
    limit: Option<usize>,
    wait: i64,
}

fn timer_rules(projects: &[ProjectRules]) -> Vec<TimerRule> {
    let mut out = Vec::new();
    for p in projects {
        for r in p.set.rules.iter().filter(|r| r.enabled) {
            for a in r.actions.iter().filter(|a| a.verb == "job.queue") {
                let arg = |k: &str| a.args.get(k).and_then(Value::as_i64);
                if a.args.get("kind").and_then(Value::as_str) != Some("batch") {
                    continue;
                }
                let (Some(_min_cards), Some(wait), Some(from)) = (
                    arg("min_cards"),
                    arg("max_wait_minutes"),
                    a.args.get("from").and_then(Value::as_str),
                ) else {
                    continue;
                };
                out.push(TimerRule {
                    project: p.project.clone(),
                    rule: r.name.clone(),
                    from: from.to_owned(),
                    limit: arg("limit").and_then(|n| usize::try_from(n).ok()),
                    wait,
                });
            }
        }
    }
    out
}

/// Write a `rule.timer` event ([`TIMER_KIND`]) for each batch rule of
/// `projects` whose wait ended: cards wait in its `from` column, and the
/// first one came `max_wait_minutes` before `now` or earlier. That holds
/// also when `min_cards` or more wait: no card move ran the batch then
/// (a hold released in place), or its run failed (TER-227). The event
/// plans a run of that rule only (no card), and the verb checks the time
/// again. `fired` holds the due time of
/// each timer written, so one wait writes one event (a restart may write
/// it once more; the verb is then a no-op or queues the batch, as it
/// should). An error of one rule (for example a board that cannot be
/// read) is logged, and the other rules and projects go on (TER-227
/// review). Returns the events written.
pub async fn fire_timers(
    db: &Db,
    projects: &[ProjectRules],
    fired: &mut HashMap<(String, String), String>,
    now: DateTime<Utc>,
) -> ApiResult<usize> {
    let mut written = fire_hourly(db, projects, now).await?;
    let timers = timer_rules(projects);
    fired.retain(|(p, r), _| timers.iter().any(|t| t.project == *p && t.rule == *r));
    for t in timers {
        match fire_timer(db, &t, fired, now).await {
            Ok(true) => written += 1,
            Ok(false) => {}
            Err(e) => warn!(
                "the rule engine cannot check the timer of rule {} of project {}: {e}",
                t.rule, t.project
            ),
        }
    }
    Ok(written)
}

/// One persisted hourly event per rules project, including across restarts.
/// There is no catch-up burst after downtime.
async fn fire_hourly(db: &Db, projects: &[ProjectRules], now: DateTime<Utc>) -> ApiResult<usize> {
    let hour = now.format("%Y-%m-%dT%H").to_string();
    let mut written = 0;
    for p in projects {
        if !p.set.rules.iter().any(|r| r.enabled && r.on.iter().any(|k| k == "rule.hourly")) {
            continue;
        }
        let (project, hour) = (p.project.clone(), hour.clone());
        written += db.write(TIMER_ACTOR, move |tx| {
            let exists: bool = tx.query_row(
                "SELECT EXISTS(SELECT 1 FROM events WHERE project = ?1 AND kind = 'rule.hourly' AND json_extract(data, '$.hour') = ?2)",
                rusqlite::params![project, hour], |r| r.get(0),
            )?;
            if exists { return Ok(0); }
            tx.emit(events::EventDraft::new("rule.hourly", "hourly board check", json!({"hour": hour})).project(project))?;
            Ok(1)
        }).await?;
    }
    Ok(written)
}

/// [`fire_timers`] for one rule: `true` when it wrote the timer.
async fn fire_timer(
    db: &Db,
    t: &TimerRule,
    fired: &mut HashMap<(String, String), String>,
    now: DateTime<Utc>,
) -> ApiResult<bool> {
    let (project, from, limit) = (t.project.clone(), t.from.clone(), t.limit);
    let pick = db
        .read(move |c| {
            let columns: Vec<BoardColumn> = board::column_roles(c, &project)?
                .into_iter()
                .map(|(name, role)| BoardColumn { name, role })
                .collect();
            let Some(col) = rules_file::resolve_column(&columns, &from) else {
                return Ok(None);
            };
            Ok(Some(rules_verbs::pick_batch(
                c, &project, &col.name, limit,
            )?))
        })
        .await?;
    let Some(pick) = pick.filter(|p| p.waiting > 0) else {
        return Ok(false);
    };
    let Some(due) = pick
        .first_since
        .map(|f| f + chrono::Duration::minutes(t.wait))
    else {
        return Ok(false);
    };
    if due > now {
        return Ok(false);
    }
    let due = schedules::stamp(due);
    let key = (t.project.clone(), t.rule.clone());
    if fired.get(&key) == Some(&due) {
        return Ok(false);
    }
    let (project, rule, waiting, d) =
        (t.project.clone(), t.rule.clone(), pick.waiting, due.clone());
    db.write(TIMER_ACTOR, move |tx| {
        let summary = format!("the wait of rule {rule} ended: {waiting} cards wait");
        tx.emit(
            events::EventDraft::new(
                TIMER_KIND,
                summary,
                json!({"rule": rule, "due": d, "waiting": waiting}),
            )
            .project(project),
        )?;
        Ok(())
    })
    .await?;
    fired.insert(key, due);
    Ok(true)
}

// ------------------------------------------------------------ the engine

/// One round: the pending rows of before, then a plan of the new events
/// and their runs, until the log after the cursor is read. The timers of
/// the batch rules are checked at most every [`TICK`].
pub async fn pass(state: &Shared, cache: &mut Cache) {
    let mut timers_checked = false;
    loop {
        let projects = match load(&state.db, cache).await {
            Ok(p) => p,
            Err(e) => {
                warn!("the rule engine cannot read the rules: {e}");
                return;
            }
        };
        if !timers_checked && cache.timers_at.is_none_or(|at| at.elapsed() >= TICK) {
            timers_checked = true;
            cache.timers_at = Some(std::time::Instant::now());
            if let Err(e) = state
                .db
                .write("rule:group-timeout", crate::store::groups::expire)
                .await
            {
                warn!("the rule engine cannot close expired groups: {e}");
            }
            if let Err(e) = fire_timers(&state.db, &projects, &mut cache.fired, Utc::now()).await {
                warn!("the rule engine cannot check the timers: {e}");
            }
        }
        let live = Arc::new(live_rules(&projects));
        if let Err(e) = drain_runs(state, &live).await {
            warn!("the rule engine cannot run its outbox: {e}");
            return;
        }
        match plan_pass(&state.db, projects, Utc::now()).await {
            Ok((_, true)) => {}
            Ok((_, false)) => {
                if let Err(e) = drain_runs(state, &live).await {
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

/// Run the pending rows until none wait, or until a lane stops on an
/// error (the next pass tries again, after the next wake-up).
async fn drain_runs(state: &Shared, live: &Arc<HashSet<(String, String)>>) -> ApiResult<()> {
    while run_pending(state, live.clone()).await? {}
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
    let evs: Vec<StoredEvent> =
        events::list(conn, &filter, from.saturating_sub(1), MAX_DRY_RUN_EVENTS)?
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

fn planned_action(a: &Action, ctx: &RunCtx) -> Value {
    let fill = |key: &str, v: &Value| -> Value {
        let quoted = rules_verbs::quoted(&a.verb, key);
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
        "implemented": implemented(&a.verb),
    })
}
