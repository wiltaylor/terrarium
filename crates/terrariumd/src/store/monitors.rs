//! Monitors (docs/api.md §30): the rows, the results, and the dashboard
//! layout of a project.
//!
//! A monitor runs a shell script on a target on an interval or a cron
//! spec, and keeps each result. The scheduler task (`crate::scheduler`)
//! finds the due monitors with [`due`] and moves them on with
//! [`advance`], as it does for schedules; the run itself is
//! `crate::monitors`. The time math is the schedules' own
//! ([`schedules::next_after_min`]).
//!
//! Each result emits `monitor.result`; a result whose status differs from
//! the one before also emits `monitor.status`. The triggers (PLAN §12c)
//! take both. Results go after `retention_days`, and past
//! [`MAX_POINTS`] for one monitor.

use std::collections::HashMap;

use chrono::{DateTime, Utc};
use rusqlite::{Connection, OptionalExtension, Row, params};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use terrarium_core::schedule::ScheduleSpec;

use crate::db::Tx;
use crate::error::{ApiError, ApiResult};
use crate::events::EventDraft;
use crate::monitors::layout::{self, Layout};
use crate::monitors::parse::{Direction, Kind, Parsed, Status, Thresholds};
use crate::owner::Owner;
use crate::store::schedules::{self, field_error, stamp};
use crate::store::{projects, valid_name};

/// The shortest interval of a monitor, in seconds.
pub const MIN_INTERVAL_SECS: i64 = 15;
/// The run limit of a monitor without `timeout_secs`.
pub const DEFAULT_TIMEOUT: u64 = 30;
/// The highest `timeout_secs` (10 minutes).
pub const MAX_TIMEOUT: u64 = 600;
/// The days of results that a monitor keeps by default.
pub const DEFAULT_RETENTION_DAYS: i64 = 7;
/// The highest `retention_days`.
pub const MAX_RETENTION_DAYS: i64 = 90;
/// The most results that one monitor keeps; older ones go first.
pub const MAX_POINTS: i64 = 10_000;
/// The longest script, in bytes.
pub const MAX_SCRIPT: usize = 64 * 1024;
/// The most monitors of one project.
pub const MAX_PER_PROJECT: usize = 100;
/// The default and the highest `limit` of a results read.
pub const DEFAULT_RESULTS: usize = 500;
pub const MAX_RESULTS: usize = MAX_POINTS as usize;

fn default_timeout() -> u64 {
    DEFAULT_TIMEOUT
}
fn default_retention() -> i64 {
    DEFAULT_RETENTION_DAYS
}
fn yes() -> bool {
    true
}

/// The fields of a monitor that a client writes.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct MonitorDef {
    pub name: String,
    #[serde(default)]
    pub description: Option<String>,
    /// A global target, or a target of the project.
    pub target: String,
    /// Shell text; the target runs it with `sh -c`.
    pub script: String,
    /// `{"kind": "interval", "every": "1m"}` or `{"kind": "cron", "expr":
    /// "*/5 * * * *", "tz"?: "UTC"}`.
    pub schedule: ScheduleSpec,
    #[serde(default = "default_timeout")]
    pub timeout_secs: u64,
    pub kind: Kind,
    #[serde(default)]
    pub unit: Option<String>,
    #[serde(default)]
    pub warn: Option<f64>,
    #[serde(default)]
    pub crit: Option<f64>,
    #[serde(default)]
    pub direction: Direction,
    #[serde(default = "yes")]
    pub enabled: bool,
    #[serde(default = "default_retention")]
    pub retention_days: i64,
}

impl MonitorDef {
    pub fn thresholds(&self) -> Thresholds {
        Thresholds {
            warn: self.warn,
            crit: self.crit,
            direction: self.direction,
        }
    }
}

/// One result of a monitor.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct MonitorResult {
    pub id: i64,
    pub monitor: String,
    pub at: String,
    pub status: Status,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub value: Option<f64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub text: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub json: Option<Value>,
    pub duration_ms: i64,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
    /// The actor of the run: `monitor:<project>/<name>`. Absent on a result
    /// from before migration 62.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub actor: Option<String>,
}

