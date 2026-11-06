//! The job queue of a project (TER-71): the rows and the rules that need a
//! transaction. The run time is in [`crate::jobs`].
//!
//! - A job is `validate` (run the project's recipe on a clean checkout of
//!   a card commit) or `merge` (merge a card branch onto `next`, run the
//!   recipe on the result, push `next` when it is green).
//! - Jobs start first in, first out across projects, at most the host
//!   cap on one host (TER-97, `job_hosts`), and at most `slots` running
//!   at a time per project. A merge job runs alone in its project: no
//!   other job of the project runs with it (TER-71 review, R1).
//! - A card has at most one job that is queued or running.
//! - The one rerun of a failed job is a new row with `attempt` 2 and the
//!   `created_at` of the first try, so it goes to the front of the queue.
//! - Events `job` {change, project, id, card, kind, state, attempt}.

use std::collections::{HashMap, HashSet};

use rusqlite::{Connection, OptionalExtension, Row, params};
use serde::{Deserialize, Serialize};
use serde_json::json;

use terrarium_core::config::{DeliveryDef, TargetDef};

use crate::db::{Tx, now};
use crate::error::{ApiError, ApiResult};
use crate::events::EventDraft;
use crate::store::targets;

/// The output tail that a job keeps.
pub const TAIL_MAX: usize = 4000;
/// The most jobs in a list.
pub const LIST_MAX: usize = 200;

/// The kind of a job.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum JobKind {
    Validate,
    Merge,
}

impl JobKind {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Validate => "validate",
            Self::Merge => "merge",
        }
    }

    pub fn parse(s: &str) -> Option<Self> {
        match s {
            "validate" => Some(Self::Validate),
            "merge" => Some(Self::Merge),
            _ => None,
        }
    }
}

/// The state of a job.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum JobState {
    Queued,
    Running,
    Passed,
    Failed,
    Cancelled,
}

impl JobState {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Queued => "queued",
            Self::Running => "running",
            Self::Passed => "passed",
            Self::Failed => "failed",
            Self::Cancelled => "cancelled",
        }
    }

    pub fn parse(s: &str) -> Option<Self> {
        match s {
            "queued" => Some(Self::Queued),
            "running" => Some(Self::Running),
            "passed" => Some(Self::Passed),
            "failed" => Some(Self::Failed),
            "cancelled" => Some(Self::Cancelled),
            _ => None,
        }
    }

    /// Queued or running.
    pub fn is_open(self) -> bool {
        matches!(self, Self::Queued | Self::Running)
    }
}

/// A job.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Job {
    pub id: String,
    pub project: String,
    pub card: String,
    pub kind: JobKind,
    pub repo: String,
    pub branch: String,
    /// The card commit that a validate job checks out. A merge job merges
    /// the head of the branch.
    pub commit: Option<String>,
    pub state: JobState,
    pub attempt: i64,
    /// Why a job failed: `red` (the recipe failed), `conflict` (the merge
    /// did not apply), or `error` (the job could not run).
    pub outcome: Option<String>,
    pub exit_code: Option<i64>,
    pub tail: Option<String>,
    /// The commit that the job tested (for a merge: the merge commit).
    pub head: Option<String>,
    /// The worker that did the card (its `by:<agent>` label).
    pub by_agent: Option<String>,
    pub created_by: String,
    pub created_at: String,
    pub started_at: Option<String>,
    pub ended_at: Option<String>,
    /// The host that the job runs on: set when it starts (TER-97).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub host: Option<String>,
}

/// A new job.
#[derive(Debug, Clone)]
pub struct NewJob {
    pub project: String,
    pub card: String,
    pub kind: JobKind,
    pub repo: String,
    pub branch: String,
    pub commit: Option<String>,
    pub by_agent: Option<String>,
}

/// The end of a job.
#[derive(Debug, Clone, Default)]
pub struct JobEnd {
    pub passed: bool,
    pub outcome: Option<String>,
    pub exit_code: Option<i64>,
    pub tail: Option<String>,
    pub head: Option<String>,
}

