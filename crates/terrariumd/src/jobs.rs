//! The job queue of a project (TER-71): the run time. The rows are in
//! [`crate::store::jobs`].
//!
//! - A tick (each [`TICK`], and at once after a queue or an end) starts
//!   the queued jobs that have a place: first in, first out across
//!   projects, at most the host cap of the target's host (TER-97), then
//!   at most the project's `slots` validate jobs; merge jobs have their
//!   own lane, one at a time per project (TER-159). The pick and the start are one write, so two ticks cannot
//!   overfill the slots.
//! - The repo credential never goes to the target (TER-103). The steps
//!   that need it run in the daemon, which runs no recipe, in a bare repo
//!   of the job at `<state>/jobs/<project>/<job>.git` ([`git::JobRepo`]).
//!   The target runs only steps without any credential. A job is:
//!   1. prepare, in the daemon, with the credential: fetch the card
//!      branch (validate) or `next` and the card branches (merge). A
//!      validate job takes the card commit. A merge job merges each card
//!      branch onto `next` with a merge commit (`git merge-tree`, no work
//!      tree); a conflict ends the job (never resolved, never rerun). A
//!      batch merge (TER-112) merges its card branches in order; a card
//!      that conflicts, or whose merge fails otherwise (unrelated
//!      histories), leaves the batch and the others go on. When none
//!      merges, the job ends as a conflict. The commit is recorded as
//!      `refs/job/head` or `refs/job/merge`, and goes into a bundle.
//!   2. on the target: the bundle goes in on stdin, and the job ref is
//!      fetched from it into the work tree of the job's slot,
//!      `<work_dir>/<project>/slot-<n>` (TER-190, [`slot_dir`]), checked
//!      against the recorded commit, checked out with `-f`, and every
//!      file it does not track but `target/` is removed. The tree stays
//!      for the next job of the slot, so builds reuse `target/`. With a
//!      base, the tree also gets the file of the changed paths (TER-100).
//!      Then the recipe (`sh -c <recipe>` in the tree) runs: the repo's
//!      code, with no credential and no remote. Its env names the job
//!      kind, the slot, and, when `next` gives a base, the file of the
//!      changed paths ([`recipe_env`]).
//!   3. a merge job only, when the recipe passed, in the daemon, with the
//!      credential: push the recorded commit from the daemon's repo to
//!      `next`. Not forced: when `next` moved, the push fails. The recipe
//!      never had that repo, so nothing it did reaches the push.
//!
//!   Each step on the target runs as `ionice -c3 nice -n19 <argv>`
//!   (`nice` only on a target without `ionice`), so jobs do not starve
//!   their host. Then the daemon's repo is removed; the slot's tree on
//!   the target stays.
//! - A step that hits its timeout, or a cancel, ends the process group and
//!   the session of the step on the target (`TERM`, then `KILL`), through
//!   [`crate::run_kill`]: killing the `ssh` or `docker exec` client alone
//!   leaves the recipe running. The job keeps its slot (and its place on
//!   the host cap) until that kill ended (TER-115).
//! - On the end the daemon moves the card itself (as the operator, with a
//!   comment that starts "Job queue:"): a pass moves a validate card to
//!   `review` and a merge card to `accepted`. A failure is rerun once
//!   (not a conflict). The second failure writes a "Validation failed"
//!   block at the top of the card body, moves the card to the top of
//!   `ready`, and mails the worker of its `by:<agent>` label. The final
//!   move (a pass, or the second failure) also clears the assignee.
//! - A batch merge (TER-112): a pass moves each merged card to `accepted`.
//!   A card that conflicted fails at once. A red batch of two or more
//!   merged cards splits into two halves ([`jobs::split`]), each queued at
//!   the front, the left half first, so the failing card is found by
//!   bisection and the others merge. A card split out alone that is red
//!   fails without the rerun: the red batch was its first red. An error
//!   (not red) runs the batch once more.
//! - The labels `validation:queued` and `validation:running` follow the
//!   job, so the board shows it.
//! - A daemon restart loses the runs (TER-123). The new daemon ends what
//!   still runs of them on their targets, then puts each running job back
//!   in the queue with its attempt and its place ([`jobs::requeue_lost`]):
//!   a restart is not a failure. Each step of a job writes its PID to the same file
//!   ([`crate::run_kill::job_pidfile`]), and [`recover_lost`] runs the
//!   kill of [`crate::run_kill`] on that file, for the running jobs and
//!   for the jobs cancelled in the last [`DRAIN_WINDOW_HOURS`] (they may
//!   still have been draining). Their slots stay taken until the kill
//!   ended, and the kill waits for a host agent that has not connected
//!   yet.
//! - A deploy drains the daemon first (`POST /api/system/update/drain`):
//!   while it drains, no job starts, and the running ones go on. The drain
//!   lists them ([`busy_jobs`]) and waits for them; one that still runs at
//!   the stop is killed and put back by the next daemon (above).
//! - A host that cannot be reached is not a failure (TER-160). A job whose
//!   target's host agent is offline is not started: it stays queued. A job
//!   whose target could not be reached before its recipe started (the host
//!   agent went away, or the ssh connect was refused or timed out) goes
//!   back to the queue ([`jobs::requeue`]); an ssh target is tried again
//!   after [`RETRY`]. Neither uses an attempt or moves a card. After
//!   [`LONG_WAIT`] the queue notes the wait on the cards, once per job and
//!   daemon run. A failure after the recipe started stays a failure.
//! - Each end and each cancel emits one `job.verdict` event per card
//!   (TER-167): `passed`, `red`, `conflict`, `cancelled`, or `error`, with
//!   the end comment and the failure block. It is written in the same
//!   write as the card flow above, which it does not change.
//! - With `board_flow = rules` (TER-170, [`code_flow`]) the queue runs the
//!   jobs and emits the `job` and `job.verdict` events only: the labels,
//!   the moves, the comments, the rerun, the split, and the mail above
//!   come from the board rules (the built-in pack, `rules.default.wcl`).
//!   The texts are the same: the pack fills them from [`queue_comment`],
//!   [`rerun_comment`], [`split_comment`], [`fail_comment`], and
//!   [`fail_mail`].

use std::collections::{BTreeMap, HashMap, HashSet};
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, MutexGuard, OnceLock};
use std::time::{Duration, Instant};

use serde_json::Value;
use tokio::sync::mpsc;
use tokio::task::JoinHandle;
use tracing::{debug, info, warn};

use terrarium_core::config::{DeliveryDef, TargetDef, TargetKind};

use crate::actor::Actor;
use crate::api::Shared;
use crate::db::{Tx, now};
use crate::error::{ApiError, ApiResult};
use crate::events::EventDraft;
use crate::executor::{self, Collected, Control, End, RemoteCmd};
use crate::git;
use crate::policy;
use crate::secrets::{self, RunDirs};
use crate::store::board::{self, CardPatch};
use crate::store::jobs::{self, BatchCard, Job, JobEnd, JobKind};
use crate::store::mail::{self, Address, Outgoing};
use crate::store::{repos, targets};

/// How often the queue looks for work with nothing to wake it.
pub const TICK: Duration = Duration::from_secs(5);
/// The label of a card whose job waits.
pub const LABEL_QUEUED: &str = "validation:queued";
/// The label of a card whose job runs.
pub const LABEL_RUNNING: &str = "validation:running";
/// The name of the column of a card whose job is open, used when no column
/// has the role `validating` (see `board::role_name`).
pub const COL_VALIDATING: &str = "validating";
/// The name of the column where a validate pass goes, used when no column
/// has the role `review`.
pub const COL_REVIEW: &str = "review";
/// The name of the column where a merge pass goes, used when no column has
/// the role `accepted`.
pub const COL_ACCEPTED: &str = "accepted";
/// Where a failure goes, at the top.
pub const COL_READY: &str = "ready";
/// The most output of a prepare or push step that the daemon reads.
const STEP_MAX: usize = 64 * 1024;
/// The most output of the recipe that the daemon reads per stream.
const RECIPE_MAX: usize = 1024 * 1024;
/// The time limit of a prepare, push, or cleanup step.
const STEP_TIMEOUT: u64 = 900;
/// How long before a daemon start a cancelled job may have been still
/// draining: its kill at the start then runs again (TER-123).
pub const DRAIN_WINDOW_HOURS: i64 = 1;
/// How long a job waits for its host before the queue notes it on the
/// cards (TER-160).
pub const LONG_WAIT: Duration = Duration::from_secs(30 * 60);
/// How long a job whose ssh target could not be reached waits before the
/// next try (TER-160).
pub const RETRY: Duration = Duration::from_secs(60);
/// The size of one stdin chunk of a step (the bundle of a job).
const STDIN_CHUNK: usize = 64 * 1024;
/// The dir of the daemon's job repos, in the state dir (TER-103).
const JOBS_DIR: &str = "jobs";

/// The controls of the running jobs, by job ID, for a cancel. The job IDs
/// are ULIDs, so one map serves every daemon in a process (the tests).
fn controls() -> &'static Mutex<HashMap<String, mpsc::UnboundedSender<Control>>> {
    static C: OnceLock<Mutex<HashMap<String, mpsc::UnboundedSender<Control>>>> = OnceLock::new();
    C.get_or_init(|| Mutex::new(HashMap::new()))
}

fn set_control(job: &str, ctl: Option<mpsc::UnboundedSender<Control>>) {
    let mut m = match controls().lock() {
        Ok(m) => m,
        Err(p) => p.into_inner(),
    };
    match ctl {
        Some(c) => {
            m.insert(job.to_owned(), c);
        }
        None => {
            m.remove(job);
        }
    }
}

/// The IDs of the jobs whose run has not ended yet ([`tick`] adds and
/// removes them), and of the ones among them that got a [`kill`]. A
/// cancelled job keeps its place in the queue while it drains (TER-115).
#[derive(Default)]
struct Live {
    runs: HashSet<String>,
    killed: HashSet<String>,
}

fn live() -> MutexGuard<'static, Live> {
    static L: OnceLock<Mutex<Live>> = OnceLock::new();
    match L.get_or_init(Mutex::default).lock() {
        Ok(m) => m,
        Err(p) => p.into_inner(),
    }
}

/// A job in [`Live::runs`] while this lives: a run that ends in any way,
/// a panic too, frees its place.
struct LiveRun(String);

impl LiveRun {
    fn new(job: &str) -> Self {
        live().runs.insert(job.to_owned());
        Self(job.to_owned())
    }
}

impl Drop for LiveRun {
    fn drop(&mut self) {
        let mut l = live();
        l.runs.remove(&self.0);
        l.killed.remove(&self.0);
    }
}

/// True when the job got a [`kill`]: its next steps do not start.
fn killed(job: &str) -> bool {
    live().killed.contains(job)
}

/// Kill the current step of a running job, and keep the next steps from
/// starting (the clean-up of its clone still runs). The job then ends as
/// cancelled (its row is cancelled first, so the end does nothing). The
/// kill ends the processes of the step on the target, and the job keeps
/// its place until they are gone.
pub fn kill(job: &str) {
    // Marked whether the run is there yet or not, so a run that starts
    // after the cancel starts no step. [`LiveRun`] clears the mark.
    live().killed.insert(job.to_owned());
    let m = match controls().lock() {
        Ok(m) => m,
        Err(p) => p.into_inner(),
    };
    if let Some(c) = m.get(job) {
        let _ = c.send(Control::Kill);
    }
}

/// Start the queue: take up the jobs that the last daemon left
/// ([`recover_lost`]), then tick. The kills of their runs go on in the
/// background, while their slots stay taken.
pub fn spawn(state: &Shared) -> JoinHandle<()> {
    // The daemon's job repos of the last daemon: its jobs are lost (they
    // start again from the queue below), so none of these is in use. Before any tick.
    let stale = state.state_dir.join(JOBS_DIR);
    if let Err(e) = std::fs::remove_dir_all(&stale)
        && e.kind() != std::io::ErrorKind::NotFound
    {
        warn!(dir = %stale.display(), "cannot remove the old job repos: {e}");
    }
    let weak = Arc::downgrade(state);
    tokio::spawn(async move {
        if let Some(state) = weak.upgrade() {
            if let Err(e) = prepare_hosts(&state.db, None).await {
                warn!("cannot initialize stored job host keys: {e}");
            }
            drop(recover_lost(&state).await);
        }
        loop {
            let Some(state) = weak.upgrade() else { break };
            if let Err(e) = tick(&state).await {
                warn!("job queue tick failed: {e}");
            }
            drop(state);
            tokio::time::sleep(TICK).await;
        }
    })
}

/// A job that the last daemon left, found by [`recover_lost`].
struct Lost {
    job: Job,
    /// Still `running` in the database (else cancelled in the drain
    /// window).
    running: bool,
    /// The delivery target of its project, when it is a target of that
    /// project.
    def: Option<terrarium_core::config::TargetDef>,
    /// The job is in [`Live::runs`] while its kill runs, so its slot is
    /// not free, also after a cancel of a running one (TER-123).
    live: LiveRun,
    /// It ran on another host than the delivery target's now: the kill
    /// may miss its processes.
    other_host: bool,
}

/// TER-123: end the processes that the jobs of the last daemon left on
/// their targets, before their slots count as free.
///
/// The jobs are the running ones, and the ones cancelled in the last
/// [`DRAIN_WINDOW_HOURS`] (they may still have been draining). Jobs of
/// this daemon (in [`Live::runs`]) are not among them. They are found in
/// one write, so a tick (a write too) sees them in order: a running job
/// keeps its slot by its state, and each one is put in [`Live::runs`] in
/// the same write, so a cancel during its kill does not free its slot.
///
/// Then, in the returned task, for each job at the same time: the kill of
/// [`crate::run_kill`] runs on the PID file of its steps
/// ([`crate::run_kill::job_pidfile`]) on the delivery target of its
/// project. It waits for a host agent that has not connected to this
/// daemon yet ([`crate::run_kill::HOST_WAIT_SECS`]) and is bounded by the
/// timeout of the kill command. Then a running job goes back to the
/// queue with its attempt (the run is lost), each leaves [`Live::runs`],
/// and a tick follows. A kill that fails is logged, and the job goes on
/// anyway.
pub async fn recover_lost(state: &Shared) -> JoinHandle<()> {
    let since = (chrono::Utc::now() - chrono::Duration::hours(DRAIN_WINDOW_HOURS))
        .to_rfc3339_opts(chrono::SecondsFormat::Millis, true);
    let found = state
        .db
        .write("system", move |t| {
            let ours: HashSet<String> = live().runs.iter().cloned().collect();
            let lost = jobs::running(t)?
                .into_iter()
                .map(|j| (j, true))
                .chain(
                    jobs::cancelled_since(t, &since)?
                        .into_iter()
                        .map(|j| (j, false)),
                )
                .filter(|(j, _)| !ours.contains(&j.id))
                .collect::<Vec<_>>();
            let mut out = Vec::new();
            for (job, running) in lost {
                let delivery = jobs::delivery(t, &job.project)?;
                let target = delivery.target.clone();
                let def = match target.as_deref().map(|n| targets::get(t, n)) {
                    Some(Ok(row)) if row.def.project.as_ref().is_none_or(|p| *p == job.project) => {
                        Some(row.def)
                    }
                    _ => {
                        warn!(job = %job.id, delivery_target = ?target,
                            "cannot kill a lost job run: no such target of its project");
                        None
                    }
                };
                // The kill goes to the current delivery target. When the
                // job ran on another host, the kill misses its processes
                // (it cannot hit others: the PID file is per job).
                let mut other_host = false;
                if def.is_some()
                    && let Some(ran_on) = job.host.as_deref()
                {
                    let now_on = jobs::host_of(t, &delivery);
                    if ran_on != now_on {
                        other_host = true;
                        warn!(job = %job.id, ran_on, delivery_host = %now_on,
                            "a lost job ran on another host than its delivery target now; its kill may miss");
                    }
                }
                // Every lost job is in [`Live::runs`] while its kill runs:
                // a running one that gets a cancel meanwhile keeps its
                // slot as a draining job (TER-123 review 2, major 1).
                let live = LiveRun::new(&job.id);
                out.push(Lost {
                    job,
                    running,
                    def,
                    live,
                    other_host,
                });
            }
            Ok(out)
        })
        .await;
    let found = match found {
        Ok(f) => f,
        Err(e) => {
            warn!("cannot read the lost jobs: {e}");
            Vec::new()
        }
    };
    let state = state.clone();
    tokio::spawn(async move {
        let ends = found.into_iter().map(|l| end_lost(&state, l));
        futures_util::future::join_all(ends).await;
    })
}