/// A monitor as the API shows it.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct Monitor {
    pub id: String,
    pub project: String,
    #[serde(flatten)]
    pub def: MonitorDef,
    /// The seconds between two runs (for a cron spec, between the next
    /// two). A block is stale after three periods with no result.
    pub period_secs: i64,
    pub created_by: String,
    /// `actor` (the runs act as `run_as`), `project` (the rights come from
    /// the project), or `system` (docs/api.md §30.7).
    pub owner: Owner,
    /// The actor whose rights each run of an actor-owned monitor checks
    /// again: the creator, or the last caller that changed the script, the
    /// target, or the owner. A project- or system-owned monitor does not
    /// use it.
    pub run_as: String,
    /// Why the daemon disabled the monitor, when it did (its `run_as`
    /// agent was deleted).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub disabled_reason: Option<String>,
    pub created_at: String,
    pub updated_at: String,
    pub next_run_at: Option<String>,
    /// The ticks that found the previous run still running.
    pub skipped: i64,
    pub last: Option<MonitorResult>,
    /// A run is in progress now.
    pub running: bool,
}

impl Monitor {
    /// `project/name`.
    pub fn key(&self) -> String {
        format!("{}/{}", self.project, self.def.name)
    }

    /// The actor label of the runs: `monitor:<project>/<name>`.
    pub fn label(&self) -> String {
        crate::owner::monitor_label(&self.project, &self.def.name)
    }
}

// ------------------------------------------------------------------ checks

/// The seconds between two runs of `spec`.
pub fn period_secs(spec: &ScheduleSpec, now: DateTime<Utc>) -> i64 {
    match spec {
        ScheduleSpec::Interval { every } => schedules::parse_every(every)
            .unwrap_or(MIN_INTERVAL_SECS)
            .max(MIN_INTERVAL_SECS),
        _ => {
            let a = schedules::next_after_min(spec, now, now, MIN_INTERVAL_SECS)
                .ok()
                .flatten();
            let b = a.and_then(|a| {
                schedules::next_after_min(spec, now, a, MIN_INTERVAL_SECS)
                    .ok()
                    .flatten()
            });
            match (a, b) {
                (Some(a), Some(b)) => (b - a).num_seconds().max(MIN_INTERVAL_SECS),
                _ => 86_400,
            }
        }
    }
}

/// Check the fields. The error names the field.
pub fn validate(d: &MonitorDef) -> ApiResult<()> {
    if !valid_name(&d.name) {
        return Err(field_error(
            "name",
            format!(
                "bad monitor name `{}`: use 1-63 of a-z, 0-9, `-`, `_`, starting with a letter or digit",
                d.name
            ),
        ));
    }
    if d.description
        .as_ref()
        .is_some_and(|s| s.chars().count() > 500)
    {
        return Err(field_error("description", "longer than 500 characters"));
    }
    if d.target.trim().is_empty() {
        return Err(field_error("target", "name a target"));
    }
    if d.script.trim().is_empty() {
        return Err(field_error("script", "the script is empty"));
    }
    if d.script.len() > MAX_SCRIPT {
        return Err(field_error(
            "script",
            format!("{} bytes; a script is at most {MAX_SCRIPT}", d.script.len()),
        ));
    }
    match &d.schedule {
        ScheduleSpec::Interval { every } => {
            let secs =
                schedules::parse_every(every).map_err(|e| field_error("schedule.every", e))?;
            if secs < MIN_INTERVAL_SECS {
                return Err(field_error(
                    "schedule.every",
                    format!("`{every}` is under the shortest interval, {MIN_INTERVAL_SECS} s"),
                ));
            }
        }
        ScheduleSpec::Cron { expr, tz } => {
            schedules::parse_cron(expr).map_err(|e| {
                field_error(
                    "schedule.expr",
                    format!("bad cron expression `{expr}`: {e}"),
                )
            })?;
            schedules::parse_tz(tz).map_err(|e| field_error("schedule.tz", e))?;
        }
        _ => {
            return Err(field_error(
                "schedule",
                "a monitor runs on an interval or a cron spec",
            ));
        }
    }
    if d.timeout_secs == 0 || d.timeout_secs > MAX_TIMEOUT {
        return Err(field_error(
            "timeout_secs",
            format!("{} is not 1-{MAX_TIMEOUT}", d.timeout_secs),
        ));
    }
    if d.unit.as_ref().is_some_and(|u| u.chars().count() > 32) {
        return Err(field_error("unit", "longer than 32 characters"));
    }
    if (d.warn.is_some() || d.crit.is_some()) && !d.kind.has_value() {
        return Err(field_error(
            "warn",
            format!(
                "a {} monitor has no value; thresholds need kind number or json",
                d.kind.as_str()
            ),
        ));
    }
    d.thresholds().check().map_err(|e| field_error("warn", e))?;
    if !(1..=MAX_RETENTION_DAYS).contains(&d.retention_days) {
        return Err(field_error(
            "retention_days",
            format!("{} is not 1-{MAX_RETENTION_DAYS}", d.retention_days),
        ));
    }
    Ok(())
}

// -------------------------------------------------------------------- rows

const COLS: &str = "id, project, def, created_by, run_as, created_at, updated_at, \
                    next_run_at, skipped, owner, disabled_reason";

