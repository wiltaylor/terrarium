//! The job queue of a project (TER-71): the rows and the rules that need a
//! transaction. The run time is in [`crate::jobs`].
//!
//! - A job is `validate` (run the project's recipe on a clean checkout of
//!   a card commit) or `merge` (merge a card branch onto `next`, run the
//!   recipe on the result, push `next` when it is green).
//! - Jobs start first in, first out across projects, at most the host
//!   cap on one host (TER-97, `job_hosts`), and at most `slots` validate
//!   jobs running at a time per project. Merge jobs have their own lane
//!   (TER-159): one at a time per project, first in, first out, beside
//!   the validate jobs and outside the slots.
//! - A card has at most one job that is queued or running, also as a
//!   card of a batch.
//! - A merge job can merge several cards in order (TER-112, `batch`): one
//!   validation for all. A red batch splits into two halves, queued at
//!   the front (`parent` is the red batch). `split_path` keeps the halves
//!   of one root batch in merge order, also over nested splits: the left
//!   half adds `0` to the path of its parent, the right half `1`, and the
//!   queue sorts on the path after `created_at` and `attempt`.
//! - The one rerun of a failed job is a new row with `attempt` 2 and the
//!   `created_at` of the first try, so it goes to the front of the queue.
//! - The planned duration of a job (TER-155 D2, `planned_secs`): the
//!   agent gives it when it queues the job. A queued job's cards have no
//!   ETA; when the job starts, each card gets `eta_at` = start + planned
//!   duration. A rerun, a split, and a job back in the queue clear the ETA
//!   again, and their start sets it again. No duration, no ETA.
//! - Events `job` {change, project, id, card, kind, state, attempt, outcome,
//!   cards?} (`cards`: the cards of a batch).

use std::collections::{HashMap, HashSet};

use rusqlite::{Connection, OptionalExtension, Row, params};
use serde::{Deserialize, Serialize};
use serde_json::json;

use terrarium_core::config::{DeliveryDef, TargetDef};

use crate::db::{Tx, now};
use crate::error::{ApiError, ApiResult};
use crate::events::EventDraft;
use crate::store::{board, hosts, targets};

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
    /// The cards of a batch merge, in merge order (TER-112). Empty for a
    /// job of one card. `card`, `branch`, and `by_agent` are then those of
    /// the first card.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub batch: Vec<BatchCard>,
    /// The red batch that this job was split from (TER-112).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub parent: Option<String>,
    /// The place of a split job in its root batch: `0` and `1` for each
    /// left and right half on the way down. Empty for a job that is no
    /// split. The queue sorts on it, so the halves run in merge order.
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub split_path: String,
    /// The planned duration in seconds (TER-155 D2): the cards' ETA is
    /// the start plus this. `None`: the job sets no ETA.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub planned_secs: Option<i64>,
    /// The slot that a running job took (TER-190): `1` to the project's
    /// `slots` for a validate job, [`MERGE_SLOT`] for a merge. Set when it
    /// starts, cleared when it goes back to the queue. Each slot has one
    /// work tree on the target, which no two jobs use at once.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub slot: Option<i64>,
}

/// The slot of the merge lane (TER-159, TER-190): one merge at a time per
/// project, in a work tree of its own, apart from the validate slots.
pub const MERGE_SLOT: i64 = 0;

/// A card of a batch merge.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct BatchCard {
    pub card: String,
    pub branch: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub by_agent: Option<String>,
    /// `conflict` when the branch did not merge onto `next` and the cards
    /// before it; the card then left the batch. `None`: it merged.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub outcome: Option<String>,
    /// The conflicting files of a conflict.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub note: Option<String>,
}

impl Job {
    /// The cards of the job: the batch, or the one card.
    pub fn cards(&self) -> Vec<BatchCard> {
        if !self.batch.is_empty() {
            return self.batch.clone();
        }
        vec![BatchCard {
            card: self.card.clone(),
            branch: self.branch.clone(),
            by_agent: self.by_agent.clone(),
            outcome: None,
            note: None,
        }]
    }
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
    /// The cards of a batch merge, in order; empty for one card. The first
    /// is the job's `card`.
    pub batch: Vec<BatchCard>,
    /// The planned duration in seconds; `None`: no ETA when it starts.
    pub planned_secs: Option<i64>,
}

/// The end of a job.
#[derive(Debug, Clone, Default)]
pub struct JobEnd {
    pub passed: bool,
    pub outcome: Option<String>,
    pub exit_code: Option<i64>,
    pub tail: Option<String>,
    pub head: Option<String>,
    /// The batch with the outcome of each card (a conflict), when the job
    /// merged a batch.
    pub batch: Option<Vec<BatchCard>>,
}

const COLS: &str = "id, project, card, kind, repo, branch, commit_sha, state, attempt, outcome,
                    exit_code, tail, head, by_agent, created_by, created_at, started_at, ended_at,
                    host, batch, parent, split_path, planned_secs, slot";

fn row(r: &Row<'_>) -> rusqlite::Result<Job> {
    let kind: String = r.get(3)?;
    let state: String = r.get(7)?;
    let batch: Option<String> = r.get(19)?;
    let batch = batch
        .and_then(|b| serde_json::from_str(&b).ok())
        .unwrap_or_default();
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
        batch,
        parent: r.get(20)?,
        split_path: r.get(21)?,
        planned_secs: r.get(22)?,
        slot: r.get(23)?,
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

/// The first-attempt numbers of the jobs of one kind, or of all (TER-135).
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct FirstAttempts {
    /// Ended first attempts that ran the recipe: `passed + failed`.
    pub ended: u64,
    pub passed: u64,
    /// Failed with `red` (or a timeout). A `conflict` and an `error` never
    /// ran the tests, so they count in [`JobStats::conflicts`] and
    /// [`JobStats::errors`] and not here.
    pub failed: u64,
    /// `passed / ended`, from 0 to 1. `None` with no ended first attempt.
    pub pass_rate: Option<f64>,
}

impl FirstAttempts {
    fn add(&mut self, passed: bool) {
        self.ended += 1;
        if passed {
            self.passed += 1;
        } else {
            self.failed += 1;
        }
        self.pass_rate = Some(self.passed as f64 / self.ended as f64);
    }
}

/// A test target that failed, and how often (TER-135).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TargetFails {
    /// As cargo names it: `-p terrariumd --test jobs`.
    pub target: String,
    /// The failed jobs whose tail names the target, all attempts.
    pub failed: u64,
    /// Of them, the first attempts.
    pub first_attempt: u64,
}

/// A recipe step that failed, and how often (TER-135).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct StepFails {
    pub step: String,
    pub failed: u64,
}

/// The pass rate of a project's job queue over a window (TER-135).
///
/// A job is in the window when its `created_at` is at or after `since`.
/// A rerun keeps the `created_at` of its first try, so both tries fall in
/// the same window. The halves of a split batch (`parent` set) are a
/// search for the failing card: they count in `splits`, not as first
/// attempts or reruns. A job with outcome `error` could not run (host
/// agent offline, prepare step failed, push to `next` failed): it says
/// nothing about test stability, so it counts in `errors`, not as a first
/// attempt or a rerun.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct JobStats {
    pub project: String,
    pub since: String,
    /// All jobs in the window, in all states.
    pub jobs: u64,
    pub first_attempt: FirstAttempts,
    pub validate: FirstAttempts,
    pub merge: FirstAttempts,
    /// Ended reruns (attempt 2 or more): `passed` is a failure that the
    /// rerun did not repeat.
    pub reruns: FirstAttempts,
    /// First attempts that ended in a merge conflict.
    pub conflicts: u64,
    /// First attempts and reruns that could not run (outcome `error`).
    pub errors: u64,
    pub cancelled: u64,
    /// Queued or running.
    pub open: u64,
    pub splits: u64,
    /// The failed recipe steps (`test`, `lint`, ...; `timeout` when the job
    /// was killed), most first.
    pub failed_steps: Vec<StepFails>,
    /// The test targets that failed, most first.
    pub failed_targets: Vec<TargetFails>,
    /// Failed jobs whose `test` step failed but whose tail names no target:
    /// the 4000-byte tail clip cut off the `error: N targets failed:`
    /// header, or the step failed before cargo listed targets.
    pub untargeted_test_fails: u64,
}

