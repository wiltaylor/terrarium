//! Pipelines (docs/api.md §31): the CI/CD pipelines of a project, and
//! their recent runs.
//!
//! A pipeline reads the runs of a CI/CD system on a schedule through an
//! adapter: `github` (GitHub Actions) or `script` (a user script on a
//! target that prints the JSON of `crate::pipelines::contract`). The
//! scheduler task finds the due pipelines with [`due`] and moves them on
//! with [`advance`], as for the monitors; the poll itself is
//! `crate::pipelines`. [`record`] keeps the result of one poll.
//!
//! Events: `pipeline.run` when a run is new or its status changed (not on
//! the first good poll, which only learns the history), `pipeline.status`
//! when the status of the pipeline (the status of its newest run)
//! changed, and `pipeline.error` when a poll fails after one that did
//! not.

use std::collections::{BTreeMap, HashMap};

use chrono::{DateTime, Utc};
use rusqlite::{Connection, OptionalExtension, Row, params};
use serde::{Deserialize, Serialize};
use serde_json::json;
use terrarium_core::schedule::ScheduleSpec;

use crate::db::Tx;
use crate::error::{ApiError, ApiResult};
use crate::events::EventDraft;
use crate::pipelines::contract::{self, RunInfo, RunStatus, Step};
use crate::pipelines::github;
use crate::store::schedules::{self, field_error, stamp};
use crate::store::{projects, valid_name};

/// The shortest interval of a pipeline, in seconds.
pub const MIN_INTERVAL_SECS: i64 = 30;
/// The poll limit of a script without `timeout_secs`.
pub const DEFAULT_TIMEOUT: u64 = 60;
/// The highest `timeout_secs` (10 minutes).
pub const MAX_TIMEOUT: u64 = 600;
/// The runs that a pipeline keeps by default.
pub const DEFAULT_MAX_RUNS: i64 = 20;
/// The highest `max_runs`.
pub const MAX_MAX_RUNS: i64 = 100;
/// The longest script, in bytes.
pub const MAX_SCRIPT: usize = 64 * 1024;
/// The most pipelines of one project.
pub const MAX_PER_PROJECT: usize = 50;
/// The most secrets in the environment of a script.
pub const MAX_ENV_SECRETS: usize = 20;

fn default_schedule() -> ScheduleSpec {
    ScheduleSpec::Interval { every: "5m".into() }
}
fn default_timeout() -> u64 {
    DEFAULT_TIMEOUT
}
fn default_max_runs() -> i64 {
    DEFAULT_MAX_RUNS
}
fn yes() -> bool {
    true
}

/// A `github` source: the runs of GitHub Actions.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct GithubSource {
    /// `owner/name`.
    pub repo: String,
    /// One workflow: its file name (`ci.yml`) or its ID. Absent: the
    /// runs of every workflow.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub workflow: Option<String>,
    /// The runs of one branch only.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub branch: Option<String>,
    /// The secret whose main value is the token: the project's secret,
    /// else the global one.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub token_secret: Option<String>,
    /// The API base URL: `https://api.github.com` by default; GitHub
    /// Enterprise Server is `https://<host>/api/v3`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub api_url: Option<String>,
}

/// A `script` source: a script on a target that prints the runs as JSON
/// (docs/api.md §31.2).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ScriptSource {
    /// A global target, or a target of the project.
    pub target: String,
    /// Shell text; the target runs it with `sh -c`.
    pub script: String,
    #[serde(default = "default_timeout")]
    pub timeout_secs: u64,
    /// Environment variable → secret name: the main value of each secret
    /// goes into the environment of the script.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub env_secrets: BTreeMap<String, String>,
}

/// Where the runs of a pipeline come from.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum Source {
    Github(GithubSource),
    Script(ScriptSource),
}

impl Source {
    pub fn kind(&self) -> &'static str {
        match self {
            Self::Github(_) => "github",
            Self::Script(_) => "script",
        }
    }

    /// The target of a script source.
    pub fn target(&self) -> Option<&str> {
        match self {
            Self::Script(s) => Some(&s.target),
            Self::Github(_) => None,
        }
    }

    /// Each secret that the source names, with its field.
    pub fn secrets(&self) -> Vec<(String, &str)> {
        match self {
            Self::Github(g) => g
                .token_secret
                .iter()
                .map(|n| ("source.token_secret".to_owned(), n.as_str()))
                .collect(),
            Self::Script(s) => s
                .env_secrets
                .iter()
                .map(|(k, n)| (format!("source.env_secrets.{k}"), n.as_str()))
                .collect(),
        }
    }
}

