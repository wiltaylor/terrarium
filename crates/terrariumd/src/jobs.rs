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
//!   at `<state>/jobs/<project>/repos/<repo>.git` ([`git::JobRepo`]).
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
//!      The receive step and the recipe hold an exclusive `flock` on
//!      `<slot>.lock`, and the recipe checks the head again under it, so
//!      a second daemon on the same work dir (the default depends only on
//!      the project name) fails with a clear error instead of replacing
//!      the slot's tree of the running job (TER-448).
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
//!   their host. The daemon repo and target slot tree stay for reuse.
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
//!   the stop is killed and put back by the next daemon (above). Before an
//!   update, the job slots alone can drain (TER-614 R4, `{"jobs_only":
//!   true}`): no job starts, the running ones finish, the agents go on.
//! - A merge job that failed on its host before its tests (TER-614 R3,
//!   [`OUTCOME_INFRA`]) is no test failure: no split, no rerun at once, no
//!   attempt used. Two paths:
//!   - an `error` end with no tested commit in a step of [`HOST_STEPS`]
//!     (the clone on the target, the secrets or the Docker tunnel, the
//!     start of the recipe). The first such end goes back to the queue
//!     for one retry. The retry waits [`INFRA_RETRY`], and not while its
//!     host agent is offline; a host agent that comes back online ends the
//!     wait early. The second such end ends the job `infra`. The retry
//!     mark and the wait live in memory: after a daemon restart the job
//!     starts at once and gets its one retry again, so a restart gives at
//!     most one extra run.
//!   - a run lost past [`MAX_LOST_RERUNS`] (below) ends `infra` at once,
//!     with no delayed retry: the lost reruns were its retries.
//!
//!   The cards of an `infra` end stay in the merge queue with no job. They
//!   go in the next batch when a card enters the merge queue or the lead
//!   queues them. A failure in the daemon (`setup`, `prepare`) stays an
//!   `error`; a card branch that cannot be fetched or merged leaves the
//!   merge as a conflict does (the top of `ready`), and the others go on.
//!   Each failed end keeps its step as the last line of its tail
//!   (`[failed step: ...]`). An `error` or `infra` end writes a comment
//!   with the step and the last lines of the output on each card; a red
//!   end has them in its failure block.
//! - A host that cannot be reached is not a failure (TER-160). A job whose
//!   target's host agent is offline is not started: it stays queued. A job
//!   whose target could not be reached before its recipe started (the host
//!   agent went away, or the ssh connect was refused or timed out) goes
//!   back to the queue ([`jobs::requeue`]); an ssh target is tried again
//!   after [`RETRY`]. Neither uses an attempt or moves a card. After
//!   [`LONG_WAIT`] the queue notes the wait on the cards, once per job and
//!   daemon run.
//! - A recipe run lost to the infrastructure is not a failure (TER-196):
//!   only an end of the recipe itself (an exit code, a signal, its
//!   timeout) is red, and only a red batch splits. A run that ends with no
//!   exit code ([`lost_run`]: the host agent no longer holds the session,
//!   the host went offline, the run process died) or that its host agent
//!   ended because the daemon link was down, or an ssh run whose link to
//!   the target broke, is lost. Its processes on the target get the kill
//!   of a lost run ([`end_lost`], with the slot wipe when the kill is not
//!   confirmed), and the same job goes back to the queue with its attempt
//!   and its place ([`jobs::requeue_infra`]): a batch reruns whole. It
//!   goes back only once the lost run let go of its slot lock
//!   ([`wait_slot_free`], TER-676); past [`LOST_SLOT_WAIT_SECS`] it ends
//!   instead, with the lock in its tail. An
//!   error end that a rerun gets again ([`FIXED_ERRORS`]) is red, and after
//!   [`MAX_LOST_RERUNS`] lost runs in a row the next one is red.
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
/// The outcome of a merge job that failed before its tests (TER-614 R3):
/// a setup error with no tested commit, or a run that the infrastructure
/// lost past [`MAX_LOST_RERUNS`]. Its batch does not split, it does not
/// rerun at once, and its cards stay in the merge queue.
pub const OUTCOME_INFRA: &str = "infra";
/// How long a merge job that failed before its tests waits before its
/// one retry (TER-614 R3), by default. The host must be back too.
pub const INFRA_RETRY: Duration = Duration::from_secs(5 * 60);
/// The steps of a job, for the `[failed step: ...]` line of a failed
/// end (TER-614 R3).
const STEP_SETUP: &str = "setup";
const STEP_PREPARE: &str = "prepare";
const STEP_RECEIVE: &str = "receive";
const STEP_SECRETS: &str = "secrets";
const STEP_RECIPE: &str = "recipe";
const STEP_PUSH: &str = "push";
/// The lines of the job output in a card comment of a failed job.
const COMMENT_TAIL_LINES: usize = 20;
/// The size of one stdin chunk of a step (the bundle of a job).
const STDIN_CHUNK: usize = 64 * 1024;
/// The dir of the daemon's job repos, in the state dir (TER-103).
const JOBS_DIR: &str = "jobs";

/// The queue state of one daemon (`AppState::job_queue`).
pub struct Runtime {
    /// True once [`spawn`] has taken up the jobs of the last daemon
    /// ([`recover_lost`]). A job that starts before that counts as lost and
    /// goes back to the queue (TER-211).
    recovered: tokio::sync::watch::Sender<bool>,
    /// True while [`record_accepted_heads`] does nothing (TER-580): a test
    /// holds the background read to see what the API pre-read recorded.
    accepted_hold: std::sync::atomic::AtomicBool,
}

impl Default for Runtime {
    fn default() -> Self {
        Self {
            recovered: tokio::sync::watch::Sender::new(false),
            accepted_hold: std::sync::atomic::AtomicBool::new(false),
        }
    }
}

impl Runtime {
    /// Hold (or release) the background read of the reviewed commits.
    pub fn hold_accepted_reads(&self, hold: bool) {
        self.accepted_hold
            .store(hold, std::sync::atomic::Ordering::SeqCst);
    }

    fn accepted_held(&self) -> bool {
        self.accepted_hold.load(std::sync::atomic::Ordering::SeqCst)
    }