/// Kill what still runs of lost job `l` on its target, then end it: see
/// [`recover_lost`].
async fn end_lost(state: &Shared, l: Lost) {
    let Lost {
        job,
        running,
        def,
        live,
        other_host,
    } = l;
    let mut report = None;
    let wraps = def.as_ref().is_some_and(crate::run_kill::kind_wraps);
    match def {
        Some(def) if wraps => {
            let r = crate::run_kill::kill_file(
                state,
                &def,
                Some(format!("{}/jobs", job.project)),
                &crate::run_kill::job_pidfile(&job.id),
            )
            .await;
            let id = &job.id;
            if r.get("error").is_some() {
                warn!(job = %id, report = %r, "cannot kill a lost job run on its target");
            } else if r["left"].as_array().is_some_and(|l| !l.is_empty()) {
                warn!(job = %id, report = %r, "the kill of a lost job run left processes on the target");
            } else if !r["stale"].is_null() {
                info!(job = %id, report = %r, "the PID file of a lost job run was stale");
            } else if r["pid"].is_null() {
                debug!(job = %id, "a lost job run had no PID file on its target");
            } else {
                info!(job = %id, report = %r, "killed a lost job run on its target");
            }
            report = Some(r);
        }
        Some(_) => debug!(job = %job.id, "a lost job run on a target without PID files"),
        None => {}
    }
    // TER-190 review, finding 2: processes of the run may still write into
    // the slot's tree. The slot is free below, so the next job of the slot
    // makes its tree new first. The mark is written before the slot frees.
    if let Some(slot) = job.slot
        && !kill_confirmed(wraps && !other_host, report.as_ref())
    {
        let (project, id) = (job.project.clone(), job.id.clone());
        let marked = state
            .db
            .write("system", move |t| {
                jobs::mark_slot_wipe(t, &project, slot, &id)
            })
            .await;
        match marked {
            Ok(()) => warn!(job = %job.id, slot,
                "the kill of a lost job run did not confirm its end; its slot's tree is made new before the next job"),
            Err(e) => {
                warn!(job = %job.id, slot, "cannot mark the slot of a lost job for a wipe: {e}")
            }
        }
    }
    if running {
        // P5: the run is lost, not failed. The job goes back to the queue
        // with its attempt and its place, as TER-160 does for a host that
        // could not be reached.
        let id = job.id.clone();
        let cards = job.cards();
        let back = state
            .db
            .write("system", move |t| {
                let j = jobs::requeue_lost(t, &id)?;
                if let Some(j) = &j
                    && code_flow(t, &j.project)?
                {
                    for c in &cards {
                        set_labels(t, &c.card, &[LABEL_QUEUED], &[LABEL_RUNNING])?;
                    }
                }
                Ok(j)
            })
            .await;
        match back {
            Ok(Some(j)) => {
                info!(job = %j.id, attempt = j.attempt, "a lost job waits in the queue again")
            }
            Ok(None) => debug!(job = %job.id, "a lost job was cancelled during its kill"),
            Err(e) => warn!(job = %job.id, "cannot put a lost job back in the queue: {e}"),
        }
    }
    drop(live);
    kick(state);
}

/// True when the kill of a lost job run shows that none of its processes
/// is left (TER-190 review, finding 2). `kill_hits`: the target has job
/// PID files ([`crate::run_kill::kind_wraps`]) and the job ran on the host
/// that got the kill. `report` is the kill's report
/// ([`crate::run_kill::kill_file`]). A failed kill, processes left, a
/// target without PID files, another host, or no target: not confirmed.
/// No PID file, or a stale one, is a run with no process left.
fn kill_confirmed(kill_hits: bool, report: Option<&Value>) -> bool {
    let Some(r) = report.filter(|_| kill_hits) else {
        return false;
    };
    r.get("error").is_none() && !r["left"].as_array().is_some_and(|l| !l.is_empty())
}

/// The checks of a job queue config before it is written, by `PUT
/// /api/projects/{name}/delivery` and by a fleet apply (TER-87). The
/// recipe runs on the target: a target of this project, or a global one
/// only when a sysadmin or the operator sets it, and one that may run
/// commands. Each of `next_repos` is a repo of the project.
pub fn check_delivery(
    conn: &rusqlite::Connection,
    actor: &Actor,
    project: &str,
    def: &DeliveryDef,
) -> ApiResult<()> {
    if let Some(target) = def.target.as_deref().filter(|t| !t.is_empty()) {
        let row = targets::get(conn, target)?;
        match &row.def.project {
            Some(p) if p == project => {}
            Some(p) => {
                return Err(ApiError::forbidden(format!(
                    "target `{target}` belongs to project `{p}`; the job queue of `{project}` uses its own targets"
                )));
            }
            None => policy::sysadmin(
                actor,
                &format!("running the job queue of `{project}` on the global target `{target}`"),
            )?,
        }
        executor::no_exec(&row.def)?;
    }
    for r in &def.next_repos {
        if repos::find_by_name(conn, project, r)?.is_none() {
            return Err(ApiError::invalid(format!(
                "`next_repos` names `{r}`, which is not a repo of `{project}`"
            )));
        }
    }
    Ok(())
}

/// The jobs of this daemon whose run has not ended yet: the running ones,
/// and the cancelled ones that still drain (TER-123, P5). A deploy waits
/// for them in its drain.
pub async fn busy_jobs(state: &Shared) -> Vec<String> {
    let live: Vec<String> = live().runs.iter().cloned().collect();
    let got = state
        .db
        .read(move |c| {
            let mut out: Vec<String> = jobs::running(c)?.into_iter().map(|j| j.id).collect();
            for id in live {
                // A job of another daemon in the same process (the tests)
                // is not in this database.
                if !out.contains(&id) && jobs::get(c, &id).is_ok() {
                    out.push(id);
                }
            }
            out.sort();
            Ok(out)
        })
        .await;
    got.unwrap_or_else(|e| {
        warn!("cannot read the busy jobs: {e}");
        Vec::new()
    })
}

/// Start a tick now, in the background.
pub fn kick(state: &Shared) {
    let state = state.clone();
    tokio::spawn(async move {
        if let Err(e) = tick(&state).await {
            warn!("job queue tick failed: {e}");
        }
    });
}

/// A queued job that waits for its host (TER-160).
struct Wait {
    /// The first time that the host could not be reached.
    since: Instant,
    /// No try before this time (an ssh target that could not be reached).
    retry: Option<Instant>,
    /// The cards have the [`LONG_WAIT`] note.
    noted: bool,
    why: String,
}

/// The jobs that wait for their host, by job ID.
fn waits() -> MutexGuard<'static, HashMap<String, Wait>> {
    static W: OnceLock<Mutex<HashMap<String, Wait>>> = OnceLock::new();
    W.get_or_init(|| Mutex::new(HashMap::new()))
        .lock()
        .unwrap_or_else(|p| p.into_inner())
}

/// Note that job `id` waits for its host, and the time of its next try.
fn hold(id: &str, why: String, retry: Option<Instant>) {
    let now = Instant::now();
    let mut w = waits();
    let e = w.entry(id.to_owned()).or_insert_with(|| Wait {
        since: now,
        retry: None,
        noted: false,
        why: String::new(),
    });
    e.why = why;
    e.retry = retry;
}

/// Why the host of `def`'s target is down now: its host agent is offline.
/// `None` for an ssh target (only a try tells) and for a target that is
/// gone (the job then fails when it runs).
fn host_down(conn: &rusqlite::Connection, state: &Shared, def: &DeliveryDef) -> Option<String> {
    let name = def.target.as_deref()?;
    let row = targets::get(conn, name).ok()?;
    agent_down(state, name, &row.def)
}

/// [`host_down`] of a target that is read.
fn agent_down(state: &Shared, name: &str, def: &TargetDef) -> Option<String> {
    if def.kind == TargetKind::Ssh {
        return None;
    }
    let host = def.host.as_deref()?;
    (!state.executor.hosts.is_online(host))
        .then(|| format!("the host agent `{host}` of target `{name}` is offline"))
}

/// Note each job that waited [`LONG_WAIT`] for its host on its cards, once.
/// Forget the jobs of this database that are no longer queued or running.
fn note_long_waits(t: &mut Tx<'_>) -> ApiResult<()> {
    let ids: Vec<(String, bool, String, u64)> = waits()
        .iter()
        .map(|(id, w)| {
            (
                id.clone(),
                !w.noted && w.since.elapsed() >= LONG_WAIT,
                w.why.clone(),
                w.since.elapsed().as_secs() / 60,
            )
        })
        .collect();
    for (id, due, why, mins) in ids {
        // A job of another daemon in the same process (the tests) is not
        // in this database.
        let Ok(j) = jobs::get(t, &id) else { continue };
        if !j.state.is_open() {
            waits().remove(&id);
            continue;
        }
        if !due || j.state != jobs::JobState::Queued {
            continue;
        }
        let note = format!(
            "Job queue: {} job {} has waited {mins} min for its host: {why}. It uses no attempt, the card stays here, and the job starts when the host is back.",
            j.kind.as_str(),
            j.id
        );
        for c in j.cards() {
            comment(t, &c.card, &note)?;
        }
        if let Some(w) = waits().get_mut(&id) {
            w.noted = true;
        }
    }
    Ok(())
}

/// Start each queued job that has a place: on its host (the host cap,
/// across projects) and in its project (the slots). One write for all
/// projects, so the host counts hold. A job whose host agent is offline,
/// or whose ssh target waits for its next try, stays queued (TER-160).
pub async fn tick(state: &Shared) -> ApiResult<()> {
    prepare_hosts(&state.db, None).await?;
    // A deploy drains the daemon first: no job starts, and the running
    // ones go on (TER-123, P5). The end of the drain kicks the queue.
    let drain = state.self_update.draining();
    // The live runs are read and added in the write, so two ticks and a
    // cancel (all writes) see them in order.
    let st = state.clone();
    let started = state
        .db
        .write("system", move |t| {
            let draining: Vec<String> = live().runs.iter().cloned().collect();
            let mut out = Vec::new();
            let picks = if drain {
                Vec::new()
            } else {
                jobs::startable(t, &draining)?
            };
            for (j, host, def) in picks {
                if let Some(why) = host_down(t, &st, &def) {
                    hold(&j.id, why, None);
                    continue;
                }
                let later = waits()
                    .get(&j.id)
                    .and_then(|w| w.retry)
                    .is_some_and(|r| r > Instant::now());
                if later {
                    continue;
                }
                if jobs::start(t, &j.id, &host, j.slot)? {
                    let run = LiveRun::new(&j.id);
                    if code_flow(t, &j.project)? {
                        for c in j.cards() {
                            set_labels(t, &c.card, &[LABEL_RUNNING], &[LABEL_QUEUED])?;
                        }
                    }
                    out.push((jobs::get(t, &j.id)?, def, run));
                }
            }
            note_long_waits(t)?;
            Ok(out)
        })
        .await?;
    for (job, def, live_run) in started {
        let state = state.clone();
        tokio::spawn(async move {
            let end = match run(&state, &job, &def).await {
                Ran::End(end) => end,
                Ran::Wait { why, ssh } => {
                    set_control(&job.id, None);
                    requeue(&state, &job, why, ssh).await;
                    drop(live_run);
                    return;
                }
            };
            set_control(&job.id, None);
            waits().remove(&job.id);
            let id = job.id.clone();
            let ended = state
                .db
                .write("system", move |t| jobs::finish(t, &id, &end))
                .await;
            match ended {
                Ok(Some(j)) => {
                    if let Err(e) = after_end(&state, &j).await {
                        warn!(job = %j.id, "cannot handle the end of a job: {e}");
                    }
                }
                Ok(None) => debug!(job = %job.id, "the job was cancelled while it ran"),
                Err(e) => warn!(job = %job.id, "cannot end a job: {e}"),
            }
            drop(live_run);
            kick(&state);
        });
    }
    Ok(())
}

/// Resolve only unpinned names, on the blocking pool and outside DB closures.
/// The persisted first answer (including failures) keeps counts and caps stable.
pub async fn prepare_hosts(db: &crate::db::Db, extra: Option<String>) -> ApiResult<()> {
    prepare_hosts_with(db, extra, |hostname| {
        use std::net::ToSocketAddrs;
        let mut ips: Vec<_> = (hostname, 22)
            .to_socket_addrs()
            .map(|all| all.map(|s| s.ip()).collect())
            .unwrap_or_default();
        ips.sort();
        ips.dedup();
        ips.first()
            .map_or_else(|| hostname.to_owned(), ToString::to_string)
    })
    .await
}

async fn prepare_hosts_with(
    db: &crate::db::Db,
    extra: Option<String>,
    resolve: impl Fn(&str) -> String + Send + 'static,
) -> ApiResult<()> {
    let names = db.read(move |c| jobs::unresolved_hosts(c, extra)).await?;
    if names.is_empty() {
        return Ok(());
    }
    let keys = tokio::task::spawn_blocking(move || {
        names
            .into_iter()
            .map(|name| {
                let key = resolve(&name);
                (name, key)
            })
            .collect::<Vec<_>>()
    })
    .await
    .map_err(|e| ApiError::internal(format!("host resolver failed: {e}")))?;
    db.write("system", move |t| jobs::pin_hosts(t, keys)).await
}

/// Does the daemon's code run the card flow of `project` (`board_flow =
/// code`, TER-170)? With `rules` it only emits the job events, and the
/// board rules set the labels, move, and comment.
pub fn code_flow(conn: &rusqlite::Connection, project: &str) -> ApiResult<bool> {
    Ok(!crate::store::projects::rules_flow(conn, project)?)
}

/// Remove the job labels of the cards of `j` (a cancel), as the operator.
/// A card that is gone or archived is left alone.
pub fn clear_job_labels(t: &mut Tx<'_>, j: &Job) -> ApiResult<()> {
    for c in j.cards() {
        set_labels(t, &c.card, &[], &[LABEL_QUEUED, LABEL_RUNNING])?;
    }
    Ok(())
}

/// Add and take away card labels, keeping the others. A card that is gone
/// or archived is left alone.
fn set_labels(t: &mut Tx<'_>, card: &str, add: &[&str], remove: &[&str]) -> ApiResult<()> {
    let patch = CardPatch {
        add_labels: add.iter().map(|s| (*s).to_owned()).collect(),
        remove_labels: remove.iter().map(|s| (*s).to_owned()).collect(),
        ..CardPatch::default()
    };
    match board::update_card(t, &Actor::Operator, card, patch) {
        Ok(_) => Ok(()),
        Err(e) if e.code.status().is_client_error() => {
            debug!(card, "cannot change the job labels: {}", e.message);
            Ok(())
        }
        Err(e) => Err(e),
    }
}

/// The last `TERRARIUM_JOB` line of a step, as key and value pairs.
fn result_line(stdout: &[u8]) -> HashMap<String, String> {
    let text = String::from_utf8_lossy(stdout);
    let mut out = HashMap::new();
    if let Some(line) = text
        .lines()
        .rev()
        .find(|l| l.starts_with("TERRARIUM_JOB\t"))
    {
        for kv in line.split('\t').skip(1) {
            if let Some((k, v)) = kv.split_once('=') {
                out.insert(k.to_owned(), v.to_owned());
            }
        }
    }
    out
}

/// The text of the end of a step, for the tail.
fn end_text(end: Option<&End>) -> String {
    match end {
        Some(End::Exit {
            code,
            signal,
            reason,
        }) => format!("exit code {code:?}, signal {signal:?}, {reason}"),
        Some(End::Error { code, message }) => format!("error {code}: {message}"),
        Some(End::Cp { .. }) => "a copy ended".into(),
        None => "the run ended without an end".into(),
    }
}