const COLS: &str = "id, project, card, kind, repo, branch, commit_sha, state, attempt, outcome,
                    exit_code, tail, head, by_agent, created_by, created_at, started_at, ended_at,
                    host";

fn row(r: &Row<'_>) -> rusqlite::Result<Job> {
    let kind: String = r.get(3)?;
    let state: String = r.get(7)?;
    Ok(Job {
        id: r.get(0)?,
        project: r.get(1)?,
        card: r.get(2)?,
        kind: JobKind::parse(&kind).unwrap_or(JobKind::Validate),
        repo: r.get(4)?,
        branch: r.get(5)?,
        commit: r.get(6)?,
        state: JobState::parse(&state).unwrap_or(JobState::Failed),
        attempt: r.get(8)?,
        outcome: r.get(9)?,
        exit_code: r.get(10)?,
        tail: r.get(11)?,
        head: r.get(12)?,
        by_agent: r.get(13)?,
        created_by: r.get(14)?,
        created_at: r.get(15)?,
        started_at: r.get(16)?,
        ended_at: r.get(17)?,
        host: r.get(18)?,
    })
}

/// Keep the last [`TAIL_MAX`] bytes of `s`, on a char boundary.
pub fn clip_tail(s: &str) -> String {
    if s.len() <= TAIL_MAX {
        return s.to_owned();
    }
    let mut start = s.len() - TAIL_MAX;
    while !s.is_char_boundary(start) {
        start += 1;
    }
    s[start..].to_owned()
}

/// A job by ID.
pub fn get(conn: &Connection, id: &str) -> ApiResult<Job> {
    conn.query_row(&format!("SELECT {COLS} FROM jobs WHERE id = ?1"), [id], row)
        .optional()?
        .ok_or_else(|| ApiError::not_found(format!("no job `{id}`")))
}

/// The jobs of a project, newest first. `state` filters by state.
pub fn list(conn: &Connection, project: &str, state: Option<JobState>) -> ApiResult<Vec<Job>> {
    let mut st = conn.prepare(&format!(
        "SELECT {COLS} FROM jobs WHERE project = ?1 AND (?2 IS NULL OR state = ?2)
         ORDER BY created_at DESC, attempt DESC, id DESC LIMIT ?3"
    ))?;
    let rows = st
        .query_map(
            params![project, state.map(JobState::as_str), LIST_MAX as i64],
            row,
        )?
        .collect::<Result<Vec<_>, _>>()?;
    Ok(rows)
}

/// The open (queued or running) job of a card.
pub fn open_of_card(conn: &Connection, card: &str) -> ApiResult<Option<Job>> {
    Ok(conn
        .query_row(
            &format!(
                "SELECT {COLS} FROM jobs WHERE card = ?1 AND state IN ('queued', 'running')
                 ORDER BY created_at LIMIT 1"
            ),
            [card],
            row,
        )
        .optional()?)
}

/// The jobs that are running, in all projects.
pub fn running(conn: &Connection) -> ApiResult<Vec<Job>> {
    let mut st = conn.prepare(&format!(
        "SELECT {COLS} FROM jobs WHERE state = 'running' ORDER BY started_at"
    ))?;
    let rows = st.query_map([], row)?.collect::<Result<Vec<_>, _>>()?;
    Ok(rows)
}

/// The host key of a target (TER-97): its host agent (`host`), else the
/// host part of its `address` (without the user and the port), else its
/// name. The host cap of the job queue counts the running jobs per key.
pub fn host_key(name: &str, def: &TargetDef) -> String {
    if let Some(h) = def.host.as_deref().map(str::trim).filter(|h| !h.is_empty()) {
        return h.to_owned();
    }
    if let Some(a) = def
        .address
        .as_deref()
        .map(str::trim)
        .filter(|a| !a.is_empty())
    {
        let a = a.rsplit_once('@').map_or(a, |(_, h)| h);
        let a = match a.strip_prefix('[') {
            Some(v6) => v6.split(']').next().unwrap_or(v6),
            None => a.split(':').next().unwrap_or(a),
        };
        if !a.is_empty() {
            return a.to_owned();
        }
    }
    name.to_owned()
}