fn row(r: &Row<'_>) -> rusqlite::Result<(Monitor, String)> {
    let def: String = r.get(2)?;
    Ok((
        Monitor {
            id: r.get(0)?,
            project: r.get(1)?,
            def: MonitorDef {
                name: String::new(),
                description: None,
                target: String::new(),
                script: String::new(),
                schedule: ScheduleSpec::Interval { every: "1m".into() },
                timeout_secs: DEFAULT_TIMEOUT,
                kind: Kind::Text,
                unit: None,
                warn: None,
                crit: None,
                direction: Direction::Above,
                enabled: true,
                retention_days: DEFAULT_RETENTION_DAYS,
            },
            period_secs: 0,
            created_by: r.get(3)?,
            owner: Owner::from_db(&r.get::<_, String>(9)?),
            run_as: r.get(4)?,
            disabled_reason: r.get(10)?,
            created_at: r.get(5)?,
            updated_at: r.get(6)?,
            next_run_at: r.get(7)?,
            skipped: r.get(8)?,
            last: None,
            running: false,
        },
        def,
    ))
}

fn finish(conn: &Connection, (mut m, def): (Monitor, String)) -> ApiResult<Monitor> {
    m.def = serde_json::from_str(&def)
        .map_err(|e| ApiError::internal(format!("monitor {} has a bad row: {e}", m.id)))?;
    m.period_secs = period_secs(&m.def.schedule, Utc::now());
    m.last = latest(conn, &m.id)?;
    Ok(m)
}

fn query(conn: &Connection, sql: &str, args: &[&dyn rusqlite::ToSql]) -> ApiResult<Vec<Monitor>> {
    let mut st = conn.prepare(sql)?;
    let rows = st.query_map(args, row)?.collect::<Result<Vec<_>, _>>()?;
    rows.into_iter().map(|r| finish(conn, r)).collect()
}

/// A monitor by its ULID.
pub fn get(conn: &Connection, id: &str) -> ApiResult<Monitor> {
    query(
        conn,
        &format!("SELECT {COLS} FROM monitors WHERE id = ?1"),
        &[&id],
    )?
    .into_iter()
    .next()
    .ok_or_else(|| ApiError::not_found(format!("no monitor `{id}`")))
}

/// A monitor by its ULID or `project/name`.
pub fn find(conn: &Connection, key: &str) -> ApiResult<Monitor> {
    if let Some((p, n)) = key.split_once('/') {
        return query(
            conn,
            &format!("SELECT {COLS} FROM monitors WHERE project = ?1 AND name = ?2"),
            &[&p, &n],
        )?
        .into_iter()
        .next()
        .ok_or_else(|| ApiError::not_found(format!("no monitor `{n}` in project `{p}`")));
    }
    get(conn, key)
}

/// The monitors of a project, by name.
pub fn list(conn: &Connection, project: &str) -> ApiResult<Vec<Monitor>> {
    query(
        conn,
        &format!("SELECT {COLS} FROM monitors WHERE project = ?1 ORDER BY name"),
        &[&project],
    )
}

/// The ID and the kind of each monitor of a project.
pub fn kinds(conn: &Connection, project: &str) -> ApiResult<HashMap<String, Kind>> {
    Ok(list(conn, project)?
        .into_iter()
        .map(|m| (m.id, m.def.kind))
        .collect())
}

/// The enabled monitors whose next run is at or before `now`.
pub fn due(conn: &Connection, now: DateTime<Utc>) -> ApiResult<Vec<Monitor>> {
    let now = stamp(now);
    query(
        conn,
        &format!(
            "SELECT {COLS} FROM monitors WHERE enabled = 1 AND next_run_at IS NOT NULL
             AND next_run_at <= ?1 ORDER BY next_run_at"
        ),
        &[&now],
    )
}

fn anchor_of(conn: &Connection, id: &str) -> ApiResult<DateTime<Utc>> {
    let a: String = conn.query_row("SELECT anchor_at FROM monitors WHERE id = ?1", [id], |r| {
        r.get(0)
    })?;
    Ok(DateTime::parse_from_rfc3339(&a)
        .map(|t| t.with_timezone(&Utc))
        .unwrap_or_else(|_| Utc::now()))
}

fn next_run(spec: &ScheduleSpec, anchor: DateTime<Utc>, after: DateTime<Utc>) -> Option<String> {
    schedules::next_after_min(spec, anchor, after, MIN_INTERVAL_SECS)
        .ok()
        .flatten()
        .map(stamp)
}