fn exit_code(end: Option<&End>) -> Option<i64> {
    match end {
        Some(End::Exit { code: Some(c), .. }) => Some(i64::from(*c)),
        _ => None,
    }
}

/// The output of a step: stdout, then stderr.
fn output_of(c: &Collected) -> String {
    let mut s = String::from_utf8_lossy(&c.stdout).into_owned();
    let err = String::from_utf8_lossy(&c.stderr);
    if !err.trim().is_empty() {
        if !s.is_empty() && !s.ends_with('\n') {
            s.push('\n');
        }
        s.push_str(&err);
    }
    s
}

fn error_end(message: impl Into<String>) -> JobEnd {
    JobEnd {
        passed: false,
        outcome: Some("error".into()),
        tail: Some(message.into()),
        ..JobEnd::default()
    }
}

/// Run one step on the target and read it to the end. The step can be
/// killed through [`kill`] while it runs.
///
/// A kill, or the step's `timeout_secs`, ends the whole process group and
/// session of the step on the target ([`crate::run_kill`]), not only the
/// local `ssh` or `docker exec` client (TER-115). The step returns only
/// after that kill ended, so the job keeps its slot until its processes
/// are gone.
async fn step(
    state: &Shared,
    job: &str,
    def: &terrarium_core::config::TargetDef,
    cmd: &RemoteCmd,
    max: usize,
    input: Option<Vec<u8>>,
) -> ApiResult<Collected> {
    let pidfile = crate::run_kill::job_pidfile(job);
    let (running, report) = crate::run_kill::start_named(state, def, cmd, pidfile)?;
    set_control(job, Some(running.ctl.clone()));
    // A kill between the check in [`StepCtx::run`] and set_control found
    // the old control or none: kill this step now.
    if killed(job) {
        let _ = running.ctl.send(Control::Kill);
    }
    // The input goes in chunks, then the end of stdin. Without input,
    // stdin stays open and unused, as before.
    if let Some(data) = input {
        for chunk in data.chunks(STDIN_CHUNK) {
            let _ = running.ctl.send(Control::Stdin(chunk.to_vec()));
        }
        let _ = running.ctl.send(Control::Eof);
    }
    let mut c = running.collect(max).await;
    let kill = match report {
        Some(rx) => crate::run_kill::report(rx).await,
        None => None,
    };
    if let Some(r) = kill {
        c.stderr
            .extend_from_slice(format!("\n[killed on the target: {r}]\n").as_bytes());
    }
    Ok(c)
}

/// How a run of a job ended.
enum Ran {
    /// The job ran: a pass or a failure.
    End(JobEnd),
    /// The target could not be reached before the recipe started
    /// (TER-160): not a failure. `ssh`: an ssh target, tried again after
    /// [`RETRY`].
    Wait { why: String, ssh: bool },
}

/// Put a job that could not reach its host back in the queue: no attempt,
/// no card move (TER-160).
async fn requeue(state: &Shared, job: &Job, why: String, ssh: bool) {
    let retry = ssh.then(|| Instant::now() + RETRY);
    hold(&job.id, why.clone(), retry);
    let id = job.id.clone();
    let cards = job.cards();
    let back = state
        .db
        .write("system", move |t| {
            let j = jobs::requeue(t, &id, &why)?;
            if let Some(j) = &j
                && code_flow(t, &j.project)?
            {
                for c in &cards {
                    set_labels(t, &c.card, &[LABEL_QUEUED], &[LABEL_RUNNING])?;
                }
            }
            Ok(j)
        })
        .await;
    match back {
        Ok(Some(j)) => debug!(job = %j.id, "the job waits for its host"),
        Ok(None) => debug!(job = %job.id, "the job was cancelled while it ran"),
        Err(e) => warn!(job = %job.id, "cannot put a job back in the queue: {e}"),
    }
}

/// Run a job to its end. Never fails: an error is a failed end, or a
/// wait when the target could not be reached before the recipe started.
async fn run(state: &Shared, job: &Job, def: &DeliveryDef) -> Ran {
    let recipe = AtomicBool::new(false);
    let end = match run_inner(state, job, def, &recipe).await {
        Ok(end) => end,
        Err(e) => error_end(format!("the job could not run: {}", e.message)),
    };
    if end.passed || end.outcome.as_deref() != Some("error") || recipe.load(Ordering::SeqCst) {
        return Ran::End(end);
    }
    match unreachable(state, def, &end).await {
        Some((why, ssh)) => Ran::Wait { why, ssh },
        None => Ran::End(end),
    }
}

/// Why the target of `def` could not be reached, for an error end before
/// the recipe started: its host agent is offline now, or the ssh client
/// could not connect to the target (exit code 255 and its connect error).
/// `true` with an ssh target.
async fn unreachable(state: &Shared, def: &DeliveryDef, end: &JobEnd) -> Option<(String, bool)> {
    let name = def.target.clone()?;
    let n = name.clone();
    let row = state.db.read(move |c| targets::get(c, &n)).await.ok()?;
    if row.def.kind != TargetKind::Ssh {
        return agent_down(state, &name, &row.def).map(|w| (w, false));
    }
    if end.exit_code != Some(255) {
        return None;
    }
    let host = jobs::address_key(row.def.address.as_deref()?);
    ssh_connect_error(end.tail.as_deref()?, &host)
        .map(|l| (format!("target `{name}` could not be reached: {l}"), true))
}

/// The line of `text` in which the ssh client says that it could not
/// connect to `host` (lower case): refused, timed out, no route, or a
/// name that does not resolve. Not a line about another host, such as the
/// git remote of the prepare step.
fn ssh_connect_error<'a>(text: &'a str, host: &str) -> Option<&'a str> {
    text.lines().map(str::trim).find(|l| {
        let low = l.to_lowercase();
        low.starts_with(&format!("ssh: connect to host {host} port "))
            || low.starts_with(&format!("ssh: could not resolve hostname {host}:"))
            || (low.starts_with(&format!("connection to {host} port "))
                && low.contains("timed out"))
    })
}

async fn run_inner(
    state: &Shared,
    job: &Job,
    def: &DeliveryDef,
    recipe_started: &AtomicBool,
) -> ApiResult<JobEnd> {
    let target = def
        .target
        .clone()
        .ok_or_else(|| ApiError::invalid("the project has no delivery target"))?;
    let recipe = def
        .recipe
        .clone()
        .ok_or_else(|| ApiError::invalid("the project has no delivery recipe"))?;
    let (project, repo_name, t_name) = (job.project.clone(), job.repo.clone(), target.clone());
    let (repo, row) = state
        .db
        .read(move |c| {
            let repo = repos::find_by_name(c, &project, &repo_name)?.ok_or_else(|| {
                ApiError::not_found(format!("no repo `{repo_name}` in `{project}`"))
            })?;
            Ok((repo, targets::get(c, &t_name)?))
        })
        .await?;
    if let Some(p) = &row.def.project
        && *p != job.project
    {
        return Err(ApiError::forbidden(format!(
            "target `{target}` belongs to project `{p}`, not `{}`",
            job.project
        )));
    }
    let secret = secrets::secret_of(state, repo.owner(), repo.credential.as_deref()).await?;
    let slot = job
        .slot
        .ok_or_else(|| ApiError::internal("the job has no slot"))?;
    let dir = slot_dir(def, &job.project, slot);
    check_ref(&job.branch)?;
    if let Some(c) = &job.commit {
        check_commit(c)?;
    }
    // The daemon's repo of the job (TER-103): the credential steps run
    // there, never on the target.
    let local = job_repo_dir(state, &job.project, &job.id);
    let checkout = git::Checkout {
        url: repo.url.clone(),
        dest: local.clone(),
        branch: None,
        depth: None,
        secret: secret.map(|(_, s)| s),
        store: RunDirs::new(&state.state_dir),
        deny_local_under: vec![state.state_dir.clone(), state.work_dir.clone()],
        timeout: Duration::from_secs(STEP_TIMEOUT),
        hooks: git::Hooks::default(),
        origin: git::Origin::default(),
        fetch_from: None,
    };
    let made = tokio::task::spawn_blocking(move || git::JobRepo::create(checkout))
        .await
        .map_err(|e| ApiError::internal(format!("the git task of the job failed: {e}")))?;
    let job_repo = match made {
        Ok(r) => Arc::new(r),
        Err(e) => {
            remove_job_repo(local).await;
            return Err(ApiError::conflict(format!(
                "cannot make the job repo in the daemon: {e}"
            )));
        }
    };
    let holder = Some(format!("{}/jobs", job.project));
    let ionice = has_ionice(state, job, &target, &row.def, holder.clone()).await;
    let project = job.project.clone();
    let wipe = state
        .db
        .read(move |c| jobs::slot_wipe_pending(c, &project, slot))
        .await?;
    let ctx = StepCtx {
        state,
        job,
        def: &row.def,
        dir: &dir,
        repo: job_repo,
        bundle: local.with_extension("bundle"),
        holder,
        ionice,
        recipe_started,
        slot,
        wipe,
    };
    // The slot's tree stays on the target for the next job of the slot
    // (TER-190): no delete of the tree and its build output per job.
    let result = steps(&ctx, recipe, def.timeout_secs()).await;
    drop(ctx);
    remove_job_repo(local).await;
    result
}

/// The work tree of a slot on the target (TER-190):
/// `<work_dir>/<project>/slot-<n>`, kept from job to job. Only one running
/// job holds a slot ([`jobs::pick`]), so no two jobs share a tree.
pub fn slot_dir(def: &DeliveryDef, project: &str, slot: i64) -> String {
    format!(
        "{}/{project}/slot-{slot}",
        def.work_dir().trim_end_matches('/')
    )
}

/// The daemon's repo of a job: `<state>/jobs/<project>/<job>.git`, and its
/// bundle beside it (`.bundle`).
fn job_repo_dir(state: &Shared, project: &str, job: &str) -> PathBuf {
    state
        .state_dir
        .join(JOBS_DIR)
        .join(project)
        .join(format!("{job}.git"))
}

/// Remove the daemon's repo of a job and its bundle.
async fn remove_job_repo(dir: PathBuf) {
    let r = tokio::task::spawn_blocking(move || {
        let _ = std::fs::remove_file(dir.with_extension("bundle"));
        match std::fs::remove_dir_all(&dir) {
            Err(e) if e.kind() != std::io::ErrorKind::NotFound => Err(e),
            _ => Ok(()),
        }
    })
    .await;
    match r {
        Ok(Ok(())) => {}
        Ok(Err(e)) => warn!("cannot remove the job repo in the daemon: {e}"),
        Err(e) => warn!("cannot remove the job repo in the daemon: {e}"),
    }
}

/// The command that the steps of a job run under (TER-97): the idle IO
/// class and the lowest CPU priority, so that a job does not starve the
/// agents and the other work on its host. Without `ionice`, `nice` only.
pub fn low_priority(argv: Vec<String>, ionice: bool) -> Vec<String> {
    let mut out: Vec<String> = if ionice {
        vec!["ionice".into(), "-c3".into()]
    } else {
        Vec::new()
    };
    out.extend(["nice".to_owned(), "-n19".to_owned()]);
    out.extend(argv);
    out
}

/// The targets that have a working `ionice`, by target name: checked once
/// per target in a daemon run.
fn ionice_of() -> &'static Mutex<HashMap<String, bool>> {
    static C: OnceLock<Mutex<HashMap<String, bool>>> = OnceLock::new();
    C.get_or_init(|| Mutex::new(HashMap::new()))
}

/// True when `ionice -c3` works on the target. Checked once per target;
/// a check that cannot run is not kept, and counts as false.
async fn has_ionice(
    state: &Shared,
    job: &Job,
    name: &str,
    def: &terrarium_core::config::TargetDef,
    holder: Option<String>,
) -> bool {
    let known = match ionice_of().lock() {
        Ok(m) => m.get(name).copied(),
        Err(p) => p.into_inner().get(name).copied(),
    };
    if let Some(k) = known {
        return k;
    }
    let probe = RemoteCmd {
        argv: vec![
            "sh".into(),
            "-c".into(),
            "command -v ionice >/dev/null 2>&1 && ionice -c3 true".into(),
        ],
        timeout_secs: Some(60),
        holder,
        ..RemoteCmd::default()
    };
    match step(state, &job.id, def, &probe, STEP_MAX, None).await {
        Ok(c) => {
            let ok = c.end.as_ref().is_some_and(End::success);
            // Keep only an answer: an exit code. An error, a signal, a
            // timeout, or 255 (an ssh client that could not connect,
            // TER-160) is checked again on the next job.
            if matches!(c.end, Some(End::Exit { code: Some(code), .. }) if code != 255) {
                let mut m = match ionice_of().lock() {
                    Ok(m) => m,
                    Err(p) => p.into_inner(),
                };
                m.insert(name.to_owned(), ok);
            }
            ok
        }
        Err(e) => {
            warn!(job = %job.id, target = name, "cannot check for ionice: {e}");
            false
        }
    }
}

/// What the steps of one job share.
struct StepCtx<'a> {
    state: &'a Shared,
    job: &'a Job,
    def: &'a terrarium_core::config::TargetDef,
    dir: &'a str,
    /// The daemon's repo of the job: the only place that gets the repo
    /// credential (TER-103).
    repo: Arc<git::JobRepo>,
    /// Where the daemon writes the bundle for the target.
    bundle: PathBuf,
    holder: Option<String>,
    /// The target has a working `ionice` ([`has_ionice`]).
    ionice: bool,
    /// Set when the recipe step started: a failure after it is a failure,
    /// also when the host is gone then (TER-160).
    recipe_started: &'a AtomicBool,
    /// The job's slot ([`jobs::pick`]).
    slot: i64,
    /// A lost job may still run in the slot's tree: the receive step makes
    /// all of it new, `target/` too ([`jobs::mark_slot_wipe`]).
    wipe: bool,
}

impl StepCtx<'_> {
    /// A git step on the target, without any credential.
    fn plain_cmd(&self, script: &str, args: &[&str]) -> RemoteCmd {
        let mut argv = vec![
            "sh".to_owned(),
            "-c".to_owned(),
            format!("{PLAIN_PRELUDE}{script}"),
            "sh".to_owned(),
        ];
        argv.extend(args.iter().map(|a| (*a).to_owned()));
        RemoteCmd {
            argv,
            timeout_secs: Some(STEP_TIMEOUT),
            holder: self.holder.clone(),
            ..RemoteCmd::default()
        }
    }

    /// Run a step at low IO and CPU priority ([`low_priority`]), with
    /// `input` on its stdin. A job that got a [`kill`] starts no more
    /// steps.
    async fn run(
        &self,
        cmd: &RemoteCmd,
        max: usize,
        input: Option<Vec<u8>>,
    ) -> ApiResult<Collected> {
        if killed(&self.job.id) {
            return Err(ApiError::conflict("the job was cancelled"));
        }
        let cmd = RemoteCmd {
            argv: low_priority(cmd.argv.clone(), self.ionice),
            ..cmd.clone()
        };
        step(self.state, &self.job.id, self.def, &cmd, max, input).await
    }

    /// Run `f` on the daemon's repo of the job, on the blocking pool. A
    /// job that got a [`kill`] starts no more steps.
    async fn daemon<T, F>(&self, f: F) -> ApiResult<T>
    where
        T: Send + 'static,
        F: FnOnce(&git::JobRepo) -> T + Send + 'static,
    {
        if killed(&self.job.id) {
            return Err(ApiError::conflict("the job was cancelled"));
        }
        let repo = self.repo.clone();
        tokio::task::spawn_blocking(move || f(&repo))
            .await
            .map_err(|e| ApiError::internal(format!("the git task of the job failed: {e}")))
    }
}

/// What the daemon's prepare step gave.
enum Prepared {
    /// The commit to test, in the bundle; the batch with its conflicts
    /// marked.
    Ready {
        head: String,
        batch: Option<Vec<BatchCard>>,
        /// The base of the changed files (TER-100), an ancestor of `head`.
        base: Option<String>,
    },
    /// The job ends here.
    End(JobEnd),
}