/// What the pick knows of a project that has queued jobs.
#[derive(Debug, Clone)]
pub struct QueueOf {
    /// The project's `slots`.
    pub slots: usize,
    /// The host key of the project's delivery target.
    pub host: String,
}

/// The queued jobs that may start now, with the host each one takes a
/// place on, in all projects: first in, first out across projects. A
/// job starts when (1) its host is under its cap (`caps`; a host without
/// one has no cap), and (2) its project is under its `slots`. A merge job
/// runs alone in its project (TER-71 review, R1: the recipes of a target
/// run as one user): it starts only when no other job of the project runs
/// or starts, and once the pick reaches it (started or not) or while it
/// runs, no other job of its project starts. A job that must wait does not
/// hold back the jobs of other projects behind it. A project missing from
/// `queues` has no queue, and its jobs wait.
pub fn pick(
    queued: Vec<Job>,
    running: &[Job],
    queues: &HashMap<String, QueueOf>,
    caps: &HashMap<String, usize>,
) -> Vec<(Job, String)> {
    let mut on_host: HashMap<String, usize> = HashMap::new();
    let mut in_project: HashMap<String, usize> = HashMap::new();
    // The projects where no more job may start in this pick.
    let mut closed: HashSet<String> = HashSet::new();
    for j in running {
        let host = j
            .host
            .clone()
            .or_else(|| queues.get(&j.project).map(|q| q.host.clone()));
        if let Some(h) = host {
            *on_host.entry(h).or_default() += 1;
        }
        *in_project.entry(j.project.clone()).or_default() += 1;
        if j.kind == JobKind::Merge {
            closed.insert(j.project.clone());
        }
    }
    let mut out = Vec::new();
    for j in queued {
        let Some(q) = queues.get(&j.project) else {
            continue;
        };
        if closed.contains(&j.project) {
            continue;
        }
        let merge = j.kind == JobKind::Merge;
        if merge {
            // The jobs behind a merge wait for it, whether it starts now
            // or not.
            closed.insert(j.project.clone());
        }
        let used = on_host.get(&q.host).copied().unwrap_or(0);
        if caps.get(&q.host).is_some_and(|cap| used >= *cap) {
            continue;
        }
        let busy = in_project.get(&j.project).copied().unwrap_or(0);
        if (merge && busy > 0) || busy >= q.slots {
            continue;
        }
        *on_host.entry(q.host.clone()).or_default() += 1;
        *in_project.entry(j.project.clone()).or_default() += 1;
        out.push((j, q.host.clone()));
    }
    out
}

/// The queued jobs of all projects, first in, first out (a rerun keeps the
/// `created_at` of its first try, and goes before a job of the same time).
pub fn queued(conn: &Connection) -> ApiResult<Vec<Job>> {
    let mut st = conn.prepare(&format!(
        "SELECT {COLS} FROM jobs WHERE state = 'queued' ORDER BY created_at, attempt DESC, id"
    ))?;
    let rows = st.query_map([], row)?.collect::<Result<Vec<_>, _>>()?;
    Ok(rows)
}