/// The fields of a pipeline that a client writes.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PipelineDef {
    pub name: String,
    #[serde(default)]
    pub description: Option<String>,
    pub source: Source,
    /// `{"kind": "interval", "every": "5m"}` (the default) or a cron spec.
    #[serde(default = "default_schedule")]
    pub schedule: ScheduleSpec,
    #[serde(default = "yes")]
    pub enabled: bool,
    /// The runs to keep, newest first.
    #[serde(default = "default_max_runs")]
    pub max_runs: i64,
}

/// One run of a pipeline, as the store keeps it.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct Run {
    /// The adapter's ID of the run.
    pub id: String,
    pub pipeline: String,
    pub name: Option<String>,
    pub status: RunStatus,
    pub url: Option<String>,
    pub branch: Option<String>,
    pub commit: Option<String>,
    pub actor: Option<String>,
    pub event: Option<String>,
    pub environment: Option<String>,
    pub started_at: Option<String>,
    pub finished_at: Option<String>,
    pub summary: Option<String>,
    /// `null`: the steps were not read yet.
    pub steps: Option<Vec<Step>>,
    /// When Terrarium first saw the run.
    pub first_seen: String,
    /// When its status last changed.
    pub changed_at: String,
}

/// A pipeline as the API shows it.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct Pipeline {
    pub id: String,
    pub project: String,
    #[serde(flatten)]
    pub def: PipelineDef,
    pub period_secs: i64,
    pub created_by: String,
    /// The actor whose rights each poll checks again.
    pub run_as: String,
    pub created_at: String,
    pub updated_at: String,
    pub next_run_at: Option<String>,
    pub skipped: i64,
    /// The status of the newest run; `null` before the first good poll
    /// or with no runs.
    pub status: Option<RunStatus>,
    /// The time of the last poll, good or not.
    pub polled_at: Option<String>,
    /// The time of the last good poll.
    pub ok_at: Option<String>,
    /// The error of the last poll; `null` when it was good.
    pub error: Option<String>,
    /// The newest run.
    pub latest: Option<Run>,
    /// A poll is in progress now.
    pub running: bool,
}

impl Pipeline {
    /// `project/name`.
    pub fn key(&self) -> String {
        format!("{}/{}", self.project, self.def.name)
    }
}

// ------------------------------------------------------------------ checks

/// The seconds between two polls of `spec`.
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

fn env_name_ok(k: &str) -> bool {
    let mut cs = k.chars();
    matches!(cs.next(), Some(c) if c.is_ascii_uppercase() || c == '_')
        && cs.all(|c| c.is_ascii_uppercase() || c.is_ascii_digit() || c == '_')
        && k.len() <= 64
        && !k.starts_with("TERRARIUM_")
}

fn short(field: &str, v: &Option<String>, max: usize) -> ApiResult<()> {
    match v {
        Some(s) if s.trim().is_empty() => Err(field_error(field, "empty; leave it out")),
        Some(s) if s.chars().count() > max => {
            Err(field_error(field, format!("longer than {max} characters")))
        }
        _ => Ok(()),
    }
}