/// The ref of the job's commit in the bundle and in the target's clone.
fn job_ref(kind: JobKind) -> &'static str {
    if kind == JobKind::Merge {
        "refs/job/merge"
    } else {
        "refs/job/head"
    }
}

/// Step 1, in the daemon, with the repo credential (TER-103): fetch, check
/// out or merge, record the commit as [`job_ref`], and write the bundle.
/// Blocking.
fn prepare(repo: &git::JobRepo, job: &Job, bundle: &std::path::Path) -> Prepared {
    let failed = |what: String| {
        Prepared::End(JobEnd {
            passed: false,
            outcome: Some("error".into()),
            tail: Some(format!("the prepare step failed: {what}")),
            ..JobEnd::default()
        })
    };
    let refname = job_ref(job.kind);
    let mut batch = None;
    // The base of the changed files (TER-100): `next` before the merge for
    // a merge, the merge-base of the commit and `next` for a validate.
    let base;
    let head = if job.kind == JobKind::Merge {
        // The branches of the merge, in order: the job's, then the rest of
        // a batch.
        let mut branches = vec![job.branch.clone()];
        branches.extend(job.cards().iter().skip(1).map(|c| c.branch.clone()));
        let mut specs = vec!["+refs/heads/next:refs/remotes/job/next".to_owned()];
        specs.extend(
            branches
                .iter()
                .enumerate()
                .map(|(i, b)| format!("+refs/heads/{b}:refs/remotes/job/card{i}")),
        );
        if let Err(e) = repo.fetch(&specs) {
            return failed(format!(
                "cannot fetch next and {} (does the repo have a next branch?): {e}",
                branches.join(" ")
            ));
        }
        let mut cur = match repo.commit_of("refs/remotes/job/next") {
            Ok(Some(c)) => c,
            Ok(None) => return failed("the repo has no next branch".into()),
            Err(e) => return failed(e.to_string()),
        };
        base = Some(cur.clone());
        // A branch that conflicts, or whose merge fails otherwise, leaves
        // the merge; the next branch merges onto the ones before it.
        let mut left: HashMap<usize, String> = HashMap::new();
        let mut merged = 0;
        for (i, b) in branches.iter().enumerate() {
            let other = format!("refs/remotes/job/card{i}");
            match repo.merge(&cur, &other, &format!("Merge {b} onto next")) {
                Ok(git::MergeStep::Merged(c)) => {
                    cur = c;
                    merged += 1;
                }
                Ok(git::MergeStep::Conflict(files)) => {
                    left.insert(i, files.join("\n"));
                }
                Ok(git::MergeStep::Failed(msg)) => {
                    left.insert(i, format!("the merge failed without a conflict:\n{msg}"));
                }
                Err(e) => return failed(format!("cannot merge {b}: {e}")),
            }
        }
        if !job.batch.is_empty() {
            batch = Some(mark_conflicts(&job.batch, &left));
        }
        if merged == 0 {
            let what = if job.batch.is_empty() {
                format!("the branch `{}` does", job.branch)
            } else {
                "none of the branches of the batch does".to_owned()
            };
            let mut files: Vec<(usize, String)> = left.into_iter().collect();
            files.sort();
            let files: Vec<String> = files.into_iter().map(|(_, f)| f).collect();
            return Prepared::End(JobEnd {
                passed: false,
                outcome: Some("conflict".into()),
                tail: Some(format!(
                    "{what} not merge onto `next` without conflicts; the queue does not resolve them. Files:\n{}",
                    files.join("\n")
                )),
                batch,
                ..JobEnd::default()
            });
        }
        cur
    } else {
        let spec = format!("+refs/heads/{}:refs/remotes/job/card", job.branch);
        if let Err(e) = repo.fetch(&[spec]) {
            return failed(format!("cannot fetch {}: {e}", job.branch));
        }
        let tip = match repo.commit_of("refs/remotes/job/card") {
            Ok(Some(c)) => c,
            Ok(None) => return failed(format!("cannot fetch {}", job.branch)),
            Err(e) => return failed(e.to_string()),
        };
        let head = match &job.commit {
            None => tip,
            Some(commit) => match repo.commit_of(commit) {
                Ok(Some(c)) if repo.is_ancestor(&c, &tip).unwrap_or(false) => c,
                Ok(_) => return failed(format!("commit {commit} is not on {}", job.branch)),
                Err(e) => return failed(e.to_string()),
            },
        };
        // A repo without `next` is no error: the recipe then gets no list
        // of changed files.
        let next = "+refs/heads/next:refs/remotes/job/next".to_owned();
        base = match repo.fetch(&[next]) {
            Ok(()) => repo
                .merge_base(&head, "refs/remotes/job/next")
                .unwrap_or(None),
            Err(_) => None,
        };
        head
    };
    if let Err(e) = repo
        .set_ref(refname, &head)
        .and_then(|()| repo.bundle(refname, bundle))
    {
        return failed(format!("cannot write the bundle: {e}"));
    }
    Prepared::Ready { head, batch, base }
}

/// Prepare in the daemon, clone on the target from the bundle, run the
/// recipe there, and push `next` from the daemon for a merge.
async fn steps(x: &StepCtx<'_>, recipe: String, timeout: u64) -> ApiResult<JobEnd> {
    let job = x.job;
    if job.kind == JobKind::Merge {
        for c in job.cards().iter().skip(1) {
            check_ref(&c.branch)?;
        }
    }
    let (j, bundle) = (job.clone(), x.bundle.clone());
    let (head, batch, base) = match x.daemon(move |r| prepare(r, &j, &bundle)).await? {
        Prepared::Ready { head, batch, base } => (head, batch, base),
        Prepared::End(end) => return Ok(end),
    };
    check_commit(&head)?;
    // The tree that the slot's work tree must have (TER-190 review,
    // finding 1), from the daemon's repo, not from the slot.
    let h = head.clone();
    let tree = match x.daemon(move |r| r.tree_of(&h)).await? {
        Ok(Some(t)) if check_commit(&t).is_ok() => t,
        Ok(_) => {
            return Err(ApiError::internal(format!(
                "no tree of {head} in the job repo"
            )));
        }
        Err(e) => {
            return Err(ApiError::internal(format!(
                "cannot read the tree of {head}: {e}"
            )));
        }
    };
    // Step 2, on the target and without any credential: a clone from the
    // bundle, at the recorded commit.
    let b = x.bundle.clone();
    let data = tokio::fs::read(&b)
        .await
        .map_err(|e| ApiError::internal(format!("cannot read the job bundle: {e}")))?;
    let _ = tokio::fs::remove_file(&b).await;
    let base = base.filter(|b| check_commit(b).is_ok()).unwrap_or_default();
    let wipe = if x.wipe { "wipe" } else { "" };
    let receive = x.plain_cmd(
        RECEIVE_SCRIPT,
        &[x.dir, &head, job.kind.as_str(), &base, &tree, wipe],
    );
    let c = x.run(&receive, STEP_MAX, Some(data)).await?;
    let received = result_line(&c.stdout).get("outcome").map(String::as_str) == Some("ok");
    if received && x.wipe {
        // The slot is new: the mark of the lost job goes (finding 2).
        let (project, slot) = (job.project.clone(), x.slot);
        if let Err(e) = x
            .state
            .db
            .write("system", move |t| jobs::clear_slot_wipe(t, &project, slot))
            .await
        {
            warn!(job = %job.id, "cannot clear the wipe mark of slot {slot}: {e}");
        }
    }
    if !received {
        return Ok(JobEnd {
            passed: false,
            outcome: Some("error".into()),
            exit_code: exit_code(c.end.as_ref()),
            tail: Some(format!(
                "the clone on the target failed ({}):\n{}",
                end_text(c.end.as_ref()),
                output_of(&c)
            )),
            head: None,
            batch,
        });
    }
    let r = result_line(&c.stdout);
    let base = r
        .get("base")
        .filter(|b| check_commit(b).is_ok())
        .map(String::as_str);
    let recipe_cmd = RemoteCmd {
        argv: vec!["sh".into(), "-c".into(), recipe_script(recipe, base)],
        cwd: Some(x.dir.to_owned()),
        env: recipe_env(job, x.dir, &head, base),
        timeout_secs: Some(timeout),
        holder: x.holder.clone(),
        ..RemoteCmd::default()
    };
    // An error here is a recipe that did not start: no session.
    let c = x.run(&recipe_cmd, RECIPE_MAX, None).await?;
    x.recipe_started.store(true, Ordering::SeqCst);
    let ok = c.end.as_ref().is_some_and(End::success);
    let mut tail = format!(
        "{}\n[{}]\n[run as: {} sh -c <recipe>]",
        output_of(&c).trim_end(),
        end_text(c.end.as_ref()),
        low_priority(Vec::new(), x.ionice).join(" ")
    );
    if let Some(b) = &batch {
        tail.push_str(&format!("\n[{}]", batch_summary(b)));
    }
    let result = JobEnd {
        passed: ok,
        outcome: (!ok).then(|| "red".to_owned()),
        exit_code: exit_code(c.end.as_ref()),
        tail: Some(tail),
        head: Some(head.clone()),
        batch: batch.clone(),
    };
    if !ok || job.kind != JobKind::Merge {
        return Ok(result);
    }
    // Step 3, in the daemon, with the credential: push the recorded merge
    // commit from the daemon's repo. The recipe never had that repo, so
    // nothing it did to its clone reaches the push.
    let h = head.clone();
    let pushed = x
        .daemon(move |r| match r.commit_of("refs/job/merge") {
            Ok(Some(c)) if c == h => r.push(&h, "next"),
            Ok(_) => Err(git::GitError::Refused(format!(
                "the job repo does not hold the merge commit {h}"
            ))),
            Err(e) => Err(e),
        })
        .await?;
    match pushed {
        Ok(()) => Ok(result),
        Err(e) => Ok(JobEnd {
            passed: false,
            outcome: Some("error".into()),
            tail: Some(format!(
                "the recipe passed, but the push to `next` failed; did `next` move?\n{e}"
            )),
            head: Some(head),
            batch,
            ..JobEnd::default()
        }),
    }
}

/// The file of the changed paths in a job clone, relative to its root
/// (written by [`RECEIVE_SCRIPT`]).
pub const CHANGED_FILES: &str = ".git/terrarium/changed-files";

/// The recipe as its shell runs it. Without a base the recipe must not see
/// a `TERRARIUM_BASE` or `TERRARIUM_CHANGED_FILES` from the target's own
/// env (for example a daemon test that runs inside a validate job), so the
/// script unsets both first. With a base, [`recipe_env`] sets both.
fn recipe_script(recipe: String, base: Option<&str>) -> String {
    if base.is_some() {
        return recipe;
    }
    format!("unset TERRARIUM_BASE TERRARIUM_CHANGED_FILES\n{recipe}")
}

/// The env of the recipe (TER-100, docs/api.md 35.2): the job kind and
/// ID, the tested head, the slot (TER-190), and with a base, the base and the absolute path
/// of the changed files. No credential goes in here.
fn recipe_env(job: &Job, dir: &str, head: &str, base: Option<&str>) -> BTreeMap<String, String> {
    let mut env = BTreeMap::new();
    env.insert("TERRARIUM_JOB_KIND".into(), job.kind.as_str().to_owned());
    env.insert("TERRARIUM_JOB_ID".into(), job.id.clone());
    env.insert("TERRARIUM_HEAD".into(), head.to_owned());
    if let Some(s) = job.slot {
        env.insert("TERRARIUM_JOB_SLOT".into(), s.to_string());
    }
    if let Some(b) = base {
        env.insert("TERRARIUM_BASE".into(), b.to_owned());
        env.insert(
            "TERRARIUM_CHANGED_FILES".into(),
            format!("{}/{CHANGED_FILES}", dir.trim_end_matches('/')),
        );
    }
    env
}

/// The batch with each card that left the merge (by its index in the
/// batch) marked `conflict`, with its files or the reason.
fn mark_conflicts(batch: &[BatchCard], found: &HashMap<usize, String>) -> Vec<BatchCard> {
    batch
        .iter()
        .enumerate()
        .map(|(i, c)| match found.get(&i) {
            Some(files) => BatchCard {
                outcome: Some("conflict".into()),
                note: Some(files.clone()),
                ..c.clone()
            },
            None => BatchCard {
                outcome: None,
                note: None,
                ..c.clone()
            },
        })
        .collect()
}

/// One line on the cards of a batch: those that merged, and those that
/// conflicted.
fn batch_summary(batch: &[BatchCard]) -> String {
    let (bad, good): (Vec<&BatchCard>, Vec<&BatchCard>) = batch
        .iter()
        .partition(|c| c.outcome.as_deref() == Some("conflict"));
    let names = |l: &[&BatchCard]| {
        l.iter()
            .map(|c| c.card.as_str())
            .collect::<Vec<_>>()
            .join(" ")
    };
    let mut s = format!("batch: merged {}", names(&good));
    if !bad.is_empty() {
        s.push_str(&format!("; left out on a conflict {}", names(&bad)));
    }
    s
}

/// The head commit of `branch` of `repo` on its remote, read with `git
/// ls-remote` in the daemon (TER-103: the repo credential never goes to a
/// target). `None`: the branch is not on the remote.
pub async fn remote_head(
    state: &Shared,
    project: &str,
    repo_name: &str,
    branch: &str,
) -> ApiResult<Option<String>> {
    check_ref(branch)?;
    let (p, r) = (project.to_owned(), repo_name.to_owned());
    let repo = state
        .db
        .read(move |c| {
            repos::find_by_name(c, &p, &r)?
                .ok_or_else(|| ApiError::not_found(format!("no repo `{r}` in `{p}`")))
        })
        .await?;
    let secret = secrets::secret_of(state, repo.owner(), repo.credential.as_deref()).await?;
    let c = git::Checkout {
        url: repo.url.clone(),
        dest: state.state_dir.join(JOBS_DIR),
        branch: None,
        depth: None,
        secret: secret.map(|(_, s)| s),
        store: RunDirs::new(&state.state_dir),
        deny_local_under: vec![state.state_dir.clone(), state.work_dir.clone()],
        timeout: Duration::from_secs(120),
        hooks: git::Hooks::default(),
        origin: git::Origin::default(),
        fetch_from: None,
    };
    let b = branch.to_owned();
    tokio::task::spawn_blocking(move || git::ls_remote(&c, &b))
        .await
        .map_err(|e| ApiError::internal(format!("the git task failed: {e}")))?
        .map_err(|e| {
            ApiError::conflict(format!(
                "cannot read branch `{branch}` of `{repo_name}` on its remote: {e}"
            ))
        })
}

/// A branch name that git and the scripts take.
pub fn check_ref(b: &str) -> ApiResult<()> {
    if b.is_empty()
        || b.starts_with(['-', '.', '/'])
        || b.ends_with(['/', '.'])
        || b.ends_with(".lock")
        || b.contains("..")
        || b.contains("//")
        || b.contains("@{")
        || b.chars()
            .any(|c| c.is_whitespace() || c.is_control() || "~^:?*[\\".contains(c))
    {
        return Err(ApiError::invalid(format!("bad branch name `{b}`")));
    }
    Ok(())
}

/// A commit ID: 7 to 64 hex digits.
pub fn check_commit(c: &str) -> ApiResult<()> {
    if !(7..=64).contains(&c.len()) || !c.chars().all(|ch| ch.is_ascii_hexdigit()) {
        return Err(ApiError::invalid(format!(
            "bad commit `{c}`: give 7 to 64 hex digits"
        )));
    }
    Ok(())
}