/// The queued jobs that may start now ([`pick`]), read in `conn`, with
/// the delivery config of each job's project.
pub fn startable(conn: &Connection) -> ApiResult<Vec<(Job, String, DeliveryDef)>> {
    let queued = queued(conn)?;
    if queued.is_empty() {
        return Ok(Vec::new());
    }
    let running = running(conn)?;
    let mut defs: HashMap<String, DeliveryDef> = HashMap::new();
    let mut queues: HashMap<String, QueueOf> = HashMap::new();
    let projects: HashSet<&str> = queued
        .iter()
        .chain(running.iter())
        .map(|j| j.project.as_str())
        .collect();
    for p in projects {
        let def = delivery(conn, p)?;
        if !def.enabled() {
            continue;
        }
        let host = host_of(conn, &def);
        queues.insert(
            p.to_owned(),
            QueueOf {
                slots: def.slots(),
                host,
            },
        );
        defs.insert(p.to_owned(), def);
    }
    let caps: HashMap<String, usize> = host_caps(conn)?
        .into_iter()
        .map(|c| (c.host, c.heavy_jobs as usize))
        .collect();
    Ok(pick(queued, &running, &queues, &caps)
        .into_iter()
        .filter_map(|(j, h)| {
            let def = defs.get(&j.project)?.clone();
            Some((j, h, def))
        })
        .collect())
}

/// The host key of the delivery target of `def`. A target that is gone
/// keeps its name as the key; the job then fails when it runs.
fn host_of(conn: &Connection, def: &DeliveryDef) -> String {
    let target = def.target.clone().unwrap_or_default();
    match targets::get(conn, &target) {
        Ok(row) => host_key(&target, &row.def),
        Err(_) => target,
    }
}

/// The host of the delivery target of `project`, its running jobs, and
/// its cap. `None` when the project has no queue or the host no cap.
pub fn host_room(conn: &Connection, project: &str) -> ApiResult<Option<(String, usize, u32)>> {
    let def = delivery(conn, project)?;
    if !def.enabled() {
        return Ok(None);
    }
    let host = host_of(conn, &def);
    let Some(cap) = host_caps(conn)?.into_iter().find(|c| c.host == host) else {
        return Ok(None);
    };
    let used = running_per_host(conn)?.get(&host).copied().unwrap_or(0);
    Ok(Some((host, used, cap.heavy_jobs)))
}

/// The cap of a host: the most jobs of all projects that run on it at
/// once (TER-97).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct HostCap {
    pub host: String,
    pub heavy_jobs: u32,
    pub updated_by: String,
    pub updated_at: String,
}

/// The host caps, by host.
pub fn host_caps(conn: &Connection) -> ApiResult<Vec<HostCap>> {
    let mut st = conn
        .prepare("SELECT host, heavy_jobs, updated_by, updated_at FROM job_hosts ORDER BY host")?;
    let rows = st
        .query_map([], |r| {
            Ok(HostCap {
                host: r.get(0)?,
                heavy_jobs: r.get(1)?,
                updated_by: r.get(2)?,
                updated_at: r.get(3)?,
            })
        })?
        .collect::<Result<Vec<_>, _>>()?;
    Ok(rows)
}

/// Set the cap of a host, or take it away (`None`: no cap).
pub fn set_host_cap(
    t: &mut Tx<'_>,
    host: &str,
    heavy_jobs: Option<u32>,
) -> ApiResult<Option<HostCap>> {
    let host = host.trim();
    if host.is_empty() || host.len() > 253 || host.chars().any(|c| c.is_control() || c == '/') {
        return Err(ApiError::invalid(format!("bad host `{host}`")));
    }
    let old = host_caps(t)?.into_iter().find(|c| c.host == host);
    match heavy_jobs {
        Some(0) => return Err(ApiError::invalid("`heavy_jobs` must be 1 or more")),
        Some(n) => {
            t.execute(
                "INSERT INTO job_hosts (host, heavy_jobs, updated_by, updated_at)
                 VALUES (?1, ?2, ?3, ?4)
                 ON CONFLICT(host) DO UPDATE SET heavy_jobs = ?2, updated_by = ?3, updated_at = ?4",
                params![host, n, t.actor, now()],
            )?;
        }
        None => {
            t.execute("DELETE FROM job_hosts WHERE host = ?1", [host])?;
        }
    }
    t.audit(
        "job.host_cap",
        host,
        None,
        json!({"old": old.as_ref().map(|c| c.heavy_jobs), "new": heavy_jobs}),
    )?;
    Ok(host_caps(t)?.into_iter().find(|c| c.host == host))
}