    /// Wait until [`spawn`] has taken up the jobs of the last daemon. From
    /// then on, the start does not take a running job as lost.
    pub async fn recovered(&self) {
        let mut rx = self.recovered.subscribe();
        // The sender lives as long as `self`, so the wait cannot fail.
        let _ = rx.wait_for(|done| *done).await;
    }
}

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
    // Persistent daemon repositories survive recovery too.
    if let Err(e) = cleanup_job_files(&state.state_dir.join(JOBS_DIR)) {
        warn!("cannot remove stale job files: {e}");
    }
    let weak = Arc::downgrade(state);
    tokio::spawn(async move {
        if let Some(state) = weak.upgrade() {
            if let Err(e) = prepare_hosts(&state.db, None).await {
                warn!("cannot initialize stored job host keys: {e}");
            }
            drop(recover_lost(&state).await);
            state.job_queue.recovered.send_replace(true);
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

/// A job whose run is lost: one that the last daemon left, found by
/// [`recover_lost`], or one whose recipe run this daemon lost to the
/// infrastructure (TER-196, [`lost_run`]).
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
    /// Why this daemon lost the run (TER-196). `None`: the last daemon
    /// left it.
    why: Option<String>,
    /// The red end of a run past [`MAX_LOST_RERUNS`]: after the kill the
    /// job ends with it instead of going back to the queue.
    red: Option<JobEnd>,
    /// The work tree of its slot on the target ([`slot_dir`]), when it
    /// has a slot: the job goes back to the queue only after the lock of
    /// that tree is free ([`wait_slot_free`], TER-676).
    dir: Option<String>,
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
                // A release ran on the release target (TER-623).
                let delivery = jobs::job_delivery(&jobs::delivery(t, &job.project)?, job.kind)
                    .unwrap_or_default();
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
                let dir = job.slot.map(|s| slot_dir(&delivery, &job.project, s));
                out.push(Lost {
                    job,
                    running,
                    def,
                    live,
                    other_host,
                    why: None,
                    red: None,
                    dir,
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
/// [`recover_lost`]. A run that this daemon lost (TER-196) gets the same
/// kill: the host agent may have left its processes running.
async fn end_lost(state: &Shared, l: Lost) {
    let Lost {
        job,
        running,
        def,
        live,
        other_host,
        why,
        red,
        dir,
    } = l;
    let mut report = None;
    let wraps = def.as_ref().is_some_and(crate::run_kill::kind_wraps);
    match &def {
        Some(def) if wraps => {
            let r = crate::run_kill::kill_file(
                state,
                def,
                Some(terrarium_host::native::service_holder(&job.project, "jobs")),
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
    // TER-676: a job that goes back to the queue starts again only after
    // the lost run let go of its slot. Its receive step takes the slot
    // lock without a wait, so a rerun that starts while the lost run still
    // ends fails as held. Past the cap the job ends, with why.
    let red = match (&def, dir.as_deref()) {
        (Some(def), Some(dir)) if running && red.is_none() && wraps && !other_host => {
            match wait_slot_free(state, def, &job, dir).await {
                Some(held) => {
                    let merge = job.kind == JobKind::Merge;
                    let (outcome, counts) = if merge {
                        (OUTCOME_INFRA, "an infra failure, not a test failure")
                    } else {
                        ("red", "red")
                    };
                    Some(JobEnd {
                        passed: false,
                        outcome: Some(outcome.into()),
                        tail: Some(format!(
                            "the run was lost ({}), and its kill did not end it: {held}. \
                             The job does not rerun and counts as {counts}. Stop the \
                             processes that hold the lock on the target, then queue the job again.",
                            why.as_deref().unwrap_or("the last daemon left it running")
                        )),
                        ..JobEnd::default()
                    })
                }
                None => None,
            }
        }
        _ => red,
    };
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
    if running && let Some(end) = red {
        // TER-196 review 2, major 1: a lost run past the cap ends red, but
        // only after the kill and the slot mark above.
        finish_job(state, &job.id, end).await;
    } else if running && job.kind == JobKind::Release {
        // TER-623: a release never runs again by itself. Its push, its
        // signatures or its GitHub release may be done: the lead looks
        // first, and queues a new release.
        let why = why.unwrap_or_else(|| "the daemon stopped during the run".into());
        finish_job(state, &job.id, release_lost_end(&why)).await;
    } else if running {
        // P5: the run is lost, not failed. The job goes back to the queue
        // with its attempt and its place, as TER-160 does for a host that
        // could not be reached. TER-196: the same for a run lost to the
        // infrastructure; a batch reruns whole.
        let id = job.id.clone();
        let cards = job.cards();
        let back = state
            .db
            .write("system", move |t| {
                let j = match &why {
                    Some(why) => jobs::requeue_infra(t, &id, why)?,
                    None => jobs::requeue_lost(t, &id)?,
                };
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

/// The end of a release whose run was lost (TER-623): an error, never a
/// rerun.
fn release_lost_end(why: &str) -> JobEnd {
    error_end(format!(
        "the release run was lost ({why}); a release does not run again by itself: check \
         the tag, the image and the GitHub release of the version, then queue a new release"
    ))
}

/// How long, in all, the end of a lost run waits for its slot lock to be
/// free before the job ends instead of going back to the queue (TER-676).
pub const LOST_SLOT_WAIT_SECS: u64 = 120;

/// The longest single wait for the slot lock on the target. Between two
/// waits the kill of the lost run runs again.
const LOST_SLOT_PROBE_SECS: u64 = 15;

/// The wait of [`LOST_SLOT_WAIT_SECS`] of some jobs, for the tests of the
/// capped path.
fn lost_slot_waits() -> MutexGuard<'static, HashMap<String, u64>> {
    static W: OnceLock<Mutex<HashMap<String, u64>>> = OnceLock::new();
    match W.get_or_init(Mutex::default).lock() {
        Ok(m) => m,
        Err(p) => p.into_inner(),
    }
}

/// Set the wait for the slot lock of lost job `job` to `secs`. For the
/// tests of the capped path only ([`wait_slot_free`]).
#[doc(hidden)]
pub fn set_lost_slot_wait(job: &str, secs: u64) {
    lost_slot_waits().insert(job.to_owned(), secs);
}

/// `sh -c SLOT_FREE sh <dir> <secs>`: wait at most `secs` for the lock of
/// slot tree `dir` (TER-448, [`RECEIVE_SCRIPT`]) and let go of it at
/// once. Prints `free`, `held` (still locked after the wait), `noflock`,
/// or `error`.
const SLOT_FREE: &str = r#"lock=$1.lock; w=$2
command -v flock >/dev/null 2>&1 || { echo noflock; exit 0; }
[ -e "$lock" ] || { echo free; exit 0; }
flock -E 75 -w "$w" "$lock" true 2>/dev/null
case $? in 0) echo free ;; 75) echo held ;; *) echo error ;; esac
"#;

/// Wait until no process of lost job `job` holds the lock of its slot
/// tree `dir` on target `def` (TER-676). The kill of a lost run
/// ([`end_lost`]) can end before the run does: under load, or when the
/// kill could not start. While the lock is held, the kill runs again
/// (a PID file that is still there gets it), up to
/// [`LOST_SLOT_WAIT_SECS`]. `None` when the lock is free, or when the
/// target cannot tell (no `flock`, an error): the receive step then
/// decides as before. `Some(why)` when it is still held at the cap.
async fn wait_slot_free(state: &Shared, def: &TargetDef, job: &Job, dir: &str) -> Option<String> {
    let holder = Some(terrarium_host::native::service_holder(&job.project, "jobs"));
    let cap = lost_slot_waits()
        .remove(&job.id)
        .unwrap_or(LOST_SLOT_WAIT_SECS);
    let started = Instant::now();
    let until = started + Duration::from_secs(cap);
    loop {
        let left = until.saturating_duration_since(Instant::now()).as_secs();
        let wait = left.clamp(1, LOST_SLOT_PROBE_SECS);
        let probe = RemoteCmd {
            argv: vec![
                "sh".into(),
                "-c".into(),
                SLOT_FREE.into(),
                "sh".into(),
                dir.to_owned(),
                wait.to_string(),
            ],
            timeout_secs: Some(wait + 30),
            holder: holder.clone(),
            ..RemoteCmd::default()
        };
        let said = match crate::executor::start_cmd(state, def, &probe) {
            Ok(run) => {
                let c = run.collect(4096).await;
                String::from_utf8_lossy(&c.stdout).trim().to_owned()
            }
            Err(e) => {
                warn!(job = %job.id, "cannot check the slot lock of a lost job run: {e}");
                return None;
            }
        };
        match said.as_str() {
            "free" => {
                if started.elapsed() >= Duration::from_secs(1) {
                    info!(job = %job.id, secs = started.elapsed().as_secs(),
                        "the slot of a lost job run is free; the job goes back to the queue");
                }
                return None;
            }
            "held" => {}
            other => {
                debug!(job = %job.id, said = other, "the target cannot tell if the slot of a lost job run is free");
                return None;
            }
        }
        if Instant::now() >= until {
            let lock = format!("{dir}.lock");
            warn!(job = %job.id, lock, cap, "the processes of a lost job run still hold its slot");
            return Some(format!(
                "its processes still held the slot lock {lock} after {} s",
                started.elapsed().as_secs()
            ));
        }
        warn!(job = %job.id, dir, "the slot of a lost job run is still held; its kill runs again");
        let r = crate::run_kill::kill_file(
            state,
            def,
            holder.clone(),
            &crate::run_kill::job_pidfile(&job.id),
        )
        .await;
        debug!(job = %job.id, report = %r, "the kill of a lost job run, again");
    }
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
    check_merge_refs(def)?;
    check_release_refs(def)?;
    check_release_owner(conn, actor, project, def)?;
    check_release_target(conn, project, def)?;
    if def
        .failure_patterns
        .as_ref()
        .is_some_and(|patterns| patterns.iter().any(|p| p.trim().is_empty()))
    {
        return Err(ApiError::invalid(
            "failure_patterns must not contain empty strings",
        ));
    }
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

/// Record the reviewed commit of the cards that came into the merge queue
/// (TER-553), in the background: the read of each head is a `git
/// ls-remote` in the daemon. The API move and comment, the rule effects,
/// and a timer call it.
pub fn record_accepted_soon(state: &Shared) {
    let state = state.clone();
    tokio::spawn(async move { record_accepted_heads(&state).await });
}

/// Read and record the head of each card branch whose round in the merge
/// queue has no commit yet (TER-553). A card without a usable repo or
/// branch, or a branch that is not on the remote, keeps its round open
/// with a note: a batch leaves the card out until its head is recorded.
/// The read takes the tip at read time, so it fills only a round accepted
/// within [`accepted_heads::FILL_MAX_AGE_SECS`] (TER-580): an older open
/// round gets [`accepted_heads::EXPIRED_NOTE`] and needs a fresh accept.
/// Returns the count of recorded heads.
pub async fn record_accepted_heads(state: &Shared) -> usize {
    use crate::store::accepted_heads;
    if state.job_queue.accepted_held() {
        return 0;
    }
    let now = chrono::Utc::now();
    // A failed read waits a minute: a card without its branch on the
    // remote does not cost a `git ls-remote` on each queue kick.
    let retry_before =
        (now - chrono::Duration::seconds(60)).to_rfc3339_opts(chrono::SecondsFormat::Millis, true);
    let accepted_after = accepted_heads::fill_cutoff();
    let cutoff = accepted_after.clone();
    if let Err(e) = state
        .db
        .write("system", move |t| {
            accepted_heads::expire(t, &cutoff)?;
            note_stalls(t)
        })
        .await
    {
        warn!("cannot note the expired reviewed-commit reads: {e}");
    }
    let cutoff = accepted_after;
    let pending = match state
        .db
        .read(move |c| accepted_heads::pending(c, &retry_before, &cutoff))
        .await
    {
        Ok(p) => p,
        Err(e) => {
            warn!("cannot read the pending reviewed commits: {e}");
            return 0;
        }
    };
    let mut recorded = 0;
    for p in pending {
        let card = p.card.clone();
        let place = state
            .db
            .read(move |c| {
                let card = board::get_card(c, &card)?;
                let repo = crate::api::jobs::repo_of(c, &card, None)?;
                let branch = crate::api::jobs::branch_of(&card, None)?;
                Ok((card.project, repo, branch))
            })
            .await;
        let read = match place {
            Ok((project, repo, branch)) => {
                match remote_head(state, &project, &repo, &branch).await {
                    Ok(Some(h)) if check_commit(&h).is_ok() => Ok((branch, h)),
                    Ok(Some(h)) => Err(format!("bad head `{h}` of `{branch}`")),
                    Ok(None) => Err(format!(
                        "branch `{branch}` is not on the remote of `{repo}`"
                    )),
                    Err(e) => Err(e.message),
                }
            }
            Err(e) => Err(e.message),
        };
        let (card, round) = (p.card.clone(), p.round.clone());
        let done = state
            .db
            .write("system", move |t| match &read {
                // The cutoff at the write: the reads before took time.
                Ok((branch, head)) => accepted_heads::record(
                    t,
                    &card,
                    &round,
                    branch,
                    head,
                    &accepted_heads::fill_cutoff(),
                ),
                Err(why) => accepted_heads::note(t, &card, &round, why).map(|()| false),
            })
            .await;
        match done {
            Ok(true) => recorded += 1,
            Ok(false) => {}
            Err(e) => warn!(card = %p.card, "cannot record the reviewed commit: {e}"),
        }
    }
    recorded
}

/// Post one card comment on each round that needs a fresh accept (the
/// upgrade backfilled it, or the read did not fill it in time), so the
/// card does not wait in the merge queue without a signal (TER-580).
fn note_stalls(t: &mut crate::db::Tx<'_>) -> ApiResult<()> {
    use crate::store::accepted_heads;
    for s in accepted_heads::stalls(t)? {
        let body = format!(
            "Job queue: the reviewed commit of this card is not recorded: {}. A merge leaves \
             the card out. Fix: review the head of the branch, then move the card out of \
             `{}` and back in (a fresh accept records the head then).",
            s.note, s.column
        );
        board::system_comment(t, &s.card, &body, None)?;
        accepted_heads::stall_noted(t, &s.card)?;
    }
    Ok(())
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
    if matches!(def.kind, TargetKind::Ssh | TargetKind::Exedev) {
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
    // ones go on (TER-123, P5). The end of the drain kicks the queue. A
    // drain of the job slots only (TER-614 R4) does the same for jobs.
    let drain = state.self_update.jobs_draining();
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
                    if j.kind != JobKind::Release && code_flow(t, &j.project)? {
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
                // A merge that failed before its tests (TER-614 R3): one
                // retry after the wait, with no attempt used; the second
                // such end is an infra end.
                Ran::End(end) if before_tests(&job, &end) => {
                    let first = infra_retried().insert(job.id.clone());
                    if first {
                        set_control(&job.id, None);
                        requeue_before_tests(&state, &job, &end).await;
                        drop(live_run);
                        return;
                    }
                    infra_retried().remove(&job.id);
                    JobEnd {
                        outcome: Some(OUTCOME_INFRA.into()),
                        ..end
                    }
                }
                Ran::End(end) => end,
                Ran::Wait { why, ssh } => {
                    set_control(&job.id, None);
                    requeue(&state, &job, why, ssh).await;
                    drop(live_run);
                    return;
                }
                Ran::Lost { why, red } => {
                    set_control(&job.id, None);
                    rerun_lost(&state, job.clone(), why, red, live_run).await;
                    return;
                }
            };
            set_control(&job.id, None);
            finish_job(&state, &job.id, end).await;
            drop(live_run);
            kick(&state);
        });
    }
    Ok(())
}

/// End running job `id` with `end`, then run its card flow
/// ([`after_end`]). A job that was cancelled meanwhile stays cancelled.
async fn finish_job(state: &Shared, id: &str, end: JobEnd) {
    waits().remove(id);
    lost_runs().remove(id);
    let jid = id.to_owned();
    let ended = state
        .db
        .write("system", move |t| jobs::finish(t, &jid, &end))
        .await;
    match ended {
        Ok(Some(j)) => {
            if let Err(e) = after_end(state, &j).await {
                warn!(job = %j.id, "cannot handle the end of a job: {e}");
            }
        }
        Ok(None) => debug!(job = %id, "the job was cancelled while it ran"),
        Err(e) => warn!(job = %id, "cannot end a job: {e}"),
    }
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
    resolve: impl Fn(&str) -> String + Send + Sync + 'static,
) -> ApiResult<()> {
    let _prepare = db.host_pins().lock().await;
    let names = db.read(move |c| jobs::unresolved_hosts(c, extra)).await?;
    if names.is_empty() {
        return Ok(());
    }
    // Keep the guard in the blocking task: timing out does not cancel libc
    // DNS. Even an explicitly cleared pin must not start a second lookup.
    static LOOKUPS: std::sync::LazyLock<std::sync::Mutex<std::collections::HashSet<String>>> =
        std::sync::LazyLock::new(|| std::sync::Mutex::new(std::collections::HashSet::new()));
    struct Lookup(String);
    impl Drop for Lookup {
        fn drop(&mut self) {
            LOOKUPS
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .remove(&self.0);
        }
    }
    let resolve = Arc::new(resolve);
    let mut pending = Vec::new();
    for name in names {
        if !LOOKUPS
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .insert(name.clone())
        {
            continue;
        }
        let guard = Lookup(name.clone());
        let resolve = resolve.clone();
        let task = tokio::task::spawn_blocking(move || {
            let key = resolve(&guard.0);
            (guard, key)
        });
        pending.push(async move {
            let key = match tokio::time::timeout(Duration::from_secs(2), task).await {
                Ok(Ok((_guard, key))) => key,
                _ => name.clone(),
            };
            (name, key)
        });
    }
    let keys = futures_util::future::join_all(pending).await;
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

/// The label and the queue comment of a new merge job on each of its
/// cards, as the API's queue writes them with `board_flow = code`
/// (`card merge`, `POST /api/merges`). A rule's `job.queue` uses it, so a
/// job that a rule queues looks the same (TER-172).
pub fn mark_queued(t: &mut Tx<'_>, j: &Job) -> ApiResult<()> {
    for c in j.cards() {
        set_labels(t, &c.card, &[LABEL_QUEUED], &[])?;
        board::system_comment(t, &c.card, &queue_comment(j, &c.card), None)?;
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

/// The marker of the failed step in the tail of a job.
const FAILED_STEP: &str = "[failed step: ";

/// A failed end with the step that failed as the last line of its tail
/// (TER-614 R3), so the job keeps it. The tail is clipped from its start,
/// so the last line stays.
fn with_failed_step(mut end: JobEnd, step: &str) -> JobEnd {
    if end.passed {
        return end;
    }
    let mut tail = end.tail.take().unwrap_or_default();
    if !tail.is_empty() && !tail.ends_with('\n') {
        tail.push('\n');
    }
    tail.push_str(&format!("{FAILED_STEP}{step}]"));
    end.tail = Some(tail);
    end
}

/// The steps of a job, in order.
const STEPS: [&str; 6] = [
    STEP_SETUP,
    STEP_PREPARE,
    STEP_RECEIVE,
    STEP_SECRETS,
    STEP_RECIPE,
    STEP_PUSH,
];

/// The step that failed, from the last line of the tail of a job
/// ([`with_failed_step`]). Only one of the six step names counts: the
/// output of a recipe cannot name a step (TER-614 review, minor 6).
pub fn failed_step(tail: Option<&str>) -> Option<&'static str> {
    let last = tail?.trim_end().lines().last()?.trim();
    let name = last.strip_prefix(FAILED_STEP)?.strip_suffix(']')?;
    STEPS.into_iter().find(|s| *s == name)
}

/// The steps whose failure is one of the host or its transport, not of a
/// card (TER-614 review, major 1): the clone on the target, the secrets
/// and the Docker tunnel, the start of the recipe. A failure in the
/// daemon (`setup`, `prepare`) stays an error: a card branch that cannot
/// be fetched or merged leaves the merge instead ([`Left::unfetched`]).
const HOST_STEPS: [&str; 3] = [STEP_RECEIVE, STEP_SECRETS, STEP_RECIPE];

/// A merge end that failed on its host before its tests (TER-614 R3):
/// outcome `error`, no tested commit, and a failed step of
/// [`HOST_STEPS`]. A failed push after the tests has a commit, and stays
/// an error.
fn before_tests(job: &Job, end: &JobEnd) -> bool {
    job.kind == JobKind::Merge
        && !end.passed
        && end.outcome.as_deref() == Some("error")
        && end.head.is_none()
        && failed_step(end.tail.as_deref()).is_some_and(|s| HOST_STEPS.contains(&s))
}

/// The merge jobs that failed before their tests and wait for their one
/// retry ([`OUTCOME_INFRA`]), by job ID. An end of the job clears it.
fn infra_retried() -> MutexGuard<'static, HashSet<String>> {
    static S: OnceLock<Mutex<HashSet<String>>> = OnceLock::new();
    match S.get_or_init(Mutex::default).lock() {
        Ok(m) => m,
        Err(p) => p.into_inner(),
    }
}

/// The wait before the retry of a merge job that failed before its tests,
/// in milliseconds ([`INFRA_RETRY`]; the tests set it).
static INFRA_RETRY_MS: std::sync::atomic::AtomicU64 =
    std::sync::atomic::AtomicU64::new(INFRA_RETRY.as_millis() as u64);

/// Set the wait before the retry of a merge job that failed before its
/// tests. For the tests only.
#[doc(hidden)]
pub fn set_infra_retry(d: Duration) {
    INFRA_RETRY_MS.store(d.as_millis() as u64, Ordering::SeqCst);
}

fn infra_retry() -> Duration {
    Duration::from_millis(INFRA_RETRY_MS.load(Ordering::SeqCst))
}

/// The comment with the failed step and the last lines of the output of
/// failed job `j` (TER-614 R3), next to its rerun, split, or infra
/// comment. `None` without output. The output is untrusted text: it goes
/// indented, as in the failure block. The daemon writes it in both
/// flows ([`on_end`]).
pub fn output_comment(j: &Job) -> Option<String> {
    let t = j.tail.as_deref().filter(|t| !t.trim().is_empty())?;
    let lines: Vec<&str> = t.trim_end().lines().collect();
    let from = lines.len().saturating_sub(COMMENT_TAIL_LINES);
    let step = failed_step(Some(t)).unwrap_or("-");
    let body: String = lines[from..].iter().map(|l| format!("    {l}\n")).collect();
    Some(format!(
        "Job queue: {} job {} (attempt {}), failed step: {step}. Job output, last lines:\n\n{body}",
        j.kind.as_str(),
        j.id,
        j.attempt
    ))
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
    step_redacted(state, job, def, cmd, max, input, &[]).await
}

#[allow(clippy::too_many_arguments)]
async fn step_redacted(
    state: &Shared,
    job: &str,
    def: &terrarium_core::config::TargetDef,
    cmd: &RemoteCmd,
    max: usize,
    input: Option<Vec<u8>>,
    own: &[(String, zeroize::Zeroizing<String>)],
) -> ApiResult<Collected> {
    let pidfile = crate::run_kill::job_pidfile(job);
    let admission = state.executor.target_resource_lock(def).lock_owned().await;
    let (running, report) =
        crate::run_kill::start_named_locked(state, def, cmd, pidfile, &admission)?;
    let running = crate::executor::track_resource_run(state, def, cmd, running, report.clone())?;
    let running = crate::executor::redact_run(
        running,
        crate::executor::own_redactor(own),
        state.db.redactor().get(),
        terrarium_core::redact::View::All,
    );
    set_control(job, Some(running.ctl.clone()));
    drop(admission);
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
    /// The recipe started, and its run was lost to the infrastructure
    /// (TER-196, [`lost_run`]), with why. It gets the kill of a lost run
    /// ([`end_lost`]). Then the job reruns, or, past
    /// [`MAX_LOST_RERUNS`], ends with `red` (TER-196 review 2, major 1).
    Lost { why: String, red: Option<JobEnd> },
}

/// Put a job whose recipe run was lost to the infrastructure back in the
/// queue (TER-196): first the kill of a lost run ends what may still run
/// of it on the target ([`end_lost`]), then the same job waits again with
/// its attempt and its place. `live` keeps its slot taken until then.
async fn rerun_lost(state: &Shared, job: Job, why: String, red: Option<JobEnd>, live: LiveRun) {
    warn!(job = %job.id, "the run of a job was lost after its recipe started: {why}");
    hold(&job.id, why.clone(), None);
    // As in [`recover_lost`]: the kill goes to the current delivery
    // target, and a job that ran on another host gets no confirmed kill.
    let (project, ran_on, kind, slot) = (job.project.clone(), job.host.clone(), job.kind, job.slot);
    let found = state
        .db
        .read(move |c| {
            // A release ran on the release target (TER-623).
            let delivery =
                jobs::job_delivery(&jobs::delivery(c, &project)?, kind).unwrap_or_default();
            let dir = slot.map(|s| slot_dir(&delivery, &project, s));
            let def = match delivery.target.as_deref().map(|n| targets::get(c, n)) {
                Some(Ok(row)) if row.def.project.as_ref().is_none_or(|p| *p == project) => {
                    Some(row.def)
                }
                _ => None,
            };
            let other = def.is_some()
                && ran_on
                    .as_deref()
                    .is_some_and(|h| h != jobs::host_of(c, &delivery));
            Ok((def, other, dir))
        })
        .await;
    let (def, other_host, dir) = found.unwrap_or_else(|e| {
        warn!(job = %job.id, "cannot read the target of a lost job run: {e}");
        (None, false, None)
    });
    end_lost(
        state,
        Lost {
            job,
            running: true,
            def,
            live,
            other_host,
            why: Some(why),
            red,
            dir,
        },
    )
    .await;
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

/// Put a merge job that failed before its tests back in the queue for
/// its one retry (TER-614 R3): no attempt, no split, no card move. It
/// waits [`INFRA_RETRY`], and while its host agent is offline. The job
/// keeps the output and the failed step in its tail, and each card gets
/// them in a comment.
async fn requeue_before_tests(state: &Shared, job: &Job, end: &JobEnd) {
    let wait = infra_retry();
    let tail = end.tail.clone().unwrap_or_default();
    let why = tail
        .lines()
        .find(|l| !l.trim().is_empty())
        .unwrap_or("no output")
        .to_owned();
    warn!(job = %job.id, "a merge job failed before its tests: {why}");
    hold(
        &job.id,
        format!("it failed before its tests: {why}"),
        Some(Instant::now() + wait),
    );
    let id = job.id.clone();
    let after = if wait >= Duration::from_secs(60) {
        format!("{} min", wait.as_secs().div_ceil(60))
    } else {
        format!("{} s", wait.as_secs().max(1))
    };
    let back = state
        .db
        .write("system", move |t| {
            let Some(j) = jobs::requeue_before_tests(t, &id, &tail)? else {
                return Ok(None);
            };
            let flow = code_flow(t, &j.project)?;
            for v in card_views(&j) {
                if flow {
                    set_labels(t, &v.card, &[LABEL_QUEUED], &[LABEL_RUNNING])?;
                }
                comment(t, &v.card, &infra_comment(&v, Some(&after)))?;
                output_note(t, &v)?;
            }
            Ok(Some(j))
        })
        .await;
    match back {
        Ok(Some(j)) => debug!(job = %j.id, "the job waits for its retry"),
        Ok(None) => debug!(job = %job.id, "the job was cancelled while it ran"),
        Err(e) => warn!(job = %job.id, "cannot put a job back in the queue: {e}"),
    }
}

/// The job as each of its cards sees it ([`card_view`]): the job itself
/// for a job of one card.
fn card_views(j: &Job) -> Vec<Job> {
    if j.batch.is_empty() {
        vec![j.clone()]
    } else {
        j.batch.iter().map(|c| card_view(j, c)).collect()
    }
}

/// Run a job to its end. Never fails: an error is a failed end, a wait
/// when the target could not be reached before the recipe started, or a
/// lost run when the infrastructure lost the recipe run (TER-196).
async fn run(state: &Shared, job: &Job, def: &DeliveryDef) -> Ran {
    let recipe = AtomicBool::new(false);
    let lost = OnceLock::new();
    let step = Mutex::new(STEP_SETUP);
    let end = match run_inner(state, job, def, &recipe, &lost, &step).await {
        Ok(end) => end,
        Err(e) => error_end(format!("the job could not run: {}", e.message)),
    };
    let at = *step
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let end = with_failed_step(end, at);
    if !end.passed
        && let Some(why) = lost.into_inner()
    {
        if job.kind == JobKind::Release {
            // TER-623: the kill of [`rerun_lost`] runs, and then the
            // release ends as an error, never a rerun ([`end_lost`]).
            return Ran::Lost { why, red: None };
        }
        return lost_or_red(&job.id, job.kind == JobKind::Merge, why, end);
    }
    if end.passed || end.outcome.as_deref() != Some("error") || recipe.load(Ordering::SeqCst) {
        return Ran::End(end);
    }
    match unreachable(state, def, &end).await {
        Some((why, ssh)) => Ran::Wait { why, ssh },
        None => Ran::End(end),
    }
}

/// A lost run of job `job`: it reruns ([`Ran::Lost`] without `red`) up
/// to [`MAX_LOST_RERUNS`] times in a row. The next lost run ends red,
/// with the reason in its tail, so a run that keeps getting lost ends.
/// Both get the kill of a lost run first. A merge job (`merge`) ends with
/// [`OUTCOME_INFRA`], not red (TER-614 R3): the tests did not fail, so its
/// batch does not split and its cards stay in the merge queue.
fn lost_or_red(job: &str, merge: bool, why: String, end: JobEnd) -> Ran {
    let n = {
        let mut m = lost_runs();
        let n = m.entry(job.to_owned()).or_insert(0);
        *n += 1;
        *n
    };
    if n <= MAX_LOST_RERUNS {
        return Ran::Lost { why, red: None };
    }
    lost_runs().remove(job);
    let (outcome, counts) = if merge {
        (OUTCOME_INFRA, "an infra failure, not a test failure")
    } else {
        ("red", "red")
    };
    let tail = format!(
        "the infrastructure lost the run {n} times in a row, so this run counts as {counts}: {why}\n{}",
        end.tail.as_deref().unwrap_or("")
    );
    let red = JobEnd {
        outcome: Some(outcome.into()),
        tail: Some(tail),
        ..end
    };
    Ran::Lost {
        why,
        red: Some(red),
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

/// The error codes of a run end that a rerun gets again: the command or
/// its path is refused or cannot start. Such an end is red, not lost
/// (TER-196 review, minor 2).
const FIXED_ERRORS: &[&str] = &[
    "spawn_failed",
    "path_denied",
    "unsupported",
    "bad_request",
    "refused",
];

/// How often the queue reruns one job whose recipe run the infrastructure
/// lost, in a row and in one daemon run (TER-196 review, minor 2). The
/// next lost run is red, with the reason.
pub const MAX_LOST_RERUNS: u32 = 3;

/// The lost runs in a row of each job ([`MAX_LOST_RERUNS`]). An end of
/// the job clears its count.
fn lost_runs() -> MutexGuard<'static, HashMap<String, u32>> {
    static L: OnceLock<Mutex<HashMap<String, u32>>> = OnceLock::new();
    match L.get_or_init(Mutex::default).lock() {
        Ok(m) => m,
        Err(p) => p.into_inner(),
    }
}

/// Set the count of lost runs in a row of job `job`. For the tests of the
/// capped path only: the daemon counts by itself ([`lost_or_red`]).
#[doc(hidden)]
pub fn set_lost_runs(job: &str, n: u32) {
    lost_runs().insert(job.to_owned(), n);
}

/// Why a recipe run that did not pass was lost to the infrastructure
/// (TER-196), or `None` for an end of the recipe itself (an exit code, a
/// signal, its timeout): only that is red. Lost: no end, an error end
/// with no exit code (the host agent no longer holds the session, the
/// host is offline, the run process could not be read), except the codes
/// that a rerun gets again ([`FIXED_ERRORS`]), an end by the
/// host agent because the daemon link was down, or, with `ssh_host`, exit
/// code 255 with a line of the ssh client that its link to that host
/// broke. A cancelled job is never lost: the caller checks [`killed`].
fn lost_run(end: Option<&End>, stderr: &[u8], ssh_host: Option<&str>) -> Option<String> {
    match end {
        None => Some("the run ended without an end".into()),
        Some(End::Error { code, .. }) if FIXED_ERRORS.contains(&code.as_str()) => None,
        Some(End::Error { code, message }) => Some(format!("error {code}: {message}")),
        Some(End::Exit { reason, .. })
            if reason == "detached_timeout" || reason == "output_overflow" =>
        {
            Some(format!(
                "the host agent ended the run ({reason}): its link to the daemon was down"
            ))
        }
        Some(End::Exit {
            code: Some(255), ..
        }) => {
            let text = String::from_utf8_lossy(stderr);
            ssh_link_lost(&text, ssh_host?)
                .map(|l| format!("the ssh link to the target broke: {l}"))
        }
        _ => None,
    }
}

/// The line of `text` in which the ssh client says that its link to
/// `host` (lower case) broke or could not be made: closed by the remote
/// host, reset, a broken pipe, or a server that does not answer. Each line
/// names the host: `client_loop: send disconnect:` does not, and it may
/// come from an ssh of the recipe to another host, so it is red (TER-196
/// review, minor 3).
fn ssh_link_lost(text: &str, host: &str) -> Option<String> {
    if let Some(l) = ssh_connect_error(text, host) {
        return Some(l.to_owned());
    }
    text.lines()
        .map(str::trim)
        .find(|l| {
            let low = l.to_lowercase();
            low.starts_with(&format!("connection to {host} closed by remote host"))
                || (low.starts_with(&format!("connection to {host} port "))
                    && (low.contains("broken pipe") || low.contains("reset by peer")))
                || low.starts_with(&format!("packet_write_wait: connection to {host} port "))
                || low.starts_with(&format!("read from remote host {host}:"))
                || low.starts_with(&format!("timeout, server {host} not responding"))
        })
        .map(str::to_owned)
}

async fn run_inner(
    state: &Shared,
    job: &Job,
    def: &DeliveryDef,
    recipe_started: &AtomicBool,
    recipe_lost: &OnceLock<String>,
    step: &Mutex<&'static str>,
) -> ApiResult<JobEnd> {
    let target = def
        .target
        .clone()
        .ok_or_else(|| ApiError::invalid("the project has no delivery target"))?;
    let recipe = if job.kind == JobKind::E2e {
        "RESULTS_DIR=\"$PWD/results\" TERRARIUM_DOCKER_CI=1 python3 scripts/docker_ci.py".to_owned()
    } else {
        def.recipe
            .clone()
            .ok_or_else(|| ApiError::invalid("the project has no delivery recipe"))?
    };
    let (project, repo_name, t_name) = (job.project.clone(), job.repo.clone(), target.clone());
    let kind = job.kind;
    let (repo, row) = state
        .db
        .read(move |c| {
            let repo = repos::find_by_name(c, &project, &repo_name)?.ok_or_else(|| {
                ApiError::not_found(format!("no repo `{repo_name}` in `{project}`"))
            })?;
            // review-ter-623-r4 F1: only a release job runs on a protected
            // release target or its machine, also a job queued before the
            // protection.
            if kind != JobKind::Release {
                jobs::check_not_release_target(c, &t_name, "running a job on the target")?;
            }
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
    let local = job_repo_dir(state, &job.project, &job.repo);
    let locks = repo_lock(&local);
    let guard = locks.prepare.clone().lock_owned().await;
    let lifetime = locks.lifetime.clone().read_owned().await;
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
    let recovery = checkout.clone();
    let made = tokio::task::spawn_blocking(move || {
        git::JobRepo::open(checkout).map(|repo| (repo, guard, lifetime))
    })
    .await
    .map_err(|e| ApiError::internal(format!("the git task of the job failed: {e}")))?;
    let (job_repo, guard, lifetime) = match made {
        Ok((Some(r), guard, lifetime)) => (Arc::new(r), guard, lifetime),
        Ok((None, guard, lifetime)) => {
            drop(lifetime);
            let lifetime = locks.lifetime.clone().write_owned().await;
            tokio::task::spawn_blocking(move || {
                git::JobRepo::create(recovery).map(|r| (Arc::new(r), guard, lifetime.downgrade()))
            })
            .await
            .map_err(|e| ApiError::internal(format!("the git task of the job failed: {e}")))?
            .map_err(|e| ApiError::conflict(format!("cannot rebuild the job repo: {e}")))?
        }
        Err(e) => {
            return Err(ApiError::conflict(format!(
                "cannot make the job repo in the daemon: {e}"
            )));
        }
    };
    let holder = Some(terrarium_host::native::service_holder(&job.project, "jobs"));
    let ionice = has_ionice(state, job, &target, &row.def, holder.clone()).await;
    let project = job.project.clone();
    let wipe = state
        .db
        .read(move |c| jobs::slot_wipe_pending(c, &project, slot))
        .await?;
    let mut ctx = StepCtx {
        state,
        job,
        def: &row.def,
        dir: &dir,
        repo: job_repo,
        lifetime: Some(Arc::new(lifetime)),
        repo_locks: locks,
        bundle: local.with_file_name(format!("{}.bundle", job.id)),
        holder,
        ionice,
        recipe_started,
        recipe_lost,
        step,
        ssh_host: row
            .def
            .address
            .as_deref()
            .filter(|_| row.def.kind == TargetKind::Ssh)
            .map(jobs::address_key),
        slot,
        wipe,
    };
    // The slot's tree stays on the target for the next job of the slot
    // (TER-190): no delete of the tree and its build output per job.
    let bundle = ctx.bundle.clone();
    let result = steps(&mut ctx, recipe, def, guard).await;
    drop(ctx);
    let _ = tokio::fs::remove_file(bundle).await;
    result
}

/// The work tree of a slot on the target (TER-190):
/// `<work_dir>/<project>/slot-<n>`, kept from job to job. Only one running
/// job holds a slot ([`jobs::pick`]), so no two jobs share a tree. The
/// steps of the job hold an exclusive lock on `<dir>.lock` (TER-448), so
/// a second daemon on the same work dir cannot take the tree; it fails
/// with a clear error instead.
pub fn slot_dir(def: &DeliveryDef, project: &str, slot: i64) -> String {
    format!(
        "{}/{project}/slot-{slot}",
        def.work_dir().trim_end_matches('/')
    )
}

/// One daemon repo per project and registered repository. Bundles remain
/// job-specific. Different registered repositories never share objects.
fn job_repo_dir(state: &Shared, project: &str, repo: &str) -> PathBuf {
    state
        .state_dir
        .join(JOBS_DIR)
        .join(project)
        .join("repos")
        .join(format!("{repo}.git"))
}

/// Serialize preparation, whose scratch refs are shared. A separate lifetime
/// lock permits concurrent recipes but excludes rebuilds until all jobs and
/// blocking git tasks have stopped using the cache. Both guards move into
/// blocking mutations so cancellation cannot unlock a task still running.
#[derive(Default)]
struct RepoLocks {
    prepare: Arc<tokio::sync::Mutex<()>>,
    lifetime: Arc<tokio::sync::RwLock<()>>,
}

fn repo_lock(path: &std::path::Path) -> Arc<RepoLocks> {
    static LOCKS: OnceLock<Mutex<HashMap<PathBuf, Arc<RepoLocks>>>> = OnceLock::new();
    LOCKS
        .get_or_init(Mutex::default)
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .entry(path.to_owned())
        .or_default()
        .clone()
}

/// Before starting jobs, remove pre-cache per-job repositories and all
/// interrupted bundles. Preserve `repos/*.git` and never follow symlinks.
fn cleanup_job_files(root: &std::path::Path) -> std::io::Result<()> {
    fn visit(root: &std::path::Path, depth: usize, error: &mut Option<std::io::Error>) {
        let entries = match std::fs::read_dir(root) {
            Ok(entries) => entries,
            Err(e) if depth == 0 && e.kind() == std::io::ErrorKind::NotFound => return,
            Err(e) => {
                error.get_or_insert(e);
                return;
            }
        };
        for entry in entries {
            let result = (|| {
                let entry = entry?;
                let path = entry.path();
                let kind = entry.file_type()?;
                #[cfg(test)]
                tests::cleanup_fault(&path)?;
                if kind.is_dir() && (depth == 0 || (depth == 1 && entry.file_name() == "repos")) {
                    visit(&path, depth + 1, error);
                } else if depth == 1
                    && kind.is_dir()
                    && path.extension().is_some_and(|e| e == "git")
                {
                    std::fs::remove_dir_all(path)?;
                } else if depth > 0
                    && kind.is_file()
                    && path.extension().is_some_and(|e| e == "bundle")
                {
                    std::fs::remove_file(path)?;
                }
                Ok::<_, std::io::Error>(())
            })();
            if let Err(e) = result {
                error.get_or_insert(e);
            }
        }
    }
    let mut error = None;
    visit(root, 0, &mut error);
    error.map_or(Ok(()), Err)
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
    lifetime: Option<Arc<tokio::sync::OwnedRwLockReadGuard<()>>>,
    repo_locks: Arc<RepoLocks>,
    /// Where the daemon writes the bundle for the target.
    bundle: PathBuf,
    holder: Option<String>,
    /// The target has a working `ionice` ([`has_ionice`]).
    ionice: bool,
    /// Set when the recipe step started: a failure after it is no wait for
    /// the host (TER-160).
    recipe_started: &'a AtomicBool,
    /// Set when the infrastructure lost the recipe run, with why
    /// ([`lost_run`], TER-196): the job goes back to the queue.
    recipe_lost: &'a OnceLock<String>,
    /// The step that runs now, for the `[failed step: ...]` line of a
    /// failed end (TER-614 R3).
    step: &'a Mutex<&'static str>,
    /// The host of an ssh target ([`jobs::address_key`]), for the lines of
    /// the ssh client about a broken link.
    ssh_host: Option<String>,
    /// The job's slot ([`jobs::pick`]).
    slot: i64,
    /// A lost job may still run in the slot's tree: the receive step makes
    /// all of it new, `target/` too ([`jobs::mark_slot_wipe`]).
    wipe: bool,
}

impl StepCtx<'_> {
    /// Note that step `name` of the job starts now.
    fn at(&self, name: &'static str) {
        *self
            .step
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner) = name;
    }

    /// A git step on the target, without any credential.
    fn plain_cmd(&self, script: &str, args: &[&str]) -> RemoteCmd {
        let mut argv = vec![
            "sh".to_owned(),
            "-c".to_owned(),
            format!("{PLAIN_PRELUDE}{script}"),
            "sh".to_owned(),
        ];
        argv.extend(args.iter().map(|a| (*a).to_owned()));
        let mut env = std::collections::BTreeMap::new();
        crate::git_identity::Identity::from_env().env(&mut env);
        RemoteCmd {
            argv,
            env,
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
        let lifetime = self.lifetime.clone();
        tokio::task::spawn_blocking(move || {
            let _lifetime = lifetime;
            f(&repo)
        })
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

/// Why a card left a merge: a `conflict` (the conflicting files, or why
/// the merge failed), or `moved` (TER-553: the head of the branch is no
/// longer the reviewed commit).
#[derive(Debug, Clone, PartialEq, Eq)]
struct Left {
    outcome: &'static str,
    note: String,
}

impl Left {
    fn conflict(note: String) -> Self {
        Left {
            outcome: OUTCOME_CONFLICT,
            note,
        }
    }

    /// A card branch that the daemon could not fetch (TER-614 review,
    /// major 1): deleted or rewritten after the accept. It leaves the
    /// merge as a conflict does: the top of ready, for its worker.
    fn unfetched(branch: &str, why: &str) -> Self {
        Left {
            outcome: OUTCOME_CONFLICT,
            note: format!(
                "the branch `{branch}` cannot be fetched (deleted or rewritten after the accept?): {why}"
            ),
        }
    }

    fn moved(branch: &str, reviewed: &str, tip: &str) -> Self {
        Left {
            outcome: OUTCOME_MOVED,
            note: format!(
                "the branch `{branch}` moved after its review: the reviewed commit is {reviewed}, the head is now {tip}"
            ),
        }
    }
}

/// The outcome of a card whose branch did not merge onto `next`.
pub const OUTCOME_CONFLICT: &str = "conflict";
/// The outcome of a card whose branch head is no longer its reviewed
/// commit (TER-553). It goes back to review, with no rerun.
pub const OUTCOME_MOVED: &str = "moved";

/// Is `outcome` one that takes a card out of its merge (a conflict, or a
/// branch that moved after its review)? Such a card never runs again.
pub fn left_merge(outcome: Option<&str>) -> bool {
    matches!(outcome, Some(OUTCOME_CONFLICT | OUTCOME_MOVED))
}

/// The end of a merge in which no card merged: a `conflict` when a card
/// conflicted, else `moved`.
fn none_merged(job: &Job, left: HashMap<usize, Left>, batch: Option<Vec<BatchCard>>) -> JobEnd {
    let mut all: Vec<(usize, Left)> = left.into_iter().collect();
    all.sort_by_key(|(i, _)| *i);
    let conflict = all.iter().any(|(_, l)| l.outcome == OUTCOME_CONFLICT);
    let notes = |o: &str| -> Vec<String> {
        all.iter()
            .filter(|(_, l)| l.outcome == o)
            .map(|(_, l)| l.note.clone())
            .collect()
    };
    let tail = if conflict {
        let what = if job.batch.is_empty() {
            format!("the branch `{}` does", job.branch)
        } else {
            "none of the branches of the batch does".to_owned()
        };
        let mut t = format!(
            "{what} not merge onto `{}` without conflicts; the queue does not resolve them. Files:\n{}",
            job.onto(),
            notes(OUTCOME_CONFLICT).join("\n")
        );
        let moved = notes(OUTCOME_MOVED);
        if !moved.is_empty() {
            t.push_str(&format!(
                "\nLeft out, moved after review:\n{}",
                moved.join("\n")
            ));
        }
        t
    } else {
        format!(
            "nothing merged: each branch moved after its review, and the queue merges only the reviewed commit.\n{}",
            notes(OUTCOME_MOVED).join("\n")
        )
    };
    JobEnd {
        passed: false,
        outcome: Some(
            if conflict {
                OUTCOME_CONFLICT
            } else {
                OUTCOME_MOVED
            }
            .into(),
        ),
        tail: Some(tail),
        batch,
        ..JobEnd::default()
    }
}

/// The checks of a merge onto the release branch `release` (TER-607),
/// fetched as `refs/remotes/job/next`, with `next` as
/// `refs/remotes/job/upstream`: the job has one card with a tested commit,
/// that commit is on `next` (its passed merge onto `next` took it), and
/// it brings nothing into the release that `next` had before the fix.
///
/// The last rule (review round 3): let M be the oldest commit of the
/// first-parent line of `next` that holds the tested commit (the merge of
/// the fix). No commit of `release..tested` may be in `release..M^1`, the
/// history of `next` before the fix landed. So a fix branch made from
/// `next` after the cut, or stacked on (or merging) a card that merged
/// onto `next` before the fix, is refused: it would take unreleased
/// cards into the frozen version. A fix that merges a card branch that
/// is not on `next` yet cannot be told apart from the fix's own commits.
///
/// M must be the fix's own merge (TER-637): its second parent is the
/// tested commit, as [`git::JobRepo::merge`] writes it. A passed merge
/// onto `next` of a commit that `next` held already makes no merge
/// commit, so M is then the merge of another card, and `M^1` does not
/// hold that card's commits: such a fix is refused, as is an M with no
/// first parent.
fn release_fix(repo: &git::JobRepo, job: &Job, release: &str) -> Result<(), String> {
    let onto = job.onto();
    let cards = job.cards();
    let [card] = cards.as_slice() else {
        return Err(format!("a merge onto `{onto}` takes one card"));
    };
    let Some(want) = &card.commit else {
        return Err(format!(
            "card {} has no tested commit: merge it onto next first",
            card.card
        ));
    };
    let upstream = "refs/remotes/job/upstream";
    let tested = match repo.commit_of(want) {
        Ok(Some(c)) => c,
        Ok(None) => return Err(format!("the tested commit {want} is not on next")),
        Err(e) => return Err(e.to_string()),
    };
    match repo.is_ancestor(&tested, upstream) {
        Ok(true) => {}
        Ok(false) => return Err(format!("the tested commit {want} is not on next")),
        Err(e) => return Err(e.to_string()),
    }
    let foreign = brought_from_next(repo, release, &tested, upstream)?;
    if !foreign.is_empty() {
        let shown: Vec<&str> = foreign.iter().take(5).map(String::as_str).collect();
        return Err(format!(
            "branch `{}` is not based on `{onto}`: its commit {want} brings {} commit(s) that \
             were on next before the fix and are not in the release ({}). Make the fix \
             branch from `{onto}`, with no other card in it",
            card.branch,
            foreign.len(),
            shown.join(", ")
        ));
    }
    Ok(())
}

/// The commits of `release..tested` that `next` (`upstream`) held before
/// the merge of `tested` (see [`release_fix`]), newest first. M, the
/// oldest first-parent commit of `upstream` that holds `tested`, is found
/// by a binary search: along the first-parent line, once a commit holds
/// `tested`, each newer one does too. An M that is not the merge of
/// `tested` (no first parent, or a second parent that is not `tested`)
/// is an error: the check cannot tell what `next` held before the fix.
fn brought_from_next(
    repo: &git::JobRepo,
    release: &str,
    tested: &str,
    upstream: &str,
) -> Result<Vec<String>, String> {
    let git = |e: git::GitError| e.to_string();
    // Newest first, so the commits that hold `tested` are a prefix.
    let line = repo.rev_list(upstream, release, true).map_err(git)?;
    let (mut lo, mut hi) = (0, line.len());
    while lo < hi {
        let mid = lo + (hi - lo) / 2;
        if repo.is_ancestor(tested, &line[mid]).map_err(git)? {
            lo = mid + 1;
        } else {
            hi = mid;
        }
    }
    // `line[lo - 1]` is M. Without one, `tested` is in the release already.
    let Some(m) = lo.checked_sub(1).map(|i| &line[i]) else {
        return Ok(Vec::new());
    };
    let Some(before) = repo.commit_of(&format!("{m}^1")).map_err(git)? else {
        return Err(format!(
            "the merge {m} that took the tested commit {tested} onto next has no first \
             parent: cannot check what next held before the fix"
        ));
    };
    let own = repo.commit_of(&format!("{m}^2")).map_err(git)?;
    if own.as_deref() != Some(tested) {
        return Err(format!(
            "the tested commit {tested} came onto next with commit {m}, which is not its own \
             merge: next held it already when its merge onto next passed (that merge made no \
             commit). Make the fix branch from the release branch, with a commit of its own, \
             and merge it onto next again"
        ));
    }
    let had: HashSet<String> = repo
        .rev_list(&before, release, false)
        .map_err(git)?
        .into_iter()
        .collect();
    Ok(repo
        .rev_list(tested, release, false)
        .map_err(git)?
        .into_iter()
        .filter(|c| had.contains(c))
        .collect())
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
        // The cards of the merge, in order: the job's, then the rest of a
        // batch, each with its reviewed commit (TER-553).
        let cards = job.cards();
        let branches: Vec<String> = cards.iter().map(|c| c.branch.clone()).collect();
        // `next`, or the release branch of `card merge --onto` (TER-607).
        let onto = job.onto();
        let release = onto != jobs::NEXT;
        let mut specs = vec![format!("+refs/heads/{onto}:refs/remotes/job/next")];
        if release {
            // The tested commit is on `next`, so the card branch is not
            // fetched; `next` also shows the cards that the release must
            // not take.
            specs.push("+refs/heads/next:refs/remotes/job/upstream".to_owned());
        } else {
            specs.extend(
                branches
                    .iter()
                    .enumerate()
                    .map(|(i, b)| format!("+refs/heads/{b}:refs/remotes/job/card{i}")),
            );
        }
        // A card branch that is gone (deleted after the accept) fails
        // the whole fetch (TER-614 review, major 1): then `next` alone,
        // and each branch on its own. A branch that does not come leaves
        // the merge with the reason; the others go on. A release job
        // fetches no card branch, so a failed fetch fails it.
        let mut gone: HashMap<usize, String> = HashMap::new();
        if let Err(e) = repo.fetch(&specs) {
            if release {
                return failed(format!(
                    "cannot fetch {onto} and next (does the repo have a {onto} branch?): {e}"
                ));
            }
            if let Err(e) = repo.fetch(&specs[..1]) {
                return failed(format!(
                    "cannot fetch next and {} (does the repo have a next branch?): {e}",
                    branches.join(" ")
                ));
            }
            for (i, spec) in specs.iter().skip(1).enumerate() {
                if let Err(one) = repo.fetch(std::slice::from_ref(spec)) {
                    gone.insert(i, one.to_string());
                }
            }
            if gone.is_empty() {
                return failed(format!("cannot fetch next and {}: {e}", branches.join(" ")));
            }
        }
        let mut cur = match repo.commit_of("refs/remotes/job/next") {
            Ok(Some(c)) => c,
            Ok(None) => return failed(format!("the repo has no {onto} branch")),
            Err(e) => return failed(e.to_string()),
        };
        base = Some(cur.clone());
        if release && let Err(why) = release_fix(repo, job, &cur) {
            return failed(why);
        }
        // A branch that conflicts, or whose merge fails otherwise, leaves
        // the merge; the next branch merges onto the ones before it. So
        // does a branch whose head is no longer its reviewed commit
        // (TER-553): a push after the accept was not reviewed.
        let mut left: HashMap<usize, Left> = HashMap::new();
        let mut merged = 0;
        for (i, b) in branches.iter().enumerate() {
            // The ref of a branch that did not come may be one of an
            // older job of this repo: it is not read.
            if let Some(e) = gone.get(&i) {
                left.insert(i, Left::unfetched(b, e));
                continue;
            }
            // Onto a release branch, the tested commit that
            // [`release_fix`] checked, never the branch head: a push after
            // the merge onto `next` does not count (TER-607).
            let fetched = match &cards[i].commit {
                Some(c) if release => c.clone(),
                _ => format!("refs/remotes/job/card{i}"),
            };
            let tip = match repo.commit_of(&fetched) {
                Ok(Some(c)) => c,
                Ok(None) => {
                    left.insert(i, Left::unfetched(b, "the fetch brought no commit"));
                    continue;
                }
                Err(e) => return failed(e.to_string()),
            };
            if let Some(want) = cards[i].commit.as_ref().filter(|_| !release) {
                // A reviewed commit that the fetch did not bring (the
                // branch was rewritten) is no head either.
                let reviewed = repo.commit_of(want).ok().flatten();
                if reviewed.as_deref() != Some(tip.as_str()) {
                    left.insert(i, Left::moved(b, want, &tip));
                    continue;
                }
            }
            // The commit, not the ref: the merge takes what was checked.
            match repo.merge(&cur, &tip, &format!("Merge {b} onto {onto}")) {
                Ok(git::MergeStep::Merged(c)) => {
                    cur = c;
                    merged += 1;
                }
                Ok(git::MergeStep::Conflict(files)) => {
                    left.insert(i, Left::conflict(files.join("\n")));
                }
                Ok(git::MergeStep::Failed(msg)) => {
                    left.insert(
                        i,
                        Left::conflict(format!("the merge failed without a conflict:\n{msg}")),
                    );
                }
                Err(e) => {
                    left.insert(i, Left::conflict(format!("the merge of `{b}` failed: {e}")));
                }
            }
        }
        if !job.batch.is_empty() {
            batch = Some(mark_left(&job.batch, &left));
        }
        if merged == 0 {
            return Prepared::End(none_merged(job, left, batch));
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
        // A release (TER-623) tests and builds its commit on `main`: no
        // list of changed files.
        if job.kind == JobKind::Release {
            if job.commit.is_none() {
                return failed("a release job needs its commit on main".into());
            }
            base = None;
            return bundle_ready(repo, refname, bundle, head, batch, base);
        }
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
    bundle_ready(repo, refname, bundle, head, batch, base)
}

/// Write the job ref and its bundle: the end of [`prepare`].
fn bundle_ready(
    repo: &git::JobRepo,
    refname: &str,
    bundle: &std::path::Path,
    head: String,
    batch: Option<Vec<BatchCard>>,
    base: Option<String>,
) -> Prepared {
    if let Err(e) = repo
        .set_ref(refname, &head)
        .and_then(|()| repo.bundle(refname, bundle))
    {
        return Prepared::End(JobEnd {
            passed: false,
            outcome: Some("error".into()),
            tail: Some(format!(
                "the prepare step failed: cannot write the bundle: {e}"
            )),
            ..JobEnd::default()
        });
    }
    Prepared::Ready { head, batch, base }
}

/// Prepare in the daemon, clone on the target from the bundle, run the
/// recipe there, and push `next` from the daemon for a merge.
async fn steps(
    x: &mut StepCtx<'_>,
    recipe: String,
    delivery: &DeliveryDef,
    guard: tokio::sync::OwnedMutexGuard<()>,
) -> ApiResult<JobEnd> {
    let job = x.job;
    if job.kind == JobKind::Merge {
        for c in job.cards().iter().skip(1) {
            check_ref(&c.branch)?;
        }
        for c in job.cards() {
            if let Some(commit) = &c.commit {
                check_commit(commit)?;
            }
        }
    }
    x.at(STEP_PREPARE);
    let guard = Arc::new(guard);
    let mut prepared = None;
    for attempt in 0..3 {
        let (j, bundle, prepare_guard) = (job.clone(), x.bundle.clone(), guard.clone());
        let (result, damaged) = x
            .daemon(move |r| {
                let _guard = prepare_guard;
                #[cfg(test)]
                tests::prepare_fault(r);
                let result = prepare(r, &j, &bundle);
                let damaged = matches!(result, Prepared::End(_)) && r.needs_rebuild();
                (result, damaged)
            })
            .await?;
        if !damaged || attempt == 2 {
            prepared = Some(result);
            break;
        }
        // No blocking user remains here. New users cannot prepare while we
        // retain the mutex. Existing jobs can finish before the rebuild.
        x.lifetime.take();
        let lifetime = x.repo_locks.lifetime.clone().write_owned().await;
        let repo = x.repo.clone();
        let prepare_guard = guard.clone();
        let (repo, lifetime) = tokio::task::spawn_blocking(move || {
            let _guard = prepare_guard;
            repo.rebuild().map(|r| (Arc::new(r), lifetime.downgrade()))
        })
        .await
        .map_err(|e| ApiError::internal(format!("the git task of the job failed: {e}")))?
        .map_err(|e| ApiError::conflict(format!("cannot rebuild the job repo: {e}")))?;
        x.repo = repo;
        x.lifetime = Some(Arc::new(lifetime));
    }
    drop(guard);
    let (head, batch, base) =
        match prepared.ok_or_else(|| ApiError::internal("the prepare loop did not run"))? {
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
    // A release starts on a new tree (TER-623): nothing of an older
    // release is in it.
    let wipe = if x.wipe || job.kind == JobKind::Release {
        "wipe"
    } else {
        ""
    };
    let receive = x.plain_cmd(
        RECEIVE_SCRIPT,
        &[x.dir, &head, job.kind.as_str(), &base, &tree, wipe],
    );
    x.at(STEP_RECEIVE);
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
    let log = format!(
        "{}/{}/logs/{}-{}.log",
        delivery.work_dir().trim_end_matches('/'),
        job.project,
        job.id,
        job.attempt
    );
    if job.kind == JobKind::Merge {
        let card = job.card.clone();
        if let Err(e) = x
            .state
            .db
            .write("system", move |t| {
                crate::store::groups::merge_recipe_started(t, &card)
            })
            .await
        {
            warn!(job = %job.id, "cannot update merge group recipe boundary: {e}");
        }
    }
    x.at(STEP_SECRETS);
    let injected = merge_secrets(x.state, job, delivery).await?;
    let release = release_secrets(x.state, job, delivery).await?;
    let own: Vec<_> = injected
        .iter()
        .chain(release.env.iter())
        .chain(release.key.iter())
        .map(|i| (i.label(), i.value.clone()))
        .collect();
    let mut env = recipe_env(job, x.dir, &head, base);
    let tunnel = if let Some(key) = injected.iter().find(|i| i.env == "DOCKER_CI_KEY") {
        let reset = injected
            .iter()
            .find(|i| i.env == "DOCKER_CI_RESET_KEY")
            .ok_or_else(|| ApiError::invalid("Docker tunnel requires DOCKER_CI_RESET_KEY"))?;
        let config = delivery.docker_ci.as_ref().ok_or_else(|| {
            ApiError::invalid("merge_secrets requires docker_ci endpoint configuration")
        })?;
        crate::docker_tunnel::reset(x.state, &reset.value, config).await?;
        let tunnel = crate::docker_tunnel::open(
            x.state,
            x.def,
            &key.value,
            x.dir,
            delivery.timeout_secs(),
            config.into(),
        )
        .await?;
        // Never `DOCKER_HOST` here: see [`crate::docker_tunnel::ENDPOINT_ENV`].
        env.insert(
            crate::docker_tunnel::ENDPOINT_ENV.into(),
            tunnel.endpoint.clone(),
        );
        Some(tunnel)
    } else if let Some(key) = &release.key {
        // TER-623: the release VM, with no reset. The key stays here.
        let config = delivery.docker_release.as_ref().ok_or_else(|| {
            ApiError::invalid("a release key requires the docker_release endpoint")
        })?;
        let tunnel = crate::docker_tunnel::open(
            x.state,
            x.def,
            &key.value,
            x.dir,
            delivery.timeout_secs(),
            config.into(),
        )
        .await?;
        env.insert(
            crate::docker_tunnel::ENDPOINT_ENV.into(),
            tunnel.endpoint.clone(),
        );
        Some(tunnel)
    } else {
        None
    };
    // TER-623: the release secrets, in the env of the release recipe only.
    // The executor sends the env on stdin, never on a command line.
    for i in &release.env {
        env.insert(i.env.clone(), i.value.to_string());
    }
    drop(release);
    // Credentials stay in the daemon, so retain the normal target recipe log.
    let script = recipe_script(recipe, base);
    let script = if job.kind == JobKind::Release {
        release_home(&script)
    } else {
        script
    };
    let recipe_cmd = RemoteCmd {
        argv: vec!["sh".into(), "-c".into(), logged_recipe(script, &log)],
        cwd: Some(x.dir.to_owned()),
        env,
        timeout_secs: Some(delivery.timeout_secs()),
        holder: x.holder.clone(),
        ..RemoteCmd::default()
    };
    x.at(STEP_RECIPE);
    // An error here is a recipe that did not start: no session.
    if killed(&job.id) {
        return Err(ApiError::conflict("the job was cancelled"));
    }
    let recipe_cmd = RemoteCmd {
        argv: low_priority(recipe_cmd.argv.clone(), x.ionice),
        ..recipe_cmd
    };
    let c = step_redacted(x.state, &job.id, x.def, &recipe_cmd, RECIPE_MAX, None, &own).await?;
    if let Some(tunnel) = tunnel {
        tunnel.close().await;
    }
    x.recipe_started.store(true, Ordering::SeqCst);
    let ok = c.end.as_ref().is_some_and(End::success);
    // TER-196: a run that the infrastructure lost is no red; [`run`]
    // puts the job back in the queue. A cancel is never lost.
    let lost = if ok || killed(&job.id) {
        None
    } else {
        lost_run(c.end.as_ref(), &c.stderr, x.ssh_host.as_deref())
    };
    // The recipe group is gone now, even after timeout or cancel. Read its
    // log in a separate, bounded session that is not controlled by job.kill.
    let excerpt = if lost.is_none() {
        let cmd = RemoteCmd {
            argv: low_priority(
                vec![
                    "sh".into(),
                    "-c".into(),
                    log_excerpt(&log, delivery.failure_patterns.as_deref()),
                ],
                x.ionice,
            ),
            timeout_secs: Some(30),
            holder: x.holder.clone(),
            ..RemoteCmd::default()
        };
        match crate::executor::start_cmd(x.state, x.def, &cmd) {
            Ok(run) => {
                // The log may hold what the recipe printed: the same
                // redaction as the recipe output (TER-623).
                let run = crate::executor::redact_run(
                    run,
                    crate::executor::own_redactor(&own),
                    x.state.db.redactor().get(),
                    terrarium_core::redact::View::All,
                );
                output_of(&run.collect(RECIPE_MAX).await)
            }
            Err(e) => format!("[cannot read retained log: {e}]"),
        }
    } else {
        String::new()
    };
    let log_location = format!(
        "Full log on target {}: {}",
        delivery.target.as_deref().unwrap_or("-"),
        log
    );
    let mut tail = format!(
        "{}\n{}\n[{}]\n[run as: {} sh -c <recipe>]\n{}",
        excerpt.trim_end(),
        output_of(&c).trim_end(),
        end_text(c.end.as_ref()),
        low_priority(Vec::new(), x.ionice).join(" "),
        log_location
    );
    if let Some(b) = &batch {
        tail.push_str(&format!("\n[{}]", batch_summary(b)));
    }
    if let Some(why) = lost {
        let _ = x.recipe_lost.set(why);
    }
    let result = JobEnd {
        passed: ok,
        outcome: (!ok).then(|| {
            if x.recipe_lost.get().is_some() {
                "error".to_owned()
            } else {
                "red".to_owned()
            }
        }),
        exit_code: exit_code(c.end.as_ref()),
        tail: Some(tail),
        head: Some(head.clone()),
        batch: batch.clone(),
    };
    if !ok || job.kind != JobKind::Merge {
        return Ok(result);
    }
    x.at(STEP_PUSH);
    // Step 3, in the daemon, with the credential: push the recorded merge
    // commit from the daemon's repo. The recipe never had that repo, so
    // nothing it did to its clone reaches the push. The shared scratch ref
    // may belong to a newer prepare, so check the recorded commit itself.
    let h = head.clone();
    let onto = job.onto().to_owned();
    let to = onto.clone();
    let pushed = x
        .daemon(move |r| match r.commit_of(&h) {
            Ok(Some(c)) if c == h => r.push(&h, &to),
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
                "the recipe passed, but the push to `{onto}` failed; did `{onto}` move?\n{e}"
            )),
            head: Some(head),
            batch,
            ..JobEnd::default()
        }),
    }
}

/// Resolve only the project's delivery principal, only for cut-time e2e.
async fn merge_secrets(
    state: &Shared,
    job: &Job,
    delivery: &DeliveryDef,
) -> ApiResult<Vec<secrets::run_env::Injected>> {
    if job.kind != JobKind::E2e || delivery.merge_secrets.is_empty() {
        return Ok(Vec::new());
    }
    check_merge_refs(delivery)?;
    let refs = secrets::run_env::parse_refs(&delivery.merge_secrets)?;
    let key = secrets::key(&state.state_dir)?;
    let project = job.project.clone();
    let target = delivery.target.clone().unwrap_or_default();
    let proxy_on = state.git_proxy.is_on();
    let injected = state
        .db
        .read(move |c| {
            let use_only = crate::git_proxy::use_only_secrets(c, proxy_on)?;
            secrets::run_env::resolve_delivery(c, &key, &project, &target, &refs, &use_only)
        })
        .await?;
    let audits: Vec<_> = injected
        .iter()
        .map(|i| {
            (
                format!("{}/{}", i.scope, i.name),
                serde_json::json!({"job": job.id, "project": job.project,
            "target": delivery.target, "field": i.field, "env": i.env,
            "principal": format!("delivery:{}", job.project), "level": "run"}),
            )
        })
        .collect();
    let project = job.project.clone();
    state
        .db
        .write("system", move |t| {
            for (object, detail) in audits {
                t.audit("secret.run", &object, Some(&project), detail)?;
            }
            Ok(())
        })
        .await?;
    Ok(injected)
}

fn check_merge_refs(delivery: &DeliveryDef) -> ApiResult<()> {
    if !delivery.merge_secrets.is_empty() && delivery.docker_ci.is_none() {
        return Err(ApiError::invalid(
            "merge_secrets requires docker_ci with separate tunnel and reset pins",
        ));
    }
    if let Some(config) = &delivery.docker_ci {
        for destination in [&config.destination, &config.reset_destination] {
            if destination.is_empty()
                || destination.starts_with('-')
                || destination.chars().any(char::is_whitespace)
            {
                return Err(ApiError::invalid(
                    "Docker CI SSH destinations must be nonempty user@host values, not options",
                ));
            }
        }
        if config.port == 0
            || config.reset_port == 0
            || config.known_hosts.trim().is_empty()
            || config.reset_known_hosts.trim().is_empty()
        {
            return Err(ApiError::invalid(
                "Docker CI requires nonzero SSH ports and a pinned known_hosts line",
            ));
        }
    }
    for name in delivery.merge_secrets.keys() {
        secrets::grants::check_env_name(name)?;
        if name != "DOCKER_CI_KEY" && name != "DOCKER_CI_RESET_KEY" {
            return Err(ApiError::invalid(
                "merge_secrets supports only DOCKER_CI_KEY and DOCKER_CI_RESET_KEY, consumed by the daemon, never recipe env",
            ));
        }
    }
    if !delivery.merge_secrets.is_empty() && delivery.merge_secrets.len() != 2 {
        return Err(ApiError::invalid(
            "Docker tunnel requires both DOCKER_CI_KEY and DOCKER_CI_RESET_KEY",
        ));
    }
    let refs = secrets::run_env::parse_refs(&delivery.merge_secrets)?;
    if refs.len() != delivery.merge_secrets.len() {
        return Err(ApiError::invalid(
            "merge_secrets accepts only @secret references",
        ));
    }
    Ok(())
}

/// The test holds of a target write (review-ter-623-r4 F2), by target
/// name: the PATCH or DELETE of that target sends on `ready` after its read
/// and waits for `go` before its write.
type WritePause = (
    tokio::sync::oneshot::Sender<()>,
    tokio::sync::oneshot::Receiver<()>,
);

fn write_pauses() -> MutexGuard<'static, HashMap<String, WritePause>> {
    static P: OnceLock<Mutex<HashMap<String, WritePause>>> = OnceLock::new();
    match P.get_or_init(Mutex::default).lock() {
        Ok(m) => m,
        Err(p) => p.into_inner(),
    }
}

/// Hold the next PATCH or DELETE of target `name` between its read and its
/// write. For the tests only. Returns the `ready` receiver (the request
/// reached the hold) and the `go` sender (let it write).
#[doc(hidden)]
pub fn pause_next_target_write(
    name: &str,
) -> (
    tokio::sync::oneshot::Receiver<()>,
    tokio::sync::oneshot::Sender<()>,
) {
    let (ready_tx, ready_rx) = tokio::sync::oneshot::channel();
    let (go_tx, go_rx) = tokio::sync::oneshot::channel();
    write_pauses().insert(name.to_owned(), (ready_tx, go_rx));
    (ready_rx, go_tx)
}

/// The hold of [`pause_next_target_write`], if a test set one for `name`.
pub(crate) async fn target_write_pause(name: &str) {
    let hold = write_pauses().remove(name);
    if let Some((ready, go)) = hold {
        let _ = ready.send(());
        let _ = go.await;
    }
}

/// The env names that `release_secrets` may set (TER-623): the secrets of
/// `scripts/release.py`, and no other name.
pub const RELEASE_SECRET_ENV: &[&str] = &[
    "GHCR_TOKEN",
    "GH_TOKEN",
    "COSIGN_PRIVATE_KEY",
    "COSIGN_PASSWORD",
];

/// The env name under which the release VM key is resolved. The daemon
/// uses it for the tunnel; it is never recipe env.
const RELEASE_KEY_ENV: &str = "DOCKER_RELEASE_KEY";

/// What a release job resolved (TER-623): the recipe env, and the key of
/// the release VM that only the daemon uses. `Debug` of each shows no value.
#[derive(Default)]
struct ReleaseSecrets {
    env: Vec<secrets::run_env::Injected>,
    key: Option<secrets::run_env::Injected>,
}

/// Resolve the release secrets and the release VM key (TER-623): only for
/// a release job, only on the release target. `delivery` is the job's
/// [`DeliveryDef::release_view`], so its `target` is the release target;
/// the delivery grant of each secret must name it ([`secrets::run_env::resolve_delivery`]),
/// else the job fails here, before the recipe. Each value read is audited
/// by env name and secret, never by value.
async fn release_secrets(
    state: &Shared,
    job: &Job,
    delivery: &DeliveryDef,
) -> ApiResult<ReleaseSecrets> {
    if job.kind != JobKind::Release {
        return Ok(ReleaseSecrets::default());
    }
    let target = delivery
        .release_target()
        .filter(|t| delivery.target.as_deref() == Some(*t))
        .ok_or_else(|| ApiError::invalid("a release job runs only on the release target"))?
        .to_owned();
    check_release_refs(delivery)?;
    let mut refs_env = delivery.release_secrets.clone();
    if let Some(d) = &delivery.docker_release {
        refs_env.insert(RELEASE_KEY_ENV.into(), d.key.clone());
    }
    if refs_env.is_empty() {
        return Ok(ReleaseSecrets::default());
    }
    let refs = secrets::run_env::parse_refs(&refs_env)?;
    let key = secrets::key(&state.state_dir)?;
    let project = job.project.clone();
    let proxy_on = state.git_proxy.is_on();
    let t = target.clone();
    let injected = state
        .db
        .read(move |c| {
            let use_only = crate::git_proxy::use_only_secrets(c, proxy_on)?;
            secrets::run_env::resolve_delivery(c, &key, &project, &t, &refs, &use_only)
        })
        .await?;
    let audits: Vec<_> = injected
        .iter()
        .map(|i| {
            (
                format!("{}/{}", i.scope, i.name),
                serde_json::json!({"job": job.id, "project": job.project, "kind": "release",
                    "target": target, "field": i.field, "env": i.env,
                    "principal": format!("delivery:{}", job.project), "level": "run"}),
            )
        })
        .collect();
    let project = job.project.clone();
    state
        .db
        .write("system", move |t| {
            for (object, detail) in audits {
                t.audit("secret.run", &object, Some(&project), detail)?;
            }
            Ok(())
        })
        .await?;
    let mut out = ReleaseSecrets::default();
    for i in injected {
        if i.env == RELEASE_KEY_ENV {
            out.key = Some(i);
        } else {
            out.env.push(i);
        }
    }
    Ok(out)
}

/// The checks of the release settings that need no database (TER-623):
/// the secret names and references, and the release VM endpoint.
fn check_release_refs(delivery: &DeliveryDef) -> ApiResult<()> {
    let has_release = !delivery.release_secrets.is_empty()
        || delivery.docker_release.is_some()
        || delivery.release_recipe.is_some();
    if has_release && delivery.release_target().is_none() {
        return Err(ApiError::invalid(
            "release_secrets, release_recipe and docker_release require release_target",
        ));
    }
    for (name, value) in &delivery.release_secrets {
        secrets::grants::check_env_name(name)?;
        if !RELEASE_SECRET_ENV.contains(&name.as_str()) {
            return Err(ApiError::invalid(format!(
                "release_secrets sets only {}, not `{name}`",
                RELEASE_SECRET_ENV.join(", ")
            )));
        }
        if !value.starts_with(secrets::run_env::REF_PREFIX) {
            return Err(ApiError::invalid(format!(
                "release_secrets `{name}` must be an @secret reference"
            )));
        }
    }
    secrets::run_env::parse_refs(&delivery.release_secrets)?;
    if let Some(d) = &delivery.docker_release {
        if d.destination.is_empty()
            || d.destination.starts_with('-')
            || d.destination.chars().any(char::is_whitespace)
        {
            return Err(ApiError::invalid(
                "the docker_release SSH destination must be a nonempty user@host value, not options",
            ));
        }
        if d.port == 0 || d.known_hosts.trim().is_empty() {
            return Err(ApiError::invalid(
                "docker_release requires a nonzero SSH port and a pinned known_hosts line",
            ));
        }
        let key = BTreeMap::from([(RELEASE_KEY_ENV.to_owned(), d.key.clone())]);
        if !d.key.starts_with(secrets::run_env::REF_PREFIX)
            || secrets::run_env::parse_refs(&key)?.len() != 1
        {
            return Err(ApiError::invalid(
                "the docker_release key must be an @secret reference",
            ));
        }
    }
    Ok(())
}

/// The release config belongs to the operator (review-ter-623-r2 finding
/// 1): `release_target`, `release_recipe`, `release_secrets` and
/// `docker_release` decide what runs with the release secrets and where.
/// Any other actor (a project admin, the lead, a sysadmin agent) may set
/// the other fields, and must leave these four as they are stored.
fn check_release_owner(
    conn: &rusqlite::Connection,
    actor: &Actor,
    project: &str,
    def: &DeliveryDef,
) -> ApiResult<()> {
    if actor.is_operator() {
        return Ok(());
    }
    let old = jobs::delivery(conn, project)?;
    let fields = [
        ("release_target", old.release_target != def.release_target),
        ("release_recipe", old.release_recipe != def.release_recipe),
        (
            "release_secrets",
            old.release_secrets != def.release_secrets,
        ),
        ("docker_release", old.docker_release != def.docker_release),
    ];
    let changed: Vec<&str> = fields.iter().filter(|(_, c)| *c).map(|(f, _)| *f).collect();
    if changed.is_empty() {
        return Ok(());
    }
    Err(ApiError::forbidden(format!(
        "changing {} of `{project}`: the release config is the operator's (TER-623): only the \
         operator sets what runs with the release secrets and where; a project admin may \
         start a release with it",
        changed.join(", ")
    )))
}

/// The checks of the release target (TER-623, D2): the target of this
/// project (or a global one set by a sysadmin) that runs release jobs
/// only. It is not the job target of any project, and not the release
/// target of another project; and the job target of this project is not
/// the release target of another project.
fn check_release_target(
    conn: &rusqlite::Connection,
    project: &str,
    def: &DeliveryDef,
) -> ApiResult<()> {
    let mut others: Vec<(String, DeliveryDef)> = Vec::new();
    let mut st = conn.prepare("SELECT project FROM project_delivery WHERE project != ?1")?;
    let names = st
        .query_map([project], |r| r.get::<_, String>(0))?
        .collect::<Result<Vec<_>, _>>()?;
    for p in names {
        let d = jobs::delivery(conn, &p)?;
        others.push((p, d));
    }
    let machine = |t: &str| jobs::machine_of(conn, t);
    if let Some(target) = def.target.as_deref().filter(|t| !t.is_empty()) {
        let host = machine(target);
        if let Some((p, r)) = others.iter().find_map(|(p, d)| {
            d.release_target()
                .filter(|r| *r == target || machine(r) == host)
                .map(|r| (p, r))
        }) {
            return Err(ApiError::conflict(format!(
                "target `{target}` is, or is on the machine of, `{r}`, the release target of project `{p}`: it runs release jobs only"
            )));
        }
        // review-ter-623-r4 F1: also a protected (earlier) release target
        // and its machine, also when this config has no release target.
        jobs::check_not_release_target(conn, target, "running the job queue on the target")?;
    }
    let Some(release) = def.release_target() else {
        return Ok(());
    };
    let host = machine(release);
    if let Some(t) = def
        .target
        .as_deref()
        .filter(|t| *t == release || machine(t) == host)
    {
        return Err(ApiError::invalid(format!(
            "release_target `{release}` is, or is on the machine of, the job target `{t}`: the release target runs release jobs only"
        )));
    }
    for (p, d) in &others {
        let shared = [d.target.as_deref(), d.release_target()]
            .into_iter()
            .flatten()
            .find(|t| *t == release || machine(t) == host);
        if let Some(t) = shared {
            return Err(ApiError::conflict(format!(
                "release_target `{release}` is, or is on the machine of, `{t}`, a job or release target of project `{p}`: the release target runs release jobs only"
            )));
        }
    }
    // F1 (review-ter-623): no agent may hold the release target already.
    let mut st = conn.prepare("SELECT project, name, policy FROM agents ORDER BY project, name")?;
    let agents = st
        .query_map([], |r| {
            Ok((
                r.get::<_, String>(0)?,
                r.get::<_, String>(1)?,
                r.get::<_, String>(2)?,
            ))
        })?
        .collect::<Result<Vec<_>, _>>()?;
    for (p, n, policy) in agents {
        let policy = crate::store::json_col::<terrarium_core::config::Policy>(&policy);
        if policy.targets.iter().any(|t| t == release) {
            return Err(ApiError::conflict(format!(
                "release_target `{release}` is in the policy.targets of agent {p}/{n}: remove it first; a release target runs release jobs only"
            )));
        }
    }
    if let Some(l) = crate::store::leases::active_on(conn, release)?.first() {
        return Err(ApiError::conflict(format!(
            "release_target `{release}` has an active lease of agent {}: revoke it first; a release target runs release jobs only",
            l.agent
        )));
    }
    // review-ter-623-r2 finding 4: no tool runs on it or on its machine.
    for tool in crate::store::tools::list(conn)? {
        let t = &tool.def.target;
        if t == release || machine(t) == host {
            return Err(ApiError::conflict(format!(
                "release_target `{release}` is, or is on the machine of, `{t}`, the target of tool `{}`: remove the tool first; a release target runs release jobs only",
                tool.def.name
            )));
        }
    }
    // review-ter-623-r2 finding 2: a global target, which only the operator
    // defines (and patches or deletes, `api::targets`).
    let row = targets::get(conn, release)?;
    if let Some(p) = &row.def.project {
        return Err(ApiError::invalid(format!(
            "release_target `{release}` belongs to project `{p}`: a release target is a global target that the operator defines"
        )));
    }
    executor::no_exec(&row.def)?;
    Ok(())
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

/// The release recipe in a new HOME (TER-623, review-ter-623 F1): a 0700
/// dir from `mktemp -d`, removed when the recipe ends, with the XDG dirs in
/// it, and no global or system git config. So no `~/.docker`,
/// `~/.gitconfig`, `~/.config` or cache that was left in the release
/// user's home is read by the release. `XDG_RUNTIME_DIR` stays (the RAM dir
/// of `scripts/release.py`). It first removes the HOMEs that a killed
/// release left (the EXIT trap did not run), and sets [`RELEASE_PATH`].
fn release_home(recipe: &str) -> String {
    format!(
        r#"tmp=${{TMPDIR:-/tmp}}
# Leftovers of a release that was killed before its EXIT trap: one release
# runs at a time on the release target.
for old in "$tmp"/terrarium-release-home.*; do
  [ -d "$old" ] && [ ! -L "$old" ] && [ -O "$old" ] && rm -rf -- "$old"
done
export PATH={path}
home=$(mktemp -d "$tmp/terrarium-release-home.XXXXXXXX") || {{ echo "cannot make the release HOME"; exit 125; }}
trap 'rm -rf -- "$home"' EXIT
trap 'exit 143' TERM INT HUP
export HOME="$home" XDG_CONFIG_HOME="$home/.config" XDG_CACHE_HOME="$home/.cache" XDG_DATA_HOME="$home/.local/share" XDG_STATE_HOME="$home/.local/state" GIT_CONFIG_NOSYSTEM=1 GIT_CONFIG_GLOBAL=/dev/null
unset DOCKER_CONFIG CARGO_HOME RUSTUP_HOME UV_CACHE_DIR npm_config_cache
sh -c {recipe}
"#,
        path = RELEASE_PATH,
        recipe = crate::executor::sh_quote(recipe)
    )
}

/// The `PATH` of a release recipe (review-ter-623-r2 finding 6): the system
/// dirs only, nothing under the release user's home. The release tools are
/// installed system-wide on the release client.
pub const RELEASE_PATH: &str = "/usr/local/sbin:/usr/local/bin:/usr/sbin:/usr/bin:/sbin:/bin";

/// Keep merged stdout/stderr outside the reusable slot, so the next checkout
/// cannot clean it away. Only a bounded excerpt crosses the runner protocol.
/// No pipeline wraps the recipe: its exact exit status remains the job status.
///
/// The slot's tree stays this job's alone (TER-448): the script holds the
/// receive step's lock (`TERRARIUM_SLOT_LOCK`) for the whole recipe, and a
/// head that is not `TERRARIUM_HEAD` (another daemon took the slot between
/// the two steps and replaced the tree) ends the job before the recipe
/// runs. Both fail with exit 125 and the reason on stderr; without `flock`
/// on the target, the recipe runs as before this, unlocked, and the step
/// says so in one line.
///
/// Only this wrapper shell holds the lock (fd 8). The recipe runs as its
/// child with fd 8 closed, so a process that the recipe leaves behind (a
/// background server, a build daemon) cannot keep the slot locked after
/// the job. The wrapper catches TERM, INT and HUP with a no-op trap (a
/// caught signal goes back to its default in the child), so on a cancel it
/// holds the lock until its recipe child ends; a SIGKILL of the group ends
/// both. With the lock, a recipe that a signal ends gives exit 128+n, not
/// the signal: both are a red end.
fn logged_recipe(recipe: String, log: &str) -> String {
    use crate::executor::sh_quote;
    format!(
        r#"log={log}
mkdir -p -- "$(dirname -- "$log")" || exit 125
(umask 077 && : > "$log") || exit 125
lock=${{TERRARIUM_SLOT_LOCK:-}}
if [ -z "$lock" ]; then
  exec sh -c {recipe} > "$log" 2>&1
fi
if ! command -v flock >/dev/null 2>&1; then
  echo "no flock on the target: slot not locked"
  exec sh -c {recipe} > "$log" 2>&1
fi
exec 8>>"$lock" || {{ echo "cannot open the slot lock $lock"; exit 125; }}
flock -n 8 || {{
  echo "the slot tree of this job is held by another job or daemon: $lock"
  echo "stop it, or give this project its own work_dir"
  exit 125
}}
if [ -n "${{TERRARIUM_HEAD:-}}" ]; then
  got=$(git rev-parse -q --verify 'HEAD^{{commit}}' 2>/dev/null 8>&-)
  if [ "$got" != "$TERRARIUM_HEAD" ]; then
    echo "another daemon replaced the slot tree between the steps of this job"
    echo "(HEAD is ${{got:-nothing}}, not $TERRARIUM_HEAD); the recipe did not run"
    exit 125
  fi
fi
trap : TERM INT HUP
sh -c {recipe} > "$log" 2>&1 8>&-
exit $?
"#,
        log = sh_quote(log),
        recipe = sh_quote(&recipe),
    )
}

/// Run after the recipe's session ends, including after its group was killed.
fn log_excerpt(log: &str, patterns: Option<&[String]>) -> String {
    use crate::executor::sh_quote;
    let defaults = ["FAILED", "panicked", "test result: FAILED", "failures:"].map(str::to_owned);
    let patterns = patterns.unwrap_or(&defaults);
    let grep = if patterns.is_empty() {
        String::new()
    } else {
        format!(
            "printf 'Failure lines (first 1800 bytes, with context):\\n'; grep -a -F -A 3 {} -- \"$log\" | head -c 1800; printf '\\n'",
            patterns
                .iter()
                .map(|p| format!("-e {}", sh_quote(p)))
                .collect::<Vec<_>>()
                .join(" ")
        )
    };
    format!(
        r#"log={log}
{grep}
printf '\nOutput tail (last 1200 bytes):\n'
tail -c 1200 -- "$log"
"#,
        log = sh_quote(log),
    )
}

/// The env of the recipe (TER-100, docs/api.md 35.2): the job kind and
/// ID, the tested head, the slot (TER-190), the slot's lock file (TER-448),
/// and with a base, the base and the absolute path
/// of the changed files. No credential goes in here.
fn recipe_env(job: &Job, dir: &str, head: &str, base: Option<&str>) -> BTreeMap<String, String> {
    let mut env = BTreeMap::new();
    crate::git_identity::Identity::from_env().env(&mut env);
    env.insert("TERRARIUM_JOB_KIND".into(), job.kind.as_str().to_owned());
    env.insert("TERRARIUM_JOB_ID".into(), job.id.clone());
    env.insert("TERRARIUM_HEAD".into(), head.to_owned());
    if let Some(s) = job.slot {
        env.insert("TERRARIUM_JOB_SLOT".into(), s.to_string());
    }
    // TER-448: the recipe holds this lock, the receive step's lock, for its
    // whole run, so a second daemon on the same work dir cannot take the
    // slot's tree while the recipe runs in it.
    env.insert(
        "TERRARIUM_SLOT_LOCK".into(),
        format!("{}.lock", dir.trim_end_matches('/')),
    );
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
fn mark_left(batch: &[BatchCard], found: &HashMap<usize, Left>) -> Vec<BatchCard> {
    batch
        .iter()
        .enumerate()
        .map(|(i, c)| match found.get(&i) {
            Some(l) => BatchCard {
                outcome: Some(l.outcome.into()),
                note: Some(l.note.clone()),
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
    let with = |o: Option<&str>| -> String {
        batch
            .iter()
            .filter(|c| match o {
                Some(o) => c.outcome.as_deref() == Some(o),
                None => !left_merge(c.outcome.as_deref()),
            })
            .map(|c| c.card.as_str())
            .collect::<Vec<_>>()
            .join(" ")
    };
    let mut s = format!("batch: merged {}", with(None));
    let bad = with(Some(OUTCOME_CONFLICT));
    if !bad.is_empty() {
        s.push_str(&format!("; left out on a conflict {bad}"));
    }
    let moved = with(Some(OUTCOME_MOVED));
    if !moved.is_empty() {
        s.push_str(&format!("; left out, moved after review {moved}"));
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
        .map(|l| format!("    {l}\n"))
        .collect();
    format!(
        "Validation failed: {}, {} job {}, repo {}, branch {}, commit {}, tested {}, exit code {}, outcome {}, attempt {}.\nJob output (failure lines, then tail):\n\n{tail}\n",
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
        JobKind::Validate | JobKind::E2e => format!(
            "Job queue: validate job {} queued. Repo {}, branch {}, commit {}.",
            j.id,
            j.repo,
            j.branch,
            j.commit
                .as_deref()
                .unwrap_or("the branch head at the start")
        ),
        JobKind::Release => format!(
            "Job queue: release job {} queued: {} of {} at commit {}.",
            j.id,
            j.branch,
            j.repo,
            j.commit.as_deref().unwrap_or("-")
        ),
        JobKind::Merge if j.batch.is_empty() => format!(
            "Job queue: merge job {} queued: {} onto {} of {}.",
            j.id,
            j.branch,
            j.onto(),
            j.repo
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

/// The comment of a merge job that failed before its tests
/// ([`OUTCOME_INFRA`], TER-614 R3). `retry`: the wait to its one retry
/// (`5 min`), `None` at the end (the retry failed too).
pub fn infra_comment(v: &Job, retry: Option<&str>) -> String {
    match retry {
        Some(m) => format!(
            "Job queue: {} job {} (attempt {}) failed before its tests: an infra failure, not a test failure. No attempt is used and the batch does not split. The cards stay in the merge queue, and the job runs once more in {m}, when its host is back.",
            v.kind.as_str(),
            v.id,
            v.attempt
        ),
        None => format!(
            "{} The job failed before its tests again: an infra failure, not a test failure. It does not rerun and the batch does not split. The cards stay in the merge queue with no job: they go in the next batch when a card enters the merge queue or the lead queues them (`card merge`), or move them out of the merge queue and back.",
            end_comment(v, "failed")
        ),
    }
}

/// The verdict word of the final failure of a card: `j` is the job, `v`
/// the card's view of it ([`card_view`]).
pub fn fail_phrase(j: &Job, v: &Job) -> &'static str {
    if v.outcome.as_deref() == Some(OUTCOME_MOVED) {
        return MOVED_PHRASE;
    }
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

/// The verdict words of a card whose branch moved after its review
/// (TER-553).
const MOVED_PHRASE: &str =
    "left the merge: the branch moved after its review, so the card goes back to review";

/// The comment of the final failure of a card ([`fail_phrase`]).
pub fn fail_comment(j: &Job, v: &Job) -> String {
    let mut s = end_comment(v, fail_phrase(j, v));
    // A branch that moved (TER-553): the reviewed commit and the head, in
    // the comment of the move back to review. The daemon wrote the tail.
    if v.outcome.as_deref() == Some(OUTCOME_MOVED)
        && let Some(tail) = &v.tail
    {
        s.push(' ');
        s.push_str(tail);
    }
    s
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
pub(crate) fn on_end(t: &mut Tx<'_>, j: &Job) -> ApiResult<()> {
    if j.kind == JobKind::E2e {
        let group = crate::store::groups::get(t, &j.card)?;
        let state = if j.state == jobs::JobState::Passed {
            "done"
        } else {
            "failed"
        };
        crate::store::groups::step(
            t,
            &group,
            "version branch e2e",
            Some(state),
            Some(&end_comment(j, state)),
            None,
        )?;
        return Ok(());
    }
    emit_verdicts(t, j)?;
    if j.kind == JobKind::Release {
        return release_end(t, j);
    }
    // The failed step and the output on each card (TER-614 R3), from the
    // daemon in both flows: the rules quote a template value onto one
    // line, which would lose the lines of the output. Not for red: a red
    // end has the failure block with the output already (review, minor 5).
    for v in card_views(j) {
        if matches!(verdict_of(&v), "error" | "infra") {
            output_note(t, &v)?;
        }
    }
    // With `board_flow = rules` the board rules act on the verdicts.
    if !code_flow(t, &j.project)? {
        return Ok(());
    }
    if j.outcome.as_deref() == Some(OUTCOME_INFRA) {
        return infra_end(t, j);
    }
    if j.batch.is_empty() {
        single_end(t, j)
    } else {
        batch_end(t, j)
    }
}

/// The end of a release job (TER-623): a comment on its card with the
/// result, and the output for a failure. No move, no label, no rerun, in
/// both board flows: the lead's release step reads it.
fn release_end(t: &mut Tx<'_>, j: &Job) -> ApiResult<()> {
    comment(t, &j.card, &verdict_comment(j))?;
    if j.state != jobs::JobState::Passed {
        output_note(t, j)?;
    }
    Ok(())
}

/// The end of a merge job that failed before its tests twice
/// ([`OUTCOME_INFRA`], TER-614 R3): no rerun, no split, no failure of a
/// card. Each card keeps its column (the merge queue), loses the job
/// labels, and gets the comment with the failed step and the output.
fn infra_end(t: &mut Tx<'_>, j: &Job) -> ApiResult<()> {
    for v in &card_views(j) {
        // A card that left the merge (a conflict, a moved branch) did so
        // before the run was lost: it ends as in a batch end.
        match v.outcome.as_deref() {
            Some(OUTCOME_MOVED) => back_to_review(t, v)?,
            Some(OUTCOME_CONFLICT) => fail_card(t, v, fail_phrase(j, v))?,
            _ => {
                set_labels(t, &v.card, &[], &[LABEL_RUNNING, LABEL_QUEUED])?;
                comment(t, &v.card, &infra_comment(v, None))?;
            }
        }
    }
    Ok(())
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
        Some(OUTCOME_MOVED) => "moved",
        Some(OUTCOME_INFRA) => "infra",
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
    matches!(
        verdict_of(v),
        "red" | "conflict" | "moved" | "error" | "infra"
    )
    .then(|| failure_block(v))
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
    // Each end and each cancel (API or rule) comes here: the job's count
    // of lost runs goes (TER-196 review 2, minor 2).
    lost_runs().remove(&j.id);
    infra_retried().remove(&j.id);
    // A release (TER-623) is no verdict on a card: the board rules, which
    // move cards on verdicts, never see one. The `job` event says its end.
    if j.kind == JobKind::Release {
        return Ok(());
    }
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
    if outcome == Some(OUTCOME_MOVED) {
        return back_to_review(t, j);
    }
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
        .partition(|c| left_merge(c.outcome.as_deref()));
    for c in &conflicted {
        let v = card_view(j, c);
        if c.outcome.as_deref() == Some(OUTCOME_MOVED) {
            back_to_review(t, &v)?;
        } else {
            fail_card(t, &v, fail_phrase(j, &v))?;
        }
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
    v.commit = c.commit.clone();
    if c.outcome.as_deref() == Some(OUTCOME_MOVED) {
        v.outcome = Some(OUTCOME_MOVED.into());
        v.exit_code = None;
        v.head = None;
        v.tail = Some(format!(
            "{}; the queue merges only the reviewed commit. Review the new commits, then accept the card again.",
            c.note
                .as_deref()
                .unwrap_or("the branch moved after its review")
        ));
    }
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
        JobKind::Validate | JobKind::E2e => {
            board::role_name(t, &j.project, board::REVIEW, COL_REVIEW)?
        }
        JobKind::Merge => board::role_name(t, &j.project, board::ACCEPTED, COL_ACCEPTED)?,
        // [`on_end`] ends a release in [`release_end`].
        JobKind::Release => return release_end(t, j),
    };
    let note = end_comment(j, "passed");
    move_or_comment(t, &j.card, &to, None, &note)?;
    unassign(t, &j.card)
}

/// A card whose branch moved after its review (TER-553): its new commits
/// were not reviewed, so it goes back to review with the reason, and is
/// free. The move out of the merge queue ends its round, and the next
/// accept records the head then. No rerun: the branch must be reviewed.
fn back_to_review(t: &mut Tx<'_>, j: &Job) -> ApiResult<()> {
    set_labels(t, &j.card, &[], &[LABEL_RUNNING, LABEL_QUEUED])?;
    let to = board::role_name(t, &j.project, board::REVIEW, COL_REVIEW)?;
    move_or_comment(t, &j.card, &to, None, &fail_comment(j, j))?;
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

/// The comment with the failed step and the output of `v`
/// ([`output_comment`]), when it has output.
fn output_note(t: &mut Tx<'_>, v: &Job) -> ApiResult<()> {
    match output_comment(v) {
        Some(text) => comment(t, &v.card, &text),
        None => Ok(()),
    }
}

fn comment(t: &mut Tx<'_>, card: &str, text: &str) -> ApiResult<()> {
    match board::system_comment(t, card, text, None) {
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
    -c "user.name=${GIT_COMMITTER_NAME:-Terrarium}" -c "user.email=${GIT_COMMITTER_EMAIL:-terrarium@localhost}" -c commit.gpgsign=false "$@"
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
/// cache larger than 10 GiB (allocated disk space) is removed before the
/// recipe starts. This bounds retained caches, not a running build's size.
/// Only this job's reserved slot is inspected. Links are never followed.
/// A
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
///
/// The slot's tree belongs to one job of one daemon at a time (TER-448):
/// the step holds an exclusive `flock` on `<dir>.lock`, a sibling of the
/// dir that a wipe keeps, and refuses the job with a clear error when
/// another job or daemon holds it. The recipe step holds the same lock
/// ([`logged_recipe`]). Without `flock` on the target, both run as before
/// this, unlocked, and print one line that says so.
const RECEIVE_SCRIPT: &str = r#"dir=$1; sha=$2; kind=$3; base=${4:-}; tree=${5:-}; wipe_first=${6:-}
case "$dir" in /*/*) ;; *) echo "the job dir is not absolute"; res error; exit 2 ;; esac
case "$sha" in *[!0-9a-f]*|'') echo "bad commit"; res error; exit 2 ;; esac
case "$base" in *[!0-9a-f]*) echo "bad base"; res error; exit 2 ;; esac
case "$tree" in *[!0-9a-f]*|'') echo "bad tree"; res error; exit 2 ;; esac
# TER-448: one job of one daemon at a time in the slot's tree. The lock
# file sits next to the dir, so a wipe of the slot keeps it, and flock
# lets go of it when this step and its children are gone.
lock=$dir.lock
if command -v flock >/dev/null 2>&1; then
  mkdir -p -- "$(dirname -- "$dir")" || { echo "cannot make the dir of $lock"; res error; exit 2; }
  exec 9>>"$lock" || { echo "cannot open the slot lock $lock"; res error; exit 2; }
  flock -n 9 || {
    echo "the slot $dir is held by another job or daemon; stop it, or give this project its own work_dir"
    res error; exit 2
  }
else
  echo "no flock on the target: slot not locked"
fi
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
# Bound the cache retained between jobs. du does not follow symlinks.
# Refuse the job if the check fails rather than silently growing forever.
target_cap_kib=10485760
if [ -L "$dir/target" ]; then
  rm -f -- "$dir/target" || { echo "cannot remove target link"; res error; exit 2; }
elif [ -e "$dir/target" ]; then
  usage=$(du -sk -- "$dir/target") || { echo "cannot size target/"; res error; exit 2; }
  set -- $usage
  case "$1" in ''|*[!0-9]*) echo "bad target/ size"; res error; exit 2 ;; esac
  if [ "$1" -gt "$target_cap_kib" ]; then
    echo "target/ exceeds $target_cap_kib KiB; removing the slot cache"
    rm -rf -- "$dir/target" || { echo "cannot remove target/"; res error; exit 2; }
  fi
fi
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
  T config user.name "${GIT_COMMITTER_NAME:-Terrarium}" || return 1
  T config user.email "${GIT_COMMITTER_EMAIL:-terrarium@localhost}" || return 1
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

    include!("jobs_cache_tests.rs");

    /// TER-623 F1: the release recipe runs in a new private HOME, with no
    /// global git config, and the HOME is gone after it; the exit code is
    /// the recipe's.
    #[test]
    fn a_release_recipe_runs_in_a_new_home_that_goes_after_it() {
        let dir = tempfile::tempdir().unwrap();
        let out = dir.path().join("out");
        let recipe = format!(
            "printf '%s\\n' \"$HOME\" > '{o}'; test \"$(stat -c %a \"$HOME\")\" = 700 \
             && test \"$GIT_CONFIG_GLOBAL\" = /dev/null && test -z \"${{DOCKER_CONFIG:-}}\" \
             && test \"$XDG_CONFIG_HOME\" = \"$HOME/.config\" \
             && test \"$PATH\" = '{path}' && exit 7",
            path = RELEASE_PATH,
            o = out.display()
        );
        let left = dir.path().join("terrarium-release-home.left");
        std::fs::create_dir(&left).unwrap();
        let status = recipe_sh()
            .env("HOME", dir.path())
            .env(
                "PATH",
                format!("{}/bin:/usr/bin:/bin", dir.path().display()),
            )
            .env("DOCKER_CONFIG", dir.path())
            .env("TMPDIR", dir.path())
            .args(["-c", &release_home(&recipe)])
            .status()
            .unwrap();
        assert_eq!(status.code(), Some(7));
        let home = std::fs::read_to_string(&out).unwrap();
        let home = home.trim();
        assert!(home.contains("terrarium-release-home."), "{home}");
        assert!(
            !std::path::Path::new(home).exists(),
            "the release HOME stayed"
        );
        assert!(!left.exists(), "the HOME of a killed release stayed");
    }

    #[test]
    fn merge_references_refuse_literals_and_reserved_env_names() {
        for (name, value) in [
            ("KEY", "literal"),
            ("KEY", "@secret:"),
            ("PATH", "@secret:vm"),
        ] {
            let mut delivery = DeliveryDef::default();
            delivery.merge_secrets.insert(name.into(), value.into());
            assert!(check_merge_refs(&delivery).is_err());
        }
        let mut delivery = DeliveryDef::default();
        delivery
            .merge_secrets
            .insert("DOCKER_CI_KEY".into(), "@secret:vm.private_key".into());
        delivery.merge_secrets.insert(
            "DOCKER_CI_RESET_KEY".into(),
            "@secret:reset.private_key".into(),
        );
        assert!(check_merge_refs(&delivery).is_err());
        delivery.docker_ci = Some(terrarium_core::config::DockerCiDef {
            destination: "root@vm".into(),
            port: 2250,
            known_hosts: "vm ssh-ed25519 test-pin".into(),
            reset_destination: "reset@host".into(),
            reset_port: 22,
            reset_known_hosts: "".into(),
        });
        assert!(check_merge_refs(&delivery).is_err());
        delivery.docker_ci.as_mut().unwrap().reset_known_hosts =
            "host ssh-ed25519 reset-pin".into();
        assert!(check_merge_refs(&delivery).is_ok());
    }

    #[test]
    fn job_log_keeps_early_panics_before_a_long_noisy_tail() {
        let dir = tempfile::tempdir().unwrap();
        let log = dir.path().join("logs with 'quotes/job.log");
        let recipe = "printf 'test early_failure ... FAILED\\n'\nprintf \"thread 'early_failure' panicked at test.rs:1:2:\\nexpected value\\n\" >&2\ni=0; while [ \"$i\" -lt 2000 ]; do printf 'unrelated build noise\\n'; i=$((i + 1)); done\nexit 101";
        let script = logged_recipe(recipe.into(), log.to_str().unwrap());
        let out = recipe_sh().args(["-c", &script]).output().unwrap();
        assert_eq!(out.status.code(), Some(101));
        let full = std::fs::read_to_string(&log).unwrap();
        assert!(full.len() > RECIPE_MAX / 32);
        assert!(full.contains("early_failure"));
        assert!(full.contains("expected value"));
        assert_eq!(full.matches("unrelated build noise").count(), 2000);
        let summary = std::process::Command::new("sh")
            .args(["-c", &log_excerpt(log.to_str().unwrap(), None)])
            .output()
            .unwrap();
        let excerpt = String::from_utf8_lossy(&summary.stdout);
        assert!(excerpt.len() < jobs::TAIL_MAX);
        let mut j = ended(JobKind::Merge, jobs::JobState::Failed, Some("red"));
        j.tail = Some(jobs::clip_tail(&excerpt));
        let block = failure_block(&j);
        assert!(block.contains("test early_failure ... FAILED"));
        assert!(block.contains("panicked at test.rs:1:2"));
        assert!(block.contains("expected value"));
        assert!(block.find("expected value").unwrap() < block.find("Output tail").unwrap());
    }

    #[test]
    fn job_log_patterns_are_literal_and_can_be_disabled() {
        let dir = tempfile::tempdir().unwrap();
        let log = dir.path().join("job.log");
        for patterns in [Some(vec!["custom [failure] 'marker'".into()]), Some(vec![])] {
            let script = logged_recipe(
                "printf \"custom [failure] 'marker'\\nmessage\\n\"; exit 7".into(),
                log.to_str().unwrap(),
            );
            let out = recipe_sh().args(["-c", &script]).output().unwrap();
            assert_eq!(out.status.code(), Some(7));
            let summary = std::process::Command::new("sh")
                .args([
                    "-c",
                    &log_excerpt(log.to_str().unwrap(), patterns.as_deref()),
                ])
                .output()
                .unwrap();
            let excerpt = String::from_utf8_lossy(&summary.stdout);
            assert_eq!(
                excerpt.contains("Failure lines"),
                !patterns.unwrap().is_empty()
            );
            assert!(excerpt.contains("custom [failure] 'marker'"));
        }
    }

    #[test]
    fn job_log_is_private_without_changing_the_recipe_umask() {
        use std::os::unix::fs::PermissionsExt;
        let dir = tempfile::tempdir().unwrap();
        let log = dir.path().join("recipe.log");
        let script = format!(
            "umask 022\n{}",
            logged_recipe("umask; touch output".into(), log.to_str().unwrap())
        );
        let out = recipe_sh()
            .current_dir(dir.path())
            .args(["-c", &script])
            .output()
            .unwrap();
        assert!(out.status.success());
        assert!(
            std::fs::read_to_string(&log)
                .unwrap()
                .trim()
                .ends_with("022")
        );
        assert_eq!(
            std::fs::metadata(&log).unwrap().permissions().mode() & 0o777,
            0o600
        );
        assert_eq!(
            std::fs::metadata(dir.path().join("output"))
                .unwrap()
                .permissions()
                .mode()
                & 0o777,
            0o644
        );
    }

    #[tokio::test]
    async fn repo_lifetime_allows_readers_but_blocks_rebuild_after_cancellation() {
        let tmp = tempfile::tempdir().unwrap();
        let locks = repo_lock(tmp.path());
        let first = Arc::new(locks.lifetime.clone().read_owned().await);
        let second = locks.lifetime.clone().try_read_owned().unwrap();
        drop(second);
        let (started, ready) = tokio::sync::oneshot::channel();
        let (release, released) = std::sync::mpsc::channel();
        let reader = first.clone();
        let prepare = locks.prepare.clone().lock_owned().await;
        let task = tokio::spawn(async move {
            tokio::task::spawn_blocking(move || {
                let _reader = reader;
                let _prepare = prepare;
                started.send(()).unwrap();
                released.recv().unwrap();
            })
            .await
            .unwrap();
        });
        ready.await.unwrap();
        task.abort();
        let _ = task.await;
        drop(first);
        let rebuild_blocked = locks.lifetime.clone().try_write_owned().is_err();
        let prepare_blocked = locks.prepare.clone().try_lock_owned().is_err();
        release.send(()).unwrap();
        let writer = tokio::time::timeout(
            Duration::from_secs(10),
            locks.lifetime.clone().write_owned(),
        )
        .await
        .unwrap();
        assert!(rebuild_blocked);
        assert!(prepare_blocked);
        drop(writer);
        assert!(locks.prepare.clone().try_lock_owned().is_ok());
    }

    #[test]
    fn startup_removes_legacy_repos_and_bundles_but_keeps_cached_repos() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path().join("jobs");
        cleanup_job_files(&root).unwrap();
        let project = root.join("p");
        std::fs::create_dir_all(project.join("J1.git/objects")).unwrap();
        std::fs::create_dir_all(project.join("repos/app.git")).unwrap();
        std::fs::write(project.join("J1.bundle"), "old").unwrap();
        std::fs::write(project.join("repos/J2.bundle"), "old").unwrap();
        std::fs::write(project.join("repos/app.git/HEAD"), "kept").unwrap();
        cleanup_job_files(&root).unwrap();
        assert!(!project.join("J1.git").exists());
        assert!(!project.join("J1.bundle").exists());
        assert!(!project.join("repos/J2.bundle").exists());
        assert_eq!(
            std::fs::read_to_string(project.join("repos/app.git/HEAD")).unwrap(),
            "kept"
        );
    }

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

    /// A shell for a [`logged_recipe`] script, without the job env of an
    /// outer job: a merge job runs these tests inside its own recipe, and
    /// its `TERRARIUM_HEAD` (a head this test's dir does not have) and
    /// `TERRARIUM_SLOT_LOCK` (the lock that job holds) would end the
    /// wrapper with exit 125 before the recipe.
    fn recipe_sh() -> std::process::Command {
        let mut c = std::process::Command::new("sh");
        c.env_remove("TERRARIUM_HEAD")
            .env_remove("TERRARIUM_SLOT_LOCK");
        c
    }

    /// The exit status and both outputs of a step, for an assertion.
    fn shown(out: &std::process::Output) -> String {
        format!(
            "{:?}\nstdout:\n{}\nstderr:\n{}",
            out.status,
            String::from_utf8_lossy(&out.stdout),
            String::from_utf8_lossy(&out.stderr)
        )
    }

    /// True when `flock` works here: the TER-448 lock tests need it.
    fn flock_works() -> bool {
        std::process::Command::new("sh")
            .args([
                "-c",
                "command -v flock >/dev/null 2>&1 && flock -n /dev/null true",
            ])
            .output()
            .is_ok_and(|o| o.status.success())
    }

    /// Hold the lock of `path` (as another daemon would) until the holder
    /// is stopped. The holder runs in its own process group, so
    /// [`stop_holder`] can end the whole tree: the lock goes with it. It
    /// waits for the lock (up to 60 s, so a slow start or a step that ends
    /// late under load does not make it give up), then prints `held`:
    /// [`until_held`] reads that line.
    fn flock_holder(path: &std::path::Path) -> std::process::Child {
        use std::os::unix::process::CommandExt;
        std::process::Command::new("flock")
            .arg("-w")
            .arg("60")
            .arg(path)
            .arg("sh")
            .arg("-c")
            .arg("echo held; exec sleep 60")
            .stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::piped())
            .process_group(0)
            .spawn()
            .unwrap()
    }

    /// End the holder and its children, so its lock goes.
    fn stop_holder(holder: &mut std::process::Child) {
        let pgid = nix::unistd::Pid::from_raw(holder.id() as i32);
        let _ = nix::sys::signal::kill(
            nix::unistd::Pid::from_raw(-pgid.as_raw()),
            nix::sys::signal::Signal::SIGKILL,
        );
        let _ = holder.wait();
    }

    /// Wait until the holder holds `path`: it prints `held` once it has the
    /// lock. A holder that ends first (it could not take the lock) fails
    /// the test with its exit status and stderr.
    fn until_held(holder: &mut std::process::Child, path: &std::path::Path) {
        use std::io::{BufRead, Read};
        let mut line = String::new();
        std::io::BufReader::new(holder.stdout.as_mut().unwrap())
            .read_line(&mut line)
            .unwrap();
        if line.trim() != "held" {
            let status = holder.wait().unwrap();
            let mut err = String::new();
            let _ = holder.stderr.as_mut().unwrap().read_to_string(&mut err);
            panic!(
                "the holder never took {}: {status:?}, stdout {line:?}, stderr {err:?}",
                path.display()
            );
        }
    }

    /// TER-448: the receive step holds an exclusive lock on `<slot>.lock`,
    /// so a second daemon on the same work dir cannot replace the tree of
    /// a slot: its step fails with a clear error and leaves the tree as it
    /// is. The lock goes when its holder ends.
    #[test]
    fn a_busy_slot_lock_refuses_the_receive_step() {
        if !flock_works() {
            return;
        }
        let s = Src::new();
        let (one, t1) = s.commit("one");
        let (ok, text) = s.receive(&one, &t1, "");
        assert!(ok, "{text}");
        let slot = s.slot();
        let lock = slot.with_file_name("slot-1.lock");
        assert!(lock.exists(), "the receive step leaves the slot lock");
        let mut holder = flock_holder(&lock);
        until_held(&mut holder, &lock);
        let (two, t2) = s.commit("two");
        let (ok, text) = s.receive(&two, &t2, "");
        assert!(!ok, "{text}");
        assert!(text.contains("held by another job or daemon"), "{text}");
        // The tree of the running job is untouched.
        assert_eq!(git_in(&slot, &["rev-parse", "HEAD"]), one);
        stop_holder(&mut holder);
        // The lock went with its holder: the next job of the slot passes.
        let (ok, text) = s.receive(&two, &t2, "");
        assert!(ok, "{text}");
        assert_eq!(git_in(&slot, &["rev-parse", "HEAD"]), two);
    }

    /// TER-448: the recipe holds the slot's lock for its whole run (its
    /// own second flock fails), and a busy lock, or a head that is not the
    /// job's head (another daemon replaced the tree between the steps),
    /// ends the job before the recipe runs.
    #[test]
    fn the_recipe_holds_the_slot_lock_and_rechecks_the_head() {
        if !flock_works() {
            return;
        }
        let tmp = tempfile::tempdir().unwrap();
        let slot = tmp.path().join("slot-1");
        std::fs::create_dir_all(&slot).unwrap();
        git_in(&slot, &["init", "-q", "-b", "main"]);
        std::fs::write(slot.join("OK"), "yes\n").unwrap();
        git_in(&slot, &["add", "."]);
        git_in(&slot, &["commit", "-q", "-m", "one"]);
        let head = git_in(&slot, &["rev-parse", "HEAD"]);
        let lock = tmp.path().join("slot-1.lock");
        let log = tmp.path().join("logs/1-1.log");
        let script = logged_recipe(
            "flock -n \"$TERRARIUM_SLOT_LOCK\" true && exit 9; echo ran".into(),
            log.to_str().unwrap(),
        );
        let run = |head: &str| {
            recipe_sh()
                .arg("-c")
                .arg(&script)
                .current_dir(&slot)
                .env("TERRARIUM_SLOT_LOCK", &lock)
                .env("TERRARIUM_HEAD", head)
                .output()
                .unwrap()
        };
        // The recipe runs with the lock held: its own second flock fails.
        let out = run(&head);
        assert!(
            out.status.success(),
            "{}",
            String::from_utf8_lossy(&out.stderr)
        );
        assert_eq!(std::fs::read_to_string(&log).unwrap(), "ran\n");
        // Another daemon replaced the tree between the steps.
        let out = run("5555555555555555555555555555555555555555");
        assert_eq!(out.status.code(), Some(125));
        let text = format!(
            "{}{}",
            String::from_utf8_lossy(&out.stdout),
            String::from_utf8_lossy(&out.stderr)
        );
        assert!(text.contains("replaced the slot tree"), "{text}");
        assert_eq!(std::fs::read_to_string(&log).unwrap(), "");
        // A busy lock: the recipe does not run either.
        let mut holder = flock_holder(&lock);
        until_held(&mut holder, &lock);
        let out = run(&head);
        assert_eq!(out.status.code(), Some(125));
        let text = format!(
            "{}{}",
            String::from_utf8_lossy(&out.stdout),
            String::from_utf8_lossy(&out.stderr)
        );
        assert!(text.contains("held by another job or daemon"), "{text}");
        assert_eq!(std::fs::read_to_string(&log).unwrap(), "");
        stop_holder(&mut holder);
    }

    /// TER-448 review 1: only the wrapper holds the slot lock. A process
    /// that the recipe leaves behind does not get the lock fd, so the slot
    /// is free as soon as the job ends, while that process still runs.
    #[test]
    fn a_child_that_outlives_the_recipe_does_not_hold_the_slot() {
        if !flock_works() {
            return;
        }
        let tmp = tempfile::tempdir().unwrap();
        let lock = tmp.path().join("slot-1.lock");
        let log = tmp.path().join("logs/1-1.log");
        let pid_file = tmp.path().join("sleeper.pid");
        let script = logged_recipe(
            format!(
                "sleep 30 </dev/null >/dev/null 2>&1 & echo $! > {}; echo ran",
                crate::executor::sh_quote(pid_file.to_str().unwrap())
            ),
            log.to_str().unwrap(),
        );
        let out = recipe_sh()
            .arg("-c")
            .arg(&script)
            .current_dir(tmp.path())
            .env("TERRARIUM_SLOT_LOCK", &lock)
            .output()
            .unwrap();
        assert!(out.status.success(), "the wrapper failed: {}", shown(&out));
        assert_eq!(
            std::fs::read_to_string(&log).unwrap(),
            "ran\n",
            "{}",
            shown(&out)
        );
        let pid: i32 = std::fs::read_to_string(&pid_file)
            .unwrap()
            .trim()
            .parse()
            .unwrap();
        let pid = nix::unistd::Pid::from_raw(pid);
        let alive = nix::sys::signal::kill(pid, None).is_ok();
        // The next job of the slot can take the lock.
        let free = std::process::Command::new("flock")
            .arg("-n")
            .arg(&lock)
            .arg("true")
            .status()
            .unwrap()
            .success();
        let _ = nix::sys::signal::kill(pid, nix::sys::signal::Signal::SIGKILL);
        assert!(alive, "the sleeper outlives the recipe");
        assert!(free, "a child of the recipe holds the slot lock");
    }

    /// TER-448 review 3: without `flock` on the target the recipe runs
    /// unlocked, and the step says so in one line.
    #[test]
    fn without_flock_the_recipe_says_the_slot_is_not_locked() {
        let tmp = tempfile::tempdir().unwrap();
        // A PATH with the tools of the wrapper, but no flock.
        let bin = tmp.path().join("bin");
        std::fs::create_dir_all(&bin).unwrap();
        for tool in ["sh", "mkdir", "dirname"] {
            let path = String::from_utf8(
                std::process::Command::new("sh")
                    .args(["-c", &format!("command -v {tool}")])
                    .output()
                    .unwrap()
                    .stdout,
            )
            .unwrap();
            std::os::unix::fs::symlink(path.trim(), bin.join(tool)).unwrap();
        }
        let lock = tmp.path().join("slot-1.lock");
        let log = tmp.path().join("logs/1-1.log");
        let script = logged_recipe("echo ran".into(), log.to_str().unwrap());
        let out = std::process::Command::new(bin.join("sh"))
            .arg("-c")
            .arg(&script)
            .current_dir(tmp.path())
            .env_remove("TERRARIUM_HEAD")
            .env("PATH", &bin)
            .env("TERRARIUM_SLOT_LOCK", &lock)
            .output()
            .unwrap();
        assert!(out.status.success(), "the wrapper failed: {}", shown(&out));
        let text = String::from_utf8_lossy(&out.stdout);
        assert!(
            text.contains("no flock on the target: slot not locked"),
            "{text}"
        );
        assert_eq!(std::fs::read_to_string(&log).unwrap(), "ran\n");
    }

    #[test]
    fn oversized_slot_caches_are_removed_and_small_caches_reused() {
        let s = Src::new();
        let (sha, tree) = s.commit("one");
        let script = RECEIVE_SCRIPT.replace("target_cap_kib=10485760", "target_cap_kib=64");
        let d = s.slot().display().to_string();
        let bundle = s.bundle(&sha);
        let receive = || run_plain(&script, &[&d, &sha, "validate", "", &tree], &bundle);
        let (ok, text) = receive();
        assert!(ok, "{text}");
        let cache = s.slot().join("target");
        let other = s.tmp.path().join("jobs/p/slot-2/target");
        std::fs::create_dir_all(&other).unwrap();
        std::fs::write(other.join("kept"), vec![1; 128 * 1024]).unwrap();
        for _ in 0..3 {
            std::fs::create_dir_all(&cache).unwrap();
            std::fs::write(cache.join("small"), "built").unwrap();
            let (ok, text) = receive();
            assert!(ok, "{text}");
            assert!(cache.join("small").exists());
            std::fs::write(cache.join("large"), vec![1; 128 * 1024]).unwrap();
            let (ok, text) = receive();
            assert!(ok, "{text}");
            assert!(text.contains("removing the slot cache"), "{text}");
            assert!(!cache.exists());
            assert!(other.join("kept").exists());
        }
        #[cfg(unix)]
        {
            std::os::unix::fs::symlink(&other, &cache).unwrap();
            let (ok, text) = receive();
            assert!(ok, "{text}");
            assert!(!cache.exists());
            assert!(other.join("kept").exists());
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

    fn exit(code: Option<i32>, signal: Option<i32>, reason: &str) -> End {
        End::Exit {
            code,
            signal,
            reason: reason.into(),
        }
    }

    /// TER-196: only an end of the recipe itself is red. A run with no
    /// exit code, one that the host agent ended for a down daemon link, and
    /// an ssh run whose link broke are lost to the infrastructure.
    #[test]
    fn a_lost_run_is_told_from_a_recipe_end() {
        let lost = End::Error {
            code: "lost".into(),
            message: "the host agent no longer holds the session".into(),
        };
        let why = lost_run(Some(&lost), b"", None).unwrap();
        assert!(why.contains("no longer holds the session"), "{why}");
        let offline = End::Error {
            code: "offline".into(),
            message: "host lab1 is offline".into(),
        };
        assert!(lost_run(Some(&offline), b"", None).is_some());
        assert!(lost_run(None, b"", None).is_some());
        // A code that a rerun gets again is red (review, minor 2).
        for code in FIXED_ERRORS {
            let e = End::Error {
                code: (*code).into(),
                message: "no".into(),
            };
            assert!(lost_run(Some(&e), b"", None).is_none(), "{code}");
        }
        for reason in ["detached_timeout", "output_overflow"] {
            let w = lost_run(Some(&exit(None, Some(9), reason)), b"", None).unwrap();
            assert!(w.contains(reason), "{w}");
        }
        // The recipe's own ends: an exit code, a signal, its timeout.
        for e in [
            exit(Some(1), None, "exited"),
            exit(Some(101), None, "exited"),
            exit(None, Some(9), "signaled"),
            exit(None, Some(9), "timeout"),
            exit(Some(255), None, "exited"),
        ] {
            assert!(
                lost_run(Some(&e), b"error: tests failed\n", None).is_none(),
                "{e:?}"
            );
        }
        // An ssh target: 255 with a broken link to that host is lost; 255
        // from the recipe, or a broken link to another host, is red.
        let e255 = exit(Some(255), None, "exited");
        for line in [
            "Connection to 10.3.0.2 closed by remote host.",
            "Connection to 10.3.0.2 port 22: Broken pipe",
            "packet_write_wait: Connection to 10.3.0.2 port 22: Broken pipe",
            "Read from remote host 10.3.0.2: Connection reset by peer",
            "Timeout, server 10.3.0.2 not responding.",
            "ssh: connect to host 10.3.0.2 port 22: Connection refused",
        ] {
            let stderr = format!("compiling\n{line}\n");
            let w = lost_run(Some(&e255), stderr.as_bytes(), Some("10.3.0.2"));
            assert!(w.is_some_and(|w| w.contains(line)), "{line}");
        }
        let other = b"Connection to github.com closed by remote host.\n";
        let no_host = b"client_loop: send disconnect: Broken pipe\n";
        assert!(lost_run(Some(&e255), no_host, Some("10.3.0.2")).is_none());
        assert!(lost_run(Some(&e255), other, Some("10.3.0.2")).is_none());
        assert!(lost_run(Some(&e255), b"exit 255\n", Some("10.3.0.2")).is_none());
        let closed = b"Connection to 10.3.0.2 closed by remote host.\n";
        assert!(
            lost_run(
                Some(&exit(Some(1), None, "exited")),
                closed,
                Some("10.3.0.2")
            )
            .is_none()
        );
        // No ssh target: the ssh lines do not count.
        assert!(lost_run(Some(&e255), closed, None).is_none());
    }

    /// TER-196 review, minor 2: a job reruns after a lost run at most
    /// [`MAX_LOST_RERUNS`] times in a row; the next lost run is red with
    /// the reason, and the count starts again.
    #[test]
    fn lost_reruns_have_a_cap() {
        let job = ulid::Ulid::new().to_string();
        let end = || JobEnd {
            outcome: Some("error".into()),
            tail: Some("output".into()),
            ..JobEnd::default()
        };
        for _ in 0..MAX_LOST_RERUNS {
            let r = lost_or_red(&job, false, "error lost: gone".into(), end());
            assert!(matches!(r, Ran::Lost { ref why, red: None } if why == "error lost: gone"));
        }
        // Past the cap: still a lost run, so it gets the kill of a lost
        // run, but it ends red (review 2, major 1).
        let Ran::Lost { red: Some(e), .. } =
            lost_or_red(&job, false, "error lost: gone".into(), end())
        else {
            panic!("the run after the cap has no red end");
        };
        assert!(!e.passed);
        assert_eq!(e.outcome.as_deref(), Some("red"));
        let tail = e.tail.unwrap();
        assert!(
            tail.starts_with(&format!(
                "the infrastructure lost the run {} times in a row",
                MAX_LOST_RERUNS + 1
            )),
            "{tail}"
        );
        assert!(
            tail.contains("error lost: gone") && tail.ends_with("output"),
            "{tail}"
        );
        assert!(matches!(
            lost_or_red(&job, false, "again".into(), end()),
            Ran::Lost { red: None, .. }
        ));
        lost_runs().remove(&job);
    }

    /// TER-614 R3: a merge job whose runs keep getting lost ends as an
    /// infra failure, not red, so its batch does not split.
    #[test]
    fn lost_merge_runs_past_the_cap_are_infra() {
        let job = ulid::Ulid::new().to_string();
        let end = || JobEnd {
            outcome: Some("error".into()),
            tail: Some("output".into()),
            ..JobEnd::default()
        };
        for _ in 0..MAX_LOST_RERUNS {
            assert!(matches!(
                lost_or_red(&job, true, "error lost: gone".into(), end()),
                Ran::Lost { red: None, .. }
            ));
        }
        let Ran::Lost { red: Some(e), .. } =
            lost_or_red(&job, true, "error lost: gone".into(), end())
        else {
            panic!("the run after the cap has no end");
        };
        assert_eq!(e.outcome.as_deref(), Some(OUTCOME_INFRA));
        assert!(e.tail.unwrap().contains("an infra failure"));
    }

    /// TER-614 R3: a failed end keeps its step as the last line of its
    /// tail, and the card comment shows the step and the last lines.
    #[test]
    fn a_failed_end_keeps_its_step_and_tail() {
        let end = with_failed_step(error_end("line 1\n[error lost: gone]"), STEP_RECEIVE);
        let tail = end.tail.clone().unwrap();
        assert!(tail.ends_with("[failed step: receive]"), "{tail}");
        assert_eq!(failed_step(Some(&tail)), Some("receive"));
        // Only a known step on the last line counts (review, minor 6).
        assert_eq!(failed_step(Some("out\n[failed step: evil]")), None);
        assert_eq!(
            failed_step(Some("[failed step: recipe]\nmore output")),
            None
        );
        // A failure in the daemon is no host failure (review, major 1).
        let job = ended(JobKind::Merge, jobs::JobState::Failed, Some("error"));
        let prepare = with_failed_step(error_end("cannot fetch next"), STEP_PREPARE);
        assert!(!before_tests(&job, &prepare));
        assert!(before_tests(&job, &end));
        let pass = with_failed_step(
            JobEnd {
                passed: true,
                tail: Some("ok".into()),
                ..JobEnd::default()
            },
            STEP_RECIPE,
        );
        assert_eq!(pass.tail.as_deref(), Some("ok"));
        let long: String = (0..50).map(|i| format!("line {i}\n")).collect();
        let j = Job {
            tail: with_failed_step(error_end(long), STEP_RECIPE).tail,
            ..ended(JobKind::Merge, jobs::JobState::Failed, Some("error"))
        };
        let note = output_comment(&j).unwrap();
        assert!(note.contains("failed step: recipe."), "{note}");
        assert!(
            note.contains("    line 49\n") && !note.contains("line 30\n"),
            "{note}"
        );
        let none = Job {
            tail: None,
            ..ended(JobKind::Merge, jobs::JobState::Failed, Some("error"))
        };
        assert!(output_comment(&none).is_none());
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
        let found: HashMap<usize, Left> = [
            (1, Left::conflict("README\nsrc/a.rs".to_owned())),
            (3, Left::conflict("b.txt".to_owned())),
            (4, Left::moved("t-5", "abc1234", "def5678")),
        ]
        .into_iter()
        .collect();
        let card = |c: &str| BatchCard {
            card: c.into(),
            branch: c.to_lowercase(),
            by_agent: None,
            commit: None,
            outcome: None,
            note: None,
        };
        let batch = vec![
            card("T-1"),
            card("T-2"),
            card("T-3"),
            card("T-4"),
            card("T-5"),
        ];
        let marked = mark_left(&batch, &found);
        let outcomes: Vec<Option<&str>> = marked.iter().map(|c| c.outcome.as_deref()).collect();
        assert_eq!(
            outcomes,
            [
                None,
                Some("conflict"),
                None,
                Some("conflict"),
                Some("moved")
            ]
        );
        assert_eq!(marked[1].note.as_deref(), Some("README\nsrc/a.rs"));
        assert!(marked[4].note.as_deref().unwrap().contains("abc1234"));
        assert_eq!(
            batch_summary(&marked),
            "batch: merged T-1 T-3; left out on a conflict T-2 T-4; left out, moved after review T-5"
        );
    }

    #[test]
    fn by_agent_label() {
        let labels = vec!["ci".to_owned(), "by:dash-ci".to_owned()];
        assert_eq!(by_agent_of(&labels).as_deref(), Some("dash-ci"));
        assert_eq!(by_agent_of(&[]), None);
    }

    #[tokio::test]
    async fn host_resolution_timeout_pins_once_and_address_reset_clears_it() {
        let dir = tempfile::tempdir().unwrap();
        let db = crate::db::Db::new(
            crate::db::open(&dir.path().join("state.db")).unwrap(),
            crate::events::EventHub::default(),
        );
        db.write("operator", |t| {
            let def = serde_json::from_value(serde_json::json!({
                "name": "dns-box", "kind": "ssh", "address": "SLOW.INVALID"
            }))
            .unwrap();
            crate::store::targets::create(t, &def)?;
            Ok(())
        })
        .await
        .unwrap();
        let (release, wait) = std::sync::mpsc::channel();
        let wait = std::sync::Mutex::new(wait);
        let started = Arc::new(tokio::sync::Notify::new());
        let ready = started.clone();
        let other = db.clone();
        let first = tokio::spawn(async move {
            prepare_hosts_with(&other, None, move |_| {
                ready.notify_one();
                wait.lock().unwrap().recv().unwrap();
                "192.0.2.2".into()
            })
            .await
            .unwrap();
        });
        started.notified().await;
        prepare_hosts_with(&db, Some("slow.invalid".into()), |_| {
            panic!("parallel kick must not look up the name twice")
        })
        .await
        .unwrap();
        first.await.unwrap();
        let key = db
            .read(|c| {
                Ok(c.query_row(
                    "SELECT host_key FROM job_host_keys WHERE hostname = 'slow.invalid'",
                    [],
                    |r| r.get::<_, String>(0),
                )?)
            })
            .await
            .unwrap();
        assert_eq!(key, "slow.invalid");
        db.write("operator", |t| {
            crate::store::targets::update(
                t,
                "dns-box",
                &serde_json::json!({"address": "SLOW.INVALID"}),
            )?;
            assert_eq!(jobs::unresolved_hosts(t, None)?, vec!["slow.invalid"]);
            Ok(())
        })
        .await
        .unwrap();
        prepare_hosts_with(&db, None, |_| {
            panic!("timed out libc lookup is still in flight")
        })
        .await
        .unwrap();
        release.send(()).unwrap();
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
        let wait = std::sync::Mutex::new(wait);
        let resolver_db = db.clone();
        let task = tokio::spawn(async move {
            prepare_hosts_with(
                &resolver_db,
                Some("round-robin.invalid".into()),
                move |_| {
                    ready.notify_one();
                    wait.lock().unwrap().recv().unwrap();
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
        // move a running job away from its cap.
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
            "192.0.2.3".into()
        })
        .await
        .unwrap();
        reopened
            .read(|c| {
                let key: String = c.query_row(
                    "SELECT host_key FROM job_host_keys WHERE hostname = 'failed.invalid'",
                    [],
                    |r| r.get(0),
                )?;
                assert_eq!(key, "192.0.2.3");
                Ok(())
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
            onto: None,
        }
    }

    fn batch_card(card: &str, by: &str, outcome: Option<&str>) -> BatchCard {
        BatchCard {
            card: card.into(),
            branch: card.to_lowercase(),
            by_agent: Some(by.into()),
            commit: None,
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
            (Failed, Some(OUTCOME_INFRA), "infra", "failed"),
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

    #[tokio::test]
    async fn release_e2e_records_outcome_without_moving_version_group() {
        use crate::store::{groups, projects};
        let mut c = rusqlite::Connection::open_in_memory().unwrap();
        crate::db::migrate(&mut c).unwrap();
        let db = crate::db::Db::new(c, crate::events::EventHub::default());
        db.write("system", |t| {
            projects::create(
                t,
                &projects::NewProject {
                    name: "p".into(),
                    ..Default::default()
                },
                projects::SOURCE_API,
            )?;
            groups::open(
                t,
                groups::Open {
                    project: "p",
                    rule: "version-cut",
                    key: "version:test",
                    title: "Version test",
                    column: "review",
                    done_when: "done",
                    eta: None,
                    close_after: None,
                },
            )?;
            let g = groups::by_key(t, "p", "version:test")?.unwrap();
            for (state, expected) in [
                (jobs::JobState::Passed, "done"),
                (jobs::JobState::Failed, "failed"),
                (jobs::JobState::Cancelled, "failed"),
            ] {
                let mut j = ended(JobKind::E2e, state, None);
                j.card = g.id.clone();
                j.project = "p".into();
                on_end(t, &j)?;
                let after = groups::get(t, &g.id)?;
                assert_eq!(after.column, g.column);
                assert_eq!(after.members, g.members);
                assert_eq!(
                    after
                        .steps
                        .iter()
                        .find(|s| s.name == "version branch e2e")
                        .unwrap()
                        .state,
                    expected
                );
            }
            Ok(())
        })
        .await
        .unwrap();
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
        // API queueing also runs inside an operator transaction. Its
        // generated notes must still be authored by system.
        db.write("operator", move |t| {
            comment(t, &id, "Job queue: a comment.")
        })
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