/// Check the fields. The error names the field.
pub fn validate(d: &PipelineDef) -> ApiResult<()> {
    if !valid_name(&d.name) {
        return Err(field_error(
            "name",
            format!(
                "bad pipeline name `{}`: use 1-63 of a-z, 0-9, `-`, `_`, starting with a letter or digit",
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
    match &d.source {
        Source::Github(g) => {
            github::check_repo(&g.repo).map_err(|e| field_error("source.repo", e))?;
            short("source.workflow", &g.workflow, 200)?;
            if g.workflow.as_ref().is_some_and(|w| w.contains('/')) {
                return Err(field_error(
                    "source.workflow",
                    "give the file name of the workflow (`ci.yml`) or its ID, not a path",
                ));
            }
            short("source.branch", &g.branch, 200)?;
            short("source.token_secret", &g.token_secret, 100)?;
            if let Some(u) = &g.api_url {
                github::check_api(u).map_err(|e| field_error("source.api_url", e))?;
            }
        }
        Source::Script(s) => {
            if s.target.trim().is_empty() {
                return Err(field_error("source.target", "name a target"));
            }
            if s.script.trim().is_empty() {
                return Err(field_error("source.script", "the script is empty"));
            }
            if s.script.len() > MAX_SCRIPT {
                return Err(field_error(
                    "source.script",
                    format!("{} bytes; a script is at most {MAX_SCRIPT}", s.script.len()),
                ));
            }
            if s.timeout_secs == 0 || s.timeout_secs > MAX_TIMEOUT {
                return Err(field_error(
                    "source.timeout_secs",
                    format!("{} is not 1-{MAX_TIMEOUT}", s.timeout_secs),
                ));
            }
            if s.env_secrets.len() > MAX_ENV_SECRETS {
                return Err(field_error(
                    "source.env_secrets",
                    format!("at most {MAX_ENV_SECRETS} secrets"),
                ));
            }
            for (k, n) in &s.env_secrets {
                if !env_name_ok(k) {
                    return Err(field_error(
                        "source.env_secrets",
                        format!(
                            "`{}` is not a variable name: use A-Z, 0-9, `_`, not starting with a digit or TERRARIUM_",
                            contract::cut(k, 64)
                        ),
                    ));
                }
                if n.trim().is_empty() {
                    return Err(field_error(
                        &format!("source.env_secrets.{k}"),
                        "name a secret",
                    ));
                }
            }
        }
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
                "a pipeline polls on an interval or a cron spec",
            ));
        }
    }
    if !(1..=MAX_MAX_RUNS).contains(&d.max_runs) {
        return Err(field_error(
            "max_runs",
            format!("{} is not 1-{MAX_MAX_RUNS}", d.max_runs),
        ));
    }
    Ok(())
}

// -------------------------------------------------------------------- rows

const COLS: &str = "id, project, def, created_by, run_as, created_at, updated_at, \
                    next_run_at, skipped, status, polled_at, ok_at, error";

struct RawRow {
    p: Pipeline,
    def: String,
    status: Option<String>,
}

fn row(r: &Row<'_>) -> rusqlite::Result<RawRow> {
    Ok(RawRow {
        p: Pipeline {
            id: r.get(0)?,
            project: r.get(1)?,
            def: PipelineDef {
                name: String::new(),
                description: None,
                source: Source::Script(ScriptSource {
                    target: String::new(),
                    script: String::new(),
                    timeout_secs: DEFAULT_TIMEOUT,
                    env_secrets: BTreeMap::new(),
                }),
                schedule: default_schedule(),
                enabled: true,
                max_runs: DEFAULT_MAX_RUNS,
            },
            period_secs: 0,
            created_by: r.get(3)?,
            run_as: r.get(4)?,
            created_at: r.get(5)?,
            updated_at: r.get(6)?,
            next_run_at: r.get(7)?,
            skipped: r.get(8)?,
            status: None,
            polled_at: r.get(10)?,
            ok_at: r.get(11)?,
            error: r.get(12)?,
            latest: None,
            running: false,
        },
        def: r.get(2)?,
        status: r.get(9)?,
    })
}

fn finish(conn: &Connection, raw: RawRow) -> ApiResult<Pipeline> {
    let mut p = raw.p;
    p.def = serde_json::from_str(&raw.def)
        .map_err(|e| ApiError::internal(format!("pipeline {} has a bad row: {e}", p.id)))?;
    p.status = raw.status.as_deref().and_then(RunStatus::parse);
    p.period_secs = period_secs(&p.def.schedule, Utc::now());
    p.latest = runs(conn, &p.id, 1)?.into_iter().next();
    Ok(p)
}

fn query(conn: &Connection, sql: &str, args: &[&dyn rusqlite::ToSql]) -> ApiResult<Vec<Pipeline>> {
    let mut st = conn.prepare(sql)?;
    let rows = st.query_map(args, row)?.collect::<Result<Vec<_>, _>>()?;
    rows.into_iter().map(|r| finish(conn, r)).collect()
}

/// A pipeline by its ULID.
pub fn get(conn: &Connection, id: &str) -> ApiResult<Pipeline> {
    query(
        conn,
        &format!("SELECT {COLS} FROM pipelines WHERE id = ?1"),
        &[&id],
    )?
    .into_iter()
    .next()
    .ok_or_else(|| ApiError::not_found(format!("no pipeline `{id}`")))
}

/// A pipeline by its ULID or `project/name`.
pub fn find(conn: &Connection, key: &str) -> ApiResult<Pipeline> {
    if let Some((p, n)) = key.split_once('/') {
        return query(
            conn,
            &format!("SELECT {COLS} FROM pipelines WHERE project = ?1 AND name = ?2"),
            &[&p, &n],
        )?
        .into_iter()
        .next()
        .ok_or_else(|| ApiError::not_found(format!("no pipeline `{n}` in project `{p}`")));
    }
    get(conn, key)
}

/// The pipelines of a project, by name.
pub fn list(conn: &Connection, project: &str) -> ApiResult<Vec<Pipeline>> {
    query(
        conn,
        &format!("SELECT {COLS} FROM pipelines WHERE project = ?1 ORDER BY name"),
        &[&project],
    )
}

/// The enabled pipelines whose next poll is at or before `now`.
pub fn due(conn: &Connection, now: DateTime<Utc>) -> ApiResult<Vec<Pipeline>> {
    let now = stamp(now);
    query(
        conn,
        &format!(
            "SELECT {COLS} FROM pipelines WHERE enabled = 1 AND next_run_at IS NOT NULL
             AND next_run_at <= ?1 ORDER BY next_run_at"
        ),
        &[&now],
    )
}

fn anchor_of(conn: &Connection, id: &str) -> ApiResult<DateTime<Utc>> {
    let a: String = conn.query_row("SELECT anchor_at FROM pipelines WHERE id = ?1", [id], |r| {
        r.get(0)
    })?;
    Ok(DateTime::parse_from_rfc3339(&a)
        .map(|t| t.with_timezone(&Utc))
        .unwrap_or_else(|_| Utc::now()))
}

/// Move the next poll of pipeline `id` past `now`.
pub fn advance(t: &Tx<'_>, id: &str, now: DateTime<Utc>) -> ApiResult<()> {
    let p = get(t, id)?;
    let next =
        schedules::next_after_min(&p.def.schedule, anchor_of(t, id)?, now, MIN_INTERVAL_SECS)
            .ok()
            .flatten()
            .map(stamp);
    t.execute(
        "UPDATE pipelines SET next_run_at = ?2 WHERE id = ?1",
        params![id, next],
    )?;
    Ok(())
}

/// Count a tick that found the poll of pipeline `id` still running.
pub fn add_skip(t: &Tx<'_>, id: &str) -> ApiResult<()> {
    t.execute(
        "UPDATE pipelines SET skipped = skipped + 1 WHERE id = ?1",
        [id],
    )?;
    Ok(())
}

fn def_json(def: &PipelineDef) -> ApiResult<String> {
    serde_json::to_string(def).map_err(|e| ApiError::internal(e.to_string()))
}

/// The source fields for the audit log: no script body past 200
/// characters, no secret value (the source holds only secret names).
fn audit_source(s: &Source) -> serde_json::Value {
    match s {
        Source::Github(g) => serde_json::to_value(g).unwrap_or_default(),
        Source::Script(sc) => json!({"target": sc.target,
            "script": contract::cut(&sc.script, 200),
            "env_secrets": sc.env_secrets}),
    }
}

fn name_taken(conn: &Connection, project: &str, name: &str, not: &str) -> ApiResult<bool> {
    Ok(conn
        .query_row(
            "SELECT id FROM pipelines WHERE project = ?1 AND name = ?2 AND id != ?3",
            params![project, name, not],
            |r| r.get::<_, String>(0),
        )
        .optional()?
        .is_some())
}

/// Create a pipeline. The caller checked the rights, the target, and the
/// secrets.
pub fn create(
    t: &mut Tx<'_>,
    project: &str,
    def: &PipelineDef,
    actor: &str,
    now: DateTime<Utc>,
) -> ApiResult<Pipeline> {
    projects::get(t, project)?;
    validate(def)?;
    let n: i64 = t.query_row(
        "SELECT COUNT(*) FROM pipelines WHERE project = ?1",
        [project],
        |r| r.get(0),
    )?;
    if n as usize >= MAX_PER_PROJECT {
        return Err(ApiError::conflict(format!(
            "project `{project}` has {n} pipelines; the limit is {MAX_PER_PROJECT}"
        )));
    }
    if name_taken(t, project, &def.name, "")? {
        return Err(ApiError::conflict(format!(
            "project `{project}` has a pipeline named `{}`",
            def.name
        ))
        .with_details(json!({"field": "name"})));
    }
    let id = ulid::Ulid::new().to_string();
    // The first poll comes at once: a new pipeline shows its runs soon.
    let next = def.enabled.then(|| stamp(now));
    let at = stamp(now);
    t.execute(
        "INSERT INTO pipelines (id, project, name, adapter, enabled, def, created_by, run_as,
                                created_at, updated_at, anchor_at, next_run_at)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?7, ?8, ?8, ?8, ?9)",
        params![
            id,
            project,
            def.name,
            def.source.kind(),
            def.enabled,
            def_json(def)?,
            actor,
            at,
            next
        ],
    )?;
    let p = get(t, &id)?;
    t.audit(
        "pipeline.create",
        &p.key(),
        Some(project),
        json!({"id": p.id, "adapter": def.source.kind(), "source": audit_source(&def.source),
               "schedule": def.schedule}),
    )?;
    Ok(p)
}