/// The running jobs per host key.
pub fn running_per_host(conn: &Connection) -> ApiResult<HashMap<String, usize>> {
    let mut out = HashMap::new();
    for j in running(conn)? {
        if let Some(h) = j.host {
            *out.entry(h).or_default() += 1;
        }
    }
    Ok(out)
}

fn emit(t: &mut Tx<'_>, j: &Job, change: &str) -> ApiResult<()> {
    let summary = format!(
        "{} job {} for {} is {}",
        j.kind.as_str(),
        j.id,
        j.card,
        j.state.as_str()
    );
    t.emit(
        EventDraft::new(
            "job",
            summary,
            json!({"change": change, "project": j.project, "id": j.id, "card": j.card,
                   "kind": j.kind, "state": j.state, "attempt": j.attempt,
                   "outcome": j.outcome}),
        )
        .project(j.project.clone()),
    )?;
    Ok(())
}

/// Queue a job. A card has at most one open job.
pub fn create(t: &mut Tx<'_>, n: NewJob) -> ApiResult<Job> {
    if let Some(open) = open_of_card(t, &n.card)? {
        return Err(ApiError::conflict(format!(
            "card {} has a {} job {} that is {} already; cancel it first",
            n.card,
            open.kind.as_str(),
            open.id,
            open.state.as_str()
        )));
    }
    let id = ulid::Ulid::new().to_string();
    t.execute(
        "INSERT INTO jobs (id, project, card, kind, repo, branch, commit_sha, state, attempt,
                           by_agent, created_by, created_at)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, 'queued', 1, ?8, ?9, ?10)",
        params![
            id,
            n.project,
            n.card,
            n.kind.as_str(),
            n.repo,
            n.branch,
            n.commit,
            n.by_agent,
            t.actor,
            now()
        ],
    )?;
    let job = get(t, &id)?;
    t.audit(
        "job.create",
        &id,
        Some(&job.project),
        json!({"card": job.card, "kind": job.kind, "repo": job.repo, "branch": job.branch,
               "commit": job.commit}),
    )?;
    emit(t, &job, "queued")?;
    Ok(job)
}

/// Queue the one rerun of a failed job: attempt 2, at the front.
pub fn rerun(t: &mut Tx<'_>, failed: &Job) -> ApiResult<Job> {
    let id = ulid::Ulid::new().to_string();
    t.execute(
        "INSERT INTO jobs (id, project, card, kind, repo, branch, commit_sha, state, attempt,
                           by_agent, created_by, created_at)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, 'queued', ?8, ?9, ?10, ?11)",
        params![
            id,
            failed.project,
            failed.card,
            failed.kind.as_str(),
            failed.repo,
            failed.branch,
            failed.commit,
            failed.attempt + 1,
            failed.by_agent,
            failed.created_by,
            failed.created_at
        ],
    )?;
    let job = get(t, &id)?;
    emit(t, &job, "queued")?;
    Ok(job)
}

/// Mark a queued job as running on `host`. False when it is no longer queued (a
/// cancel came first).
pub fn start(t: &mut Tx<'_>, id: &str, host: &str) -> ApiResult<bool> {
    let n = t.execute(
        "UPDATE jobs SET state = 'running', started_at = ?2, host = ?3
         WHERE id = ?1 AND state = 'queued'",
        params![id, now(), host],
    )?;
    if n == 0 {
        return Ok(false);
    }
    let job = get(t, id)?;
    emit(t, &job, "started")?;
    Ok(true)
}