/// The test targets that a job tail names as failed: the indented lines
/// after `error: N target(s) failed:`, as cargo `--no-fail-fast` prints
/// them, without the backticks. In order, without duplicates.
pub fn failed_targets(tail: &str) -> Vec<String> {
    let mut out: Vec<String> = Vec::new();
    let mut lines = tail.lines();
    while let Some(line) = lines.next() {
        let l = line.trim();
        let Some(rest) = l.strip_prefix("error: ") else {
            continue;
        };
        let Some(n) = rest
            .strip_suffix(" targets failed:")
            .or_else(|| rest.strip_suffix(" target failed:"))
        else {
            continue;
        };
        if n.parse::<u64>().is_err() {
            continue;
        }
        for item in lines.by_ref() {
            if !item.starts_with([' ', '\t']) {
                break;
            }
            let t = item.trim().trim_matches('`').trim();
            if !t.is_empty() && !out.iter().any(|o| o == t) {
                out.push(t.to_owned());
            }
        }
    }
    out
}

/// The recipe step that failed in a job tail: the first `error: recipe
/// `<step>` failed` line (just prints the inner recipe first), else
/// `timeout` when the job was killed at its timeout.
pub fn failed_step(tail: &str) -> Option<String> {
    for line in tail.lines() {
        if let Some(rest) = line.trim().strip_prefix("error: recipe `")
            && let Some((step, after)) = rest.split_once('`')
            && after.trim_start().starts_with("failed")
        {
            return Some(step.to_owned());
        }
    }
    tail.contains(", timeout]").then(|| "timeout".to_owned())
}

/// The pass rate of the jobs of `project` created at or after `since`
/// (RFC 3339, UTC, as `created_at` is stored).
pub fn stats(conn: &Connection, project: &str, since: &str) -> ApiResult<JobStats> {
    let mut st = conn.prepare(
        "SELECT kind, state, attempt, outcome, parent,
                CASE WHEN state = 'failed' THEN tail END
         FROM jobs WHERE project = ?1 AND created_at >= ?2",
    )?;
    let rows = st
        .query_map(params![project, since], |r| {
            Ok((
                r.get::<_, String>(0)?,
                r.get::<_, String>(1)?,
                r.get::<_, i64>(2)?,
                r.get::<_, Option<String>>(3)?,
                r.get::<_, Option<String>>(4)?,
                r.get::<_, Option<String>>(5)?,
            ))
        })?
        .collect::<Result<Vec<_>, _>>()?;
    let mut s = JobStats {
        project: project.to_owned(),
        since: since.to_owned(),
        ..JobStats::default()
    };
    let mut targets: HashMap<String, (u64, u64)> = HashMap::new();
    let mut steps: HashMap<String, u64> = HashMap::new();
    for (kind, state, attempt, outcome, parent, tail) in rows {
        s.jobs += 1;
        let state = JobState::parse(&state).unwrap_or(JobState::Failed);
        let first = attempt <= 1 && parent.is_none();
        if let Some(tail) = tail.as_deref() {
            let found = failed_targets(tail);
            let step = failed_step(tail);
            if found.is_empty() && step.as_deref() == Some("test") {
                s.untargeted_test_fails += 1;
            }
            for t in found {
                let e = targets.entry(t).or_default();
                e.0 += 1;
                e.1 += u64::from(first);
            }
            if let Some(step) = step {
                *steps.entry(step).or_default() += 1;
            }
        }
        match state {
            JobState::Queued | JobState::Running => {
                s.open += 1;
                continue;
            }
            JobState::Cancelled => {
                s.cancelled += 1;
                continue;
            }
            JobState::Passed | JobState::Failed => {}
        }
        if parent.is_some() {
            s.splits += 1;
            continue;
        }
        if outcome.as_deref() == Some("error") {
            s.errors += 1;
            continue;
        }
        let passed = state == JobState::Passed;
        if !first {
            s.reruns.add(passed);
            continue;
        }
        if outcome.as_deref() == Some("conflict") {
            s.conflicts += 1;
            continue;
        }
        s.first_attempt.add(passed);
        match JobKind::parse(&kind) {
            Some(JobKind::Merge) => s.merge.add(passed),
            _ => s.validate.add(passed),
        }
    }
    let mut targets: Vec<TargetFails> = targets
        .into_iter()
        .map(|(target, (failed, first_attempt))| TargetFails {
            target,
            failed,
            first_attempt,
        })
        .collect();
    targets.sort_by(|a, b| b.failed.cmp(&a.failed).then(a.target.cmp(&b.target)));
    s.failed_targets = targets;
    let mut steps: Vec<StepFails> = steps
        .into_iter()
        .map(|(step, failed)| StepFails { step, failed })
        .collect();
    steps.sort_by(|a, b| b.failed.cmp(&a.failed).then(a.step.cmp(&b.step)));
    s.failed_steps = steps;
    Ok(s)
}

/// The open (queued or running) job of a card.
pub fn open_of_card(conn: &Connection, card: &str) -> ApiResult<Option<Job>> {
    Ok(conn
        .query_row(
            &format!(
                "SELECT {COLS} FROM jobs WHERE state IN ('queued', 'running')
                   AND (card = ?1 OR EXISTS (SELECT 1 FROM json_each(jobs.batch)
                                             WHERE json_extract(value, '$.card') = ?1))
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
    let mut rows = st.query_map([], row)?.collect::<Result<Vec<_>, _>>()?;
    for job in &mut rows {
        if let Some(host) = &job.host {
            job.host = Some(canonical_key(conn, host)?);
        }
    }
    Ok(rows)
}

/// The jobs that ran and got a cancel at `since` or later, in all
/// projects. A cancelled job drains until the kill of its step ended
/// (TER-115), so after a daemon restart these may still have processes
/// on their target (TER-123).
pub fn cancelled_since(conn: &Connection, since: &str) -> ApiResult<Vec<Job>> {
    let mut st = conn.prepare(&format!(
        "SELECT {COLS} FROM jobs
         WHERE state = 'cancelled' AND started_at IS NOT NULL AND ended_at >= ?1
         ORDER BY started_at"
    ))?;
    let mut rows = st.query_map([since], row)?.collect::<Result<Vec<_>, _>>()?;
    for job in &mut rows {
        if let Some(host) = &job.host {
            job.host = Some(canonical_key(conn, host)?);
        }
    }
    Ok(rows)
}

/// The address-based key of a target. Host-agent names are resolved to
/// stable IDs by `host_of`, which has access to the host registry.
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
        return address_key(a);
    }
    name.to_owned()
}

/// The host part of an ssh address: no user, no port, lower case.
pub fn address_key(a: &str) -> String {
    let a = a.rsplit_once('@').map_or(a, |(_, h)| h);
    if let Ok(ip) = a.parse::<std::net::IpAddr>() {
        return ip.to_string();
    }
    let a = match a.strip_prefix('[') {
        Some(v6) => v6.split(']').next().unwrap_or(v6),
        None => a.split(':').next().unwrap_or(a),
    };
    a.to_lowercase()
}

/// Accept both host-agent names and stable keys, including old stored keys.
fn canonical_key(conn: &Connection, key: &str) -> ApiResult<String> {
    if let Some(host) = hosts::list(conn)?
        .into_iter()
        .find(|h| h.name == key || h.id == key)
    {
        return Ok(host.id);
    }
    let hostname = address_key(key);
    let pinned: String = conn
        .query_row(
            "SELECT host_key FROM job_host_keys WHERE hostname = ?1",
            [&hostname],
            |r| r.get(0),
        )
        .optional()?
        .unwrap_or_else(|| hostname.clone());
    // An address that a host agent lists as its own (TER-105): by the name
    // as written, or by the pinned key of both.
    let agent: Option<String> = conn
        .query_row(
            "SELECT a.host_id FROM job_host_aliases a
             JOIN hosts h ON h.id = a.host_id
             LEFT JOIN job_host_keys k ON k.hostname = a.alias
             WHERE a.alias = ?1 OR a.alias = ?2 OR COALESCE(k.host_key, a.alias) = ?2
             ORDER BY a.alias LIMIT 1",
            params![hostname, pinned],
            |r| r.get(0),
        )
        .optional()?;
    Ok(agent.unwrap_or(pinned))
}

/// The most aliases of one host agent.
const MAX_ALIASES: usize = 32;

/// The aliases of each host agent, by host-agent ID, sorted.
pub fn host_aliases(conn: &Connection) -> ApiResult<HashMap<String, Vec<String>>> {
    let mut st = conn.prepare("SELECT host_id, alias FROM job_host_aliases ORDER BY alias")?;
    let rows = st
        .query_map([], |r| Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?)))?
        .collect::<Result<Vec<_>, _>>()?;
    let mut out: HashMap<String, Vec<String>> = HashMap::new();
    for (id, alias) in rows {
        out.entry(id).or_default().push(alias);
    }
    Ok(out)
}