/// Move the next run of monitor `id` past `now`.
pub fn advance(t: &Tx<'_>, id: &str, now: DateTime<Utc>) -> ApiResult<()> {
    let m = get(t, id)?;
    let next = next_run(&m.def.schedule, anchor_of(t, id)?, now);
    t.execute(
        "UPDATE monitors SET next_run_at = ?2 WHERE id = ?1",
        params![id, next],
    )?;
    Ok(())
}

/// Count a tick that found the run of monitor `id` still running.
pub fn add_skip(t: &Tx<'_>, id: &str) -> ApiResult<()> {
    t.execute(
        "UPDATE monitors SET skipped = skipped + 1 WHERE id = ?1",
        [id],
    )?;
    Ok(())
}

/// Create a monitor. The caller checked the rights and the target.
pub fn create(
    t: &mut Tx<'_>,
    project: &str,
    def: &MonitorDef,
    owner: Owner,
    actor: &str,
    now: DateTime<Utc>,
) -> ApiResult<Monitor> {
    projects::get(t, project)?;
    validate(def)?;
    let n: i64 = t.query_row(
        "SELECT COUNT(*) FROM monitors WHERE project = ?1",
        [project],
        |r| r.get(0),
    )?;
    if n as usize >= MAX_PER_PROJECT {
        return Err(ApiError::conflict(format!(
            "project `{project}` has {n} monitors; the limit is {MAX_PER_PROJECT}"
        )));
    }
    let taken: Option<String> = t
        .query_row(
            "SELECT id FROM monitors WHERE project = ?1 AND name = ?2",
            params![project, def.name],
            |r| r.get(0),
        )
        .optional()?;
    if taken.is_some() {
        return Err(ApiError::conflict(format!(
            "project `{project}` has a monitor named `{}`",
            def.name
        ))
        .with_details(json!({"field": "name"})));
    }
    let id = ulid::Ulid::new().to_string();
    // The first run comes at once: a new monitor shows a value soon.
    let next = def.enabled.then(|| stamp(now));
    let at = stamp(now);
    t.execute(
        "INSERT INTO monitors (id, project, name, kind, enabled, def, created_by, run_as,
                               created_at, updated_at, anchor_at, next_run_at, skipped, owner)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?7, ?8, ?8, ?8, ?9, 0, ?10)",
        params![
            id,
            project,
            def.name,
            def.kind.as_str(),
            def.enabled,
            serde_json::to_string(def).map_err(|e| ApiError::internal(e.to_string()))?,
            actor,
            at,
            next,
            owner.as_str()
        ],
    )?;
    let m = get(t, &id)?;
    t.audit(
        "monitor.create",
        &m.key(),
        Some(project),
        json!({"id": m.id, "owner": owner.as_str(), "target": def.target, "kind": def.kind.as_str(),
               "schedule": def.schedule, "script": cut_script(&def.script)}),
    )?;
    Ok(m)
}

fn cut_script(s: &str) -> String {
    crate::monitors::parse::cut_chars(s, 200)
}

