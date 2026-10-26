//! Schedules (docs/api.md §19): the rows, the time math, and the run log.
//!
//! A schedule runs one action (mail, a turn, an exec, an agent verb, or a
//! quota probe) on a cron, interval, or one-time spec. It runs as the
//! actor in `run_as`, and the daemon checks that actor's rights again at
//! each run (`crate::api::schedules`). The tick that finds the due
//! schedules is `crate::scheduler`.
//!
//! Times are stored as RFC 3339 UTC with milliseconds ([`stamp`]), so a
//! text compare orders them.

use std::str::FromStr;

use chrono::{DateTime, SecondsFormat, TimeZone, Utc};
use rusqlite::{Connection, OptionalExtension, Row, params};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use terrarium_core::schedule::{CatchUp, ScheduleAction, ScheduleDef, ScheduleSpec};

use crate::db::Tx;
use crate::error::{ApiError, ApiResult, Code};
use crate::events::EventDraft;
use crate::store::mail::{self, Address, Outgoing};
use crate::store::{Page, Paged, agents, projects, subscriptions, targets, valid_name};

/// The shortest interval between two runs, in seconds. A five-field cron
/// expression cannot go under it.
pub const MIN_INTERVAL_SECS: i64 = 60;
/// The run limit of an `exec` action without `timeout_secs`.
pub const DEFAULT_EXEC_TIMEOUT: u64 = 300;
/// The highest `timeout_secs` of an `exec` action.
pub const MAX_EXEC_TIMEOUT: u64 = 3600;
/// The runs of each schedule that the daily prune keeps.
pub const KEEP_RUNS: usize = 500;
/// The rights checks in a row that skip a run before the schedule is
/// disabled.
pub const MAX_SKIPS: i64 = 3;
/// `TERRARIUM_MAX_SCHEDULES_PER_PROJECT`: the most schedules of one
/// project. A create past it is `409 conflict`.
pub const ENV_MAX_SCHEDULES: &str = "TERRARIUM_MAX_SCHEDULES_PER_PROJECT";
/// The default of [`ENV_MAX_SCHEDULES`].
pub const DEFAULT_MAX_SCHEDULES: usize = 20;

/// The cap: [`ENV_MAX_SCHEDULES`], else [`DEFAULT_MAX_SCHEDULES`]. Read
/// once.
pub fn max_per_project() -> usize {
    static MAX: std::sync::OnceLock<usize> = std::sync::OnceLock::new();
    *MAX.get_or_init(|| {
        std::env::var(ENV_MAX_SCHEDULES)
            .ok()
            .and_then(|v| v.trim().parse().ok())
            .unwrap_or(DEFAULT_MAX_SCHEDULES)
    })
}

/// A time as stored.
pub fn stamp(t: DateTime<Utc>) -> String {
    t.to_rfc3339_opts(SecondsFormat::Millis, true)
}

fn parse_time(s: &str) -> Option<DateTime<Utc>> {
    DateTime::parse_from_rfc3339(s)
        .ok()
        .map(|t| t.with_timezone(&Utc))
}

/// A `422 invalid` that names the field: the message starts with it, and
/// `details.field` holds it.
pub fn field_error(field: &str, msg: impl std::fmt::Display) -> ApiError {
    ApiError::invalid(format!("{field}: {msg}")).with_details(json!({"field": field}))
}

// ---------------------------------------------------------------- the spec

/// Standard cron numbers the days of the week 0-7 (0 and 7 are Sunday);
/// the `cron` crate numbers them 1-7 from Sunday. Names stay as they are.
fn dow_number(s: &str) -> Result<Option<u32>, String> {
    if s.chars().all(|c| c.is_ascii_digit()) && !s.is_empty() {
        let n: u32 = s.parse().map_err(|_| format!("bad day `{s}`"))?;
        if n > 7 {
            return Err(format!("day of week {n} is not 0-7"));
        }
        return Ok(Some(if n == 0 || n == 7 { 1 } else { n + 1 }));
    }
    Ok(None)
}

fn convert_dow(field: &str) -> Result<String, String> {
    let mut out = Vec::new();
    for item in field.split(',') {
        let (base, step) = match item.split_once('/') {
            Some((b, s)) => (b, Some(s)),
            None => (item, None),
        };
        let step_txt = step.map(|s| format!("/{s}")).unwrap_or_default();
        if base == "*" || base == "?" {
            out.push(format!("{base}{step_txt}"));
            continue;
        }
        match base.split_once('-') {
            Some((a, b)) => {
                let na = dow_number(a)?;
                let raw_b: Option<u32> = b.parse().ok();
                let nb = dow_number(b)?;
                let a_txt = na.map_or_else(|| a.to_owned(), |n| n.to_string());
                if raw_b == Some(7) && na.is_some() {
                    // `5-7` is Friday to Sunday: Sunday is 1 in the crate.
                    let start = na.unwrap_or(1);
                    if start == 1 {
                        out.push(format!("1-7{step_txt}"));
                    } else if step.is_some() {
                        return Err(format!(
                            "write `{item}` without 7 (Sunday is 0), for example `{a}-6/..,0`"
                        ));
                    } else {
                        out.push(format!("{start}-7"));
                        out.push("1".into());
                    }
                    continue;
                }
                let b_txt = nb.map_or_else(|| b.to_owned(), |n| n.to_string());
                out.push(format!("{a_txt}-{b_txt}{step_txt}"));
            }
            None => {
                let n = dow_number(base)?;
                out.push(format!(
                    "{}{step_txt}",
                    n.map_or_else(|| base.to_owned(), |n| n.to_string())
                ));
            }
        }
    }
    Ok(out.join(","))
}