/// Does the change of `old` to `new` move the rights the polls use: a
/// new adapter, target, script, repo, API, or secret?
pub fn takes_over(old: &PipelineDef, new: &PipelineDef) -> bool {
    match (&old.source, &new.source) {
        (Source::Github(a), Source::Github(b)) => {
            a.token_secret != b.token_secret || a.api_url != b.api_url
        }
        (Source::Script(a), Source::Script(b)) => {
            a.target != b.target || a.script != b.script || a.env_secrets != b.env_secrets
        }
        _ => true,
    }
}

/// Replace the fields of pipeline `id`. A new schedule, a new source, or
/// a pipeline that is enabled again polls at once. A change of the source
/// that [`takes_over`] makes the caller the `run_as`. A new adapter or
/// repo drops the runs.
pub fn update(
    t: &mut Tx<'_>,
    id: &str,
    def: &PipelineDef,
    actor: &str,
    now: DateTime<Utc>,
) -> ApiResult<Pipeline> {
    validate(def)?;
    let old = get(t, id)?;
    if def.name != old.def.name && name_taken(t, &old.project, &def.name, id)? {
        return Err(ApiError::conflict(format!(
            "project `{}` has a pipeline named `{}`",
            old.project, def.name
        ))
        .with_details(json!({"field": "name"})));
    }
    let at = stamp(now);
    let new_source = def.source != old.def.source;
    let restart =
        def.schedule != old.def.schedule || new_source || (def.enabled && !old.def.enabled);
    t.execute(
        "UPDATE pipelines SET name = ?2, adapter = ?3, enabled = ?4, def = ?5, updated_at = ?6
         WHERE id = ?1",
        params![
            id,
            def.name,
            def.source.kind(),
            def.enabled,
            def_json(def)?,
            at
        ],
    )?;
    if takes_over(&old.def, def) {
        t.execute(
            "UPDATE pipelines SET run_as = ?2 WHERE id = ?1",
            params![id, actor],
        )?;
    }
    let other_runs = match (&old.def.source, &def.source) {
        (Source::Github(a), Source::Github(b)) => {
            a.repo != b.repo
                || a.api_url != b.api_url
                || a.workflow != b.workflow
                || a.branch != b.branch
        }
        (Source::Script(_), Source::Script(_)) => false,
        _ => true,
    };
    if other_runs {
        t.execute("DELETE FROM pipeline_runs WHERE pipeline = ?1", [id])?;
        t.execute(
            "UPDATE pipelines SET status = NULL, ok_at = NULL, error = NULL, polled_at = NULL
             WHERE id = ?1",
            [id],
        )?;
    }
    if !def.enabled {
        t.execute(
            "UPDATE pipelines SET next_run_at = NULL WHERE id = ?1",
            [id],
        )?;
    } else if restart || old.next_run_at.is_none() {
        t.execute(
            "UPDATE pipelines SET anchor_at = ?2, next_run_at = ?2 WHERE id = ?1",
            params![id, at],
        )?;
    }
    if def.max_runs < old.def.max_runs {
        prune(t, id, def.max_runs)?;
    }
    let p = get(t, id)?;
    let mut changed: Vec<&str> = Vec::new();
    let (a, b) = (&old.def, def);
    for (name, diff) in [
        ("name", a.name != b.name),
        ("description", a.description != b.description),
        ("source", new_source),
        ("schedule", a.schedule != b.schedule),
        ("enabled", a.enabled != b.enabled),
        ("max_runs", a.max_runs != b.max_runs),
    ] {
        if diff {
            changed.push(name);
        }
    }
    t.audit(
        "pipeline.update",
        &p.key(),
        Some(&p.project),
        json!({"id": id, "changed": changed, "enabled": def.enabled,
               "source": new_source.then(|| audit_source(&def.source))}),
    )?;
    Ok(p)
}