/// Replace the fields and the owner of monitor `id`. A new schedule, or a
/// monitor that is enabled again, runs at once and starts a new interval.
/// `run_as`: the new `run_as` (the caller that takes the monitor over; the
/// API decides, `crate::api::monitors`).
pub fn update(
    t: &mut Tx<'_>,
    id: &str,
    def: &MonitorDef,
    owner: Owner,
    run_as: Option<&str>,
    now: DateTime<Utc>,
) -> ApiResult<Monitor> {
    validate(def)?;
    let old = get(t, id)?;
    if def.name != old.def.name {
        let taken: Option<String> = t
            .query_row(
                "SELECT id FROM monitors WHERE project = ?1 AND name = ?2 AND id != ?3",
                params![old.project, def.name, id],
                |r| r.get(0),
            )
            .optional()?;
        if taken.is_some() {
            return Err(ApiError::conflict(format!(
                "project `{}` has a monitor named `{}`",
                old.project, def.name
            ))
            .with_details(json!({"field": "name"})));
        }
    }
    let at = stamp(now);
    let restart = def.schedule != old.def.schedule || (def.enabled && !old.def.enabled);
    if def.name != old.def.name {
        super::threads::on_monitor_missing(t, &old.project, &old.def.name)?;
    }
    let reruns = def.script != old.def.script || def.target != old.def.target;
    t.execute(
        "UPDATE monitors SET name = ?2, kind = ?3, enabled = ?4, def = ?5, updated_at = ?6,
                owner = ?7
         WHERE id = ?1",
        params![
            id,
            def.name,
            def.kind.as_str(),
            def.enabled,
            serde_json::to_string(def).map_err(|e| ApiError::internal(e.to_string()))?,
            at,
            owner.as_str()
        ],
    )?;
    if let Some(r) = run_as {
        t.execute(
            "UPDATE monitors SET run_as = ?2 WHERE id = ?1",
            params![id, r],
        )?;
    }
    if def.enabled {
        t.execute(
            "UPDATE monitors SET disabled_reason = NULL WHERE id = ?1",
            [id],
        )?;
    }
    if !def.enabled {
        t.execute("UPDATE monitors SET next_run_at = NULL WHERE id = ?1", [id])?;
    } else if restart || old.next_run_at.is_none() {
        t.execute(
            "UPDATE monitors SET anchor_at = ?2, next_run_at = ?2 WHERE id = ?1",
            params![id, at],
        )?;
    }
    let m = get(t, id)?;
    let mut changed: Vec<&str> = Vec::new();
    let (a, b) = (&old.def, def);
    for (name, diff) in [
        ("name", a.name != b.name),
        ("description", a.description != b.description),
        ("target", a.target != b.target),
        ("script", a.script != b.script),
        ("schedule", a.schedule != b.schedule),
        ("timeout_secs", a.timeout_secs != b.timeout_secs),
        ("kind", a.kind != b.kind),
        ("unit", a.unit != b.unit),
        ("thresholds", a.thresholds() != b.thresholds()),
        ("enabled", a.enabled != b.enabled),
        ("retention_days", a.retention_days != b.retention_days),
        ("owner", old.owner != owner),
        ("run_as", run_as.is_some_and(|r| r != old.run_as)),
    ] {
        if diff {
            changed.push(name);
        }
    }
    t.audit(
        "monitor.update",
        &m.key(),
        Some(&m.project),
        json!({"id": id, "changed": changed, "enabled": def.enabled, "owner": owner.as_str(),
               "run_as": m.run_as, "target": def.target,
               "script": reruns.then(|| cut_script(&def.script))}),
    )?;
    // A kind change can leave a block that the layout no longer allows:
    // the dashboard read skips it, and the next layout save says why.
    Ok(m)
}

/// Disable monitor `id` with `reason` (the daemon's own disable: its
/// `run_as` agent was deleted, `crate::owner::on_agent_deleted`).
pub fn disable(t: &mut Tx<'_>, id: &str, reason: &str) -> ApiResult<Monitor> {
    let old = get(t, id)?;
    let mut def = old.def.clone();
    def.enabled = false;
    t.execute(
        "UPDATE monitors SET enabled = 0, def = ?2, next_run_at = NULL, disabled_reason = ?3,
                updated_at = ?4
         WHERE id = ?1",
        params![
            id,
            serde_json::to_string(&def).map_err(|e| ApiError::internal(e.to_string()))?,
            reason,
            crate::db::now()
        ],
    )?;
    let m = get(t, id)?;
    t.audit(
        "monitor.disable",
        &m.key(),
        Some(&m.project),
        json!({"id": id, "reason": reason, "run_as": m.run_as}),
    )?;
    Ok(m)
}

/// Delete monitor `id`, its results, and its blocks on the dashboard.
pub fn delete(t: &mut Tx<'_>, id: &str) -> ApiResult<Monitor> {
    let m = get(t, id)?;
    t.execute("DELETE FROM monitors WHERE id = ?1", [id])?;
    super::threads::on_monitor_missing(t, &m.project, &m.def.name)?;
    super::pages::drop_dashboard_monitor(t, &m.project, id)?;
    t.audit(
        "monitor.delete",
        &m.key(),
        Some(&m.project),
        json!({"id": id, "target": m.def.target}),
    )?;
    Ok(m)
}

// ----------------------------------------------------------------- results

const RCOLS: &str = "pk, monitor, at, status, value, text, json, duration_ms, error, actor";

fn result_row(r: &Row<'_>) -> rusqlite::Result<MonitorResult> {
    let status: String = r.get(3)?;
    let json: Option<String> = r.get(6)?;
    Ok(MonitorResult {
        id: r.get(0)?,
        monitor: r.get(1)?,
        at: r.get(2)?,
        status: Status::parse(&status).unwrap_or(Status::Unknown),
        value: r.get(4)?,
        text: r.get(5)?,
        json: json.and_then(|j| serde_json::from_str(&j).ok()),
        duration_ms: r.get(7)?,
        error: r.get(8)?,
        actor: r.get(9)?,
    })
}

/// The newest result of monitor `id`.
pub fn latest(conn: &Connection, id: &str) -> ApiResult<Option<MonitorResult>> {
    Ok(conn
        .query_row(
            &format!(
                "SELECT {RCOLS} FROM monitor_results WHERE monitor = ?1 ORDER BY pk DESC LIMIT 1"
            ),
            [id],
            result_row,
        )
        .optional()?)
}