/// Parse a five-field cron expression, or a shorthand (`@hourly`,
/// `@daily`, `@weekly`, `@monthly`, `@yearly`). The error says why.
pub fn parse_cron(expr: &str) -> Result<cron::Schedule, String> {
    let expr = expr.trim();
    if expr.starts_with('@') {
        return match expr {
            "@hourly" | "@daily" | "@weekly" | "@monthly" | "@yearly" => {
                cron::Schedule::from_str(expr).map_err(|e| e.to_string())
            }
            "@midnight" => cron::Schedule::from_str("@daily").map_err(|e| e.to_string()),
            "@annually" => cron::Schedule::from_str("@yearly").map_err(|e| e.to_string()),
            _ => Err("the shorthands are @hourly, @daily, @weekly, @monthly, and @yearly".into()),
        };
    }
    let fields: Vec<&str> = expr.split_whitespace().collect();
    if fields.len() != 5 {
        return Err(format!(
            "use five fields (minute hour day-of-month month day-of-week), not {}",
            fields.len()
        ));
    }
    let dow = convert_dow(fields[4])?;
    let full = format!(
        "0 {} {} {} {} {}",
        fields[0], fields[1], fields[2], fields[3], dow
    );
    cron::Schedule::from_str(&full).map_err(|e| e.to_string())
}

/// Parse an IANA zone (`UTC`, `Europe/London`).
pub fn parse_tz(tz: &str) -> Result<chrono_tz::Tz, String> {
    chrono_tz::Tz::from_str(tz.trim()).map_err(|_| format!("unknown time zone `{tz}`"))
}

/// Parse an interval: numbers with `s`, `m`, `h`, or `d`, for example
/// `90s`, `30m`, `1h30m`. Returns seconds.
pub fn parse_every(s: &str) -> Result<i64, String> {
    let s = s.trim();
    if s.is_empty() {
        return Err("the interval is empty".into());
    }
    let mut total: i64 = 0;
    let mut num = String::new();
    for c in s.chars() {
        if c.is_ascii_digit() {
            num.push(c);
            continue;
        }
        let unit = match c {
            's' => 1,
            'm' => 60,
            'h' => 3600,
            'd' => 86_400,
            _ => return Err(format!("bad interval `{s}`: use s, m, h, or d, as `30m`")),
        };
        let n: i64 = num
            .parse()
            .map_err(|_| format!("bad interval `{s}`: a number goes before each unit"))?;
        total = total.saturating_add(n.saturating_mul(unit));
        num.clear();
    }
    if !num.is_empty() {
        return Err(format!("bad interval `{s}`: give a unit, as `{num}m`"));
    }
    Ok(total)
}

/// Check a spec. `now` is the time of the write: an `at` before it is an
/// error.
pub fn validate_spec(spec: &ScheduleSpec, now: DateTime<Utc>) -> ApiResult<()> {
    match spec {
        ScheduleSpec::Cron { expr, tz } => {
            parse_cron(expr).map_err(|e| {
                field_error("spec.expr", format!("bad cron expression `{expr}`: {e}"))
            })?;
            parse_tz(tz).map_err(|e| field_error("spec.tz", e))?;
        }
        ScheduleSpec::Interval { every } => {
            let secs = parse_every(every).map_err(|e| field_error("spec.every", e))?;
            if secs < MIN_INTERVAL_SECS {
                return Err(field_error(
                    "spec.every",
                    format!("`{every}` is under the shortest interval, {MIN_INTERVAL_SECS} s"),
                ));
            }
        }
        ScheduleSpec::At { at } => {
            let t = parse_time(at)
                .ok_or_else(|| field_error("spec.at", format!("`{at}` is not an RFC 3339 time")))?;
            if t <= now {
                return Err(field_error("spec.at", format!("`{at}` is in the past")));
            }
        }
    }
    Ok(())
}

/// The first run of `spec` after `after`. `anchor` is the start of an
/// interval: its runs are `anchor + k * every`, k >= 1. `None`: no more
/// runs (an `at` in the past).
pub fn next_after(
    spec: &ScheduleSpec,
    anchor: DateTime<Utc>,
    after: DateTime<Utc>,
) -> ApiResult<Option<DateTime<Utc>>> {
    match spec {
        ScheduleSpec::Cron { expr, tz } => {
            let sched = parse_cron(expr).map_err(|e| {
                field_error("spec.expr", format!("bad cron expression `{expr}`: {e}"))
            })?;
            let zone = parse_tz(tz).map_err(|e| field_error("spec.tz", e))?;
            let local = zone.from_utc_datetime(&after.naive_utc());
            Ok(sched.after(&local).next().map(|t| t.with_timezone(&Utc)))
        }
        ScheduleSpec::Interval { every } => {
            let secs = parse_every(every)
                .map_err(|e| field_error("spec.every", e))?
                .max(MIN_INTERVAL_SECS);
            let gone = (after - anchor).num_milliseconds();
            let step = secs * 1000;
            let k = if gone < 0 { 1 } else { gone / step + 1 };
            Ok(Some(anchor + chrono::Duration::milliseconds(k * step)))
        }
        ScheduleSpec::At { at } => Ok(parse_time(at).filter(|t| *t > after)),
    }
}

// --------------------------------------------------------------- the rows

/// The last run of a schedule, on the schedule.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct LastRun {
    pub at: String,
    /// `ok`, `failed`, or `skipped`.
    pub status: String,
    pub summary: String,
    pub run_id: String,
}

/// A schedule as the API shows it.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct Schedule {
    pub id: String,
    pub name: String,
    /// `None`: a global schedule.
    pub project: Option<String>,
    pub enabled: bool,
    pub spec: ScheduleSpec,
    pub action: ScheduleAction,
    pub catch_up: CatchUp,
    pub max_concurrency: u32,
    /// `operator` or `agent:<ulid>`.
    pub created_by: String,
    /// The actor the runs act as. The daemon checks its rights at each run.
    pub run_as: String,
    pub next_run_at: Option<String>,
    pub last_run: Option<LastRun>,
    pub runs_count: i64,
    /// `api`, `file`, or `fleet:<name>`.
    pub source: String,
    /// Rights checks in a row that skipped a run.
    pub consecutive_skips: i64,
    /// Why the daemon disabled the schedule, when it did.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub disabled_reason: Option<String>,
    pub created_at: String,
    pub updated_at: String,
    #[serde(skip)]
    pub anchor_at: String,
}