/// The card comment of an end.
fn end_comment(j: &Job, verdict: &str) -> String {
    let mut s = format!(
        "Job queue: {} job {} (attempt {}) {verdict}. Repo {}, branch {}",
        j.kind.as_str(),
        j.id,
        j.attempt,
        j.repo,
        j.branch
    );
    if !j.batch.is_empty() {
        let cards: Vec<&str> = j.batch.iter().map(|c| c.card.as_str()).collect();
        s.push_str(&format!(", batch of {}: {}", cards.len(), cards.join(" ")));
    }
    if let Some(p) = &j.parent {
        s.push_str(&format!(", split from job {p}"));
    }
    if let Some(c) = &j.commit {
        s.push_str(&format!(", commit {c}"));
    }
    if let Some(h) = &j.head {
        s.push_str(&format!(", tested {h}"));
    }
    if let Some(c) = j.exit_code {
        s.push_str(&format!(", exit code {c}"));
    }
    if let Some(o) = &j.outcome {
        s.push_str(&format!(", outcome {o}"));
    }
    s.push_str(&format!(
        ", {} to {}.",
        j.started_at.as_deref().unwrap_or("-"),
        j.ended_at.as_deref().unwrap_or("-")
    ));
    s
}

/// The "Validation failed" block at the top of a card body.
pub(crate) fn failure_block(j: &Job) -> String {
    let tail: String = j
        .tail
        .as_deref()
        .unwrap_or("")
        .lines()
        .rev()
        .take(40)
        .collect::<Vec<_>>()
        .into_iter()
        .rev()
        .map(|l| format!("    {l}\n"))
        .collect();
    format!(
        "Validation failed: {}, {} job {}, repo {}, branch {}, commit {}, tested {}, exit code {}, outcome {}, attempt {}.\nOutput tail:\n\n{tail}\n",
        j.ended_at.as_deref().unwrap_or("-"),
        j.kind.as_str(),
        j.id,
        j.repo,
        j.branch,
        j.commit.as_deref().unwrap_or("-"),
        j.head.as_deref().unwrap_or("-"),
        j.exit_code
            .map_or_else(|| "-".to_owned(), |c| c.to_string()),
        j.outcome.as_deref().unwrap_or("-"),
        j.attempt
    )
}

/// The comment of a queued job on `card` (one of its cards): the same
/// text from the coded flow (`api/jobs.rs`) and from the built-in rule
/// pack (`job.queue_comment`, TER-170).
pub fn queue_comment(j: &Job, card: &str) -> String {
    match j.kind {
        JobKind::Validate => format!(
            "Job queue: validate job {} queued. Repo {}, branch {}, commit {}.",
            j.id,
            j.repo,
            j.branch,
            j.commit
                .as_deref()
                .unwrap_or("the branch head at the start")
        ),
        JobKind::Merge if j.batch.is_empty() => format!(
            "Job queue: merge job {} queued: {} onto next of {}.",
            j.id, j.branch, j.repo
        ),
        JobKind::Merge => {
            let names: Vec<&str> = j.batch.iter().map(|c| c.card.as_str()).collect();
            let i = j.batch.iter().position(|c| c.card == card).unwrap_or(0);
            format!(
                "Job queue: merge job {} queued: {} onto next of {}, card {} of {} in the batch {}.",
                j.id,
                j.batch[i].branch,
                j.repo,
                i + 1,
                j.batch.len(),
                names.join(" ")
            )
        }
    }
}

/// The comment of a failed job that runs again as `next`. `v` is the
/// card's view of the failed job ([`card_view`]); `batch`: the failed job
/// was a batch.
pub fn rerun_comment(v: &Job, next: &Job, batch: bool) -> String {
    if batch {
        format!(
            "{} It runs once more as job {}.",
            end_comment(v, "failed"),
            next.id
        )
    } else {
        format!(
            "{} It runs once more as job {} (flaky tests).",
            end_comment(v, "failed"),
            next.id
        )
    }
}

/// The comment of a red batch that split in `left` and `right`.
pub fn split_comment(v: &Job, left: &Job, right: &Job) -> String {
    let names = |x: &Job| {
        x.cards()
            .iter()
            .map(|c| c.card.clone())
            .collect::<Vec<_>>()
            .join(" ")
    };
    format!(
        "{} The batch splits to find the failing card: job {} ({}) first, then job {} ({}).",
        end_comment(v, "failed"),
        left.id,
        names(left),
        right.id,
        names(right)
    )
}

/// The verdict word of the final failure of a card: `j` is the job, `v`
/// the card's view of it ([`card_view`]).
pub fn fail_phrase(j: &Job, v: &Job) -> &'static str {
    if j.batch.is_empty() {
        // A card split out of a red batch alone was red there already.
        if j.parent.is_some() && j.outcome.as_deref() == Some("red") {
            return "failed (red alone after its batch was red)";
        }
        return "failed again";
    }
    if v.outcome.as_deref() == Some("conflict") {
        return "left the batch on a conflict";
    }
    "failed again"
}

/// The comment of the final failure of a card ([`fail_phrase`]).
pub fn fail_comment(j: &Job, v: &Job) -> String {
    end_comment(v, fail_phrase(j, v))
}

/// The body of the mail to the worker of a card that failed.
pub fn fail_mail(v: &Job) -> String {
    format!(
        "{}\nThe card is at the top of ready, for you first. The failure block is at the top of the card body.\n",
        end_comment(v, "failed")
    )
}

/// What happens after a job ends: the card move, or the rerun.
async fn after_end(state: &Shared, j: &Job) -> ApiResult<()> {
    let j = j.clone();
    state.db.write("system", move |t| on_end(t, &j)).await?;
    kick(state);
    Ok(())
}

/// The end of a job in one write: a `job.verdict` event for each card,
/// then the coded card flow (the move, the rerun, or the split).
fn on_end(t: &mut Tx<'_>, j: &Job) -> ApiResult<()> {
    emit_verdicts(t, j)?;
    // With `board_flow = rules` the board rules act on the verdicts.
    if !code_flow(t, &j.project)? {
        return Ok(());
    }
    if j.batch.is_empty() {
        single_end(t, j)
    } else {
        batch_end(t, j)
    }
}

/// The verdict of a job for one card (TER-167): `passed`, `red`,
/// `conflict`, `cancelled`, or `error`. `j` is the card's view
/// ([`card_view`] for a batch).
fn verdict_of(j: &Job) -> &'static str {
    if j.state == jobs::JobState::Cancelled {
        return "cancelled";
    }
    match j.outcome.as_deref() {
        Some("conflict") => "conflict",
        _ if j.state == jobs::JobState::Passed => "passed",
        Some("red") => "red",
        _ => "error",
    }
}

/// The end comment of the verdict of `v` (the card's view of an ended or
/// cancelled job): `passed`, `was cancelled`, or `failed`. One line: the
/// fields are the daemon's, and a branch name has no space or `[`
/// ([`check_ref`]).
pub fn verdict_comment(v: &Job) -> String {
    let word = match verdict_of(v) {
        "passed" => "passed",
        "cancelled" => "was cancelled",
        _ => "failed",
    };
    end_comment(v, word)
}

/// The failure block of the verdict of `v`, for a failure only. It holds
/// the tail of the job output: untrusted text.
pub fn verdict_failure(v: &Job) -> Option<String> {
    matches!(verdict_of(v), "red" | "conflict" | "error").then(|| failure_block(v))
}

/// The `job.verdict` event data of each card of an ended or cancelled
/// job, in card order (TER-167). It says what the run found for the card,
/// not what the queue does next: the rerun and the split stay in the
/// coded flow until the board rules take them over.
fn verdicts(j: &Job) -> Vec<Value> {
    let views: Vec<Job> = if j.batch.is_empty() {
        vec![j.clone()]
    } else {
        j.batch.iter().map(|c| card_view(j, c)).collect()
    };
    let batch: Option<Vec<&str>> =
        (!j.batch.is_empty()).then(|| j.batch.iter().map(|c| c.card.as_str()).collect());
    views
        .iter()
        .map(|v| {
            let verdict = verdict_of(v);
            serde_json::json!({
                "project": v.project,
                "job": v.id,
                "card": v.card,
                "kind": v.kind,
                "verdict": verdict,
                "state": v.state,
                "outcome": v.outcome,
                "attempt": v.attempt,
                "by_agent": v.by_agent,
                "repo": v.repo,
                "branch": v.branch,
                "parent": v.parent,
                "batch": batch,
                "end_comment": verdict_comment(v),
                "failure_block": verdict_failure(v),
            })
        })
        .collect()
}

/// Emit one `job.verdict` event for each card of an ended or cancelled
/// job (TER-167), in the write of the end or the cancel.
pub fn emit_verdicts(t: &mut Tx<'_>, j: &Job) -> ApiResult<()> {
    for data in verdicts(j) {
        let card = data["card"].as_str().unwrap_or_default().to_owned();
        let summary = format!(
            "{} job {} for {card}: {}",
            j.kind.as_str(),
            j.id,
            data["verdict"].as_str().unwrap_or_default()
        );
        t.emit(
            EventDraft::new("job.verdict", summary, data)
                .project(j.project.clone())
                .card(card),
        )?;
    }
    Ok(())
}

/// The end of a job of one card.
fn single_end(t: &mut Tx<'_>, j: &Job) -> ApiResult<()> {
    if j.state == jobs::JobState::Passed {
        return pass_card(t, j);
    }
    let outcome = j.outcome.as_deref();
    // A card split out of a red batch alone was red there already: a red
    // of its own is the second.
    let split_red = j.parent.is_some() && outcome == Some("red");
    if j.attempt < 2 && outcome != Some("conflict") && !split_red {
        let next = jobs::rerun(t, j, None)?;
        set_labels(t, &j.card, &[LABEL_QUEUED], &[LABEL_RUNNING])?;
        return comment(t, &j.card, &rerun_comment(j, &next, false));
    }
    fail_card(t, j, fail_phrase(j, j))
}

/// The end of a batch merge: the merged cards pass, or the batch splits,
/// or it runs once more. A card that conflicted fails now.
fn batch_end(t: &mut Tx<'_>, j: &Job) -> ApiResult<()> {
    let (conflicted, merged): (Vec<BatchCard>, Vec<BatchCard>) = j
        .batch
        .iter()
        .cloned()
        .partition(|c| c.outcome.as_deref() == Some("conflict"));
    for c in &conflicted {
        let v = card_view(j, c);
        fail_card(t, &v, fail_phrase(j, &v))?;
    }
    if j.state == jobs::JobState::Passed {
        for c in &merged {
            pass_card(t, &card_view(j, c))?;
        }
        return Ok(());
    }
    if merged.is_empty() {
        return Ok(());
    }
    let red = j.outcome.as_deref() == Some("red");
    if red && merged.len() >= 2 {
        let (left, right) = jobs::split(t, j, &merged)?;
        for c in &merged {
            let v = card_view(j, c);
            set_labels(t, &c.card, &[LABEL_QUEUED], &[LABEL_RUNNING])?;
            comment(t, &c.card, &split_comment(&v, &left, &right))?;
        }
        return Ok(());
    }
    // One card merged and the batch is red, or the job could not run.
    let split_red = red && j.parent.is_some();
    if j.attempt < 2 && !split_red {
        let next = jobs::rerun(t, j, Some(&merged))?;
        for c in &merged {
            set_labels(t, &c.card, &[LABEL_QUEUED], &[LABEL_RUNNING])?;
            comment(t, &c.card, &rerun_comment(&card_view(j, c), &next, true))?;
        }
        return Ok(());
    }
    for c in &merged {
        let v = card_view(j, c);
        fail_card(t, &v, fail_phrase(j, &v))?;
    }
    Ok(())
}

/// The job as one card of its batch sees it: the card, its branch, and
/// its worker. A card that conflicted gets the conflict as its outcome.
pub fn card_view(j: &Job, c: &BatchCard) -> Job {
    let mut v = j.clone();
    v.card = c.card.clone();
    v.branch = c.branch.clone();
    v.by_agent = c.by_agent.clone();
    if c.outcome.as_deref() == Some("conflict") {
        v.outcome = Some("conflict".into());
        v.exit_code = None;
        v.head = None;
        v.tail = Some(format!(
            "the branch `{}` does not merge onto `next` and the cards before it in the batch; the queue does not resolve conflicts. Files, or why the merge failed:\n{}",
            c.branch,
            c.note.as_deref().unwrap_or("")
        ));
    }
    v
}

/// A pass: clear the job labels, move the card on, and free it.
fn pass_card(t: &mut Tx<'_>, j: &Job) -> ApiResult<()> {
    set_labels(t, &j.card, &[], &[LABEL_RUNNING, LABEL_QUEUED])?;
    let to = match j.kind {
        JobKind::Validate => board::role_name(t, &j.project, board::REVIEW, COL_REVIEW)?,
        JobKind::Merge => board::role_name(t, &j.project, board::ACCEPTED, COL_ACCEPTED)?,
    };
    let note = end_comment(j, "passed");
    move_or_comment(t, &j.card, &to, None, &note)?;
    unassign(t, &j.card)
}

/// The final failure of a card: the failure block, the top of `ready`,
/// no assignee, and a mail to its worker.
fn fail_card(t: &mut Tx<'_>, j: &Job, verdict: &str) -> ApiResult<()> {
    set_labels(t, &j.card, &[], &[LABEL_RUNNING, LABEL_QUEUED])?;
    let card = match board::get_card(t, &j.card) {
        Ok(c) => c,
        Err(e) if e.code.status().is_client_error() => return Ok(()),
        Err(e) => return Err(e),
    };
    let body = format!("{}{}", failure_block(j), card.body);
    let patch = CardPatch {
        body: Some(body),
        ..CardPatch::default()
    };
    board::update_card(t, &Actor::Operator, &j.card, patch)?;
    let note = end_comment(j, verdict);
    move_or_comment(t, &j.card, COL_READY, Some(0), &note)?;
    unassign(t, &j.card)?;
    if let Some(who) = &j.by_agent {
        notify_worker(t, j, who)?;
    }
    Ok(())
}

/// Clear the card's assignee: the worker stays assigned while the card
/// validates (it can edit only its own cards), and the final move frees it.
fn unassign(t: &mut Tx<'_>, card: &str) -> ApiResult<()> {
    match board::assign_card(t, &Actor::Operator, card, None) {
        Ok(_) => Ok(()),
        Err(e) if e.code.status().is_client_error() => Ok(()),
        Err(e) => Err(e),
    }
}

fn comment(t: &mut Tx<'_>, card: &str, text: &str) -> ApiResult<()> {
    match board::add_comment(t, &Actor::Operator, card, text) {
        Ok(_) => Ok(()),
        Err(e) if e.code.status().is_client_error() => Ok(()),
        Err(e) => Err(e),
    }
}

/// Move the card with the comment. When the move is refused (the card is
/// gone, or the board has no such column), the comment says so.
fn move_or_comment(
    t: &mut Tx<'_>,
    card: &str,
    to: &str,
    position: Option<i64>,
    note: &str,
) -> ApiResult<()> {
    match board::move_card(t, &Actor::Operator, card, to, position, Some(note)) {
        Ok(_) => Ok(()),
        Err(e) if e.code.status().is_client_error() => comment(
            t,
            card,
            &format!("{note} The move to `{to}` failed: {}", e.message),
        ),
        Err(e) => Err(e),
    }
}

fn notify_worker(t: &mut Tx<'_>, j: &Job, who: &str) -> ApiResult<()> {
    let to = match mail::resolve(t, who, Some(&j.project)) {
        Ok(a @ Address::Agent { .. }) => a,
        Ok(_) | Err(_) => {
            debug!(job = %j.id, who, "no agent to tell about the failed job");
            return Ok(());
        }
    };
    let subject = format!("{}: validation failed", j.card);
    let body = fail_mail(j);
    mail::send(
        t,
        Outgoing {
            from: &Address::System,
            to: &to,
            subject: &subject,
            body: &body,
            thread: None,
            in_reply_to: None,
        },
    )?;
    Ok(())
}

/// The `by:<agent>` label of a card.
pub fn by_agent_of(labels: &[String]) -> Option<String> {
    labels
        .iter()
        .find_map(|l| l.strip_prefix("by:"))
        .map(str::to_owned)
}