/// End a running job. `None` when the job is not running (a cancel came
/// first).
pub fn finish(t: &mut Tx<'_>, id: &str, end: &JobEnd) -> ApiResult<Option<Job>> {
    let state = if end.passed {
        JobState::Passed
    } else {
        JobState::Failed
    };
    let n = t.execute(
        "UPDATE jobs SET state = ?2, outcome = ?3, exit_code = ?4, tail = ?5, head = ?6,
                         ended_at = ?7
         WHERE id = ?1 AND state = 'running'",
        params![
            id,
            state.as_str(),
            end.outcome,
            end.exit_code,
            end.tail.as_deref().map(clip_tail),
            end.head,
            now()
        ],
    )?;
    if n == 0 {
        return Ok(None);
    }
    let job = get(t, id)?;
    emit(t, &job, "ended")?;
    Ok(Some(job))
}

/// Cancel an open job.
pub fn cancel(t: &mut Tx<'_>, id: &str) -> ApiResult<Job> {
    let job = get(t, id)?;
    if !job.state.is_open() {
        return Err(ApiError::conflict(format!(
            "job {id} is {} already",
            job.state.as_str()
        )));
    }
    t.execute(
        "UPDATE jobs SET state = 'cancelled', ended_at = ?2 WHERE id = ?1",
        params![id, now()],
    )?;
    let job = get(t, id)?;
    t.audit(
        "job.cancel",
        id,
        Some(&job.project),
        json!({"card": job.card}),
    )?;
    emit(t, &job, "cancelled")?;
    Ok(job)
}

/// Fail each job that was running when the daemon stopped: its run is
/// lost. Returns the jobs.
pub fn fail_lost(t: &mut Tx<'_>) -> ApiResult<Vec<Job>> {
    let lost = running(t)?;
    let mut out = Vec::new();
    for j in lost {
        let end = JobEnd {
            passed: false,
            outcome: Some("error".into()),
            tail: Some("the daemon restarted while the job ran; the run is lost".into()),
            ..JobEnd::default()
        };
        if let Some(j) = finish(t, &j.id, &end)? {
            out.push(j);
        }
    }
    Ok(out)
}

/// The queue config of a project. The default (no queue) when none is set.
pub fn delivery(conn: &Connection, project: &str) -> ApiResult<DeliveryDef> {
    let raw: Option<String> = conn
        .query_row(
            "SELECT def FROM project_delivery WHERE project = ?1",
            [project],
            |r| r.get(0),
        )
        .optional()?;
    match raw {
        Some(s) => serde_json::from_str(&s)
            .map_err(|e| ApiError::internal(format!("bad delivery config of `{project}`: {e}"))),
        None => Ok(DeliveryDef::default()),
    }
}

/// Set the queue config of a project.
pub fn set_delivery(t: &mut Tx<'_>, project: &str, def: &DeliveryDef) -> ApiResult<DeliveryDef> {
    if let Some(r) = &def.recipe
        && r.contains('\0')
    {
        return Err(ApiError::invalid("`recipe` holds a NUL byte"));
    }
    if let Some(w) = &def.work_dir
        && (!w.starts_with('/')
            || w.split('/').any(|c| c == "..")
            || w.trim_end_matches('/').is_empty())
    {
        return Err(ApiError::invalid(format!(
            "`work_dir` `{w}` must be an absolute dir without `..`, and not `/`"
        )));
    }
    if def.slots == Some(0) {
        return Err(ApiError::invalid("`slots` must be 1 or more"));
    }
    let old = delivery(t, project)?;
    let text = serde_json::to_string(def)
        .map_err(|e| ApiError::internal(format!("cannot store the delivery config: {e}")))?;
    t.execute(
        "INSERT INTO project_delivery (project, def, updated_by, updated_at)
         VALUES (?1, ?2, ?3, ?4)
         ON CONFLICT(project) DO UPDATE SET def = ?2, updated_by = ?3, updated_at = ?4",
        params![project, text, t.actor, now()],
    )?;
    t.audit(
        "project.delivery",
        project,
        Some(project),
        json!({"old": old, "new": def}),
    )?;
    delivery(t, project)
}