/// Set the addresses of a host agent (`host`: its name or ID), so that an
/// SSH target on one of them counts on the host agent for the host cap
/// (TER-105). Replaces the list; an empty list takes all away. Returns the
/// host-agent ID and the stored aliases.
pub fn set_host_aliases(
    t: &mut Tx<'_>,
    host: &str,
    aliases: &[String],
) -> ApiResult<(String, Vec<String>)> {
    let agents = hosts::list(t)?;
    let Some(agent) = agents
        .iter()
        .find(|h| h.name == host.trim() || h.id == host.trim())
    else {
        return Err(ApiError::not_found(format!("no host agent `{host}`")));
    };
    if aliases.len() > MAX_ALIASES {
        return Err(ApiError::invalid(format!(
            "at most {MAX_ALIASES} aliases per host agent"
        )));
    }
    let mut keys = Vec::new();
    for a in aliases {
        let a = a.trim();
        if a.len() > 253
            || a.chars()
                .any(|c| c.is_control() || c == '/' || c.is_whitespace())
        {
            return Err(ApiError::invalid(format!("bad alias `{a}`")));
        }
        let key = address_key(a);
        if key.is_empty() {
            return Err(ApiError::invalid(format!("bad alias `{a}`")));
        }
        if agents.iter().any(|h| h.name == key || h.id == key) {
            return Err(ApiError::invalid(format!(
                "alias `{key}` is the name of a host agent"
            )));
        }
        let owner: Option<String> = t
            .query_row(
                "SELECT host_id FROM job_host_aliases WHERE alias = ?1",
                [&key],
                |r| r.get(0),
            )
            .optional()?;
        if let Some(owner) = owner.filter(|o| *o != agent.id) {
            let name = agents
                .iter()
                .find(|h| h.id == owner)
                .map_or(owner.as_str(), |h| h.name.as_str());
            return Err(ApiError::conflict(format!(
                "alias `{key}` belongs to host agent `{name}`"
            )));
        }
        keys.push(key);
    }
    keys.sort();
    keys.dedup();
    let old = host_aliases(t)?.remove(&agent.id).unwrap_or_default();
    t.execute(
        "DELETE FROM job_host_aliases WHERE host_id = ?1",
        [&agent.id],
    )?;
    let at = now();
    for key in &keys {
        t.execute(
            "INSERT INTO job_host_aliases (alias, host_id, updated_by, updated_at)
             VALUES (?1, ?2, ?3, ?4)",
            params![key, agent.id, t.actor, at],
        )?;
    }
    t.audit(
        "job.host_aliases",
        &agent.id,
        None,
        json!({"host": agent.name, "old": old, "new": keys}),
    )?;
    Ok((agent.id.clone(), keys))
}

/// Names that still need a pinned identity. No network IO under the DB lock.
pub fn unresolved_hosts(conn: &Connection, extra: Option<String>) -> ApiResult<Vec<String>> {
    let mut keys: Vec<String> = targets::list(conn)?
        .into_iter()
        .filter(|t| t.def.host.is_none())
        .filter_map(|t| t.def.address)
        .collect();
    let mut st = conn.prepare(
        "SELECT host FROM job_hosts UNION SELECT host FROM jobs WHERE host IS NOT NULL
         UNION SELECT alias FROM job_host_aliases",
    )?;
    keys.extend(
        st.query_map([], |r| r.get::<_, String>(0))?
            .collect::<Result<Vec<_>, _>>()?,
    );
    keys.extend(extra);
    let agents = hosts::list(conn)?;
    let mut names = Vec::new();
    for key in keys {
        if agents.iter().any(|h| h.name == key || h.id == key) {
            continue;
        }
        let hostname = address_key(&key);
        if hostname.parse::<std::net::IpAddr>().is_ok() || hostname.is_empty() {
            continue;
        }
        let exists: bool = conn.query_row(
            "SELECT EXISTS(SELECT 1 FROM job_host_keys WHERE hostname = ?1)",
            [&hostname],
            |r| r.get(0),
        )?;
        if !exists {
            names.push(hostname);
        }
    }
    names.sort();
    names.dedup();
    Ok(names)
}