/// A JSON view of the queue of a project, for the API: the config and the
/// jobs.
pub fn view(def: &DeliveryDef, list: &[Job]) -> Value {
    serde_json::json!({
        "delivery": def,
        "enabled": def.enabled(),
        "slots": def.slots(),
        "running": list.iter().filter(|j| j.state == jobs::JobState::Running).count(),
        "queued": list.iter().filter(|j| j.state == jobs::JobState::Queued).count(),
        "jobs": list,
        "at": now(),
    })
}

/// The git setup of the steps on the target. They hold no credential
/// (TER-103): no user or system config, no hooks, no prompts, and only
/// the file transport (the bundle). No replace refs, commit-graph or
/// multi-pack-index either (TER-190 review, finding 1).
const PLAIN_PRELUDE: &str = r#"set -u
unset GIT_DIR GIT_WORK_TREE GIT_INDEX_FILE GIT_OBJECT_DIRECTORY GIT_ALTERNATE_OBJECT_DIRECTORIES \
  GIT_CONFIG GIT_CONFIG_PARAMETERS GIT_CONFIG_COUNT GIT_NAMESPACE GIT_CREDENTIAL_HELPER \
  GIT_TEMPLATE_DIR GIT_EXEC_PATH GIT_EXTERNAL_DIFF GIT_PAGER GIT_EDITOR GIT_PROXY_COMMAND \
  GIT_SSH GIT_SSH_COMMAND GIT_SSH_VARIANT GIT_CEILING_DIRECTORIES GIT_DISCOVERY_ACROSS_FILESYSTEM \
  GIT_COMMON_DIR GIT_REPLACE_REF_BASE GIT_GRAFT_FILE GIT_SHALLOW_FILE GIT_ATTR_SOURCE
export GIT_CONFIG_GLOBAL=/dev/null GIT_CONFIG_NOSYSTEM=1 GIT_TERMINAL_PROMPT=0
export GIT_ASKPASS=false SSH_ASKPASS=false LC_ALL=C GIT_NO_REPLACE_OBJECTS=1
G() {
  git -c core.hooksPath=/dev/null -c core.fsmonitor=false -c core.untrackedCache=false \
    -c core.useReplaceRefs=false -c core.commitGraph=false -c core.multiPackIndex=false \
    -c credential.helper= -c protocol.allow=never -c protocol.file.allow=always \
    -c fetch.recurseSubmodules=false -c submodule.recurse=false -c gc.auto=0 \
    -c maintenance.auto=false -c advice.detachedHead=false -c core.pager=cat \
    -c user.name=terrarium -c user.email=terrarium@localhost -c commit.gpgsign=false "$@"
}
res() { printf 'TERRARIUM_JOB\toutcome=%s\thead=%s\n' "$1" "${2:-}"; }
"#;