/// The results of monitor `id` from `since` to `until` (stored times),
/// oldest first. With more than `limit`, the newest `limit`.
pub fn results(
    conn: &Connection,
    id: &str,
    since: Option<&str>,
    until: Option<&str>,
    limit: usize,
) -> ApiResult<Vec<MonitorResult>> {
    let mut st = conn.prepare(&format!(
        "SELECT {RCOLS} FROM monitor_results WHERE monitor = ?1
         AND (?2 IS NULL OR at >= ?2) AND (?3 IS NULL OR at <= ?3)
         ORDER BY at DESC, pk DESC LIMIT ?4"
    ))?;
    let mut rows = st
        .query_map(params![id, since, until, limit as i64], result_row)?
        .collect::<Result<Vec<_>, _>>()?;
    rows.reverse();
    Ok(rows)
}

/// Keep the result of one run: write it with the actor
/// `monitor:<project>/<name>`, emit `monitor.result` (and, when the status
/// changed, `monitor.status` and a `monitor.status` audit row), and prune.
pub fn record(
    t: &mut Tx<'_>,
    id: &str,
    p: &Parsed,
    at: DateTime<Utc>,
    duration_ms: i64,
) -> ApiResult<MonitorResult> {
    let m = get(t, id)?;
    let prev = m.last.as_ref().map(|r| r.status);
    let at_s = stamp(at);
    // The output can hold a secret value: keep it redacted, as the event
    // log does.
    let p = &Parsed {
        status: p.status,
        value: p.value,
        text: p.text.clone().map(|s| t.redact_string(s)),
        json: p.json.clone().map(|v| t.redact_values(v)),
        error: p.error.clone().map(|s| t.redact_string(s)),
    };
    t.execute(
        "INSERT INTO monitor_results (monitor, at, status, value, text, json, duration_ms, error,
                                      actor)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9)",
        params![
            id,
            at_s,
            p.status.as_str(),
            p.value,
            p.text,
            p.json.as_ref().map(Value::to_string),
            duration_ms,
            p.error,
            m.label()
        ],
    )?;
    let pk = t.last_insert_rowid();
    let r = MonitorResult {
        id: pk,
        monitor: id.to_owned(),
        at: at_s,
        status: p.status,
        value: p.value,
        text: p.text.clone(),
        json: p.json.clone(),
        duration_ms,
        error: p.error.clone(),
        actor: Some(m.label()),
    };
    let unit = p
        .json
        .as_ref()
        .and_then(|j| j["unit"].as_str().map(str::to_owned))
        .or_else(|| m.def.unit.clone());
    let message = p
        .text
        .as_ref()
        .filter(|_| m.def.kind != Kind::Text)
        .map(|s| crate::monitors::parse::cut_chars(s, 200));
    let shown = match (p.value, &unit) {
        (Some(v), Some(u)) => format!(" {v} {u}"),
        (Some(v), None) => format!(" {v}"),
        _ => String::new(),
    };
    t.emit(
        EventDraft::new(
            "monitor.result",
            format!("monitor {}: {}{shown}", m.def.name, p.status.as_str()),
            json!({"project": m.project, "monitor": m.def.name, "monitor_id": id,
                   "status": p.status.as_str(), "value": p.value, "unit": unit,
                   "message": message, "error": p.error, "at": r.at,
                   "duration_ms": duration_ms, "result_id": pk}),
        )
        .project(m.project.clone()),
    )?;
    if prev != Some(p.status) {
        let from = prev.map(Status::as_str);
        // A status change is the one audit row of the runs; the caller
        // writes with the actor `monitor:<project>/<name>`.
        t.audit(
            "monitor.status",
            &m.key(),
            Some(&m.project),
            json!({"id": id, "from": from, "to": p.status.as_str(), "value": p.value,
                   "error": p.error, "result_id": pk, "owner": m.owner.as_str()}),
        )?;
        t.emit(
            EventDraft::new(
                "monitor.status",
                format!(
                    "monitor {}: {} → {}{shown}",
                    m.def.name,
                    from.unwrap_or("none"),
                    p.status.as_str()
                ),
                json!({"project": m.project, "monitor": m.def.name, "monitor_id": id,
                       "from": from, "to": p.status.as_str(), "value": p.value,
                       "unit": unit, "message": message, "error": p.error}),
            )
            .project(m.project.clone()),
        )?;
    }
    if prev != Some(p.status) {
        crate::store::threads::on_monitor_status(t, &m.project, &m.def.name, p.status)?;
    }
    prune(t, id, m.def.retention_days, at)?;
    Ok(r)
}