/// First writer wins, including a failed resolution pinned to the hostname.
pub fn pin_hosts(t: &mut Tx<'_>, keys: Vec<(String, String)>) -> ApiResult<()> {
    for (hostname, key) in keys {
        t.execute("INSERT INTO job_host_keys (hostname, host_key) VALUES (?1, ?2) ON CONFLICT(hostname) DO NOTHING", params![hostname, key])?;
    }
    Ok(())
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
/// one has no cap), and (2) for a validate job, its project runs fewer
/// validate jobs than its `slots`. Merge jobs have their own lane
/// (TER-159): at most one merge of a project runs at a time, merges start
/// first in, first out among themselves (once the pick reaches a merge,
/// started or not, no later merge of its project starts), and a merge
/// does not take one of the project's slots, so validate jobs start
/// beside it and never wait behind it. The host cap counts every job. A
/// job that must wait does not hold back the jobs of other projects
/// behind it. A project missing from `queues` has no queue, and its jobs
/// wait.
///
/// Each picked job gets its slot in [`Job::slot`] (TER-190): a merge
/// [`MERGE_SLOT`], a validate job the lowest slot from `1` to `slots` that
/// no running job of its project holds. A validate job with no free slot
/// (the project's `slots` went down while higher slots run) waits.
pub fn pick(
    queued: Vec<Job>,
    running: &[Job],
    queues: &HashMap<String, QueueOf>,
    caps: &HashMap<String, usize>,
) -> Vec<(Job, String)> {
    let mut on_host: HashMap<String, usize> = HashMap::new();
    // The validate jobs that run or start, per project.
    let mut validating: HashMap<String, usize> = HashMap::new();
    // The projects where no more merge may start in this pick.
    let mut merging: HashSet<String> = HashSet::new();
    // The validate slots that are held, per project.
    let mut held: HashMap<String, HashSet<i64>> = HashMap::new();
    for j in running {
        let host = j
            .host
            .clone()
            .or_else(|| queues.get(&j.project).map(|q| q.host.clone()));
        if let Some(h) = host {
            *on_host.entry(h).or_default() += 1;
        }
        match j.kind {
            JobKind::Merge => {
                merging.insert(j.project.clone());
            }
            JobKind::Validate => {
                *validating.entry(j.project.clone()).or_default() += 1;
                if let Some(s) = j.slot {
                    held.entry(j.project.clone()).or_default().insert(s);
                }
            }
        }
    }
    let mut out = Vec::new();
    for mut j in queued {
        let Some(q) = queues.get(&j.project) else {
            continue;
        };
        let merge = j.kind == JobKind::Merge;
        if merge && !merging.insert(j.project.clone()) {
            // A merge of the project runs, or an older one comes first,
            // whether it starts now or not.
            continue;
        }
        let used = on_host.get(&q.host).copied().unwrap_or(0);
        if caps.get(&q.host).is_some_and(|cap| used >= *cap) {
            continue;
        }
        if merge {
            j.slot = Some(MERGE_SLOT);
        } else {
            let busy = validating.get(&j.project).copied().unwrap_or(0);
            if busy >= q.slots {
                continue;
            }
            let taken = held.entry(j.project.clone()).or_default();
            let Some(free) = (1..=q.slots as i64).find(|s| !taken.contains(s)) else {
                continue;
            };
            taken.insert(free);
            j.slot = Some(free);
            *validating.entry(j.project.clone()).or_default() += 1;
        }
        *on_host.entry(q.host.clone()).or_default() += 1;
        out.push((j, q.host.clone()));
    }
    out
}

/// The queued jobs of all projects, first in, first out (a rerun keeps the
/// `created_at` of its first try, and goes before a job of the same time;
/// the halves of a split go in the order of their `split_path`).
pub fn queued(conn: &Connection) -> ApiResult<Vec<Job>> {
    let mut st = conn.prepare(&format!(
        "SELECT {COLS} FROM jobs WHERE state = 'queued' ORDER BY created_at, attempt DESC, split_path, id"
    ))?;
    let rows = st.query_map([], row)?.collect::<Result<Vec<_>, _>>()?;
    Ok(rows)
}

/// The queued jobs that may start now ([`pick`]), read in `conn`, with
/// the delivery config of each job's project. `draining` are the IDs of
/// the jobs whose run has not ended yet: a cancelled one among them keeps
/// its place until its processes on the target are gone (TER-115).
pub fn startable(
    conn: &Connection,
    draining: &[String],
) -> ApiResult<Vec<(Job, String, DeliveryDef)>> {
    let queued = queued(conn)?;
    if queued.is_empty() {
        return Ok(Vec::new());
    }
    let mut running = running(conn)?;
    for id in draining {
        if running.iter().any(|j| j.id == *id) {
            continue;
        }
        // A job of another daemon in the same process (the tests) is not
        // in this database.
        if let Ok(j) = get(conn, id)
            && j.state == JobState::Cancelled
        {
            running.push(j);
        }
    }
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
pub fn host_of(conn: &Connection, def: &DeliveryDef) -> String {
    let target = def.target.clone().unwrap_or_default();
    let key = match targets::get(conn, &target) {
        Ok(row) => row
            .def
            .host
            .as_deref()
            .and_then(|name| hosts::find(conn, name).ok().flatten())
            .map_or_else(|| host_key(&target, &row.def), |h| h.id),
        Err(_) => target,
    };
    canonical_key(conn, &key).unwrap_or_else(|_| address_key(&key))
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
    let mut caps = std::collections::BTreeMap::<String, HostCap>::new();
    for mut cap in rows {
        cap.host = canonical_key(conn, &cap.host)?;
        // Old aliases may each have a cap. Keep the stricter one until
        // the operator explicitly replaces it through set_host_cap.
        if caps
            .get(&cap.host)
            .is_none_or(|old| cap.heavy_jobs < old.heavy_jobs)
        {
            caps.insert(cap.host.clone(), cap);
        }
    }
    Ok(caps.into_values().collect())
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
    let canonical = canonical_key(t, host)?;
    let host = canonical.as_str();
    let old = host_caps(t)?.into_iter().find(|c| c.host == host);
    let keys = {
        let mut st = t.prepare("SELECT host FROM job_hosts")?;
        st.query_map([], |r| r.get::<_, String>(0))?
            .collect::<Result<Vec<_>, _>>()?
    };
    for key in keys {
        if canonical_key(t, &key)? == host {
            t.execute("DELETE FROM job_hosts WHERE host = ?1", [key])?;
        }
    }
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
    // Where the job comes from (TER-170): a new job of the queue, the
    // rerun of a failed job, or a half of a split batch. The built-in rule
    // pack comments a new job only.
    let origin = if j.attempt > 1 {
        "rerun"
    } else if j.parent.is_some() {
        "split"
    } else {
        "queue"
    };
    let mut data = json!({"change": change, "project": j.project, "id": j.id, "card": j.card,
                          "kind": j.kind, "state": j.state, "attempt": j.attempt,
                          "outcome": j.outcome, "origin": origin, "parent": j.parent});
    if !j.batch.is_empty() {
        data["cards"] = json!(j.batch.iter().map(|c| &c.card).collect::<Vec<_>>());
    }
    t.emit(EventDraft::new("job", summary, data).project(j.project.clone()))?;
    Ok(())
}

/// A later attempt of `j` (its rerun), if one is queued or ran.
pub fn rerun_of(conn: &Connection, j: &Job) -> ApiResult<Option<String>> {
    Ok(conn
        .query_row(
            "SELECT id FROM jobs
              WHERE project = ?1 AND kind = ?2 AND repo = ?3 AND created_at = ?4
                AND split_path = ?5 AND coalesce(parent, '') = coalesce(?6, '')
                AND attempt > ?7 AND id <> ?8
              ORDER BY attempt LIMIT 1",
            params![
                j.project,
                j.kind.as_str(),
                j.repo,
                j.created_at,
                j.split_path,
                j.parent,
                j.attempt,
                j.id
            ],
            |r| r.get(0),
        )
        .optional()?)
}

/// The halves of job `id`, when a split made them: left first.
pub fn halves_of(conn: &Connection, id: &str) -> ApiResult<Vec<Job>> {
    let ids: Vec<String> = {
        let mut st = conn.prepare("SELECT id FROM jobs WHERE parent = ?1 ORDER BY split_path")?;
        st.query_map([id], |r| r.get(0))?
            .collect::<Result<Vec<_>, _>>()?
    };
    ids.iter().map(|i| get(conn, i)).collect()
}

/// The JSON text of a batch; `None` for a job of one card.
fn batch_text(batch: &[BatchCard]) -> ApiResult<Option<String>> {
    if batch.is_empty() {
        return Ok(None);
    }
    serde_json::to_string(batch)
        .map(Some)
        .map_err(|e| ApiError::internal(format!("cannot store the batch: {e}")))
}

/// The row of a new queued job. `created_at` and `created_by` come from the
/// first try for a rerun or a split.
struct Insert<'a> {
    id: String,
    first: &'a BatchCard,
    batch: &'a [BatchCard],
    project: &'a str,
    kind: JobKind,
    repo: &'a str,
    commit: Option<&'a str>,
    attempt: i64,
    parent: Option<&'a str>,
    split_path: &'a str,
    created_by: &'a str,
    created_at: &'a str,
    planned_secs: Option<i64>,
}

fn insert(t: &mut Tx<'_>, i: Insert<'_>) -> ApiResult<Job> {
    t.execute(
        "INSERT INTO jobs (id, project, card, kind, repo, branch, commit_sha, state, attempt,
                           by_agent, created_by, created_at, batch, parent, split_path,
                           planned_secs)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, 'queued', ?8, ?9, ?10, ?11, ?12, ?13, ?14, ?15)",
        params![
            i.id,
            i.project,
            i.first.card,
            i.kind.as_str(),
            i.repo,
            i.first.branch,
            i.commit,
            i.attempt,
            i.first.by_agent,
            i.created_by,
            i.created_at,
            batch_text(i.batch)?,
            i.parent,
            i.split_path,
            i.planned_secs
        ],
    )?;
    let job = get(t, &i.id)?;
    // A queued job's cards have no ETA: it starts when the job runs.
    queued_eta(t, &job)?;
    Ok(job)
}

/// The most planned duration of a job: the ETA limit
/// (`terrarium_core::eta::MAX_DAYS`).
pub const PLANNED_MAX_SECS: i64 = terrarium_core::eta::MAX_DAYS * 86_400;

/// Parse the planned duration of a job (TER-155 D2): a duration such as
/// `45m` or `2h30m`, not a time, at most [`PLANNED_MAX_SECS`].
pub fn planned_secs(v: &str) -> ApiResult<i64> {
    let d = terrarium_core::eta::parse_duration(v).map_err(|e| {
        ApiError::invalid(format!(
            "bad eta: {e}; a job takes a duration from its start, not a time"
        ))
    })?;
    let secs = d.num_seconds();
    if secs > PLANNED_MAX_SECS {
        return Err(ApiError::invalid(format!(
            "bad eta `{}`: at most {} days",
            v.trim(),
            terrarium_core::eta::MAX_DAYS
        )));
    }
    Ok(secs)
}

/// Set the planned duration of an open job (TER-155 D2): an ETA duration
/// given for a card while its job is open. A queued job's cards keep no
/// ETA; a running job's cards get its start plus `secs`. `by` is the
/// actor label of the writer. The caller checks the rights.
pub fn set_planned(t: &mut Tx<'_>, job: &Job, secs: i64, by: &str) -> ApiResult<Job> {
    if !(1..=PLANNED_MAX_SECS).contains(&secs) {
        return Err(ApiError::invalid(format!(
            "bad eta: a planned duration is more than zero and at most {} days",
            terrarium_core::eta::MAX_DAYS
        )));
    }
    t.execute(
        "UPDATE jobs SET planned_secs = ?2 WHERE id = ?1",
        params![job.id, secs],
    )?;
    let job = get(t, &job.id)?;
    let eta = match (job.state, job.started_at.as_deref()) {
        (JobState::Running, Some(s)) => started_eta(&job, s),
        _ => None,
    };
    for c in job.cards() {
        board::set_job_eta(t, &c.card, eta.as_deref(), by)?;
    }
    Ok(job)
}