impl Schedule {
    /// `project/name`, or the name of a global schedule.
    pub fn key(&self) -> String {
        match &self.project {
            Some(p) => format!("{p}/{}", self.name),
            None => self.name.clone(),
        }
    }

    fn anchor(&self) -> DateTime<Utc> {
        parse_time(&self.anchor_at).unwrap_or_else(Utc::now)
    }

    /// The definition (for a fleet export and the fleet plan).
    pub fn def(&self) -> ScheduleDef {
        ScheduleDef {
            name: self.name.clone(),
            spec: self.spec.clone(),
            action: self.action.clone(),
            enabled: self.enabled,
            catch_up: self.catch_up,
        }
    }
}

fn row(r: &Row<'_>) -> rusqlite::Result<(Schedule, String, String, Option<String>, String)> {
    let s = Schedule {
        id: r.get("id")?,
        name: r.get("name")?,
        project: r.get("project")?,
        enabled: r.get("enabled")?,
        spec: ScheduleSpec::At { at: String::new() },
        action: ScheduleAction::Probe {
            subscription: String::new(),
        },
        catch_up: CatchUp::Skip,
        max_concurrency: r.get("max_concurrency")?,
        created_by: r.get("created_by")?,
        run_as: r.get("run_as")?,
        next_run_at: r.get("next_run_at")?,
        last_run: None,
        runs_count: r.get("runs_count")?,
        source: r.get("source")?,
        consecutive_skips: r.get("skips")?,
        disabled_reason: r.get("disabled_reason")?,
        created_at: r.get("created_at")?,
        updated_at: r.get("updated_at")?,
        anchor_at: r.get("anchor_at")?,
    };
    Ok((
        s,
        r.get("spec")?,
        r.get("action")?,
        r.get("last_run")?,
        r.get("catch_up")?,
    ))
}

fn finish_row(
    (mut s, spec, action, last, catch_up): (Schedule, String, String, Option<String>, String),
) -> ApiResult<Schedule> {
    s.spec = serde_json::from_str(&spec)?;
    s.action = serde_json::from_str(&action)?;
    s.last_run = last.and_then(|l| serde_json::from_str(&l).ok());
    s.catch_up = if catch_up == "once" {
        CatchUp::Once
    } else {
        CatchUp::Skip
    };
    Ok(s)
}

fn query(conn: &Connection, sql: &str, args: &[&dyn rusqlite::ToSql]) -> ApiResult<Vec<Schedule>> {
    let mut stmt = conn.prepare(sql)?;
    let rows = stmt.query_map(args, row)?.collect::<Result<Vec<_>, _>>()?;
    rows.into_iter().map(finish_row).collect()
}

pub fn get(conn: &Connection, id: &str) -> ApiResult<Schedule> {
    query(conn, "SELECT * FROM schedules WHERE id = ?1", &[&id])?
        .pop()
        .ok_or_else(|| ApiError::not_found(format!("no schedule `{id}`")))
}

/// The schedule `name` in `project` (`None`: global).
pub fn find_in(
    conn: &Connection,
    project: Option<&str>,
    name: &str,
) -> ApiResult<Option<Schedule>> {
    let scope = project.unwrap_or_default();
    Ok(query(
        conn,
        "SELECT * FROM schedules WHERE scope = ?1 AND name = ?2",
        &[&scope, &name],
    )?
    .pop())
}

/// Find a schedule by ULID, `project/name`, or a name: in `context` first,
/// then among the global schedules.
pub fn find(conn: &Connection, key: &str, context: Option<&str>) -> ApiResult<Schedule> {
    let key = key.trim();
    if key.parse::<ulid::Ulid>().is_ok()
        && let Ok(s) = get(conn, key)
    {
        return Ok(s);
    }
    let found = match key.split_once('/') {
        Some((p, n)) => find_in(conn, Some(p), n)?,
        None => match context {
            Some(p) => match find_in(conn, Some(p), key)? {
                Some(s) => Some(s),
                None => find_in(conn, None, key)?,
            },
            None => find_in(conn, None, key)?,
        },
    };
    found.ok_or_else(|| ApiError::not_found(format!("no schedule `{key}`")))
}

/// The filters of a list.
#[derive(Debug, Clone, Default)]
pub struct ListFilter {
    /// Only this project's schedules (and the global ones with `global`).
    pub project: Option<String>,
    /// With `project`: the global schedules too. Without: only the global
    /// schedules when no `projects` list is given.
    pub global: bool,
    /// The projects a caller may read; `None` means all.
    pub projects: Option<Vec<String>>,
    pub enabled: Option<bool>,
}

/// Schedules, global first, then by project and name.
pub fn list(conn: &Connection, f: &ListFilter) -> ApiResult<Vec<Schedule>> {
    let all = query(conn, "SELECT * FROM schedules ORDER BY scope, name", &[])?;
    Ok(all
        .into_iter()
        .filter(|s| match (&s.project, &f.project) {
            (None, _) => f.global,
            (Some(p), Some(want)) => p == want,
            (Some(p), None) => f.projects.as_ref().is_none_or(|ps| ps.contains(p)),
        })
        .filter(|s| f.enabled.is_none_or(|e| e == s.enabled))
        .collect())
}

/// The schedules of a project, for a fleet export.
pub fn of_project(conn: &Connection, project: &str) -> ApiResult<Vec<Schedule>> {
    query(
        conn,
        "SELECT * FROM schedules WHERE project = ?1 ORDER BY name",
        &[&project],
    )
}

/// Enabled schedules with `next_run_at` at or before `now`.
pub fn due(conn: &Connection, now: DateTime<Utc>) -> ApiResult<Vec<Schedule>> {
    let now = stamp(now);
    query(
        conn,
        "SELECT * FROM schedules WHERE enabled = 1 AND next_run_at IS NOT NULL
           AND next_run_at <= ?1 ORDER BY next_run_at, id",
        &[&now],
    )
}