/// Step 2 on the target, without any credential: `dir sha kind base tree
/// wipe`, the bundle of the daemon on stdin. `dir` is the work tree of the
/// job's slot (TER-190), kept from job to job; `tree` is the tree of `sha`
/// in the daemon's repo. The script writes the bundle to `<dir>.bundle`,
/// fetches the job ref from it into the tree, checks that the ref is the
/// commit `sha`, checks it out with `-f` (`next` for a merge, detached for
/// a validate), and removes every file that the commit does not track,
/// ignored ones too, except `target/` at the root, so builds reuse it. A
/// dir that is no repo yet, a `.git` that is no plain dir or holds a
/// symlink, or a tree that git cannot fetch into or check out, is made new
/// once: all but `target/` goes, and `git init` runs.
///
/// The git state that a recipe can leave does not reach the next job
/// (TER-190 review, finding 1): `.git/config`, `info` (with `grafts`),
/// `hooks`, `commondir`, `shallow`, the alternates, the commit-graph, the
/// multi-pack-index and `refs/replace` are removed each time, and git runs
/// with `GIT_NO_REPLACE_OBJECTS=1` ([`PLAIN_PRELUDE`]). Then the work tree
/// is hashed again (a new index from `tree`, `git add -A` but `target/`,
/// `git write-tree`) and must be `tree`, as `HEAD^{tree}` must. Else all
/// of the slot goes (`target/` too) and it runs once more in a new tree;
/// a second miss refuses the job and wipes the slot.
///
/// `wipe` (TER-190 review, finding 2): a lost job may still run in the
/// slot, so all of it goes first, `target/` too. A wipe moves the dir
/// aside before it removes it, so such processes write into the old dir.
///
/// Old commits are pruned by a `git gc --auto` that keeps few packs. With
/// a base (an ancestor of `sha`, so in the bundle), it writes the changed
/// files to [`CHANGED_FILES`] (TER-100); else that file is gone. Prints
/// `outcome=ok head=<sha> base=<sha or empty>` or `outcome=error`.
const RECEIVE_SCRIPT: &str = r#"dir=$1; sha=$2; kind=$3; base=${4:-}; tree=${5:-}; wipe_first=${6:-}
case "$dir" in /*/*) ;; *) echo "the job dir is not absolute"; res error; exit 2 ;; esac
case "$sha" in *[!0-9a-f]*|'') echo "bad commit"; res error; exit 2 ;; esac
case "$base" in *[!0-9a-f]*) echo "bad base"; res error; exit 2 ;; esac
case "$tree" in *[!0-9a-f]*|'') echo "bad tree"; res error; exit 2 ;; esac
b=$dir.bundle
if [ "$kind" = merge ]; then r=refs/job/merge; else r=refs/job/head; fi
# Git only ever sees this tree: never a repo above it.
T() { G --git-dir="$dir/.git" --work-tree="$dir" -C "$dir" "$@"; }
# All of the slot goes, target/ too. It moves aside first: processes that
# may still run in it write into the old dir, not into the new one.
wipe() {
  rm -rf -- "$dir".wiped.* 2>/dev/null
  if [ -e "$dir" ] || [ -L "$dir" ]; then
    mv -- "$dir" "$dir.wiped.$$" || return 1
    rm -rf -- "$dir.wiped.$$" 2>/dev/null \
      || echo "cannot remove all of $dir.wiped.$$ now; the next wipe tries again"
  fi
  mkdir -p -- "$dir"
}
# A link in place of the slot is not followed.
if [ -L "$dir" ]; then
  rm -f -- "$dir" || { echo "cannot remove the link $dir"; res error; exit 2; }
fi
if [ "$wipe_first" = wipe ]; then
  echo "a lost job may still run in $dir; making the slot new, target/ too"
  wipe || { echo "cannot wipe $dir"; res error; exit 2; }
fi
mkdir -p -- "$dir" && rm -f -- "$b" || { echo "cannot make $dir"; res error; exit 2; }
if ! cat > "$b"; then rm -f -- "$b"; echo "cannot write the bundle"; res error; exit 2; fi
# All of the tree goes but target/, and git makes a new repo.
fresh() {
  find "$dir" -mindepth 1 -maxdepth 1 ! -name target -exec rm -rf -- {} + && T init -q
}
# A .git that git can take as it is: a plain dir with no link in it (no
# gitfile, no objects or refs of another place).
sane() {
  [ -d "$dir/.git" ] && [ ! -L "$dir/.git" ] \
    && [ -z "$(find "$dir/.git" -type l -print 2>/dev/null | head -n 1)" ]
}
# Fetch the job ref, check it out with -f, and clean all but target/.
# 3: the bundle holds another commit (no retry); 1: the tree failed.
sync() {
  rm -f -- "$dir/.git/index.lock" "$dir/.git/HEAD.lock" "$dir/.git/config" \
    "$dir/.git/commondir" "$dir/.git/shallow" "$dir/.git/gitdir" \
    "$dir/.git/objects/info/alternates" "$dir/.git/objects/info/http-alternates" \
    "$dir/.git/objects/info/commit-graph" "$dir/.git/objects/pack/multi-pack-index" \
    "$dir/.git/terrarium/changed-files" "$dir/.git/terrarium/verify-index"
  rm -rf -- "$dir/.git/info" "$dir/.git/hooks" "$dir/.git/refs/replace" \
    "$dir/.git/objects/info/commit-graphs" "$dir/.git/worktrees"
  T init -q && T config core.logAllRefUpdates false || return 1
  T for-each-ref --format='delete %(refname)' refs/replace/ | T update-ref --stdin || return 1
  T fetch -q --no-tags -- "$b" "+$r:$r" || return 1
  got=$(T rev-parse --verify -q "$r^{commit}")
  if [ "$got" != "$sha" ]; then echo "the bundle holds ${got:-nothing}, not $sha"; return 3; fi
  if [ "$kind" = merge ]; then
    T checkout -q -f -B next "$sha" || return 1
  else
    T checkout -q -f --detach "$sha" || return 1
  fi
  T clean -q -ffdx -e /target || return 1
}
# The work tree is the daemon's tree: each file is hashed again into a new
# index (but target/), so no replace ref, graft or forged object passes.
verify() {
  vi=$dir/.git/terrarium/verify-index
  mkdir -p -- "$dir/.git/terrarium" && rm -f -- "$vi" || return 1
  # target/ is left out as an ignored path (a pathspec that names an
  # ignored path fails `git add`).
  printf '/target/\n' > "$vi.exclude" || return 1
  GIT_INDEX_FILE=$vi; export GIT_INDEX_FILE
  w=
  if T read-tree "$tree" >/dev/null 2>&1 \
    && T -c core.excludesFile="$vi.exclude" add -A -- . >/dev/null 2>&1; then
    w=$(T write-tree 2>/dev/null)
  fi
  unset GIT_INDEX_FILE
  rm -f -- "$vi" "$vi.exclude"
  [ "$w" = "$tree" ] && [ "$(T rev-parse -q --verify 'HEAD^{tree}' 2>/dev/null)" = "$tree" ]
}
made=0
if ! sane || ! T rev-parse -q --verify 'HEAD^{commit}' >/dev/null 2>&1; then
  fresh || { rm -f -- "$b"; echo "cannot make the repo in $dir"; res error; exit 2; }
  made=1
fi
sync; rc=$?
if [ "$rc" = 1 ] && [ "$made" = 0 ]; then
  echo "the tree in $dir failed; making it new"
  if fresh; then sync; rc=$?; fi
fi
if [ "$rc" = 0 ] && ! verify; then
  echo "the checkout in $dir is not the tree $tree of $sha (git state that a recipe left?); making the slot new, target/ too"
  rc=4
  if wipe && fresh; then sync; rc=$?; fi
  if [ "$rc" = 0 ] && ! verify; then rc=4; fi
fi
rm -f -- "$b"
case $rc in
  0) ;;
  3) res error; exit 2 ;;
  4) echo "refused: the checkout of $sha is not its tree $tree, also in a new slot tree; the slot is wiped"
     wipe >/dev/null 2>&1; res error; exit 2 ;;
  *) echo "cannot read the bundle or check out $sha"; res error; exit 2 ;;
esac
T -c gc.auto=1 -c gc.autoPackLimit=4 -c gc.pruneExpire=now -c gc.autoDetach=false gc --auto -q >/dev/null 2>&1 || true
# The changed files for the recipe (TER-100): the paths that differ
# between the base and HEAD, one per line, in .git/terrarium/changed-files.
# The daemon gives the base: `next` before the merge for a merge, the
# merge-base of the commit and `next` for a validate. No base, or one that
# is not in the tree: no file, and `base=`.
if [ -n "$base" ] && ! T merge-base --is-ancestor "$base" HEAD 2>/dev/null; then
  base=
fi
if [ -n "$base" ]; then
  mkdir -p -- "$dir/.git/terrarium" \
    && T -c core.quotePath=false diff --no-ext-diff --no-textconv --name-only --no-renames "$base" HEAD \
      > "$dir/.git/terrarium/changed-files" \
    || { rm -f -- "$dir/.git/terrarium/changed-files"; base=; }
fi
printf 'TERRARIUM_JOB\toutcome=ok\thead=%s\tbase=%s\n' "$sha" "$base"
"#;

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;

    /// TER-100 review round 2: without a base, a list from the outer env
    /// does not reach the recipe. With a base, the recipe runs as given.
    #[test]
    fn a_recipe_without_a_base_sees_no_inherited_list() {
        let script = recipe_script(
            "echo \"${TERRARIUM_BASE-unset} ${TERRARIUM_CHANGED_FILES-unset}\"".into(),
            None,
        );
        let out = std::process::Command::new("sh")
            .args(["-c", &script])
            .env("TERRARIUM_BASE", "abc")
            .env("TERRARIUM_CHANGED_FILES", "/tmp/outer")
            .output()
            .unwrap();
        assert_eq!(String::from_utf8_lossy(&out.stdout), "unset unset\n");
        assert_eq!(recipe_script("just x".into(), Some("abc")), "just x");
    }

    /// Run `script` (after [`PLAIN_PRELUDE`]) with `args` and `input` on
    /// stdin, with an empty env but `PATH`. Returns the exit status and
    /// stdout.
    fn run_plain(script: &str, args: &[&str], input: &[u8]) -> (bool, String) {
        use std::io::Write;
        let mut child = std::process::Command::new("sh")
            .arg("-c")
            .arg(format!("{PLAIN_PRELUDE}{script}"))
            .arg("sh")
            .args(args)
            .env_clear()
            .env("PATH", std::env::var("PATH").unwrap())
            .stdin(std::process::Stdio::piped())
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::piped())
            .spawn()
            .unwrap();
        // A script that exits before it reads stdin (a refused dir) closes
        // the pipe: the write then fails with EPIPE, and the exit status
        // decides (TER-188). Write from a thread, so a large input cannot
        // block on a full pipe while the child blocks on a full stdout.
        let mut stdin = child.stdin.take().unwrap();
        let input = input.to_vec();
        let writer = std::thread::spawn(move || match stdin.write_all(&input) {
            Err(e) if e.kind() == std::io::ErrorKind::BrokenPipe => Ok(()),
            r => r,
        });
        let out = child.wait_with_output().unwrap();
        writer.join().unwrap().unwrap();
        let text = format!(
            "{}{}",
            String::from_utf8_lossy(&out.stdout),
            String::from_utf8_lossy(&out.stderr)
        );
        (out.status.success(), text)
    }

    fn git_in(dir: &std::path::Path, args: &[&str]) -> String {
        let out = std::process::Command::new("git")
            .args(args)
            .current_dir(dir)
            .env("GIT_CONFIG_GLOBAL", "/dev/null")
            .env("GIT_AUTHOR_NAME", "t")
            .env("GIT_AUTHOR_EMAIL", "t@t")
            .env("GIT_COMMITTER_NAME", "t")
            .env("GIT_COMMITTER_EMAIL", "t@t")
            .output()
            .unwrap();
        assert!(
            out.status.success(),
            "git {args:?}: {}",
            String::from_utf8_lossy(&out.stderr)
        );
        String::from_utf8_lossy(&out.stdout).trim().to_owned()
    }

    /// TER-103: the target gets the job's commit as a bundle on stdin, and
    /// its clone is at that commit, with no remote and no bundle left. A
    /// bundle of another commit, or a cut one, fails the step.
    #[test]
    fn the_target_clones_the_job_commit_from_the_bundle() {
        let tmp = tempfile::tempdir().unwrap();
        let src = tmp.path().join("src");
        std::fs::create_dir_all(&src).unwrap();
        git_in(&src, &["init", "-q", "-b", "main"]);
        std::fs::write(src.join("README"), "one\n").unwrap();
        git_in(&src, &["add", "."]);
        git_in(&src, &["commit", "-q", "-m", "one"]);
        let sha = git_in(&src, &["rev-parse", "HEAD"]);
        let tree = git_in(&src, &["rev-parse", "HEAD^{tree}"]);
        git_in(&src, &["update-ref", "refs/job/merge", &sha]);
        git_in(&src, &["bundle", "create", "-q", "../b", "refs/job/merge"]);
        let bundle = std::fs::read(tmp.path().join("b")).unwrap();
        let dir = tmp.path().join("jobs/p/J1");
        let d = dir.display().to_string();

        let (ok, text) = run_plain(RECEIVE_SCRIPT, &[&d, &sha, "merge", "", &tree], &bundle);
        assert!(ok, "{text}");
        assert_eq!(result_line(text.as_bytes())["head"], sha, "{text}");
        assert_eq!(git_in(&dir, &["rev-parse", "HEAD"]), sha);
        assert_eq!(git_in(&dir, &["symbolic-ref", "--short", "HEAD"]), "next");
        assert_eq!(git_in(&dir, &["remote"]), "", "the clone has no remote");
        assert!(!tmp.path().join("jobs/p/J1.bundle").exists());

        // The bundle holds another commit than the one the daemon named.
        let other = "0123456789abcdef0123456789abcdef01234567";
        let (ok, text) = run_plain(RECEIVE_SCRIPT, &[&d, other, "merge", "", &tree], &bundle);
        assert!(!ok, "{text}");
        assert!(text.contains("not 0123456789abcdef"), "{text}");
        // A cut bundle, in a new slot's tree. (A tree that has the commit
        // already does not need the bundle's objects.)
        let fresh = tmp.path().join("jobs/p/J2").display().to_string();
        let (ok, text) = run_plain(
            RECEIVE_SCRIPT,
            &[&fresh, &sha, "merge", "", &tree],
            &bundle[..bundle.len() / 2],
        );
        assert!(!ok, "{text}");
        assert!(text.contains("cannot read the bundle"), "{text}");
        // A validate job wants refs/job/head.
        let (ok, text) = run_plain(RECEIVE_SCRIPT, &[&d, &sha, "validate", "", &tree], &bundle);
        assert!(!ok, "{text}");
        // A relative dir is refused, and so is a step without a tree.
        let (ok, _) = run_plain(RECEIVE_SCRIPT, &["rel", &sha, "merge", "", &tree], &bundle);
        assert!(!ok);
        let (ok, text) = run_plain(RECEIVE_SCRIPT, &[&d, &sha, "merge"], &bundle);
        assert!(!ok && text.contains("bad tree"), "{text}");
    }

    /// TER-190: the next job of a slot reuses its tree. The tree is then at
    /// the new commit, the files that commit does not track are gone
    /// (ignored ones and nested `target/` dirs too) but `target/` at the
    /// root, a config or a list of changed files that the last job left is
    /// gone, and a tree that git cannot use is made new, `target/` kept.
    #[test]
    fn a_slot_tree_is_reused_and_cleaned_but_target() {
        let tmp = tempfile::tempdir().unwrap();
        let src = tmp.path().join("src");
        std::fs::create_dir_all(&src).unwrap();
        git_in(&src, &["init", "-q", "-b", "main"]);
        std::fs::write(src.join("README"), "one\n").unwrap();
        std::fs::write(src.join(".gitignore"), "target/\n*.o\n").unwrap();
        git_in(&src, &["add", "."]);
        git_in(&src, &["commit", "-q", "-m", "one"]);
        let one = git_in(&src, &["rev-parse", "HEAD"]);
        let bundle_of = |sha: &str| {
            git_in(&src, &["update-ref", "refs/job/head", sha]);
            git_in(&src, &["bundle", "create", "-q", "../b", "refs/job/head"]);
            std::fs::read(tmp.path().join("b")).unwrap()
        };
        let tree_of = |sha: &str| git_in(&src, &["rev-parse", &format!("{sha}^{{tree}}")]);
        let dir = tmp.path().join("jobs/p/slot-1");
        let d = dir.display().to_string();
        let (ok, text) = run_plain(
            RECEIVE_SCRIPT,
            &[&d, &one, "validate", &one, &tree_of(&one)],
            &bundle_of(&one),
        );
        assert!(ok, "{text}");
        assert!(dir.join(CHANGED_FILES).exists());

        // What the first job left.
        std::fs::create_dir_all(dir.join("target/debug")).unwrap();
        std::fs::write(dir.join("target/debug/kept"), "built").unwrap();
        std::fs::write(dir.join("junk"), "x").unwrap();
        std::fs::write(dir.join("main.o"), "x").unwrap();
        std::fs::write(dir.join("README"), "changed\n").unwrap();
        std::fs::create_dir_all(dir.join("sub/target")).unwrap();
        git_in(&dir, &["config", "alias.st", "status"]);

        std::fs::write(src.join("README"), "two\n").unwrap();
        std::fs::write(src.join("NEW"), "new\n").unwrap();
        git_in(&src, &["add", "."]);
        git_in(&src, &["commit", "-q", "-m", "two"]);
        let two = git_in(&src, &["rev-parse", "HEAD"]);
        let t2 = tree_of(&two);
        let b2 = bundle_of(&two);
        let (ok, text) = run_plain(RECEIVE_SCRIPT, &[&d, &two, "validate", "", &t2], &b2);
        assert!(ok, "{text}");
        assert!(!text.contains("making the slot new"), "{text}");
        assert_eq!(git_in(&dir, &["rev-parse", "HEAD"]), two);
        assert_eq!(
            git_in(&dir, &["status", "--porcelain", "--ignored"]),
            "!! target/"
        );
        assert_eq!(
            std::fs::read_to_string(dir.join("README")).unwrap(),
            "two\n"
        );
        assert_eq!(
            std::fs::read_to_string(dir.join("target/debug/kept")).unwrap(),
            "built"
        );
        assert!(!dir.join("junk").exists() && !dir.join("main.o").exists());
        assert!(!dir.join("sub").exists());
        assert!(!dir.join(CHANGED_FILES).exists(), "a stale list is left");
        assert!(!git_in(&dir, &["config", "--list", "--local"]).contains("alias"));
        assert!(!tmp.path().join("jobs/p/slot-1.bundle").exists());

        // A broken tree is made new; target/ stays.
        std::fs::remove_dir_all(dir.join(".git/objects")).unwrap();
        std::fs::write(dir.join("junk"), "x").unwrap();
        let (ok, text) = run_plain(RECEIVE_SCRIPT, &[&d, &two, "validate", "", &t2], &b2);
        assert!(ok, "{text}");
        assert_eq!(git_in(&dir, &["rev-parse", "HEAD"]), two);
        assert!(dir.join("target/debug/kept").exists());
        assert!(!dir.join("junk").exists());
    }

    /// A source repo with `LIB` and `README`, and the bundle of a commit
    /// as the daemon makes it for a validate job.
    struct Src {
        tmp: tempfile::TempDir,
        src: std::path::PathBuf,
    }

    impl Src {
        fn new() -> Src {
            let tmp = tempfile::tempdir().unwrap();
            let src = tmp.path().join("src");
            std::fs::create_dir_all(&src).unwrap();
            git_in(&src, &["init", "-q", "-b", "main"]);
            std::fs::write(src.join("LIB"), "lib\n").unwrap();
            Src { tmp, src }
        }

        /// Commit `README` with `text`: the commit and its tree.
        fn commit(&self, text: &str) -> (String, String) {
            std::fs::write(self.src.join("README"), text).unwrap();
            git_in(&self.src, &["add", "."]);
            git_in(&self.src, &["commit", "-q", "-m", text]);
            (
                git_in(&self.src, &["rev-parse", "HEAD"]),
                git_in(&self.src, &["rev-parse", "HEAD^{tree}"]),
            )
        }

        fn bundle(&self, sha: &str) -> Vec<u8> {
            git_in(&self.src, &["update-ref", "refs/job/head", sha]);
            git_in(
                &self.src,
                &["bundle", "create", "-q", "../b", "refs/job/head"],
            );
            std::fs::read(self.tmp.path().join("b")).unwrap()
        }

        fn slot(&self) -> std::path::PathBuf {
            self.tmp.path().join("jobs/p/slot-1")
        }

        /// The receive step of a validate job of `sha` with `tree`.
        fn receive(&self, sha: &str, tree: &str, wipe: &str) -> (bool, String) {
            let d = self.slot().display().to_string();
            run_plain(
                RECEIVE_SCRIPT,
                &[&d, sha, "validate", "", tree, wipe],
                &self.bundle(sha),
            )
        }
    }

    /// TER-190 review, finding 1: a recipe plants git state in the slot's
    /// tree: replace refs (one packed, one loose) for the blobs of `LIB`,
    /// which the next commit keeps, and of the next `README`; a
    /// `commondir` to a repo whose config has a smudge filter for every
    /// file; `shallow`, `info/grafts` and alternates. The next job still
    /// gets the real tree of its commit, no filter runs, and none of that
    /// state is left.
    #[test]
    fn git_state_that_a_recipe_plants_does_not_reach_the_next_job() {
        let s = Src::new();
        let (one, t1) = s.commit("one\n");
        let (ok, text) = s.receive(&one, &t1, "");
        assert!(ok, "{text}");
        let (two, t2) = s.commit("two\n");
        let dir = s.slot();

        // What the recipe of the first job does.
        std::fs::write(dir.join("evil"), "evil\n").unwrap();
        let evil = git_in(&dir, &["hash-object", "-w", "evil"]);
        let lib = git_in(&dir, &["rev-parse", "HEAD:LIB"]);
        git_in(&dir, &["replace", &lib, &evil]);
        assert_eq!(git_in(&dir, &["cat-file", "-p", &lib]), "evil");
        git_in(&dir, &["pack-refs", "--all"]);
        let readme2 = git_in(&s.src, &["rev-parse", "HEAD:README"]);
        git_in(
            &dir,
            &["update-ref", &format!("refs/replace/{readme2}"), &evil],
        );
        let common = s.tmp.path().join("common.git");
        git_in(
            s.tmp.path(),
            &[
                "clone",
                "-q",
                "--bare",
                &dir.display().to_string(),
                "common.git",
            ],
        );
        let pwned = s.tmp.path().join("pwned");
        git_in(
            &common,
            &[
                "config",
                "filter.x.smudge",
                &format!("touch {}; cat", pwned.display()),
            ],
        );
        std::fs::create_dir_all(common.join("info")).unwrap();
        std::fs::write(common.join("info/attributes"), "* filter=x\n").unwrap();
        std::fs::write(
            dir.join(".git/commondir"),
            format!("{}\n", common.display()),
        )
        .unwrap();
        std::fs::write(dir.join(".git/shallow"), format!("{one}\n")).unwrap();
        std::fs::create_dir_all(dir.join(".git/info")).unwrap();
        std::fs::write(dir.join(".git/info/grafts"), format!("{one}\n")).unwrap();
        std::fs::write(
            dir.join(".git/objects/info/alternates"),
            format!("{}/objects\n", common.display()),
        )
        .unwrap();

        let (ok, text) = s.receive(&two, &t2, "");
        assert!(ok, "{text}");
        assert_eq!(git_in(&dir, &["rev-parse", "HEAD"]), two);
        assert_eq!(std::fs::read_to_string(dir.join("LIB")).unwrap(), "lib\n");
        assert_eq!(
            std::fs::read_to_string(dir.join("README")).unwrap(),
            "two\n"
        );
        assert!(!pwned.exists(), "a filter of the recipe ran: {text}");
        assert_eq!(git_in(&dir, &["for-each-ref", "refs/replace/"]), "");
        for f in [
            "commondir",
            "shallow",
            "info/grafts",
            "objects/info/alternates",
        ] {
            assert!(!dir.join(".git").join(f).exists(), ".git/{f} is left");
        }
        assert_eq!(git_in(&dir, &["status", "--porcelain", "--ignored"]), "");
    }

    /// TER-190 review, finding 1: a work tree that is not the tree that
    /// the daemon named is never used. The slot is made new once (all of
    /// it, `target/` too); a second miss refuses the job and wipes the
    /// slot. The next job with the right tree works again.
    #[test]
    fn a_checkout_that_is_not_the_named_tree_is_refused_and_the_slot_wiped() {
        let s = Src::new();
        let (one, t1) = s.commit("one\n");
        let (ok, text) = s.receive(&one, &t1, "");
        assert!(ok, "{text}");
        let dir = s.slot();
        std::fs::create_dir_all(dir.join("target/debug")).unwrap();
        std::fs::write(dir.join("target/debug/kept"), "built").unwrap();

        // The empty tree: never the tree of `one`.
        let empty = "4b825dc642cb6eb9a060e54bf8d69288fbee4904";
        let (ok, text) = s.receive(&one, empty, "");
        assert!(!ok, "{text}");
        assert!(text.contains("making the slot new"), "{text}");
        assert!(text.contains("refused"), "{text}");
        assert_eq!(result_line(text.as_bytes())["outcome"], "error");
        assert!(!dir.join("target").exists(), "the slot is not wiped");
        assert!(!dir.join(".git").exists(), "the slot is not wiped");

        let (ok, text) = s.receive(&one, &t1, "");
        assert!(ok, "{text}");
        assert_eq!(git_in(&dir, &["rev-parse", "HEAD"]), one);
    }

    /// TER-190 review, finding 2: with `wipe`, all of the slot goes first,
    /// `target/` too. A process of a lost job that still runs in the old
    /// tree writes nothing into the new one.
    #[test]
    fn a_wipe_makes_the_slot_new_and_a_late_writer_misses_it() {
        let s = Src::new();
        let (one, t1) = s.commit("one\n");
        let (ok, text) = s.receive(&one, &t1, "");
        assert!(ok, "{text}");
        let dir = s.slot();
        std::fs::create_dir_all(dir.join("target/debug")).unwrap();
        std::fs::write(dir.join("target/debug/kept"), "built").unwrap();
        let mut late = std::process::Command::new("sh")
            .args(["-c", "sleep 1; mkdir -p target; echo late > target/late"])
            .current_dir(&dir)
            .stderr(std::process::Stdio::null())
            .spawn()
            .unwrap();

        let (ok, text) = s.receive(&one, &t1, "wipe");
        assert!(ok, "{text}");
        assert!(text.contains("making the slot new"), "{text}");
        assert_eq!(git_in(&dir, &["rev-parse", "HEAD"]), one);
        assert!(!dir.join("target/debug/kept").exists(), "target/ is kept");
        late.wait().unwrap();
        assert!(
            !dir.join("target/late").exists(),
            "the late writer reached the new tree"
        );
        assert_eq!(git_in(&dir, &["status", "--porcelain", "--ignored"]), "");
    }

    /// TER-190 review, finding 2: only a kill that reached the job's host
    /// and left no process confirms the end of a lost job run.
    #[test]
    fn only_a_clean_kill_on_the_jobs_host_confirms_its_end() {
        let killed = serde_json::json!({"pid": 12, "left": []});
        assert!(kill_confirmed(true, Some(&killed)));
        assert!(kill_confirmed(
            true,
            Some(&serde_json::json!({"pid": null}))
        ));
        assert!(kill_confirmed(
            true,
            Some(&serde_json::json!({"stale": "x"}))
        ));
        assert!(!kill_confirmed(
            true,
            Some(&serde_json::json!({"error": "offline"}))
        ));
        assert!(!kill_confirmed(
            true,
            Some(&serde_json::json!({"pid": 12, "left": [13]}))
        ));
        // A target without PID files, another host, or no target.
        assert!(!kill_confirmed(false, Some(&killed)));
        assert!(!kill_confirmed(true, None));
    }

    #[test]
    fn steps_run_at_low_priority() {
        let argv = vec!["sh".to_owned(), "-c".to_owned(), "just ci".to_owned()];
        assert_eq!(
            low_priority(argv.clone(), true),
            ["ionice", "-c3", "nice", "-n19", "sh", "-c", "just ci"]
        );
        assert_eq!(
            low_priority(argv, false),
            ["nice", "-n19", "sh", "-c", "just ci"]
        );
    }

    #[test]
    fn result_line_takes_the_last_one() {
        let out =
            b"noise\nTERRARIUM_JOB\toutcome=error\thead=\nTERRARIUM_JOB\toutcome=ok\thead=abc\n";
        let r = result_line(out);
        assert_eq!(r["outcome"], "ok");
        assert_eq!(r["head"], "abc");
        assert!(result_line(b"nothing").is_empty());
    }

    #[test]
    fn ssh_connect_errors_of_the_target_only() {
        let tail = "the prepare step failed (exit code Some(255)):\nssh: connect to host 10.3.0.2 port 22: Connection refused\n";
        assert_eq!(
            ssh_connect_error(tail, "10.3.0.2"),
            Some("ssh: connect to host 10.3.0.2 port 22: Connection refused")
        );
        assert!(
            ssh_connect_error(
                "ssh: Could not resolve hostname CI-Box: Name or service not known",
                "ci-box"
            )
            .is_some()
        );
        assert!(ssh_connect_error("Connection to ci-box port 22 timed out", "ci-box").is_some());
        // The git remote of the prepare step is another host: a failure.
        assert!(
            ssh_connect_error(
                "ssh: connect to host github.com port 22: Connection timed out",
                "10.3.0.2"
            )
            .is_none()
        );
        assert!(ssh_connect_error("error: tests failed", "10.3.0.2").is_none());
    }

    #[test]
    fn refs_and_commits() {
        for ok in ["ter-71", "feature/x", "release/0.1.2"] {
            check_ref(ok).unwrap();
        }
        for bad in ["", "-x", "a..b", "a b", "x.lock", "a/", "@{u", "a:b"] {
            assert!(check_ref(bad).is_err(), "{bad:?}");
        }
        check_commit("1687f10").unwrap();
        assert!(check_commit("xyz1234").is_err());
        assert!(check_commit("123").is_err());
    }

    #[test]
    fn batch_conflicts_are_marked_by_index() {
        let found: HashMap<usize, String> =
            [(1, "README\nsrc/a.rs".to_owned()), (3, "b.txt".to_owned())]
                .into_iter()
                .collect();
        let card = |c: &str| BatchCard {
            card: c.into(),
            branch: c.to_lowercase(),
            by_agent: None,
            outcome: None,
            note: None,
        };
        let batch = vec![card("T-1"), card("T-2"), card("T-3"), card("T-4")];
        let marked = mark_conflicts(&batch, &found);
        let outcomes: Vec<Option<&str>> = marked.iter().map(|c| c.outcome.as_deref()).collect();
        assert_eq!(outcomes, [None, Some("conflict"), None, Some("conflict")]);
        assert_eq!(marked[1].note.as_deref(), Some("README\nsrc/a.rs"));
        assert_eq!(
            batch_summary(&marked),
            "batch: merged T-1 T-3; left out on a conflict T-2 T-4"
        );
    }

    #[test]
    fn by_agent_label() {
        let labels = vec!["ci".to_owned(), "by:dash-ci".to_owned()];
        assert_eq!(by_agent_of(&labels).as_deref(), Some("dash-ci"));
        assert_eq!(by_agent_of(&[]), None);
    }

    #[tokio::test]
    async fn host_resolution_does_not_hold_the_database_and_is_pinned() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("state.db");
        let db = crate::db::Db::new(
            crate::db::open(&path).unwrap(),
            crate::events::EventHub::default(),
        );
        let started = Arc::new(tokio::sync::Notify::new());
        let ready = started.clone();
        let (release, wait) = std::sync::mpsc::channel();
        let resolver_db = db.clone();
        let task = tokio::spawn(async move {
            prepare_hosts_with(
                &resolver_db,
                Some("round-robin.invalid".into()),
                move |_| {
                    ready.notify_one();
                    wait.recv().unwrap();
                    "192.0.2.1".into()
                },
            )
            .await
            .unwrap();
        });
        started.notified().await;
        let read = tokio::time::timeout(
            Duration::from_secs(2),
            db.read(|c| Ok(c.query_row("SELECT 1", [], |r| r.get::<_, i64>(0))?)),
        )
        .await;
        release.send(()).unwrap();
        task.await.unwrap();
        assert_eq!(read.unwrap().unwrap(), 1);
        // A new handle simulates a restart. A changed DNS answer must not
        // move a running job away from its cap, or retry a failed lookup.
        let reopened = crate::db::Db::new(
            crate::db::open(&path).unwrap(),
            crate::events::EventHub::default(),
        );
        prepare_hosts_with(&reopened, Some("ROUND-ROBIN.INVALID:2222".into()), |_| {
            panic!("must not resolve again")
        })
        .await
        .unwrap();
        let key: String = reopened
            .read(|c| {
                Ok(c.query_row(
                    "SELECT host_key FROM job_host_keys WHERE hostname = 'round-robin.invalid'",
                    [],
                    |r| r.get(0),
                )?)
            })
            .await
            .unwrap();
        assert_eq!(key, "192.0.2.1");
        prepare_hosts_with(&reopened, Some("failed.invalid".into()), str::to_owned)
            .await
            .unwrap();
        prepare_hosts_with(&reopened, Some("FAILED.INVALID".into()), |_| {
            panic!("failed lookups must be pinned too")
        })
        .await
        .unwrap();
    }

    /// A job for the verdict tests: ended at attempt 2 on `C1`.
    fn ended(kind: JobKind, state: jobs::JobState, outcome: Option<&str>) -> Job {
        Job {
            id: "J1".into(),
            project: "p".into(),
            card: "C1".into(),
            kind,
            repo: "app".into(),
            branch: "c1".into(),
            commit: Some("abc1234".into()),
            state,
            attempt: 2,
            outcome: outcome.map(str::to_owned),
            exit_code: outcome.map(|_| 1),
            tail: Some("test x ... FAILED".into()),
            head: None,
            by_agent: Some("w1".into()),
            created_by: "agent:lead".into(),
            created_at: "t0".into(),
            started_at: Some("t1".into()),
            ended_at: Some("t2".into()),
            host: None,
            batch: vec![],
            parent: None,
            split_path: String::new(),
            planned_secs: None,
            slot: None,
        }
    }

    fn batch_card(card: &str, by: &str, outcome: Option<&str>) -> BatchCard {
        BatchCard {
            card: card.into(),
            branch: card.to_lowercase(),
            by_agent: Some(by.into()),
            outcome: outcome.map(str::to_owned),
            note: outcome.map(|_| "src/a.rs".to_owned()),
        }
    }

    fn batch(state: jobs::JobState, outcome: Option<&str>, cards: Vec<BatchCard>) -> Job {
        let mut j = ended(JobKind::Merge, state, outcome);
        j.card = cards[0].card.clone();
        j.branch = cards[0].branch.clone();
        j.by_agent = cards[0].by_agent.clone();
        j.batch = cards;
        j
    }

    fn field<'a>(v: &'a [Value], key: &str) -> Vec<&'a str> {
        v.iter().map(|d| d[key].as_str().unwrap_or("-")).collect()
    }

    /// TER-167: a job of one card has one verdict with its kind, attempt,
    /// worker, end comment, and (for a failure) the failure block.
    #[test]
    fn a_single_job_has_one_verdict() {
        use jobs::JobState::{Cancelled, Failed, Passed};
        let cases = [
            (Passed, None, "passed", "passed"),
            (Failed, Some("red"), "red", "failed"),
            (Failed, Some("conflict"), "conflict", "failed"),
            (Failed, Some("error"), "error", "failed"),
            (Failed, None, "error", "failed"),
            (Cancelled, None, "cancelled", "was cancelled"),
        ];
        for (state, outcome, verdict, word) in cases {
            let v = verdicts(&ended(JobKind::Validate, state, outcome));
            assert_eq!(v.len(), 1, "{verdict}");
            let d = &v[0];
            assert_eq!(d["verdict"], verdict);
            assert_eq!(d["project"], "p");
            assert_eq!(d["job"], "J1");
            assert_eq!(d["card"], "C1");
            assert_eq!(d["kind"], "validate");
            assert_eq!(d["attempt"], 2);
            assert_eq!(d["by_agent"], "w1");
            assert_eq!(d["branch"], "c1");
            assert!(d["batch"].is_null());
            let comment = d["end_comment"].as_str().unwrap();
            assert!(
                comment.starts_with(&format!("Job queue: validate job J1 (attempt 2) {word}.")),
                "{comment}"
            );
            let failure = &d["failure_block"];
            if matches!(verdict, "passed" | "cancelled") {
                assert!(failure.is_null(), "{verdict}: {failure}");
            } else {
                let f = failure.as_str().unwrap();
                assert!(
                    f.starts_with("Validation failed: t2, validate job J1"),
                    "{f}"
                );
                assert!(f.contains("test x ... FAILED"), "{f}");
            }
        }
    }

    /// TER-167: a green batch has a verdict per card, with each card's
    /// branch and worker. A card that left it on a conflict has its own.
    #[test]
    fn a_green_batch_has_a_verdict_per_card() {
        let j = batch(
            jobs::JobState::Passed,
            None,
            vec![
                batch_card("C1", "w1", None),
                batch_card("C2", "w2", Some("conflict")),
                batch_card("C3", "w3", None),
            ],
        );
        let v = verdicts(&j);
        assert_eq!(field(&v, "card"), ["C1", "C2", "C3"]);
        assert_eq!(field(&v, "verdict"), ["passed", "conflict", "passed"]);
        assert_eq!(field(&v, "by_agent"), ["w1", "w2", "w3"]);
        assert_eq!(field(&v, "branch"), ["c1", "c2", "c3"]);
        assert_eq!(field(&v, "kind"), ["merge", "merge", "merge"]);
        for d in &v {
            assert_eq!(d["batch"], serde_json::json!(["C1", "C2", "C3"]));
        }
        assert!(v[0]["failure_block"].is_null());
        let f = v[1]["failure_block"].as_str().unwrap();
        assert!(f.contains("`c2` does not merge onto `next`"), "{f}");
        assert!(f.contains("src/a.rs"), "{f}");
    }

    /// TER-167: a red batch and an error give each merged card the verdict
    /// of the batch; a cancelled batch gives each card `cancelled`.
    #[test]
    fn a_red_failed_or_cancelled_batch_has_a_verdict_per_card() {
        use jobs::JobState::{Cancelled, Failed};
        let cards = || vec![batch_card("C1", "w1", None), batch_card("C2", "w2", None)];
        let red = verdicts(&batch(Failed, Some("red"), cards()));
        assert_eq!(field(&red, "verdict"), ["red", "red"]);
        assert!(red.iter().all(|d| d["failure_block"].is_string()));
        let error = verdicts(&batch(Failed, Some("error"), cards()));
        assert_eq!(field(&error, "verdict"), ["error", "error"]);
        let all_conflict = verdicts(&batch(
            Failed,
            Some("conflict"),
            vec![
                batch_card("C1", "w1", Some("conflict")),
                batch_card("C2", "w2", Some("conflict")),
            ],
        ));
        assert_eq!(field(&all_conflict, "verdict"), ["conflict", "conflict"]);
        let cancelled = verdicts(&batch(Cancelled, None, cards()));
        assert_eq!(field(&cancelled, "verdict"), ["cancelled", "cancelled"]);
        assert!(cancelled.iter().all(|d| d["failure_block"].is_null()));
    }

    /// TER-167: the end writes the verdict events (kind `job.verdict`, the
    /// project and the card on the row) in its write, and the coded flow
    /// still splits a red batch.
    #[tokio::test]
    async fn the_end_writes_a_verdict_per_card_and_keeps_the_split() {
        let dir = tempfile::tempdir().unwrap();
        let db = crate::db::Db::new(
            crate::db::open(&dir.path().join("state.db")).unwrap(),
            crate::events::EventHub::default(),
        );
        let rows = db
            .write("system", |t| {
                let cards = vec![
                    batch_card("C1", "w1", None),
                    batch_card("C2", "w2", Some("conflict")),
                    batch_card("C3", "w3", None),
                ];
                let j = jobs::create(
                    t,
                    jobs::NewJob {
                        project: "p".into(),
                        card: "C1".into(),
                        kind: JobKind::Merge,
                        repo: "app".into(),
                        branch: "c1".into(),
                        commit: None,
                        by_agent: Some("w1".into()),
                        batch: cards.clone(),
                        planned_secs: None,
                    },
                )?;
                assert!(jobs::start(t, &j.id, "h", Some(1))?);
                let end = JobEnd {
                    passed: false,
                    outcome: Some("red".into()),
                    exit_code: Some(1),
                    tail: Some("red".into()),
                    head: None,
                    batch: Some(cards),
                };
                let j = jobs::finish(t, &j.id, &end)?.unwrap();
                on_end(t, &j)?;
                let mut st = t.prepare(
                    "SELECT project, card, payload FROM events WHERE kind = 'job.verdict' ORDER BY seq",
                )?;
                let rows = st
                    .query_map([], |r| {
                        Ok((
                            r.get::<_, Option<String>>(0)?,
                            r.get::<_, Option<String>>(1)?,
                            r.get::<_, String>(2)?,
                        ))
                    })?
                    .collect::<Result<Vec<_>, _>>()?;
                let open = jobs::list(t, "p", Some(jobs::JobState::Queued))?;
                Ok((rows, open.len()))
            })
            .await
            .unwrap();
        let (rows, queued) = rows;
        let got: Vec<(String, String, String)> = rows
            .into_iter()
            .map(|(p, c, d)| {
                let d: Value = serde_json::from_str(&d).unwrap();
                (
                    p.unwrap(),
                    c.unwrap(),
                    d["verdict"].as_str().unwrap().to_owned(),
                )
            })
            .collect();
        let want = [("C1", "red"), ("C2", "conflict"), ("C3", "red")]
            .map(|(c, v)| ("p".to_owned(), c.to_owned(), v.to_owned()));
        assert_eq!(got, want);
        // The coded flow: C1 and C3 merged and the batch is red, so it
        // splits into two halves of one card each.
        assert_eq!(queued, 2);
    }

    /// TER-137: the job queue writes its comments and its moves with the
    /// rights of the operator, but the author of each comment is `system`,
    /// and the audit and event rows say `system`. An operator write keeps
    /// `operator`.
    #[tokio::test]
    async fn queue_comment_and_move_show_system_author() {
        let mut c = rusqlite::Connection::open_in_memory().unwrap();
        crate::db::migrate(&mut c).unwrap();
        let db = crate::db::Db::new(c, crate::events::EventHub::default());
        let card = db
            .write("operator", |t| {
                crate::store::projects::create(
                    t,
                    &crate::store::projects::NewProject {
                        name: "p".into(),
                        ..Default::default()
                    },
                    crate::store::projects::SOURCE_API,
                )?;
                let c = board::create_card(
                    t,
                    &Actor::Operator,
                    "p",
                    serde_json::from_value(serde_json::json!({"title": "T"})).unwrap(),
                )?;
                Ok(c.id)
            })
            .await
            .unwrap();
        // The queue's result: a move with its note, then a plain comment.
        let id = card.clone();
        db.write("system", move |t| {
            move_or_comment(t, &id, COL_REVIEW, None, "Job queue: the note.")
        })
        .await
        .unwrap();
        let id = card.clone();
        db.write("system", move |t| comment(t, &id, "Job queue: a comment."))
            .await
            .unwrap();
        let id = card.clone();
        db.write("operator", move |t| {
            board::add_comment(t, &Actor::Operator, &id, "By hand.").map(|_| ())
        })
        .await
        .unwrap();

        let (column, rows, moved) = db
            .read({
                let id = card.clone();
                move |c| {
                    let column = board::get_card(c, &id)?.column;
                    let mut stmt =
                        c.prepare("SELECT author, body FROM card_comments WHERE card = ?1")?;
                    let rows: Vec<(String, String)> = stmt
                        .query_map([&id], |r| Ok((r.get(0)?, r.get(1)?)))?
                        .collect::<Result<_, _>>()?;
                    let moved: (String, String) = c.query_row(
                        "SELECT actor, summary FROM events
                         WHERE card = ?1 AND kind = 'card' AND summary LIKE '%moved%'",
                        [&id],
                        |r| Ok((r.get(0)?, r.get(1)?)),
                    )?;
                    Ok((column, rows, moved))
                }
            })
            .await
            .unwrap();
        assert_eq!(column, COL_REVIEW);
        let author_of = |body: &str| {
            rows.iter()
                .find(|(_, b)| b == body)
                .map(|(a, _)| a.clone())
                .unwrap_or_default()
        };
        assert_eq!(author_of("Job queue: the note."), "system");
        assert_eq!(author_of("Job queue: a comment."), "system");
        assert_eq!(author_of("By hand."), "operator");
        assert_eq!(moved.0, "system");
        assert!(moved.1.ends_with("by system"), "{}", moved.1);
    }
}