/// Clear the ETA of the cards of a queued job that has a planned duration.
fn queued_eta(t: &mut Tx<'_>, job: &Job) -> ApiResult<()> {
    if job.planned_secs.is_none() {
        return Ok(());
    }
    for c in job.cards() {
        board::set_job_eta(t, &c.card, None, &job.created_by)?;
    }
    Ok(())
}

/// The ETA of the cards of a job that started at `started` (RFC 3339):
/// the start plus the planned duration. `None` without a duration.
pub fn started_eta(job: &Job, started: &str) -> Option<String> {
    let secs = job.planned_secs?;
    let at = chrono::DateTime::parse_from_rfc3339(started).ok()?;
    let eta = at.with_timezone(&chrono::Utc) + chrono::Duration::seconds(secs);
    Some(crate::time_param::stored(eta))
}

/// Queue a job. A card has at most one open job. A batch (two or more
/// cards, a merge only) starts with the job's own card.
pub fn create(t: &mut Tx<'_>, n: NewJob) -> ApiResult<Job> {
    if !n.batch.is_empty() {
        if n.kind != JobKind::Merge {
            return Err(ApiError::invalid("only a merge job takes a batch"));
        }
        if n.batch.len() < 2 || n.batch[0].card != n.card {
            return Err(ApiError::invalid(
                "a batch has two or more cards and starts with the job's card",
            ));
        }
        let mut seen = HashSet::new();
        for c in &n.batch {
            if !seen.insert(c.card.as_str()) {
                return Err(ApiError::invalid(format!(
                    "card {} is twice in the batch",
                    c.card
                )));
            }
        }
    }
    let first = BatchCard {
        card: n.card.clone(),
        branch: n.branch.clone(),
        by_agent: n.by_agent.clone(),
        outcome: None,
        note: None,
    };
    let cards: Vec<&str> = if n.batch.is_empty() {
        vec![n.card.as_str()]
    } else {
        n.batch.iter().map(|c| c.card.as_str()).collect()
    };
    for card in cards {
        let is_group: bool = t.query_row(
            "SELECT EXISTS(SELECT 1 FROM cards WHERE id = ?1 AND kind = 'group')",
            [card],
            |r| r.get(0),
        )?;
        if is_group {
            return Err(ApiError::invalid(
                "group cards cannot be validated or merged",
            ));
        }
        if let Some(open) = open_of_card(t, card)? {
            return Err(ApiError::conflict(format!(
                "card {card} has a {} job {} that is {} already; cancel it first",
                open.kind.as_str(),
                open.id,
                open.state.as_str()
            )));
        }
    }
    let (actor, at) = (t.actor.to_owned(), now());
    let job = insert(
        t,
        Insert {
            id: ulid::Ulid::new().to_string(),
            first: &first,
            batch: &n.batch,
            project: &n.project,
            kind: n.kind,
            repo: &n.repo,
            commit: n.commit.as_deref(),
            attempt: 1,
            parent: None,
            split_path: "",
            created_by: &actor,
            created_at: &at,
            planned_secs: n.planned_secs,
        },
    )?;
    let mut detail = json!({"card": job.card, "kind": job.kind, "repo": job.repo,
                            "branch": job.branch, "commit": job.commit});
    if !job.batch.is_empty() {
        detail["batch"] = json!(job.batch.iter().map(|c| &c.card).collect::<Vec<_>>());
    }
    t.audit("job.create", &job.id, Some(&job.project), detail)?;
    emit(t, &job, "queued")?;
    Ok(job)
}

/// Queue the one rerun of a failed job: attempt 2, at the front. `cards`
/// is the batch to run again (the cards that did not conflict); `None`
/// keeps the job's cards.
pub fn rerun(t: &mut Tx<'_>, failed: &Job, cards: Option<&[BatchCard]>) -> ApiResult<Job> {
    let cards = clean(cards.map_or_else(|| failed.cards(), <[_]>::to_vec));
    let Some(first) = cards.first() else {
        return Err(ApiError::internal("a rerun without cards"));
    };
    let batch = if cards.len() > 1 { &cards[..] } else { &[] };
    let job = insert(
        t,
        Insert {
            id: ulid::Ulid::new().to_string(),
            first,
            batch,
            project: &failed.project,
            kind: failed.kind,
            repo: &failed.repo,
            commit: failed.commit.as_deref(),
            attempt: failed.attempt + 1,
            parent: failed.parent.as_deref(),
            split_path: &failed.split_path,
            created_by: &failed.created_by,
            created_at: &failed.created_at,
            planned_secs: failed.planned_secs,
        },
    )?;
    emit(t, &job, "queued")?;
    Ok(job)
}

/// The cards without the outcome of an earlier run.
fn clean(cards: Vec<BatchCard>) -> Vec<BatchCard> {
    cards
        .into_iter()
        .map(|c| BatchCard {
            outcome: None,
            note: None,
            ..c
        })
        .collect()
}

/// Split the cards of a red batch (two or more) into two halves, each
/// queued at the front with `parent` set to the red job. The left half
/// and all the splits below it run before the right half: their
/// `split_path` (the red job's, then `0` or `1`) sorts first at the same
/// `created_at` (TER-112 review, F1). One merge of a project runs at a
/// time, so the right half merges onto `next` with the left half in it. A half
/// of one card is a job of one card.
pub fn split(t: &mut Tx<'_>, red: &Job, cards: &[BatchCard]) -> ApiResult<(Job, Job)> {
    if cards.len() < 2 {
        return Err(ApiError::internal("a split needs two or more cards"));
    }
    let cards = clean(cards.to_vec());
    let (left, right) = cards.split_at(cards.len() / 2);
    let mut out = Vec::new();
    for (side, half) in ["0", "1"].into_iter().zip([left, right]) {
        let batch = if half.len() > 1 { half } else { &[] };
        let path = format!("{}{side}", red.split_path);
        let job = insert(
            t,
            Insert {
                id: ulid::Ulid::new().to_string(),
                first: &half[0],
                batch,
                project: &red.project,
                kind: red.kind,
                repo: &red.repo,
                commit: None,
                attempt: 1,
                parent: Some(&red.id),
                split_path: &path,
                created_by: &red.created_by,
                created_at: &red.created_at,
                planned_secs: red.planned_secs,
            },
        )?;
        emit(t, &job, "queued")?;
        out.push(job);
    }
    let right = out
        .pop()
        .ok_or_else(|| ApiError::internal("no right half"))?;
    let left = out
        .pop()
        .ok_or_else(|| ApiError::internal("no left half"))?;
    Ok((left, right))
}

/// Mark a queued job as running on `host`, in `slot` ([`pick`]). False
/// when it is no longer queued (a cancel came first).
pub fn start(t: &mut Tx<'_>, id: &str, host: &str, slot: Option<i64>) -> ApiResult<bool> {
    let n = t.execute(
        "UPDATE jobs SET state = 'running', started_at = ?2, host = ?3, slot = ?4
         WHERE id = ?1 AND state = 'queued'",
        params![id, now(), host, slot],
    )?;
    if n == 0 {
        return Ok(false);
    }
    let job = get(t, id)?;
    // The ETA starts when the job runs (TER-155 D2).
    if let Some(eta) = job.started_at.as_deref().and_then(|s| started_eta(&job, s)) {
        for c in job.cards() {
            board::set_job_eta(t, &c.card, Some(&eta), &job.created_by)?;
        }
    }
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
                         ended_at = ?7, batch = COALESCE(?8, batch)
         WHERE id = ?1 AND state = 'running'",
        params![
            id,
            state.as_str(),
            end.outcome,
            end.exit_code,
            end.tail.as_deref().map(clip_tail),
            end.head,
            now(),
            match &end.batch {
                Some(b) => batch_text(b)?,
                None => None,
            }
        ],
    )?;
    if n == 0 {
        return Ok(None);
    }
    let job = get(t, id)?;
    emit(t, &job, "ended")?;
    Ok(Some(job))
}