/// Enabled schedules whose next run is before `before`: at a daemon start,
/// the runs it missed while it was down.
pub fn missed(conn: &Connection, before: DateTime<Utc>) -> ApiResult<Vec<Schedule>> {
    let before = stamp(before);
    query(
        conn,
        "SELECT * FROM schedules WHERE enabled = 1 AND next_run_at IS NOT NULL
           AND next_run_at < ?1 ORDER BY next_run_at, id",
        &[&before],
    )
}

// ------------------------------------------------- what an action reaches

/// What an action touches, resolved in the schedule's project. The policy
/// (`policy::manage_schedule`) checks it against the actor.
#[derive(Debug, Clone, PartialEq)]
pub enum Reach {
    Mail(Address),
    Agent {
        id: String,
        name: String,
        project: String,
    },
    Target {
        name: String,
        project: Option<String>,
    },
    Subscription {
        name: String,
        /// The projects whose agents use the subscription.
        projects: Vec<String>,
    },
}

impl Reach {
    /// A readable name of the object: for summaries.
    pub fn display(&self) -> String {
        match self {
            Self::Mail(a) => a.display(),
            Self::Agent { name, project, .. } => format!("{project}/{name}"),
            Self::Target { name, .. } | Self::Subscription { name, .. } => name.clone(),
        }
    }
}

fn not_found_as_field(e: ApiError, field: &str) -> ApiError {
    if e.code == Code::NotFound {
        field_error(field, e.message)
    } else {
        e
    }
}

/// Check the fields of an action and find what it touches. `project` is
/// the schedule's project: bare agent names resolve there.
pub fn resolve_reach(
    conn: &Connection,
    project: Option<&str>,
    action: &ScheduleAction,
) -> ApiResult<Reach> {
    let agent = |key: &str| -> ApiResult<Reach> {
        if key.trim().is_empty() {
            return Err(field_error("action.agent", "name an agent"));
        }
        let a =
            agents::find(conn, key, project).map_err(|e| not_found_as_field(e, "action.agent"))?;
        Ok(Reach::Agent {
            id: a.agent.id.to_string(),
            name: a.agent.name,
            project: a.agent.project,
        })
    };
    match action {
        ScheduleAction::Mail { to, body, .. } => {
            if body.trim().is_empty() {
                return Err(field_error("action.body", "the mail body is empty"));
            }
            if to.trim().is_empty() {
                return Err(field_error("action.to", "name an address"));
            }
            let a =
                mail::resolve(conn, to, project).map_err(|e| not_found_as_field(e, "action.to"))?;
            if a == Address::System {
                return Err(field_error("action.to", "`system` only sends mail"));
            }
            Ok(Reach::Mail(a))
        }
        ScheduleAction::Turn {
            agent: key, text, ..
        } => {
            if text.trim().is_empty() {
                return Err(field_error("action.text", "the turn text is empty"));
            }
            agent(key)
        }
        ScheduleAction::Agent { agent: key, .. } => agent(key),
        ScheduleAction::Exec {
            target,
            cmd,
            timeout_secs,
            cwd,
        } => {
            if cmd.is_empty() || cmd[0].trim().is_empty() {
                return Err(field_error("action.cmd", "the command is empty"));
            }
            if let Some(t) = timeout_secs
                && (*t == 0 || *t > MAX_EXEC_TIMEOUT)
            {
                return Err(field_error(
                    "action.timeout_secs",
                    format!("{t} is not 1-{MAX_EXEC_TIMEOUT}"),
                ));
            }
            if let Some(c) = cwd
                && !c.starts_with('/')
            {
                return Err(field_error("action.cwd", "use an absolute path"));
            }
            let row = targets::find(conn, target)?
                .ok_or_else(|| field_error("action.target", format!("no target `{target}`")))?;
            Ok(Reach::Target {
                name: row.def.name,
                project: row.def.project,
            })
        }
        ScheduleAction::Probe { subscription } => {
            let s = subscriptions::find(conn, subscription)?.ok_or_else(|| {
                field_error(
                    "action.subscription",
                    format!("no subscription `{subscription}`"),
                )
            })?;
            let mut stmt = conn.prepare(
                "SELECT DISTINCT project FROM agents WHERE subscription = ?1 ORDER BY project",
            )?;
            let projects = stmt
                .query_map([&s.name], |r| r.get::<_, String>(0))?
                .collect::<Result<Vec<_>, _>>()?;
            Ok(Reach::Subscription {
                name: s.name,
                projects,
            })
        }
    }
}

// ------------------------------------------------------------- the writes

fn emit(t: &mut Tx<'_>, change: &str, s: &Schedule, run: Option<&Run>) -> ApiResult<()> {
    let mut data = json!({
        "change": change,
        "schedule": s.id,
        "name": s.name,
        "project": s.project,
    });
    let mut summary = format!("schedule {}: {change}", s.key());
    if let Some(r) = run {
        data["run"] = json!(r.id);
        data["status"] = json!(r.status);
        data["trigger"] = json!(r.trigger);
        if change == "run_finished" {
            summary = format!("schedule {}: {} ({})", s.key(), r.status, r.summary);
        }
    }
    let mut draft = EventDraft::new("schedule", summary, data);
    if let Some(p) = &s.project {
        draft = draft.project(p.clone());
    }
    t.emit(draft)?;
    Ok(())
}

/// A new schedule.
#[derive(Debug, Clone)]
pub struct NewSchedule {
    pub name: String,
    pub project: Option<String>,
    pub spec: ScheduleSpec,
    pub action: ScheduleAction,
    pub enabled: bool,
    pub catch_up: CatchUp,
    /// `operator` or `agent:<ulid>`; also the `run_as`.
    pub created_by: String,
    /// `api`, `file`, or `fleet:<name>`.
    pub source: String,
}

/// The check that every write of a name does.
pub fn check_name(name: &str) -> ApiResult<()> {
    if valid_name(name) {
        Ok(())
    } else {
        Err(field_error(
            "name",
            format!(
                "bad schedule name `{name}`: use 1-63 of a-z, 0-9, `-`, `_`, starting with a letter or digit"
            ),
        ))
    }
}