/// Delete pipeline `id` and its runs.
pub fn delete(t: &mut Tx<'_>, id: &str) -> ApiResult<Pipeline> {
    let p = get(t, id)?;
    t.execute("DELETE FROM pipelines WHERE id = ?1", [id])?;
    t.audit(
        "pipeline.delete",
        &p.key(),
        Some(&p.project),
        json!({"id": id, "adapter": p.def.source.kind()}),
    )?;
    Ok(p)
}

// -------------------------------------------------------------------- runs

const RCOLS: &str = "run_id, pipeline, name, status, url, branch, commit_sha, actor, event, \
                     environment, started_at, finished_at, summary, steps, first_seen, changed_at";
const ORDER: &str = "ORDER BY sort_at DESC, ord ASC, pk DESC";

fn run_row(r: &Row<'_>) -> rusqlite::Result<Run> {
    let status: String = r.get(3)?;
    let steps: Option<String> = r.get(13)?;
    Ok(Run {
        id: r.get(0)?,
        pipeline: r.get(1)?,
        name: r.get(2)?,
        status: RunStatus::parse(&status).unwrap_or(RunStatus::Unknown),
        url: r.get(4)?,
        branch: r.get(5)?,
        commit: r.get(6)?,
        actor: r.get(7)?,
        event: r.get(8)?,
        environment: r.get(9)?,
        started_at: r.get(10)?,
        finished_at: r.get(11)?,
        summary: r.get(12)?,
        steps: steps.and_then(|s| serde_json::from_str(&s).ok()),
        first_seen: r.get(14)?,
        changed_at: r.get(15)?,
    })
}