/// Delete the results of monitor `id` older than `retention_days` before
/// `now`, and the oldest past [`MAX_POINTS`]. Returns the rows deleted.
pub fn prune(t: &Tx<'_>, id: &str, retention_days: i64, now: DateTime<Utc>) -> ApiResult<usize> {
    let cut = stamp(now - chrono::Duration::days(retention_days.clamp(1, MAX_RETENTION_DAYS)));
    let mut n = t.execute(
        "DELETE FROM monitor_results WHERE monitor = ?1 AND at < ?2",
        params![id, cut],
    )?;
    n += t.execute(
        "DELETE FROM monitor_results WHERE monitor = ?1 AND pk <= (
             SELECT pk FROM monitor_results WHERE monitor = ?1
             ORDER BY pk DESC LIMIT 1 OFFSET ?2)",
        params![id, MAX_POINTS],
    )?;
    Ok(n)
}

/// [`prune`] for every monitor (the daily prune).
pub fn prune_all(t: &Tx<'_>, now: DateTime<Utc>) -> ApiResult<usize> {
    let ids: Vec<(String, String)> = {
        let mut st = t.prepare("SELECT id, def FROM monitors")?;
        st.query_map([], |r| Ok((r.get(0)?, r.get(1)?)))?
            .collect::<Result<_, _>>()?
    };
    let mut n = 0;
    for (id, def) in ids {
        let days = serde_json::from_str::<Value>(&def)
            .ok()
            .and_then(|v| v["retention_days"].as_i64())
            .unwrap_or(DEFAULT_RETENTION_DAYS);
        n += prune(t, &id, days, now)?;
    }
    Ok(n)
}

// --------------------------------------------------------------- dashboard

/// The layout of a project's dashboard, with `(updated_at, updated_by)`.
/// A project with no saved layout has none.
pub fn layout_of(
    conn: &Connection,
    project: &str,
) -> ApiResult<(Layout, Option<(String, String)>)> {
    let row: Option<(String, String, String)> = conn
        .query_row(
            "SELECT widgets, updated_at, updated_by FROM project_pages WHERE project = ?1 AND slug=COALESCE((SELECT slug FROM project_dashboard_pages WHERE project=?1),'dashboard')",
            [project],
            |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
        )
        .optional()?;
    match row {
        None => Ok((Layout::default(), None)),
        Some((l, at, by)) => {
            let widgets: Vec<super::pages::Widget> = serde_json::from_str(&l)?;
            let blocks = widgets
                .into_iter()
                .filter_map(|w| {
                    let mut v = serde_json::to_value(&w).ok()?;
                    let kind = v["kind"].as_str()?;
                    if !matches!(
                        kind,
                        "markdown" | "stat" | "status" | "chart" | "text" | "table"
                    ) {
                        return None;
                    }
                    if kind == "markdown" {
                        v["kind"] = json!("note");
                    }
                    let o = v.as_object_mut()?;
                    for k in ["x", "y", "w", "h", "auto_height", "wiki", "frame"] {
                        o.remove(k);
                    }
                    o.insert(
                        "size".into(),
                        json!(if w.w <= 3 {
                            "s"
                        } else if w.w <= 6 {
                            "m"
                        } else {
                            "l"
                        }),
                    );
                    serde_json::from_value(v).ok()
                })
                .collect();
            Ok((Layout { blocks }, Some((at, by))))
        }
    }
}

fn write_layout(t: &mut Tx<'_>, project: &str, l: &Layout, by: &str) -> ApiResult<()> {
    use super::pages::{self, PageInput, Widget, WidgetKind};
    let slug = pages::dashboard_slug(t, project)?;
    let old = pages::list(t, project)?
        .into_iter()
        .find(|p| p.slug == slug);
    let mut widgets = old
        .as_ref()
        .map(|p| {
            p.content
                .widgets
                .iter()
                .filter(|w| {
                    w.kind.is_panel()
                        || matches!(
                            w.kind,
                            WidgetKind::Actions
                                | WidgetKind::MonitorSummary
                                | WidgetKind::JobQueue
                                | WidgetKind::EtaAccuracy
                                | WidgetKind::Wiki
                        )
                })
                .cloned()
                .collect::<Vec<_>>()
        })
        .unwrap_or_default();
    for widget in &mut widgets {
        if l.blocks.iter().any(|b| b.id == widget.id) {
            widget.id = ulid::Ulid::new().to_string();
        }
    }
    let mut y = widgets.iter().map(|w| w.y + w.h).max().unwrap_or(0);
    let mut x = 0;
    for b in &l.blocks {
        let w = match b.size {
            layout::Size::S => 3,
            layout::Size::M => 6,
            layout::Size::L => 12,
        };
        if x + w > 12 {
            x = 0;
            y += 4;
        }
        let mut v = serde_json::to_value(b)?;
        let o = v
            .as_object_mut()
            .ok_or_else(|| ApiError::internal("invalid block"))?;
        o.remove("size");
        if b.kind == layout::BlockKind::Note {
            o.insert("kind".into(), json!("markdown"));
        }
        for (k, n) in [("x", x), ("y", y), ("w", w), ("h", 4)] {
            o.insert(k.into(), json!(n));
        }
        widgets.push(serde_json::from_value::<Widget>(v)?);
        x += w;
    }
    let input = PageInput {
        title: old
            .as_ref()
            .map(|p| p.content.title.clone())
            .unwrap_or_else(|| "Dashboard".into()),
        position: old.as_ref().map(|p| p.content.position).unwrap_or(0),
        widgets,
    };
    pages::set(t, project, &slug, &input, by)?;
    Ok(())
}