/// Write a new schedule. The caller checked the rights.
pub fn create(t: &mut Tx<'_>, n: &NewSchedule, now: DateTime<Utc>) -> ApiResult<Schedule> {
    check_name(&n.name)?;
    validate_spec(&n.spec, now)?;
    if let Some(p) = &n.project {
        projects::get(t, p).map_err(|e| not_found_as_field(e, "project"))?;
        let count: i64 = t.query_row(
            "SELECT count(*) FROM schedules WHERE project = ?1",
            [p],
            |r| r.get(0),
        )?;
        let max = max_per_project();
        if usize::try_from(count).unwrap_or(usize::MAX) >= max {
            return Err(ApiError::conflict(format!(
                "project `{p}` has {count} schedules; the most is {max} ({ENV_MAX_SCHEDULES})"
            )));
        }
    }
    if find_in(t, n.project.as_deref(), &n.name)?.is_some() {
        return Err(ApiError::conflict(format!(
            "schedule `{}` exists already in {}",
            n.name,
            n.project.as_deref().map_or_else(
                || "the global scope".to_owned(),
                |p| format!("project `{p}`")
            )
        )));
    }
    let id = ulid::Ulid::new().to_string();
    let at = stamp(now);
    let next = if n.enabled {
        next_after(&n.spec, now, now)?.map(stamp)
    } else {
        None
    };
    t.execute(
        "INSERT INTO schedules (id, name, project, scope, enabled, spec, action, catch_up,
                                max_concurrency, created_by, run_as, anchor_at, next_run_at,
                                source, created_at, updated_at)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, 1, ?9, ?9, ?10, ?11, ?12, ?10, ?10)",
        params![
            id,
            n.name,
            n.project,
            n.project.clone().unwrap_or_default(),
            n.enabled,
            serde_json::to_string(&n.spec)?,
            serde_json::to_string(&n.action)?,
            n.catch_up.as_str(),
            n.created_by,
            at,
            next,
            n.source,
        ],
    )?;
    let s = get(t, &id)?;
    emit(t, "created", &s, None)?;
    t.audit(
        "schedule.create",
        &s.key(),
        s.project.as_deref(),
        json!({"id": s.id, "spec": s.spec, "action": s.action.kind(), "source": s.source}),
    )?;
    Ok(s)
}

/// The fields a `PATCH` may change.
#[derive(Debug, Clone, Default)]
pub struct SchedulePatch {
    pub name: Option<String>,
    pub spec: Option<ScheduleSpec>,
    pub action: Option<ScheduleAction>,
    pub enabled: Option<bool>,
    pub catch_up: Option<CatchUp>,
    /// A new `run_as`: the caller that changed the action.
    pub run_as: Option<String>,
    pub source: Option<String>,
}

/// Change a schedule. The caller checked the rights. A new spec starts a
/// new interval from `now`.
pub fn update(
    t: &mut Tx<'_>,
    id: &str,
    p: &SchedulePatch,
    now: DateTime<Utc>,
) -> ApiResult<Schedule> {
    let old = get(t, id)?;
    let mut s = old.clone();
    let mut changed = Vec::new();
    if let Some(n) = p.name.as_ref().filter(|n| **n != old.name) {
        check_name(n)?;
        if find_in(t, old.project.as_deref(), n)?.is_some() {
            return Err(ApiError::conflict(format!("schedule `{n}` exists already")));
        }
        s.name = n.clone();
        changed.push("name");
    }
    let mut new_anchor = false;
    if let Some(spec) = p.spec.as_ref().filter(|x| **x != old.spec) {
        validate_spec(spec, now)?;
        s.spec = spec.clone();
        new_anchor = true;
        changed.push("spec");
    }
    if let Some(a) = p.action.as_ref().filter(|x| **x != old.action) {
        s.action = a.clone();
        changed.push("action");
        if let Some(r) = &p.run_as {
            s.run_as = r.clone();
        }
    }
    if let Some(c) = p.catch_up.filter(|x| *x != old.catch_up) {
        s.catch_up = c;
        changed.push("catch_up");
    }
    if let Some(e) = p.enabled.filter(|x| *x != old.enabled) {
        s.enabled = e;
        changed.push("enabled");
    }
    if let Some(src) = &p.source {
        s.source = src.clone();
    }
    if new_anchor {
        s.anchor_at = stamp(now);
    }
    if changed.contains(&"spec") || changed.contains(&"enabled") {
        s.next_run_at = if s.enabled {
            next_after(&s.spec, s.anchor(), now)?.map(stamp)
        } else {
            None
        };
    }
    if changed.contains(&"enabled") && s.enabled {
        s.consecutive_skips = 0;
        s.disabled_reason = None;
    }
    if changed.is_empty() && s.source == old.source {
        return Ok(old);
    }
    t.execute(
        "UPDATE schedules SET name = ?2, spec = ?3, action = ?4, catch_up = ?5, enabled = ?6,
                run_as = ?7, anchor_at = ?8, next_run_at = ?9, skips = ?10,
                disabled_reason = ?11, source = ?12, updated_at = ?13
         WHERE id = ?1",
        params![
            id,
            s.name,
            serde_json::to_string(&s.spec)?,
            serde_json::to_string(&s.action)?,
            s.catch_up.as_str(),
            s.enabled,
            s.run_as,
            s.anchor_at,
            s.next_run_at,
            s.consecutive_skips,
            s.disabled_reason,
            s.source,
            stamp(now),
        ],
    )?;
    let s = get(t, id)?;
    if !changed.is_empty() {
        let change = match changed.as_slice() {
            ["enabled"] if s.enabled => "enabled",
            ["enabled"] => "disabled",
            _ => "updated",
        };
        emit(t, change, &s, None)?;
        t.audit(
            "schedule.update",
            &s.key(),
            s.project.as_deref(),
            json!({"id": s.id, "changed": changed, "enabled": s.enabled}),
        )?;
    }
    Ok(s)
}