/// Where a queued job stands: its place among the queued jobs of its
/// project (1 starts next), and the running count.
pub fn place(conn: &Connection, job: &Job) -> ApiResult<(i64, i64)> {
    let ahead: i64 = conn.query_row(
        "SELECT count(*) FROM jobs WHERE project = ?1 AND state = 'queued'
           AND (created_at < ?2 OR (created_at = ?2 AND id < ?3))",
        params![job.project, job.created_at, job.id],
        |r| r.get(0),
    )?;
    let running: i64 = conn.query_row(
        "SELECT count(*) FROM jobs WHERE project = ?1 AND state = 'running'",
        [&job.project],
        |r| r.get(0),
    )?;
    Ok((ahead + 1, running))
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;

    fn job(id: &str, project: &str, kind: JobKind, repo: &str, at: &str) -> Job {
        Job {
            id: id.into(),
            project: project.into(),
            card: format!("C-{id}"),
            kind,
            repo: repo.into(),
            branch: "b".into(),
            commit: None,
            state: JobState::Queued,
            attempt: 1,
            outcome: None,
            exit_code: None,
            tail: None,
            head: None,
            by_agent: None,
            created_by: "t".into(),
            created_at: at.into(),
            started_at: None,
            ended_at: None,
            host: None,
        }
    }

    fn running(mut j: Job, host: &str) -> Job {
        j.state = JobState::Running;
        j.host = Some(host.into());
        j
    }

    fn queues(list: &[(&str, usize, &str)]) -> HashMap<String, QueueOf> {
        list.iter()
            .map(|(p, slots, host)| {
                (
                    (*p).to_owned(),
                    QueueOf {
                        slots: *slots,
                        host: (*host).to_owned(),
                    },
                )
            })
            .collect()
    }

    fn ids(picked: &[(Job, String)]) -> Vec<&str> {
        picked.iter().map(|(j, _)| j.id.as_str()).collect()
    }

    #[test]
    fn the_host_cap_holds_across_projects_in_order() {
        let q = queues(&[("a", 4, "helios"), ("b", 4, "helios")]);
        let caps = HashMap::from([("helios".to_owned(), 2)]);
        let queued = vec![
            job("1", "a", JobKind::Validate, "r", "t1"),
            job("2", "b", JobKind::Validate, "r", "t2"),
            job("3", "a", JobKind::Validate, "r", "t3"),
            job("4", "b", JobKind::Validate, "r", "t4"),
        ];
        let p = pick(queued.clone(), &[], &q, &caps);
        assert_eq!(ids(&p), ["1", "2"]);
        assert!(p.iter().all(|(_, h)| h == "helios"));
        // One job of `a` runs: one place is left, and it goes to the
        // oldest queued job, whatever its project.
        let run = [running(
            job("0", "a", JobKind::Validate, "r", "t0"),
            "helios",
        )];
        let p = pick(queued[1..].to_vec(), &run, &q, &caps);
        assert_eq!(ids(&p), ["2"]);
        // The host is full: nothing starts.
        let run = [
            running(job("0", "a", JobKind::Validate, "r", "t0"), "helios"),
            running(job("9", "b", JobKind::Validate, "r", "t0"), "helios"),
        ];
        assert!(pick(queued.clone(), &run, &q, &caps).is_empty());
        // Without a cap, only the slots count.
        let p = pick(queued, &run, &q, &HashMap::new());
        assert_eq!(ids(&p), ["1", "2", "3", "4"]);
    }

    #[test]
    fn the_host_cap_comes_before_the_slots_and_other_hosts_go_on() {
        let q = queues(&[("a", 1, "helios"), ("b", 2, "helios"), ("c", 2, "zeus")]);
        let caps = HashMap::from([("helios".to_owned(), 2), ("zeus".to_owned(), 1)]);
        let queued = vec![
            job("1", "a", JobKind::Validate, "r", "t1"),
            // `a` has one slot: 2 waits, and 3 of `b` goes before it.
            job("2", "a", JobKind::Validate, "r", "t2"),
            job("3", "b", JobKind::Validate, "r", "t3"),
            // helios is full now.
            job("4", "b", JobKind::Validate, "r", "t4"),
            // Another host is not held back.
            job("5", "c", JobKind::Validate, "r", "t5"),
            job("6", "c", JobKind::Validate, "r", "t6"),
            // No queue: never starts.
            job("7", "gone", JobKind::Validate, "r", "t7"),
        ];
        let p = pick(queued, &[], &q, &caps);
        assert_eq!(ids(&p), ["1", "3", "5"]);
        let hosts: Vec<&str> = p.iter().map(|(_, h)| h.as_str()).collect();
        assert_eq!(hosts, ["helios", "helios", "zeus"]);
    }

    #[test]
    fn a_merge_runs_alone_in_its_project() {
        let q = queues(&[("a", 4, "h"), ("b", 4, "h")]);
        let none = HashMap::new();
        // A merge starts when nothing of its project runs or starts, and
        // the jobs behind it in its project wait. Project b goes on.
        let queued = vec![
            job("1", "a", JobKind::Merge, "app", "t1"),
            job("2", "a", JobKind::Validate, "app", "t2"),
            job("3", "b", JobKind::Validate, "app", "t3"),
            job("4", "a", JobKind::Merge, "lib", "t4"),
            job("5", "b", JobKind::Merge, "app", "t5"),
        ];
        assert_eq!(ids(&pick(queued, &[], &q, &none)), ["1", "3"]);
        // A job of `a` runs: the merge waits, and so does the job behind it.
        let run = [running(job("0", "a", JobKind::Validate, "r", "t0"), "h")];
        let queued = vec![
            job("1", "a", JobKind::Merge, "app", "t1"),
            job("2", "a", JobKind::Validate, "app", "t2"),
            job("3", "b", JobKind::Validate, "app", "t3"),
        ];
        assert_eq!(ids(&pick(queued.clone(), &run, &q, &none)), ["3"]);
        // A validate job before the merge in the same pick: the merge waits.
        let queued = vec![
            job("1", "a", JobKind::Validate, "app", "t1"),
            job("2", "a", JobKind::Merge, "app", "t2"),
            job("3", "a", JobKind::Validate, "app", "t3"),
        ];
        assert_eq!(ids(&pick(queued, &[], &q, &none)), ["1"]);
        // A merge of `a` runs: nothing else of `a` starts.
        let run = [running(job("0", "a", JobKind::Merge, "app", "t0"), "h")];
        let queued = vec![
            job("2", "a", JobKind::Validate, "app", "t2"),
            job("3", "b", JobKind::Validate, "app", "t3"),
        ];
        assert_eq!(ids(&pick(queued, &run, &q, &none)), ["3"]);
        // A merge that waits for the host cap holds back its project.
        let caps = HashMap::from([("h".to_owned(), 1)]);
        let run = [running(job("0", "b", JobKind::Validate, "r", "t0"), "h")];
        let queued = vec![
            job("1", "a", JobKind::Merge, "app", "t1"),
            job("2", "a", JobKind::Validate, "app", "t2"),
        ];
        assert!(pick(queued, &run, &q, &caps).is_empty());
    }

    #[test]
    fn host_keys() {
        let def = |v: serde_json::Value| -> TargetDef { serde_json::from_value(v).unwrap() };
        assert_eq!(
            host_key("t", &def(json!({"kind": "host", "host": "helios"}))),
            "helios"
        );
        assert_eq!(
            host_key(
                "t",
                &def(json!({"kind": "docker", "host": "helios", "image": "x"}))
            ),
            "helios"
        );
        for (addr, want) in [
            ("ci@build1.lan", "build1.lan"),
            ("build1.lan:2222", "build1.lan"),
            ("ci@[fd00::1]:22", "fd00::1"),
            ("10.0.0.5", "10.0.0.5"),
        ] {
            assert_eq!(
                host_key("t", &def(json!({"kind": "ssh", "address": addr}))),
                want,
                "{addr}"
            );
        }
        assert_eq!(host_key("t", &def(json!({"kind": "ssh"}))), "t");
    }
}