/// Put a running job back in the queue: its target could not be reached
/// before the recipe started (TER-160). It keeps its attempt and its place
/// (`created_at`), and `tail` says why it waits. `None` when the job is
/// not running (a cancel came first).
pub fn requeue(t: &mut Tx<'_>, id: &str, why: &str) -> ApiResult<Option<Job>> {
    requeue_with(t, id, &format!("waits for its host: {why}"))
}

/// The tail of a job that [`requeue_lost`] put back.
pub const LOST_TAIL: &str =
    "the daemon restarted while the job ran; the run is lost, and the job waits in the queue again";

/// Put job `id` back in the queue: it was running when the last daemon
/// stopped, so its run is lost (TER-123, P5). As [`requeue`]: it keeps its
/// attempt and its place, and `tail` says why. `None` when it is not
/// running (any more).
pub fn requeue_lost(t: &mut Tx<'_>, id: &str) -> ApiResult<Option<Job>> {
    requeue_with(t, id, LOST_TAIL)
}

/// Put job `id` back in the queue: the infrastructure lost its recipe run
/// (TER-196), for example the host agent no longer holds the session. As
/// [`requeue`]: it keeps its attempt and its place, so a batch reruns
/// whole, and `tail` says why. `None` when it is not running (any more).
pub fn requeue_infra(t: &mut Tx<'_>, id: &str, why: &str) -> ApiResult<Option<Job>> {
    requeue_with(
        t,
        id,
        &format!(
            "the infrastructure lost the run after the recipe started ({why}); no attempt is used, and the job waits in the queue again"
        ),
    )
}

fn requeue_with(t: &mut Tx<'_>, id: &str, tail: &str) -> ApiResult<Option<Job>> {
    let n = t.execute(
        "UPDATE jobs SET state = 'queued', started_at = NULL, host = NULL, slot = NULL, tail = ?2
         WHERE id = ?1 AND state = 'running'",
        params![id, clip_tail(tail)],
    )?;
    if n == 0 {
        return Ok(None);
    }
    let job = get(t, id)?;
    queued_eta(t, &job)?;
    emit(t, &job, "requeued")?;
    Ok(Some(job))
}

/// Mark slot `slot` of `project` for a wipe (TER-190 review, finding 2):
/// the kill of lost job `job` did not confirm that its processes ended, so
/// they may still write into the slot's tree. The next job of the slot
/// makes all of its tree new, `target/` too ([`crate::jobs`]).
pub fn mark_slot_wipe(t: &mut Tx<'_>, project: &str, slot: i64, job: &str) -> ApiResult<()> {
    t.execute(
        "INSERT INTO job_slot_wipes (project, slot, job, marked_at) VALUES (?1, ?2, ?3, ?4)
         ON CONFLICT (project, slot) DO UPDATE SET job = ?3, marked_at = ?4",
        params![project, slot, job, now()],
    )?;
    Ok(())
}

/// Is slot `slot` of `project` marked for a wipe ([`mark_slot_wipe`])?
pub fn slot_wipe_pending(conn: &Connection, project: &str, slot: i64) -> ApiResult<bool> {
    let n: i64 = conn.query_row(
        "SELECT COUNT(*) FROM job_slot_wipes WHERE project = ?1 AND slot = ?2",
        params![project, slot],
        |r| r.get(0),
    )?;
    Ok(n > 0)
}

/// The slot's tree is new: its wipe mark goes.
pub fn clear_slot_wipe(t: &mut Tx<'_>, project: &str, slot: i64) -> ApiResult<()> {
    t.execute(
        "DELETE FROM job_slot_wipes WHERE project = ?1 AND slot = ?2",
        params![project, slot],
    )?;
    Ok(())
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

/// The open job of a card, as the card API shows it (TER-158, D4): the
/// board tile reads its state chip from this, not from labels.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CardJob {
    pub id: String,
    pub kind: JobKind,
    /// `queued` or `running`.
    pub state: JobState,
    /// A queued job: its place among the queued jobs of its project and
    /// lane (1 starts next).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub place: Option<i64>,
    /// A queued job: the count of queued jobs in its project and lane.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub queued: Option<i64>,
    /// The planned duration in seconds (TER-155 D2): the card's ETA
    /// starts when the job runs.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub planned_secs: Option<i64>,
}

/// The open job of a card with its queue place. `None`: no open job.
pub fn card_job(conn: &Connection, card: &str) -> ApiResult<Option<CardJob>> {
    let Some(job) = open_of_card(conn, card)? else {
        return Ok(None);
    };
    let (place, queued) = if job.state == JobState::Queued {
        let (place, _) = place(conn, &job)?;
        let queued: i64 = conn.query_row(
            "SELECT count(*) FROM jobs WHERE project = ?1 AND kind = ?2 AND state = 'queued'",
            params![job.project, job.kind.as_str()],
            |r| r.get(0),
        )?;
        (Some(place), Some(queued))
    } else {
        (None, None)
    };
    Ok(Some(CardJob {
        id: job.id,
        kind: job.kind,
        state: job.state,
        place,
        queued,
        planned_secs: job.planned_secs,
    }))
}