/// Enable or disable a schedule. `reason` is kept on a disable.
pub fn set_enabled(
    t: &mut Tx<'_>,
    id: &str,
    enabled: bool,
    reason: Option<&str>,
    now: DateTime<Utc>,
) -> ApiResult<Schedule> {
    let s = update(
        t,
        id,
        &SchedulePatch {
            enabled: Some(enabled),
            ..Default::default()
        },
        now,
    )?;
    if !enabled && let Some(r) = reason {
        t.execute(
            "UPDATE schedules SET disabled_reason = ?2 WHERE id = ?1",
            params![id, r],
        )?;
        return get(t, id);
    }
    Ok(s)
}

pub fn delete(t: &mut Tx<'_>, id: &str) -> ApiResult<Schedule> {
    let s = get(t, id)?;
    t.execute("DELETE FROM schedules WHERE id = ?1", [id])?;
    emit(t, "deleted", &s, None)?;
    t.audit(
        "schedule.delete",
        &s.key(),
        s.project.as_deref(),
        json!({"id": s.id, "source": s.source}),
    )?;
    Ok(s)
}

/// Move `next_run_at` to the first run after `now`.
pub fn advance(t: &mut Tx<'_>, id: &str, now: DateTime<Utc>) -> ApiResult<Option<String>> {
    let s = get(t, id)?;
    let next = next_after(&s.spec, s.anchor(), now)?.map(stamp);
    t.execute(
        "UPDATE schedules SET next_run_at = ?2 WHERE id = ?1",
        params![id, next],
    )?;
    Ok(next)
}

/// Write a definition (a project file or a fleet) into `project`: a new
/// schedule, or the changed fields of the one with the same name. A new
/// schedule runs as `created_by`; a changed action runs as it too.
pub fn upsert_def(
    t: &mut Tx<'_>,
    project: &str,
    def: &ScheduleDef,
    source: &str,
    created_by: &str,
    now: DateTime<Utc>,
) -> ApiResult<Schedule> {
    match find_in(t, Some(project), &def.name)? {
        None => create(
            t,
            &NewSchedule {
                name: def.name.clone(),
                project: Some(project.to_owned()),
                spec: def.spec.clone(),
                action: def.action.clone(),
                enabled: def.enabled,
                catch_up: def.catch_up,
                created_by: created_by.to_owned(),
                source: source.to_owned(),
            },
            now,
        ),
        Some(s) => update(
            t,
            &s.id,
            &SchedulePatch {
                name: None,
                spec: Some(def.spec.clone()),
                action: Some(def.action.clone()),
                enabled: Some(def.enabled),
                catch_up: Some(def.catch_up),
                run_as: Some(created_by.to_owned()),
                source: Some(source.to_owned()),
            },
            now,
        ),
    }
}

/// Sync the `[[schedules]]` of a project file: write each one with
/// `source = file`, and delete the file schedules of the project that the
/// file no longer has. Returns the errors, one line each.
pub fn sync_file(
    t: &mut Tx<'_>,
    project: &str,
    defs: &[ScheduleDef],
    now: DateTime<Utc>,
) -> ApiResult<Vec<String>> {
    let mut errors = Vec::new();
    for d in defs {
        if let Err(e) = resolve_reach(t, Some(project), &d.action)
            .and_then(|_| upsert_def(t, project, d, projects::SOURCE_FILE, "operator", now))
        {
            errors.push(format!(
                "projects/{project}.toml schedule {}: {}",
                d.name, e.message
            ));
        }
    }
    for s in of_project(t, project)? {
        if s.source == projects::SOURCE_FILE && !defs.iter().any(|d| d.name == s.name) {
            delete(t, &s.id)?;
        }
    }
    Ok(errors)
}

/// A project file is gone: its file schedules become API schedules.
pub fn detach_file(t: &Tx<'_>, project: &str) -> ApiResult<()> {
    t.execute(
        "UPDATE schedules SET source = 'api' WHERE project = ?1 AND source = 'file'",
        [project],
    )?;
    Ok(())
}

// -------------------------------------------------------------- the runs

/// One run of a schedule.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct Run {
    pub id: String,
    pub schedule: String,
    pub started_at: String,
    pub finished_at: Option<String>,
    /// `running`, `ok`, `failed`, or `skipped`.
    pub status: String,
    /// `cron`, `manual`, or `catch_up`.
    pub trigger: String,
    pub summary: String,
    /// Exec: the tail of stdout and stderr and the exit code. Mail: the
    /// message ID. Turn: the delivery. Agent: the new status. Probe: the
    /// windows.
    pub output: Value,
    pub error: Option<String>,
    /// Who triggered the run: the `run_as`, or the caller of a run-now.
    pub actor: String,
}

fn run_row(r: &Row<'_>) -> rusqlite::Result<Run> {
    let output: String = r.get("output")?;
    Ok(Run {
        id: r.get("id")?,
        schedule: r.get("schedule")?,
        started_at: r.get("started_at")?,
        finished_at: r.get("finished_at")?,
        status: r.get("status")?,
        trigger: r.get("trigger")?,
        summary: r.get("summary")?,
        output: serde_json::from_str(&output).unwrap_or(Value::Null),
        error: r.get("error")?,
        actor: r.get("actor")?,
    })
}

pub fn get_run(conn: &Connection, id: &str) -> ApiResult<Run> {
    conn.query_row("SELECT * FROM schedule_runs WHERE id = ?1", [id], run_row)
        .optional()?
        .ok_or_else(|| ApiError::not_found(format!("no schedule run `{id}`")))
}

/// The runs of a schedule, newest first. The cursor is a run ID.
pub fn list_runs(conn: &Connection, schedule: &str, page: &Page) -> ApiResult<Paged<Run>> {
    let limit = page.limit();
    let mut stmt = conn.prepare(
        "SELECT * FROM schedule_runs WHERE schedule = ?1 AND (?2 = '' OR id < ?2)
         ORDER BY id DESC LIMIT ?3",
    )?;
    let rows = stmt
        .query_map(
            params![schedule, page.cursor().unwrap_or(""), (limit + 1) as i64],
            run_row,
        )?
        .collect::<Result<Vec<_>, _>>()?;
    Ok(Paged::from_rows(rows, limit, |r: &Run| r.id.clone()))
}