/// The newest `limit` runs of pipeline `id`, newest first.
pub fn runs(conn: &Connection, id: &str, limit: usize) -> ApiResult<Vec<Run>> {
    let mut st = conn.prepare(&format!(
        "SELECT {RCOLS} FROM pipeline_runs WHERE pipeline = ?1 {ORDER} LIMIT ?2"
    ))?;
    let rows = st
        .query_map(params![id, limit as i64], run_row)?
        .collect::<Result<Vec<_>, _>>()?;
    Ok(rows)
}

/// One run of pipeline `id`.
pub fn run(conn: &Connection, id: &str, run_id: &str) -> ApiResult<Run> {
    conn.query_row(
        &format!("SELECT {RCOLS} FROM pipeline_runs WHERE pipeline = ?1 AND run_id = ?2"),
        params![id, run_id],
        run_row,
    )
    .optional()?
    .ok_or_else(|| ApiError::not_found(format!("pipeline `{id}` has no run `{run_id}`")))
}

/// The runs that the store knows: the ID to the status, and whether it
/// has the steps.
pub fn known(conn: &Connection, id: &str) -> ApiResult<HashMap<String, (RunStatus, bool)>> {
    let mut st = conn.prepare(
        "SELECT run_id, status, steps IS NOT NULL FROM pipeline_runs WHERE pipeline = ?1",
    )?;
    let rows = st
        .query_map([id], |r| {
            let s: String = r.get(1)?;
            Ok((
                r.get::<_, String>(0)?,
                (
                    RunStatus::parse(&s).unwrap_or(RunStatus::Unknown),
                    r.get::<_, bool>(2)?,
                ),
            ))
        })?
        .collect::<Result<HashMap<_, _>, _>>()?;
    Ok(rows)
}

/// Keep the steps of one run (the run detail reads them on demand).
pub fn set_steps(t: &Tx<'_>, id: &str, run_id: &str, steps: &[Step]) -> ApiResult<()> {
    let steps = redact_steps(t, steps.to_vec());
    t.execute(
        "UPDATE pipeline_runs SET steps = ?3 WHERE pipeline = ?1 AND run_id = ?2",
        params![
            id,
            run_id,
            serde_json::to_string(&steps).map_err(|e| ApiError::internal(e.to_string()))?
        ],
    )?;
    Ok(())
}

/// Delete the runs of pipeline `id` past the newest `keep`.
pub fn prune(t: &Tx<'_>, id: &str, keep: i64) -> ApiResult<usize> {
    Ok(t.execute(
        &format!(
            "DELETE FROM pipeline_runs WHERE pipeline = ?1 AND pk NOT IN (
                 SELECT pk FROM pipeline_runs WHERE pipeline = ?1 {ORDER} LIMIT ?2)"
        ),
        params![id, keep.clamp(1, MAX_MAX_RUNS)],
    )?)
}

fn redact_opt(t: &Tx<'_>, s: Option<String>) -> Option<String> {
    s.map(|s| t.redact_string(s))
}

fn redact_steps(t: &Tx<'_>, steps: Vec<Step>) -> Vec<Step> {
    steps
        .into_iter()
        .map(|mut s| {
            s.name = t.redact_string(s.name);
            s.summary = redact_opt(t, s.summary);
            s
        })
        .collect()
}

/// What one poll changed.
#[derive(Debug, Clone, Default, PartialEq, Serialize)]
pub struct PollSummary {
    pub runs: usize,
    pub new: usize,
    pub changed: usize,
    pub status: Option<RunStatus>,
    pub error: Option<String>,
}

fn run_label(r: &RunInfo) -> String {
    r.name.clone().unwrap_or_else(|| r.id.clone())
}