/// Check and save the layout of a project's dashboard.
pub fn put_layout(t: &mut Tx<'_>, project: &str, l: &Layout, actor: &str) -> ApiResult<()> {
    projects::get(t, project)?;
    let kinds = kinds(t, project)?;
    layout::check(l, &kinds).map_err(|(f, m)| field_error(&f, m))?;
    write_layout(t, project, l, actor)?;
    t.audit(
        "dashboard.update",
        project,
        Some(project),
        json!({"blocks": l.blocks.len()}),
    )?;
    t.emit(
        EventDraft::new(
            "dashboard.updated",
            format!("dashboard of {project} saved ({} blocks)", l.blocks.len()),
            json!({"project": project, "blocks": l.blocks.len()}),
        )
        .project(project.to_owned()),
    )?;
    Ok(())
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;

    fn def(v: Value) -> MonitorDef {
        serde_json::from_value(v).unwrap()
    }

    fn base() -> Value {
        json!({"name": "load", "target": "box", "script": "cat /proc/loadavg",
               "schedule": {"kind": "interval", "every": "30s"}, "kind": "number"})
    }

    #[test]
    fn defaults_and_validation() {
        let d = def(base());
        assert_eq!(d.timeout_secs, DEFAULT_TIMEOUT);
        assert_eq!(d.retention_days, DEFAULT_RETENTION_DAYS);
        assert!(d.enabled);
        assert_eq!(d.direction, Direction::Above);
        validate(&d).unwrap();
        let field = |patch: Value| {
            let mut v = base();
            for (k, x) in patch.as_object().unwrap() {
                v[k] = x.clone();
            }
            let e = validate(&def(v)).unwrap_err();
            e.details.unwrap()["field"].as_str().unwrap().to_owned()
        };
        assert_eq!(field(json!({"name": "Bad Name"})), "name");
        assert_eq!(field(json!({"script": "  "})), "script");
        assert_eq!(
            field(json!({"script": "x".repeat(MAX_SCRIPT + 1)})),
            "script"
        );
        assert_eq!(
            field(json!({"schedule": {"kind": "interval", "every": "10s"}})),
            "schedule.every"
        );
        assert_eq!(
            field(json!({"schedule": {"kind": "cron", "expr": "* * *"}})),
            "schedule.expr"
        );
        assert_eq!(
            field(json!({"schedule": {"kind": "at", "at": "2099-01-01T00:00:00Z"}})),
            "schedule"
        );
        assert_eq!(field(json!({"timeout_secs": 0})), "timeout_secs");
        assert_eq!(field(json!({"timeout_secs": 601})), "timeout_secs");
        assert_eq!(field(json!({"retention_days": 91})), "retention_days");
        assert_eq!(field(json!({"warn": 9, "crit": 1})), "warn");
        assert_eq!(field(json!({"kind": "status", "warn": 1})), "warn");
        // 15 s is the shortest interval; a cron spec is fine.
        validate(&def(
            json!({"name": "a", "target": "t", "script": "x", "kind": "text",
                              "schedule": {"kind": "interval", "every": "15s"}}),
        ))
        .unwrap();
        validate(&def(
            json!({"name": "a", "target": "t", "script": "x", "kind": "text",
                              "schedule": {"kind": "cron", "expr": "*/5 * * * *"}}),
        ))
        .unwrap();
    }

    #[test]
    fn periods() {
        let now = Utc::now();
        assert_eq!(
            period_secs(&ScheduleSpec::Interval { every: "1m".into() }, now),
            60
        );
        assert_eq!(
            period_secs(
                &ScheduleSpec::Interval {
                    every: "15s".into()
                },
                now
            ),
            15
        );
        let cron = ScheduleSpec::Cron {
            expr: "*/5 * * * *".into(),
            tz: "UTC".into(),
        };
        assert_eq!(period_secs(&cron, now), 300);
    }
}