/// The start of a run.
#[derive(Debug)]
pub enum Begin {
    /// The run is `running`; the caller does the action and calls
    /// [`finish_run`].
    Started(Run, Box<Schedule>),
    /// Another run of the schedule is still running: this one is
    /// `skipped` already.
    Overlap(Run),
}

fn set_last_run(t: &Tx<'_>, s: &Schedule, r: &Run, skips: i64) -> ApiResult<()> {
    let last = LastRun {
        at: r
            .finished_at
            .clone()
            .unwrap_or_else(|| r.started_at.clone()),
        status: r.status.clone(),
        summary: r.summary.clone(),
        run_id: r.id.clone(),
    };
    t.execute(
        "UPDATE schedules SET last_run = ?2, runs_count = runs_count + 1, skips = ?3
         WHERE id = ?1",
        params![s.id, serde_json::to_string(&last)?, skips],
    )?;
    Ok(())
}

/// Start a run of `id`: `running`, or `skipped` when a run of the schedule
/// is still running (`max_concurrency` is 1). One transaction, so two
/// triggers cannot both start.
pub fn begin_run(
    t: &mut Tx<'_>,
    id: &str,
    trigger: &str,
    actor: &str,
    now: DateTime<Utc>,
) -> ApiResult<Begin> {
    let s = get(t, id)?;
    let at = stamp(now);
    let run_id = ulid::Ulid::new().to_string();
    let running: i64 = t.query_row(
        "SELECT count(*) FROM schedule_runs WHERE schedule = ?1 AND status = 'running'",
        [id],
        |r| r.get(0),
    )?;
    if running > 0 {
        t.execute(
            "INSERT INTO schedule_runs (id, schedule, trigger, status, started_at, finished_at,
                                        summary, actor)
             VALUES (?1, ?2, ?3, 'skipped', ?4, ?4, 'skipped: still running', ?5)",
            params![run_id, id, trigger, at, actor],
        )?;
        let r = get_run(t, &run_id)?;
        set_last_run(t, &s, &r, s.consecutive_skips)?;
        let s = get(t, id)?;
        emit(t, "run_finished", &s, Some(&r))?;
        return Ok(Begin::Overlap(r));
    }
    t.execute(
        "INSERT INTO schedule_runs (id, schedule, trigger, status, started_at, actor)
         VALUES (?1, ?2, ?3, 'running', ?4, ?5)",
        params![run_id, id, trigger, at, actor],
    )?;
    let r = get_run(t, &run_id)?;
    emit(t, "run_started", &s, Some(&r))?;
    Ok(Begin::Started(r, Box::new(s)))
}

/// How a run ended.
#[derive(Debug, Clone, PartialEq)]
pub struct Outcome {
    /// `ok`, `failed`, or `skipped`.
    pub status: &'static str,
    pub summary: String,
    pub output: Value,
    pub error: Option<String>,
    /// The rights check skipped the run: it counts toward [`MAX_SKIPS`].
    pub rights: bool,
}

impl Outcome {
    pub fn ok(summary: impl Into<String>, output: Value) -> Self {
        Self {
            status: "ok",
            summary: summary.into(),
            output,
            error: None,
            rights: false,
        }
    }

    pub fn failed(summary: impl Into<String>, error: impl Into<String>, output: Value) -> Self {
        Self {
            status: "failed",
            summary: summary.into(),
            output,
            error: Some(error.into()),
            rights: false,
        }
    }

    pub fn skipped(reason: impl Into<String>) -> Self {
        let reason = reason.into();
        Self {
            status: "skipped",
            summary: format!("skipped: {reason}"),
            output: json!({}),
            error: Some(reason),
            rights: false,
        }
    }

    /// A skip of the rights check.
    pub fn refused(reason: impl Into<String>) -> Self {
        Self {
            rights: true,
            ..Self::skipped(reason)
        }
    }
}

/// End a run. A rights skip counts; the [`MAX_SKIPS`]th in a row disables
/// the schedule and mails the operator. Any other end resets the count.
pub fn finish_run(t: &mut Tx<'_>, run_id: &str, o: &Outcome, now: DateTime<Utc>) -> ApiResult<Run> {
    let at = stamp(now);
    t.execute(
        "UPDATE schedule_runs SET status = ?2, finished_at = ?3, summary = ?4, output = ?5,
                                  error = ?6
         WHERE id = ?1",
        params![
            run_id,
            o.status,
            at,
            o.summary,
            o.output.to_string(),
            o.error
        ],
    )?;
    let r = get_run(t, run_id)?;
    // The schedule can be gone (deleted during the run): keep the run.
    let Ok(s) = get(t, &r.schedule) else {
        return Ok(r);
    };
    let skips = if o.rights { s.consecutive_skips + 1 } else { 0 };
    set_last_run(t, &s, &r, skips)?;
    t.audit(
        "schedule.run",
        &s.key(),
        s.project.as_deref(),
        json!({"id": s.id, "run": r.id, "trigger": r.trigger, "status": r.status,
               "summary": r.summary, "run_as": s.run_as}),
    )?;
    let s = get(t, &s.id)?;
    emit(t, "run_finished", &s, Some(&r))?;
    if o.rights && skips >= MAX_SKIPS && s.enabled {
        let why = format!(
            "{skips} runs in a row were skipped: {}",
            o.error.as_deref().unwrap_or("the rights check failed")
        );
        let s = set_enabled(t, &s.id, false, Some(&why), now)?;
        mail::send(
            t,
            Outgoing {
                from: &Address::System,
                to: &Address::Operator,
                subject: &format!("Schedule {} disabled", s.key()),
                body: &format!(
                    "The daemon disabled schedule {} ({}). {why}. It runs as {}. Fix the \
                     cause, then run `terrarium schedule enable {}`.",
                    s.key(),
                    s.id,
                    s.run_as,
                    s.id
                ),
                thread: None,
                in_reply_to: None,
            },
        )?;
    }
    Ok(r)
}

