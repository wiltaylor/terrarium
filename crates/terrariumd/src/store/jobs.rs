//! The job queue of a project (TER-71): the rows and the rules that need a
//! transaction. The run time is in [`crate::jobs`].
//!
//! - A job is `validate` (run the project's recipe on a clean checkout of
//!   a card commit) or `merge` (merge a card branch onto `next`, run the
//!   recipe on the result, push `next` when it is green).
//! - Jobs start first in, first out, at most `slots` running at a time
//!   per project. A merge job does not start while another merge job of
//!   the same repo runs.
//! - A card has at most one job that is queued or running.
//! - The one rerun of a failed job is a new row with `attempt` 2 and the
//!   `created_at` of the first try, so it goes to the front of the queue.
//! - Events `job` {change, project, id, card, kind, state, attempt}.

use rusqlite::{Connection, OptionalExtension, Row, params};
use serde::{Deserialize, Serialize};
use serde_json::json;

use terrarium_core::config::DeliveryDef;

use crate::db::{Tx, now};
use crate::error::{ApiError, ApiResult};
use crate::events::EventDraft;

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
                    exit_code, tail, head, by_agent, created_by, created_at, started_at, ended_at";

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

/// The projects that have a queued job.
pub fn projects_with_queued(conn: &Connection) -> ApiResult<Vec<String>> {
    let mut st = conn.prepare("SELECT DISTINCT project FROM jobs WHERE state = 'queued'")?;
    let rows = st
        .query_map([], |r| r.get(0))?
        .collect::<Result<Vec<String>, _>>()?;
    Ok(rows)
}

/// The jobs that are running, in all projects.
pub fn running(conn: &Connection) -> ApiResult<Vec<Job>> {
    let mut st = conn.prepare(&format!(
        "SELECT {COLS} FROM jobs WHERE state = 'running' ORDER BY started_at"
    ))?;
    let rows = st.query_map([], row)?.collect::<Result<Vec<_>, _>>()?;
    Ok(rows)
}

/// The queued jobs of `project` that may start now with `slots` places:
/// first in, first out, at most `slots` running, and one merge job per
/// repo at a time. A merge job that must wait for its repo does not hold
/// back the jobs behind it.
pub fn startable(conn: &Connection, project: &str, slots: usize) -> ApiResult<Vec<Job>> {
    let running: Vec<Job> = {
        let mut st = conn.prepare(&format!(
            "SELECT {COLS} FROM jobs WHERE project = ?1 AND state = 'running'"
        ))?;
        st.query_map([project], row)?
            .collect::<Result<Vec<_>, _>>()?
    };
    let mut free = slots.saturating_sub(running.len());
    if free == 0 {
        return Ok(Vec::new());
    }
    let mut merging: Vec<String> = running
        .iter()
        .filter(|j| j.kind == JobKind::Merge)
        .map(|j| j.repo.clone())
        .collect();
    let mut st = conn.prepare(&format!(
        "SELECT {COLS} FROM jobs WHERE project = ?1 AND state = 'queued'
         ORDER BY created_at, attempt DESC, id"
    ))?;
    let queued = st
        .query_map([project], row)?
        .collect::<Result<Vec<_>, _>>()?;
    let mut out = Vec::new();
    for j in queued {
        if free == 0 {
            break;
        }
        if j.kind == JobKind::Merge {
            if merging.contains(&j.repo) {
                continue;
            }
            merging.push(j.repo.clone());
        }
        free -= 1;
        out.push(j);
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

/// Mark a queued job as running. False when it is no longer queued (a
/// cancel came first).
pub fn start(t: &mut Tx<'_>, id: &str) -> ApiResult<bool> {
    let n = t.execute(
        "UPDATE jobs SET state = 'running', started_at = ?2 WHERE id = ?1 AND state = 'queued'",
        params![id, now()],
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