/// Where a queued job stands: its place among the queued jobs of its
/// project and its lane (1 starts next), and the running count of that
/// lane. Validate and merge jobs have their own lanes ([`pick`]).
pub fn place(conn: &Connection, job: &Job) -> ApiResult<(i64, i64)> {
    let ahead: i64 = conn.query_row(
        "SELECT count(*) FROM jobs WHERE project = ?1 AND kind = ?6 AND state = 'queued'
           AND (created_at < ?2 OR (created_at = ?2 AND attempt > ?4)
                OR (created_at = ?2 AND attempt = ?4
                    AND (split_path < ?5 OR (split_path = ?5 AND id < ?3))))",
        params![
            job.project,
            job.created_at,
            job.id,
            job.attempt,
            job.split_path,
            job.kind.as_str()
        ],
        |r| r.get(0),
    )?;
    let running: i64 = conn.query_row(
        "SELECT count(*) FROM jobs WHERE project = ?1 AND kind = ?2 AND state = 'running'",
        params![job.project, job.kind.as_str()],
        |r| r.get(0),
    )?;
    Ok((ahead + 1, running))
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;

    /// TER-123: only jobs that started and got their cancel at `since` or
    /// later count as maybe draining.
    #[test]
    fn cancelled_since_takes_recent_cancels_of_started_jobs() {
        let mut conn = Connection::open_in_memory().unwrap();
        crate::db::migrate(&mut conn).unwrap();
        for (id, state, started, ended) in [
            (
                "J1",
                "cancelled",
                Some("2026-11-07T05:00:00.000Z"),
                "2026-11-07T05:40:00.000Z",
            ),
            (
                "J2",
                "cancelled",
                Some("2026-11-07T04:00:00.000Z"),
                "2026-11-07T04:10:00.000Z",
            ),
            ("J3", "cancelled", None, "2026-11-07T05:41:00.000Z"),
            (
                "J4",
                "failed",
                Some("2026-11-07T05:00:00.000Z"),
                "2026-11-07T05:42:00.000Z",
            ),
        ] {
            conn.execute(
                "INSERT INTO jobs (id, project, card, kind, repo, branch, state, created_by,
                                   created_at, started_at, ended_at)
                 VALUES (?1, 'p', 'C', 'validate', 'app', 'b', ?2, 't', 't', ?3, ?4)",
                params![id, state, started, ended],
            )
            .unwrap();
        }
        let got: Vec<String> = cancelled_since(&conn, "2026-11-07T05:00:00.000Z")
            .unwrap()
            .into_iter()
            .map(|j| j.id)
            .collect();
        assert_eq!(got, ["J1"]);
    }

    /// TER-123, P5: a job lost to a restart goes back to the queue with
    /// its attempt and its place; a job that is not running stays as it is.
    #[tokio::test]
    async fn a_lost_job_is_requeued_without_using_an_attempt() {
        let mut c = Connection::open_in_memory().unwrap();
        crate::db::migrate(&mut c).unwrap();
        for (id, state) in [("J1", "running"), ("J2", "cancelled")] {
            c.execute(
                "INSERT INTO jobs (id, project, card, kind, repo, branch, state, attempt,
                                   created_by, created_at, started_at, host)
                 VALUES (?1, 'p', 'C', 'validate', 'app', 'b', ?2, 1, 't',
                         '2026-11-07T05:00:00.000Z', '2026-11-07T05:01:00.000Z', 'h')",
                params![id, state],
            )
            .unwrap();
        }
        let db = crate::db::Db::new(c, crate::events::EventHub::default());
        let j = db
            .write("system", |t| requeue_lost(t, "J1"))
            .await
            .unwrap()
            .unwrap();
        assert_eq!(j.state, JobState::Queued);
        assert_eq!(j.attempt, 1);
        assert_eq!(j.created_at, "2026-11-07T05:00:00.000Z");
        assert_eq!(j.started_at, None);
        assert_eq!(j.host, None);
        assert_eq!(j.tail.as_deref(), Some(LOST_TAIL));
        let again = db.write("system", |t| requeue_lost(t, "J1")).await.unwrap();
        assert!(again.is_none(), "a queued job is not requeued again");
        let cancelled = db.write("system", |t| requeue_lost(t, "J2")).await.unwrap();
        assert!(cancelled.is_none(), "a cancel stays a cancel");
        let j2 = db.read(|c| get(c, "J2")).await.unwrap();
        assert_eq!(j2.state, JobState::Cancelled);
    }

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
            batch: Vec::new(),
            parent: None,
            split_path: String::new(),
            planned_secs: None,
            slot: None,
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
    fn validations_start_beside_a_merge() {
        let q = queues(&[("a", 2, "h"), ("b", 2, "h")]);
        let none = HashMap::new();
        // A merge at the front does not hold back the validate jobs of its
        // project, and does not take one of its 2 slots.
        let queued = vec![
            job("1", "a", JobKind::Merge, "app", "t1"),
            job("2", "a", JobKind::Validate, "app", "t2"),
            job("3", "a", JobKind::Validate, "app", "t3"),
            job("4", "a", JobKind::Validate, "app", "t4"),
            job("5", "b", JobKind::Validate, "app", "t5"),
        ];
        assert_eq!(ids(&pick(queued, &[], &q, &none)), ["1", "2", "3", "5"]);
        // A merge of `a` runs: two validate jobs of `a` start beside it.
        let run = [running(job("0", "a", JobKind::Merge, "app", "t0"), "h")];
        let queued = vec![
            job("2", "a", JobKind::Validate, "app", "t2"),
            job("3", "a", JobKind::Validate, "app", "t3"),
            job("4", "a", JobKind::Validate, "app", "t4"),
        ];
        assert_eq!(ids(&pick(queued, &run, &q, &none)), ["2", "3"]);
        // Both slots run validate jobs: a merge still starts.
        let run = [
            running(job("8", "a", JobKind::Validate, "app", "t0"), "h"),
            running(job("9", "a", JobKind::Validate, "app", "t0"), "h"),
        ];
        let queued = vec![
            job("2", "a", JobKind::Validate, "app", "t2"),
            job("3", "a", JobKind::Merge, "app", "t3"),
        ];
        assert_eq!(ids(&pick(queued, &run, &q, &none)), ["3"]);
    }

    /// TER-190: a picked job gets a slot that no running job of its
    /// project holds (a cancelled job that drains is in `running`); slots
    /// of other projects do not count; a merge gets [`MERGE_SLOT`].
    #[test]
    fn each_job_gets_a_free_slot_of_its_project() {
        let q = queues(&[("a", 3, "h"), ("b", 3, "h")]);
        let none = HashMap::new();
        let slot = |id: &str, project: &str, s: i64| {
            let mut j = running(job(id, project, JobKind::Validate, "app", "t0"), "h");
            j.slot = Some(s);
            j
        };
        let run = [slot("8", "a", 2), slot("9", "b", 1)];
        let queued = vec![
            job("1", "a", JobKind::Validate, "app", "t1"),
            job("2", "a", JobKind::Merge, "app", "t2"),
            job("3", "a", JobKind::Validate, "app", "t3"),
            job("4", "a", JobKind::Validate, "app", "t4"),
            job("5", "b", JobKind::Validate, "app", "t5"),
        ];
        let picked = pick(queued, &run, &q, &none);
        let got: Vec<(&str, Option<i64>)> = picked
            .iter()
            .map(|(j, _)| (j.id.as_str(), j.slot))
            .collect();
        assert_eq!(
            got,
            [
                ("1", Some(1)),
                ("2", Some(MERGE_SLOT)),
                ("3", Some(3)),
                ("5", Some(2))
            ]
        );
        // The slots went down from 3 to 2 while slot 3 runs: one more job
        // may run, and it gets slot 1.
        let q = queues(&[("a", 2, "h")]);
        let run = [slot("8", "a", 3)];
        let queued = vec![job("1", "a", JobKind::Validate, "app", "t1")];
        let p = pick(queued, &run, &q, &none);
        assert_eq!(p[0].0.slot, Some(1));
    }

    #[test]
    fn one_merge_at_a_time_per_project_in_order() {
        let q = queues(&[("a", 4, "h"), ("b", 4, "h")]);
        let none = HashMap::new();
        // The second merge of `a` waits for the first; `b` has its own lane.
        let queued = vec![
            job("1", "a", JobKind::Merge, "app", "t1"),
            job("2", "a", JobKind::Merge, "lib", "t2"),
            job("3", "b", JobKind::Merge, "app", "t3"),
            job("4", "a", JobKind::Validate, "app", "t4"),
        ];
        assert_eq!(ids(&pick(queued, &[], &q, &none)), ["1", "3", "4"]);
        // A merge of `a` runs: the next merge waits.
        let run = [running(job("0", "a", JobKind::Merge, "app", "t0"), "h")];
        let queued = vec![
            job("2", "a", JobKind::Merge, "lib", "t2"),
            job("4", "a", JobKind::Validate, "app", "t4"),
        ];
        assert_eq!(ids(&pick(queued, &run, &q, &none)), ["4"]);
    }

    #[test]
    fn the_host_cap_counts_a_merge() {
        let q = queues(&[("a", 4, "h"), ("b", 4, "h")]);
        let caps = HashMap::from([("h".to_owned(), 2)]);
        // A running merge takes a place on the host.
        let run = [running(job("0", "a", JobKind::Merge, "app", "t0"), "h")];
        let queued = vec![
            job("1", "a", JobKind::Validate, "app", "t1"),
            job("2", "b", JobKind::Validate, "app", "t2"),
        ];
        assert_eq!(ids(&pick(queued, &run, &q, &caps)), ["1"]);
        // A merge that waits for the host cap holds back the later merges
        // of its project, not its validate jobs or other projects.
        let caps = HashMap::from([("h".to_owned(), 1)]);
        let run = [running(job("0", "b", JobKind::Validate, "r", "t0"), "h")];
        let queued = vec![
            job("1", "a", JobKind::Merge, "app", "t1"),
            job("2", "a", JobKind::Validate, "app", "t2"),
        ];
        assert!(pick(queued, &run, &q, &caps).is_empty());
        let q = queues(&[("a", 4, "h"), ("b", 4, "z")]);
        let caps = HashMap::from([("h".to_owned(), 0)]);
        let queued = vec![
            job("1", "a", JobKind::Merge, "app", "t1"),
            job("2", "a", JobKind::Merge, "lib", "t2"),
            job("3", "b", JobKind::Merge, "app", "t3"),
        ];
        assert_eq!(ids(&pick(queued, &[], &q, &caps)), ["3"]);
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
        assert_eq!(
            address_key("ci@localhost:2222"),
            address_key("localhost:22")
        );
        assert_eq!(address_key("BUILD.INVALID"), "build.invalid");
        assert_eq!(address_key("ci@127.0.0.1:2222"), "127.0.0.1");
    }

    /// TER-105: an alias matches an address as written, or by the pinned
    /// key of both, and only while its host agent exists.
    #[test]
    fn host_agent_aliases_match_by_name_and_by_pin() {
        let dir = tempfile::tempdir().unwrap();
        let conn = crate::db::open(&dir.path().join("state.db")).unwrap();
        conn.execute_batch(
            "INSERT INTO hosts (name, id, token_hash, created_by, created_at)
               VALUES ('helios', 'H1', 'x', 'operator', '');
             INSERT INTO job_host_aliases VALUES ('10.3.0.2', 'H1', 'operator', '');
             INSERT INTO job_host_aliases VALUES ('helios.lan', 'H1', 'operator', '');
             INSERT INTO job_host_aliases VALUES ('gone.lan', 'H9', 'operator', '');
             INSERT INTO job_host_keys VALUES ('vm.lan', '10.3.0.2');
             INSERT INTO job_host_keys VALUES ('helios.lan', '10.3.0.9');",
        )
        .unwrap();
        for (key, want) in [
            ("ci@10.3.0.2:22", "H1"),
            ("HELIOS.LAN:2222", "H1"),
            // Pinned to an IP alias.
            ("vm.lan", "H1"),
            // The pinned key of an alias host name.
            ("10.3.0.9", "H1"),
            ("helios", "H1"),
            ("10.3.0.3", "10.3.0.3"),
            // The host agent of the alias is gone.
            ("gone.lan", "gone.lan"),
        ] {
            assert_eq!(canonical_key(&conn, key).unwrap(), want, "{key}");
        }
        let unresolved = unresolved_hosts(&conn, None).unwrap();
        assert!(
            unresolved.contains(&"gone.lan".to_owned()),
            "{unresolved:?}"
        );
        assert!(!unresolved.contains(&"helios.lan".to_owned()));
    }

    #[test]
    fn missing_target_and_agent_keys_match_caps() {
        let dir = tempfile::tempdir().unwrap();
        let conn = crate::db::open(&dir.path().join("state.db")).unwrap();
        let def: DeliveryDef =
            serde_json::from_value(json!({"target": "MiXeD.INVALID", "recipe": "true"})).unwrap();
        assert_eq!(
            host_of(&conn, &def),
            canonical_key(&conn, "MiXeD.INVALID").unwrap()
        );
        let target: TargetDef =
            serde_json::from_value(json!({"kind": "host", "host": "MiXeD.INVALID"})).unwrap();
        conn.execute("INSERT INTO targets (name, def, source, state, snapshots, created_at, updated_at) VALUES ('box', ?1, 'api', 'unknown', '[]', '', '')", [serde_json::to_string(&target).unwrap()]).unwrap();
        let def: DeliveryDef =
            serde_json::from_value(json!({"target": "box", "recipe": "true"})).unwrap();
        assert_eq!(
            host_of(&conn, &def),
            canonical_key(&conn, "MiXeD.INVALID").unwrap()
        );
    }

    const RED_TAIL: &str = "   Doc-tests terrariumd\nerror: 2 targets failed:\n    `-p terrariumd --test card_reminder`\n    `-p terrariumd --test displays`\nerror: recipe `test` failed with exit code 101\nerror: recipe `validate` failed with exit code 101\n[exit code Some(101), signal None, exited]\n";

    #[test]
    fn the_failed_targets_and_step_come_from_the_tail() {
        assert_eq!(
            failed_targets(RED_TAIL),
            vec![
                "-p terrariumd --test card_reminder",
                "-p terrariumd --test displays"
            ]
        );
        assert_eq!(failed_step(RED_TAIL).as_deref(), Some("test"));
        let one = "error: 1 target failed:\n    `-p terrarium-updater --lib`\nerror: recipe `test` failed with exit code 101\n";
        assert_eq!(failed_targets(one), vec!["-p terrarium-updater --lib"]);
        let lint = "error: could not compile\nerror: recipe `lint` failed on line 33 with exit code 101\nerror: recipe `validate` failed with exit code 101\n";
        assert!(failed_targets(lint).is_empty());
        assert_eq!(failed_step(lint).as_deref(), Some("lint"));
        let killed =
            "     Running tests/card_sessions.rs\n[exit code None, signal Some(9), timeout]\n";
        assert_eq!(failed_step(killed).as_deref(), Some("timeout"));
        assert_eq!(failed_step("ok"), None);
    }

    #[test]
    fn stats_count_first_attempts_reruns_and_targets() {
        let dir = tempfile::tempdir().unwrap();
        let conn = crate::db::open(&dir.path().join("state.db")).unwrap();
        #[allow(clippy::type_complexity)]
        let rows: &[(
            &str,
            &str,
            &str,
            &str,
            i64,
            Option<&str>,
            Option<&str>,
            Option<&str>,
        )] = &[
            // (id, project, kind, state, attempt, outcome, tail, parent)
            ("a1", "p", "validate", "passed", 1, None, None, None),
            (
                "a2",
                "p",
                "validate",
                "failed",
                1,
                Some("red"),
                Some(RED_TAIL),
                None,
            ),
            ("a2r", "p", "validate", "passed", 2, None, None, None),
            (
                "a3",
                "p",
                "merge",
                "failed",
                1,
                Some("conflict"),
                None,
                None,
            ),
            ("a4", "p", "merge", "passed", 1, None, None, None),
            (
                "a5",
                "p",
                "merge",
                "failed",
                1,
                Some("red"),
                Some(
                    "error: 1 target failed:\n    `-p terrariumd --test displays`\nerror: recipe `test` failed\n",
                ),
                None,
            ),
            (
                "a5s",
                "p",
                "merge",
                "failed",
                1,
                Some("red"),
                Some(RED_TAIL),
                Some("a5"),
            ),
            (
                "a8",
                "p",
                "validate",
                "failed",
                1,
                Some("error"),
                Some("the job could not run: host agent `helios` is offline\n"),
                None,
            ),
            (
                "a8r",
                "p",
                "validate",
                "failed",
                2,
                Some("error"),
                Some("the job could not run: host agent `helios` is offline\n"),
                None,
            ),
            (
                "a9",
                "p",
                "validate",
                "failed",
                1,
                Some("red"),
                Some("    `-p terrariumd --test jobs`\nerror: recipe `test` failed\n"),
                None,
            ),
            ("a9r", "p", "validate", "passed", 2, None, None, None),
            ("a6", "p", "validate", "cancelled", 1, None, None, None),
            ("a7", "p", "validate", "queued", 1, None, None, None),
            (
                "q1",
                "q",
                "validate",
                "failed",
                1,
                Some("red"),
                Some(RED_TAIL),
                None,
            ),
        ];
        for (id, project, kind, state, attempt, outcome, tail, parent) in rows {
            conn.execute(
                "INSERT INTO jobs (id, project, card, kind, repo, branch, state, attempt, outcome,
                                   tail, parent, created_by, created_at)
                 VALUES (?1, ?2, 'C', ?3, 'r', 'b', ?4, ?5, ?6, ?7, ?8, 't', '2026-11-06T00:00:00.000Z')",
                params![id, project, kind, state, attempt, outcome, tail, parent],
            )
            .unwrap();
        }
        // Out of the window.
        conn.execute(
            "INSERT INTO jobs (id, project, card, kind, repo, branch, state, attempt, created_by, created_at)
             VALUES ('old', 'p', 'C', 'validate', 'r', 'b', 'failed', 1, 't', '2026-10-01T00:00:00.000Z')",
            [],
        )
        .unwrap();
        let s = stats(&conn, "p", "2026-11-01T00:00:00.000Z").unwrap();
        assert_eq!(s.jobs, 13);
        assert_eq!(
            s.first_attempt,
            FirstAttempts {
                ended: 5,
                passed: 2,
                failed: 3,
                pass_rate: Some(0.4)
            }
        );
        assert_eq!((s.validate.ended, s.validate.passed), (3, 1));
        assert_eq!((s.merge.ended, s.merge.passed), (2, 1));
        assert_eq!((s.reruns.ended, s.reruns.passed), (2, 2));
        assert_eq!((s.conflicts, s.cancelled, s.open, s.splits), (1, 1, 1, 1));
        // The helios-offline pair counts apart, not as fails.
        assert_eq!(s.errors, 2);
        // a9: the clip cut the `error: 1 target failed:` header off.
        assert_eq!(s.untargeted_test_fails, 1);
        assert_eq!(
            s.failed_targets,
            vec![
                TargetFails {
                    target: "-p terrariumd --test displays".into(),
                    failed: 3,
                    first_attempt: 2
                },
                TargetFails {
                    target: "-p terrariumd --test card_reminder".into(),
                    failed: 2,
                    first_attempt: 1
                },
            ]
        );
        assert_eq!(
            s.failed_steps,
            vec![StepFails {
                step: "test".into(),
                failed: 4
            }]
        );
        let empty = stats(&conn, "none", "2026-11-01T00:00:00.000Z").unwrap();
        assert_eq!(empty.first_attempt.pass_rate, None);
        assert_eq!(empty.jobs, 0);
    }
}