/// Runs still `running` at a daemon start ended with the old daemon.
pub fn fail_stale_runs(t: &mut Tx<'_>, now: DateTime<Utc>) -> ApiResult<usize> {
    let n = t.execute(
        "UPDATE schedule_runs SET status = 'failed', finished_at = ?1,
                error = 'the daemon stopped during the run',
                summary = 'failed: the daemon stopped during the run'
         WHERE status = 'running'",
        [stamp(now)],
    )?;
    Ok(n)
}

/// Keep the newest `keep` runs of each schedule. Returns the rows deleted.
pub fn prune_runs(t: &mut Tx<'_>, keep: usize) -> ApiResult<usize> {
    let n = t.execute(
        "DELETE FROM schedule_runs WHERE id IN (
            SELECT id FROM (
                SELECT id, row_number() OVER (PARTITION BY schedule ORDER BY id DESC) AS n
                FROM schedule_runs)
            WHERE n > ?1)",
        [keep as i64],
    )?;
    Ok(n)
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::*;
    use chrono::TimeZone;

    fn utc(s: &str) -> DateTime<Utc> {
        parse_time(s).unwrap()
    }

    fn cron(expr: &str, tz: &str) -> ScheduleSpec {
        ScheduleSpec::Cron {
            expr: expr.into(),
            tz: tz.into(),
        }
    }

    #[test]
    fn cron_next_runs_in_the_zone() {
        let now = utc("2026-10-26T08:07:00Z");
        let next = |e: &str, tz: &str| {
            next_after(&cron(e, tz), now, now)
                .unwrap()
                .map(stamp)
                .unwrap()
        };
        assert_eq!(next("*/15 * * * *", "UTC"), "2026-10-26T08:15:00.000Z");
        // 09:00 in London on 26 Oct 2026 is GMT (UTC+0) after the change on
        // 25 Oct; in New York it is still EDT (UTC-4).
        assert_eq!(
            next("0 9 * * *", "Europe/London"),
            "2026-10-26T09:00:00.000Z"
        );
        assert_eq!(
            next("0 9 * * *", "America/New_York"),
            "2026-10-26T13:00:00.000Z"
        );
        // 2026-10-26 is a Monday. Day 0 and 7 are Sunday; 1-5 is Mon-Fri.
        assert_eq!(next("0 9 * * 0", "UTC"), "2026-11-01T09:00:00.000Z");
        assert_eq!(next("0 9 * * 7", "UTC"), "2026-11-01T09:00:00.000Z");
        assert_eq!(next("0 7 * * 1-5", "UTC"), "2026-10-27T07:00:00.000Z");
        assert_eq!(next("0 9 * * MON-FRI", "UTC"), "2026-10-26T09:00:00.000Z");
        assert_eq!(next("0 9 * * 6-7", "UTC"), "2026-10-31T09:00:00.000Z");
        assert_eq!(next("@hourly", "UTC"), "2026-10-26T09:00:00.000Z");
    }

    #[test]
    fn bad_specs_name_the_field_and_the_expression() {
        let now = utc("2026-10-26T08:07:00Z");
        let e = validate_spec(&cron("*/15 * * *", "UTC"), now).unwrap_err();
        assert_eq!(e.code, Code::Invalid);
        assert!(e.message.starts_with("spec.expr:"), "{}", e.message);
        assert!(e.message.contains("`*/15 * * *`"), "{}", e.message);
        assert_eq!(e.details.unwrap()["field"], "spec.expr");
        // Seconds are not a field: the shortest cron step is one minute.
        assert!(validate_spec(&cron("*/5 * * * * *", "UTC"), now).is_err());
        let e = validate_spec(&cron("* * * * *", "Mars/Olympus"), now).unwrap_err();
        assert!(e.message.starts_with("spec.tz:"), "{}", e.message);
        let every = |s: &str| ScheduleSpec::Interval { every: s.into() };
        let e = validate_spec(&every("30s"), now).unwrap_err();
        assert!(e.message.contains("shortest interval"), "{}", e.message);
        assert!(validate_spec(&every("60s"), now).is_ok());
        assert!(validate_spec(&every("1h30m"), now).is_ok());
        assert!(validate_spec(&every("10"), now).is_err());
        let at = |s: &str| ScheduleSpec::At { at: s.into() };
        assert!(validate_spec(&at("2026-10-26T08:00:00Z"), now).is_err());
        assert!(validate_spec(&at("tomorrow"), now).is_err());
        assert!(validate_spec(&at("2026-10-27T08:00:00+01:00"), now).is_ok());
    }

    #[test]
    fn intervals_keep_their_anchor_and_at_runs_once() {
        let anchor = utc("2026-10-26T08:00:00Z");
        let spec = ScheduleSpec::Interval {
            every: "30m".into(),
        };
        let n = |after: &str| stamp(next_after(&spec, anchor, utc(after)).unwrap().unwrap());
        assert_eq!(n("2026-10-26T08:00:00Z"), "2026-10-26T08:30:00.000Z");
        assert_eq!(n("2026-10-26T08:29:59Z"), "2026-10-26T08:30:00.000Z");
        assert_eq!(n("2026-10-26T08:30:00Z"), "2026-10-26T09:00:00.000Z");
        assert_eq!(n("2026-10-26T12:10:00Z"), "2026-10-26T12:30:00.000Z");
        let at = ScheduleSpec::At {
            at: "2026-10-26T09:00:00Z".into(),
        };
        assert!(next_after(&at, anchor, anchor).unwrap().is_some());
        assert!(
            next_after(&at, anchor, utc("2026-10-26T09:00:00Z"))
                .unwrap()
                .is_none()
        );
        // A zone with a fixed offset stays valid.
        let _ = chrono_tz::UTC.with_ymd_and_hms(2026, 1, 1, 0, 0, 0);
    }
}