/// Keep the result of one poll of pipeline `id` at `at`: the runs (newest
/// first, as the adapter gives them) or the error. Emits the events of
/// the module doc, and prunes past `max_runs`.
pub fn record(
    t: &mut Tx<'_>,
    id: &str,
    outcome: Result<Vec<RunInfo>, String>,
    at: DateTime<Utc>,
) -> ApiResult<PollSummary> {
    let p = get(t, id)?;
    let at_s = stamp(at);
    let base = json!({"project": p.project, "pipeline": p.def.name, "pipeline_id": id});
    let data = |extra: serde_json::Value| {
        let mut b = base.clone();
        if let (Some(o), serde_json::Value::Object(e)) = (b.as_object_mut(), extra) {
            o.extend(e);
        }
        b
    };
    let runs = match outcome {
        Err(e) => {
            let e = t.redact_string(contract::cut(&e, 2000));
            t.execute(
                "UPDATE pipelines SET polled_at = ?2, error = ?3 WHERE id = ?1",
                params![id, at_s, e],
            )?;
            if p.error.is_none() {
                t.emit(
                    EventDraft::new(
                        "pipeline.error",
                        format!(
                            "pipeline {}: the poll failed: {}",
                            p.def.name,
                            contract::cut(&e, 200)
                        ),
                        data(json!({"error": e})),
                    )
                    .project(p.project.clone()),
                )?;
            }
            return Ok(PollSummary {
                status: p.status,
                error: Some(e),
                ..Default::default()
            });
        }
        Ok(r) => r,
    };
    let first = p.ok_at.is_none();
    let known = known(t, id)?;
    let keep = usize::try_from(p.def.max_runs).unwrap_or(20);
    let mut sum = PollSummary {
        runs: runs.len().min(keep),
        ..Default::default()
    };
    for (ord, r) in runs.iter().take(keep).enumerate() {
        let status = r.status();
        let before = known.get(&r.id).map(|(s, _)| *s);
        let sort_at = r.started_at.clone().unwrap_or_else(|| at_s.clone());
        let steps = match &r.steps {
            Some(s) => Some(
                serde_json::to_string(&redact_steps(t, s.clone()))
                    .map_err(|e| ApiError::internal(e.to_string()))?,
            ),
            None => None,
        };
        let name = redact_opt(t, r.name.clone());
        let summary = redact_opt(t, r.summary.clone());
        t.execute(
            "INSERT INTO pipeline_runs (pipeline, run_id, name, status, url, branch, commit_sha,
                 actor, event, environment, started_at, finished_at, summary, steps, sort_at, ord,
                 first_seen, changed_at, seen_at)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?14, ?15, ?16,
                     ?17, ?17, ?17)
             ON CONFLICT(pipeline, run_id) DO UPDATE SET
                 name = ?3, status = ?4, url = ?5, branch = ?6, commit_sha = ?7, actor = ?8,
                 event = ?9, environment = ?10, started_at = ?11, finished_at = ?12,
                 summary = ?13, steps = COALESCE(?14, steps),
                 sort_at = CASE WHEN ?11 IS NULL THEN sort_at ELSE ?15 END, ord = ?16,
                 changed_at = CASE WHEN status = ?4 THEN changed_at ELSE ?17 END, seen_at = ?17",
            params![
                id,
                r.id,
                name,
                status.as_str(),
                r.url,
                r.branch,
                r.commit,
                r.actor,
                r.event,
                r.environment,
                r.started_at,
                r.finished_at,
                summary,
                steps,
                sort_at,
                ord as i64,
                at_s
            ],
        )?;
        match before {
            None => sum.new += 1,
            Some(b) if b != status => sum.changed += 1,
            Some(_) => continue,
        }
        if first {
            continue;
        }
        let from = before.map(RunStatus::as_str);
        t.emit(
            EventDraft::new(
                "pipeline.run",
                format!(
                    "pipeline {}: {} {}{}",
                    p.def.name,
                    run_label(r),
                    from.map(|f| format!("{f} → ")).unwrap_or_default(),
                    status.as_str()
                ),
                data(json!({"run_id": r.id, "name": name, "from": from,
                            "to": status.as_str(), "url": r.url, "branch": r.branch,
                            "commit": r.commit, "environment": r.environment,
                            "summary": summary.as_deref().map(|s| contract::cut(s, 200))})),
            )
            .project(p.project.clone()),
        )?;
    }
    prune(t, id, p.def.max_runs)?;
    let newest = runs.first();
    let status = newest.map(RunInfo::status);
    sum.status = status;
    t.execute(
        "UPDATE pipelines SET polled_at = ?2, ok_at = ?2, error = NULL, status = ?3 WHERE id = ?1",
        params![id, at_s, status.map(RunStatus::as_str)],
    )?;
    if status != p.status && status.is_some() {
        let from = p.status.map(RunStatus::as_str);
        let to = status.map(RunStatus::as_str).unwrap_or("unknown");
        let label = newest.map(run_label).unwrap_or_default();
        t.emit(
            EventDraft::new(
                "pipeline.status",
                format!(
                    "pipeline {}: {} → {to} ({label})",
                    p.def.name,
                    from.unwrap_or("none")
                ),
                data(json!({"from": from, "to": to,
                            "run_id": newest.map(|r| r.id.clone()),
                            "run_name": newest.and_then(|r| r.name.clone()),
                            "url": newest.and_then(|r| r.url.clone()),
                            "branch": newest.and_then(|r| r.branch.clone())})),
            )
            .project(p.project.clone()),
        )?;
    }
    Ok(sum)
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;
    use serde_json::{Value, json};

    fn def(v: Value) -> PipelineDef {
        serde_json::from_value(v).unwrap()
    }

    fn gh() -> Value {
        json!({"name": "ci", "source": {"kind": "github", "repo": "wiltaylor/terrarium",
               "workflow": "ci.yml", "token_secret": "gh-token"}})
    }

    fn sc() -> Value {
        json!({"name": "deploy", "source": {"kind": "script", "target": "wsl",
               "script": "python3 octo.py", "env_secrets": {"OCTOPUS_API_KEY": "octo-key"}}})
    }

    #[test]
    fn defaults_and_shapes() {
        let d = def(gh());
        assert_eq!(d.schedule, ScheduleSpec::Interval { every: "5m".into() });
        assert_eq!(d.max_runs, DEFAULT_MAX_RUNS);
        assert!(d.enabled);
        assert_eq!(d.source.kind(), "github");
        assert_eq!(
            d.source.secrets(),
            vec![("source.token_secret".into(), "gh-token")]
        );
        validate(&d).unwrap();
        let s = def(sc());
        let Source::Script(x) = &s.source else {
            panic!()
        };
        assert_eq!(x.timeout_secs, DEFAULT_TIMEOUT);
        assert_eq!(s.source.target(), Some("wsl"));
        validate(&s).unwrap();
        // The JSON round trip keeps the tag.
        let v = serde_json::to_value(&s).unwrap();
        assert_eq!(v["source"]["kind"], "script");
        assert_eq!(def(v), s);
        // Unknown keys and kinds fail.
        let mut bad = gh();
        bad["source"]["colour"] = json!("red");
        assert!(serde_json::from_value::<PipelineDef>(bad).is_err());
        let mut bad = gh();
        bad["source"]["kind"] = json!("jenkins");
        assert!(serde_json::from_value::<PipelineDef>(bad).is_err());
        let mut bad = gh();
        bad["colour"] = json!("red");
        assert!(serde_json::from_value::<PipelineDef>(bad).is_err());
    }

    #[test]
    fn validation_names_the_field() {
        let field = |base: Value, patch: Value| {
            let mut v = base;
            for (k, x) in patch.as_object().unwrap() {
                if let Some(rest) = k.strip_prefix("source.") {
                    v["source"][rest] = x.clone();
                } else {
                    v[k] = x.clone();
                }
            }
            let e = validate(&def(v)).unwrap_err();
            e.details.unwrap()["field"].as_str().unwrap().to_owned()
        };
        assert_eq!(field(gh(), json!({"name": "Bad Name"})), "name");
        assert_eq!(
            field(gh(), json!({"source.repo": "terrarium"})),
            "source.repo"
        );
        assert_eq!(
            field(gh(), json!({"source.workflow": ".github/workflows/ci.yml"})),
            "source.workflow"
        );
        assert_eq!(field(gh(), json!({"source.branch": " "})), "source.branch");
        assert_eq!(
            field(gh(), json!({"source.api_url": "ftp://x"})),
            "source.api_url"
        );
        assert_eq!(
            field(
                gh(),
                json!({"schedule": {"kind": "interval", "every": "10s"}})
            ),
            "schedule.every"
        );
        assert_eq!(
            field(
                gh(),
                json!({"schedule": {"kind": "at", "at": "2099-01-01T00:00:00Z"}})
            ),
            "schedule"
        );
        assert_eq!(field(gh(), json!({"max_runs": 0})), "max_runs");
        assert_eq!(field(gh(), json!({"max_runs": 101})), "max_runs");
        assert_eq!(field(sc(), json!({"source.target": ""})), "source.target");
        assert_eq!(field(sc(), json!({"source.script": " "})), "source.script");
        assert_eq!(
            field(sc(), json!({"source.timeout_secs": 0})),
            "source.timeout_secs"
        );
        assert_eq!(
            field(sc(), json!({"source.env_secrets": {"lower": "x"}})),
            "source.env_secrets"
        );
        assert_eq!(
            field(
                sc(),
                json!({"source.env_secrets": {"TERRARIUM_TOKEN": "x"}})
            ),
            "source.env_secrets"
        );
        assert_eq!(
            field(sc(), json!({"source.env_secrets": {"KEY": " "}})),
            "source.env_secrets.KEY"
        );
    }

    #[test]
    fn take_over_rules() {
        let a = def(gh());
        let mut b = a.clone();
        b.def_branch("main");
        assert!(!takes_over(&a, &b), "a branch is no new right");
        let mut c = a.clone();
        if let Source::Github(g) = &mut c.source {
            g.token_secret = Some("other".into());
        }
        assert!(takes_over(&a, &c));
        assert!(takes_over(&a, &def(sc())));
    }

    impl PipelineDef {
        fn def_branch(&mut self, b: &str) {
            if let Source::Github(g) = &mut self.source {
                g.branch = Some(b.into());
            }
        }
    }

    #[test]
    fn periods() {
        let now = Utc::now();
        assert_eq!(
            period_secs(&ScheduleSpec::Interval { every: "5m".into() }, now),
            300
        );
        let cron = ScheduleSpec::Cron {
            expr: "*/10 * * * *".into(),
            tz: "UTC".into(),
        };
        assert_eq!(period_secs(&cron, now), 600);
    }
}
