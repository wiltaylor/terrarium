//! The job queue of a project (TER-71): the run time. The rows are in
//! [`crate::store::jobs`].
//!
//! - A tick (each [`TICK`], and at once after a queue or an end) starts
//!   the queued jobs that have a place: first in, first out across
//!   projects, at most the host cap of the target's host (TER-97), then
//!   at most the project's `slots` validate jobs; merge jobs have their
//!   own lane, one at a time per project (TER-159). The pick and the start are one write, so two ticks cannot
//!   overfill the slots.
//! - A job is three commands on the project's target, each its own run:
//!   1. prepare, with the repo credential in its env: a fresh clone in
//!      `<work_dir>/<project>/<job>`. A validate job checks out the card
//!      commit. A merge job checks out `next` and merges the card branch
//!      with `--no-ff`; a conflict ends the job (never resolved, never
//!      rerun). A batch merge (TER-112) merges its card branches in
//!      order; a card that conflicts, or whose merge fails otherwise
//!      (unrelated histories), leaves the batch (its merge is aborted or
//!      reset) and the others go on. When none merges, the job ends as
//!      a conflict.
//!   2. the recipe (`sh -c <recipe>` in the clone), without any
//!      credential: it runs the repo's code. Its env names the job kind
//!      and, when `next` gives a base, the file of the changed paths
//!      (TER-100, [`recipe_env`]).
//!   3. a merge job only, when the recipe passed: push the merge commit
//!      to `next`. The recipe may have changed the clone (new commits, a
//!      moved HEAD, its `.git/config`), so the push never runs there:
//!      - without any credential: a fresh bare repo `<clone>.push`
//!        fetches exactly the merge commit that step 1 recorded, over
//!        `file://`. A commit that is gone fails the job.
//!      - with the credential: push that commit ID from the fresh repo
//!        to `next`. Not forced: when `next` moved, the push fails.
//!
//!
//!   Each step runs as `ionice -c3 nice -n19 <argv>` (`nice` only on a
//!   target without `ionice`), so jobs do not starve their host.
//!   Then the clone and the push repo are removed.
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
//! - A daemon restart loses the runs: each running job fails (`error`)
//!   and goes through the rerun rule.
//! - A host that cannot be reached is not a failure (TER-160). A job whose
//!   target's host agent is offline is not started: it stays queued. A job
//!   whose target could not be reached before its recipe started (the host
//!   agent went away, or the ssh connect was refused or timed out) goes
//!   back to the queue ([`jobs::requeue`]); an ssh target is tried again
//!   after [`RETRY`]. Neither uses an attempt or moves a card. After
//!   [`LONG_WAIT`] the queue notes the wait on the cards, once per job and
//!   daemon run. A failure after the recipe started stays a failure.

use std::collections::{BTreeMap, HashMap, HashSet};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, MutexGuard, OnceLock};
use std::time::{Duration, Instant};

use serde_json::Value;
use tokio::sync::mpsc;
use tokio::task::JoinHandle;
use tracing::{debug, warn};

use terrarium_core::config::{DeliveryDef, TargetDef, TargetKind};

use crate::actor::Actor;
use crate::api::Shared;
use crate::db::{Tx, now};
use crate::error::{ApiError, ApiResult};
use crate::executor::{self, Collected, Control, End, RemoteCmd};
use crate::policy;
use crate::secrets;
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
/// The column of a card whose job is open.
pub const COL_VALIDATING: &str = "validating";
/// Where a validate pass goes.
pub const COL_REVIEW: &str = "review";
/// Where a merge pass goes.
pub const COL_ACCEPTED: &str = "accepted";
/// Where a failure goes, at the top.
pub const COL_READY: &str = "ready";
/// The most output of a prepare or push step that the daemon reads.
const STEP_MAX: usize = 64 * 1024;
/// The most output of the recipe that the daemon reads per stream.
const RECIPE_MAX: usize = 1024 * 1024;
/// The time limit of a prepare, push, or cleanup step.
const STEP_TIMEOUT: u64 = 900;
/// How long a job waits for its host before the queue notes it on the
/// cards (TER-160).
pub const LONG_WAIT: Duration = Duration::from_secs(30 * 60);
/// How long a job whose ssh target could not be reached waits before the
/// next try (TER-160).
pub const RETRY: Duration = Duration::from_secs(60);

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

/// Start the queue: fail the jobs that the last daemon left running, then
/// tick.
pub fn spawn(state: &Shared) -> JoinHandle<()> {
    let weak = Arc::downgrade(state);
    tokio::spawn(async move {
        if let Some(state) = weak.upgrade() {
            if let Err(e) = prepare_hosts(&state.db, None).await {
                warn!("cannot initialize stored job host keys: {e}");
            }
            match state.db.write("system", jobs::fail_lost).await {
                Ok(lost) => {
                    for j in lost {
                        if let Err(e) = after_end(&state, &j).await {
                            warn!(job = %j.id, "cannot handle a lost job: {e}");
                        }
                    }
                }
                Err(e) => warn!("cannot fail the lost jobs: {e}"),
            }
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
    // The live runs are read and added in the write, so two ticks and a
    // cancel (all writes) see them in order.
    let st = state.clone();
    let started = state
        .db
        .write("system", move |t| {
            let draining: Vec<String> = live().runs.iter().cloned().collect();
            let mut out = Vec::new();
            for (j, host, def) in jobs::startable(t, &draining)? {
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
                if jobs::start(t, &j.id, &host)? {
                    let run = LiveRun::new(&j.id);
                    for c in j.cards() {
                        set_labels(t, &c.card, &[LABEL_RUNNING], &[LABEL_QUEUED])?;
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
) -> ApiResult<Collected> {
    let (running, report) = crate::run_kill::start(state, def, cmd)?;
    set_control(job, Some(running.ctl.clone()));
    // A kill between the check in [`StepCtx::run`] and set_control found
    // the old control or none: kill this step now.
    if killed(job) {
        let _ = running.ctl.send(Control::Kill);
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
            if j.is_some() {
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
    let secret = secret.as_ref().map(|(_, s)| s);
    let dir = format!(
        "{}/{}/{}",
        def.work_dir().trim_end_matches('/'),
        job.project,
        job.id
    );
    check_ref(&job.branch)?;
    if let Some(c) = &job.commit {
        check_commit(c)?;
    }
    let holder = Some(format!("{}/jobs", job.project));
    let ionice = has_ionice(state, job, &target, &row.def, holder.clone()).await;
    let ctx = StepCtx {
        state,
        job,
        def: &row.def,
        url: &repo.url,
        dir: &dir,
        secret,
        holder: holder.clone(),
        ionice,
        known_hosts: executor::git_known_hosts(state),
        recipe_started,
    };
    let result = steps(&ctx, recipe, def.timeout_secs()).await;
    let clean = RemoteCmd {
        argv: vec![
            "sh".into(),
            "-c".into(),
            CLEAN_SCRIPT.into(),
            "sh".into(),
            dir.clone(),
        ],
        timeout_secs: Some(STEP_TIMEOUT),
        holder,
        ..RemoteCmd::default()
    };
    match step(state, &job.id, &row.def, &clean, STEP_MAX).await {
        Ok(c) if c.end.as_ref().is_some_and(End::success) => {}
        Ok(c) => warn!(job = %job.id, "cannot remove the job clone: {}", end_text(c.end.as_ref())),
        Err(e) => warn!(job = %job.id, "cannot remove the job clone: {e}"),
    }
    result
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
    match step(state, &job.id, def, &probe, STEP_MAX).await {
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
    url: &'a str,
    dir: &'a str,
    secret: Option<&'a crate::secrets::Secret>,
    holder: Option<String>,
    /// The target has a working `ionice` ([`has_ionice`]).
    ionice: bool,
    /// The daemon's known hosts ([`executor::git_known_hosts`]).
    known_hosts: Option<String>,
    /// Set when the recipe step started: a failure after it is a failure,
    /// also when the host is gone then (TER-160).
    recipe_started: &'a AtomicBool,
}

impl StepCtx<'_> {
    /// A git step with the credential in its env.
    fn git_cmd(&self, script: &str, args: &[&str]) -> RemoteCmd {
        let mut argv = vec![
            "sh".to_owned(),
            "-c".to_owned(),
            format!("{GIT_PRELUDE}{script}"),
            "sh".to_owned(),
        ];
        argv.extend(args.iter().map(|a| (*a).to_owned()));
        let mut env = executor::checkout_env(self.secret);
        if let Some(k) = &self.known_hosts {
            env.insert("TERRARIUM_GIT_KNOWN_HOSTS".into(), k.clone());
        }
        // The key file and the host keys go in a 0700 dir of the job, not
        // in /tmp. A path that the shell would split gets a temp dir.
        let ssh_dir = format!("{}.ssh", self.dir);
        if ssh_dir
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || "/._-".contains(c))
        {
            env.insert("TERRARIUM_GIT_SSH_DIR".into(), ssh_dir);
        }
        RemoteCmd {
            argv,
            env,
            timeout_secs: Some(STEP_TIMEOUT),
            holder: self.holder.clone(),
            ..RemoteCmd::default()
        }
    }

    /// A git step without the credential.
    fn plain_cmd(&self, script: &str, args: &[&str]) -> RemoteCmd {
        RemoteCmd {
            env: Default::default(),
            ..self.git_cmd(script, args)
        }
    }

    /// Run a step at low IO and CPU priority ([`low_priority`]). A job
    /// that got a [`kill`] starts no more steps.
    async fn run(&self, cmd: &RemoteCmd, max: usize) -> ApiResult<Collected> {
        if killed(&self.job.id) {
            return Err(ApiError::conflict("the job was cancelled"));
        }
        let cmd = RemoteCmd {
            argv: low_priority(cmd.argv.clone(), self.ionice),
            ..cmd.clone()
        };
        step(self.state, &self.job.id, self.def, &cmd, max).await
    }
}

/// Prepare, run the recipe, and push `next` for a merge.
async fn steps(x: &StepCtx<'_>, recipe: String, timeout: u64) -> ApiResult<JobEnd> {
    let job = x.job;
    let mut args: Vec<&str> = vec![
        x.url,
        x.dir,
        job.kind.as_str(),
        &job.branch,
        job.commit.as_deref().unwrap_or(""),
    ];
    // The other branches of a batch, in order, after the first.
    let cards = job.cards();
    if job.kind == JobKind::Merge {
        for c in cards.iter().skip(1) {
            check_ref(&c.branch)?;
            args.push(&c.branch);
        }
    }
    let prep = x.git_cmd(PREPARE_SCRIPT, &args);
    let c = x.run(&prep, STEP_MAX).await?;
    let r = result_line(&c.stdout);
    // The batch with the cards that conflicted marked.
    let batch = (!job.batch.is_empty()).then(|| mark_conflicts(&job.batch, &c.stdout));
    let head = match r.get("outcome").map(String::as_str) {
        Some("ok") => r.get("head").cloned().filter(|h| !h.is_empty()),
        Some("conflict") => {
            let what = if job.batch.is_empty() {
                format!("the branch `{}` does", job.branch)
            } else {
                "none of the branches of the batch does".to_owned()
            };
            return Ok(JobEnd {
                passed: false,
                outcome: Some("conflict".into()),
                exit_code: exit_code(c.end.as_ref()),
                tail: Some(format!(
                    "{what} not merge onto `next` without conflicts; the queue does not resolve them. Files:\n{}",
                    output_of(&c)
                )),
                head: None,
                batch,
            });
        }
        _ => None,
    };
    let Some(head) = head else {
        return Ok(JobEnd {
            passed: false,
            outcome: Some("error".into()),
            exit_code: exit_code(c.end.as_ref()),
            tail: Some(format!(
                "the prepare step failed ({}):\n{}",
                end_text(c.end.as_ref()),
                output_of(&c)
            )),
            head: None,
            batch,
        });
    };
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
    let c = x.run(&recipe_cmd, RECIPE_MAX).await?;
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
    // The push never runs in the clone that the recipe ran in: a fresh
    // repo gets the recorded merge commit, and only that commit goes out.
    check_commit(&head)?;
    let push_dir = format!("{}.push", x.dir);
    let collect = x.plain_cmd(COLLECT_SCRIPT, &[x.dir, &push_dir, &head]);
    let c = x.run(&collect, STEP_MAX).await?;
    if result_line(&c.stdout).get("outcome").map(String::as_str) != Some("ok") {
        return Ok(JobEnd {
            passed: false,
            outcome: Some("error".into()),
            exit_code: exit_code(c.end.as_ref()),
            tail: Some(format!(
                "the recipe passed, but the merge commit {head} could not be taken from the job clone ({}); did the recipe change the clone?\n{}",
                end_text(c.end.as_ref()),
                output_of(&c)
            )),
            head: Some(head),
            batch,
        });
    }
    let push = x.git_cmd(PUSH_SCRIPT, &[x.url, &push_dir, &head]);
    let c = x.run(&push, STEP_MAX).await?;
    if result_line(&c.stdout).get("outcome").map(String::as_str) == Some("ok") {
        return Ok(result);
    }
    Ok(JobEnd {
        passed: false,
        outcome: Some("error".into()),
        exit_code: exit_code(c.end.as_ref()),
        tail: Some(format!(
            "the recipe passed, but the push to `next` failed ({}); did `next` move?\n{}",
            end_text(c.end.as_ref()),
            output_of(&c)
        )),
        head: Some(head),
        batch,
    })
}

/// The file of the changed paths in a job clone, relative to its root
/// (written by [`PREPARE_SCRIPT`]).
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
/// ID, the tested head, and with a base, the base and the absolute path
/// of the changed files. No credential goes in here.
fn recipe_env(job: &Job, dir: &str, head: &str, base: Option<&str>) -> BTreeMap<String, String> {
    let mut env = BTreeMap::new();
    env.insert("TERRARIUM_JOB_KIND".into(), job.kind.as_str().to_owned());
    env.insert("TERRARIUM_JOB_ID".into(), job.id.clone());
    env.insert("TERRARIUM_HEAD".into(), head.to_owned());
    if let Some(b) = base {
        env.insert("TERRARIUM_BASE".into(), b.to_owned());
        env.insert(
            "TERRARIUM_CHANGED_FILES".into(),
            format!("{}/{CHANGED_FILES}", dir.trim_end_matches('/')),
        );
    }
    env
}

/// The conflicts that the prepare step of a batch printed: each
/// `TERRARIUM_CONFLICT <index>` line, then the conflicting files, by the
/// index of the branch in the batch.
fn conflicts(stdout: &[u8]) -> HashMap<usize, String> {
    let text = String::from_utf8_lossy(stdout);
    let mut out: HashMap<usize, String> = HashMap::new();
    let mut at: Option<usize> = None;
    for line in text.lines() {
        if let Some(i) = line.strip_prefix("TERRARIUM_CONFLICT\t") {
            at = i.trim().parse().ok();
            if let Some(i) = at {
                out.entry(i).or_default();
            }
        } else if line.starts_with("TERRARIUM_") {
            at = None;
        } else if let Some(i) = at {
            let files = out.entry(i).or_default();
            if !files.is_empty() {
                files.push('\n');
            }
            files.push_str(line);
        }
    }
    out
}

/// The batch with each card that conflicted marked `conflict`, with its
/// files.
fn mark_conflicts(batch: &[BatchCard], stdout: &[u8]) -> Vec<BatchCard> {
    let found = conflicts(stdout);
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
/// ls-remote` on the project's delivery target (the daemon may not reach
/// the forge itself). `None`: the branch is not on the remote.
pub async fn remote_head(
    state: &Shared,
    project: &str,
    repo_name: &str,
    target: &str,
    branch: &str,
) -> ApiResult<Option<String>> {
    check_ref(branch)?;
    let (p, r, tn) = (project.to_owned(), repo_name.to_owned(), target.to_owned());
    let (repo, row) = state
        .db
        .read(move |c| {
            let repo = repos::find_by_name(c, &p, &r)?
                .ok_or_else(|| ApiError::not_found(format!("no repo `{r}` in `{p}`")))?;
            Ok((repo, targets::get(c, &tn)?))
        })
        .await?;
    let secret = secrets::secret_of(state, repo.owner(), repo.credential.as_deref()).await?;
    let cmd = RemoteCmd {
        argv: vec![
            "sh".into(),
            "-c".into(),
            format!("{GIT_PRELUDE}{LS_REMOTE_SCRIPT}"),
            "sh".into(),
            repo.url.clone(),
            branch.to_owned(),
        ],
        env: {
            let mut env = executor::checkout_env(secret.as_ref().map(|(_, s)| s));
            if let Some(k) = executor::git_known_hosts(state) {
                env.insert("TERRARIUM_GIT_KNOWN_HOSTS".into(), k);
            }
            env
        },
        timeout_secs: Some(120),
        holder: Some(format!("{project}/jobs")),
        ..RemoteCmd::default()
    };
    let c = executor::start_cmd(state, &row.def, &cmd)?
        .collect(STEP_MAX)
        .await;
    let r = result_line(&c.stdout);
    match r.get("outcome").map(String::as_str) {
        Some("ok") => Ok(r.get("head").cloned().filter(|h| !h.is_empty())),
        _ => Err(ApiError::conflict(format!(
            "cannot read branch `{branch}` of `{repo_name}` on target `{target}` ({}): {}",
            end_text(c.end.as_ref()),
            output_of(&c).trim()
        ))),
    }
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
fn failure_block(j: &Job) -> String {
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

/// What happens after a job ends: the card move, or the rerun.
async fn after_end(state: &Shared, j: &Job) -> ApiResult<()> {
    let j = j.clone();
    state
        .db
        .write("system", move |t| {
            if j.batch.is_empty() {
                single_end(t, &j)
            } else {
                batch_end(t, &j)
            }
        })
        .await?;
    kick(state);
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
        let note = format!(
            "{} It runs once more as job {} (flaky tests).",
            end_comment(j, "failed"),
            next.id
        );
        return comment(t, &j.card, &note);
    }
    let verdict = if split_red {
        "failed (red alone after its batch was red)"
    } else {
        "failed again"
    };
    fail_card(t, j, verdict)
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
        fail_card(t, &card_view(j, c), "left the batch on a conflict")?;
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
        let names = |x: &Job| {
            x.cards()
                .iter()
                .map(|c| c.card.clone())
                .collect::<Vec<_>>()
                .join(" ")
        };
        for c in &merged {
            let v = card_view(j, c);
            set_labels(t, &c.card, &[LABEL_QUEUED], &[LABEL_RUNNING])?;
            let note = format!(
                "{} The batch splits to find the failing card: job {} ({}) first, then job {} ({}).",
                end_comment(&v, "failed"),
                left.id,
                names(&left),
                right.id,
                names(&right)
            );
            comment(t, &c.card, &note)?;
        }
        return Ok(());
    }
    // One card merged and the batch is red, or the job could not run.
    let split_red = red && j.parent.is_some();
    if j.attempt < 2 && !split_red {
        let next = jobs::rerun(t, j, Some(&merged))?;
        for c in &merged {
            set_labels(t, &c.card, &[LABEL_QUEUED], &[LABEL_RUNNING])?;
            let note = format!(
                "{} It runs once more as job {}.",
                end_comment(&card_view(j, c), "failed"),
                next.id
            );
            comment(t, &c.card, &note)?;
        }
        return Ok(());
    }
    for c in &merged {
        fail_card(t, &card_view(j, c), "failed again")?;
    }
    Ok(())
}

/// The job as one card of its batch sees it: the card, its branch, and
/// its worker. A card that conflicted gets the conflict as its outcome.
fn card_view(j: &Job, c: &BatchCard) -> Job {
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
        JobKind::Validate => COL_REVIEW,
        JobKind::Merge => COL_ACCEPTED,
    };
    let note = end_comment(j, "passed");
    move_or_comment(t, &j.card, to, None, &note)?;
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
    let body = format!(
        "{}\nThe card is at the top of ready, for you first. The failure block is at the top of the card body.\n",
        end_comment(j, "failed")
    );
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

/// The git setup that each credential step shares: no user or system
/// config, no hooks, no prompts, the credential only from the env.
/// Copied from [`executor::CHECKOUT_SCRIPT`]'s preamble.
const GIT_PRELUDE: &str = r#"set -u
unset GIT_DIR GIT_WORK_TREE GIT_INDEX_FILE GIT_OBJECT_DIRECTORY GIT_ALTERNATE_OBJECT_DIRECTORIES \
  GIT_CONFIG GIT_CONFIG_PARAMETERS GIT_CONFIG_COUNT GIT_NAMESPACE GIT_CREDENTIAL_HELPER \
  GIT_TEMPLATE_DIR GIT_EXEC_PATH GIT_EXTERNAL_DIFF GIT_PAGER GIT_EDITOR GIT_PROXY_COMMAND \
  GIT_SSH GIT_SSH_VARIANT GIT_CEILING_DIRECTORIES GIT_DISCOVERY_ACROSS_FILESYSTEM
export GIT_CONFIG_GLOBAL=/dev/null GIT_CONFIG_NOSYSTEM=1 GIT_TERMINAL_PROMPT=0
export GIT_ASKPASS=false SSH_ASKPASS=false LC_ALL=C
# ssh reads no config file (`-F /dev/null`: a planted ~/.ssh/config
# cannot redirect the host or run a ProxyCommand), and keeps its host keys
# and the key file in a 0700 dir of its own: TERRARIUM_GIT_SSH_DIR (the
# job's dir), else a new temp dir. TERRARIUM_GIT_KNOWN_HOSTS seeds the
# known hosts (the daemon's).
sshdir=${TERRARIUM_GIT_SSH_DIR:-}
sshtmp=
if [ -n "$sshdir" ]; then
  mkdir -p -m 700 -- "$sshdir" && chmod 700 -- "$sshdir" || { echo "cannot make $sshdir"; exit 2; }
else
  sshdir=$(mktemp -d) || exit 2
  sshtmp=$sshdir
fi
printf '%s\n' "${TERRARIUM_GIT_KNOWN_HOSTS:-}" > "$sshdir/known_hosts"
unset TERRARIUM_GIT_KNOWN_HOSTS TERRARIUM_GIT_SSH_DIR
sshbase="ssh -F /dev/null -o UserKnownHostsFile=$sshdir/known_hosts -o GlobalKnownHostsFile=/dev/null -o BatchMode=yes -o StrictHostKeyChecking=accept-new"
export GIT_SSH_COMMAND="$sshbase"
helper=
keyfile=
trap 'if [ -n "$keyfile" ]; then rm -f "$keyfile"; fi; if [ -n "$sshtmp" ]; then rm -rf "$sshtmp"; fi' EXIT
if [ -n "${TERRARIUM_GIT_PASSWORD:-}" ]; then
  helper='!f() { test "$1" = get || exit 0; echo "username=$TERRARIUM_GIT_USERNAME"; echo "password=$TERRARIUM_GIT_PASSWORD"; }; f'
fi
if [ -n "${TERRARIUM_GIT_SSH_KEY:-}" ]; then
  keyfile=$sshdir/key
  (umask 077 && printf '%s\n' "$TERRARIUM_GIT_SSH_KEY" > "$keyfile")
  export GIT_SSH_COMMAND="$sshbase -i $keyfile -o IdentitiesOnly=yes"
fi
unset TERRARIUM_GIT_SSH_KEY
G() {
  git -c core.hooksPath=/dev/null -c core.fsmonitor=false -c core.untrackedCache=false \
    -c credential.helper= -c protocol.allow=never -c protocol.https.allow=always \
    -c protocol.http.allow=always -c protocol.ssh.allow=always -c protocol.file.allow=always \
    -c protocol.git.allow=never -c protocol.ext.allow=never -c protocol.fd.allow=never \
    -c protocol.ftp.allow=never -c protocol.ftps.allow=never -c fetch.recurseSubmodules=false \
    -c submodule.recurse=false -c gc.auto=0 -c maintenance.auto=false \
    -c merge.verifySignatures=false -c advice.detachedHead=false -c core.pager=cat \
    -c user.name=terrarium -c user.email=terrarium@localhost -c commit.gpgsign=false "$@"
}
g() {
  if [ -n "$helper" ]; then G -c "credential.helper=$helper" "$@"; else G "$@"; fi
}
forget() {
  unset TERRARIUM_GIT_PASSWORD TERRARIUM_GIT_USERNAME
  helper=
  if [ -n "$keyfile" ]; then rm -f "$keyfile"; fi
  keyfile=
  export GIT_SSH_COMMAND="$sshbase"
}
res() { printf 'TERRARIUM_JOB\toutcome=%s\thead=%s\n' "$1" "${2:-}"; }
"#;

/// Step 1: `url dir kind branch commit [branch...]` (the other branches of
/// a batch merge, in order). Prints `TERRARIUM_CONFLICT <index>` and the
/// files for each branch that conflicts, then `outcome=ok head=<sha>
/// base=<sha or empty>`, `outcome=conflict`, or `outcome=error`. With a
/// base, the changed files are in [`CHANGED_FILES`] of the clone.
const PREPARE_SCRIPT: &str = r#"url=$1; dir=$2; kind=$3; branch=$4; commit=$5
shift 5
case "$dir" in /*) ;; *) echo "the job dir is not absolute"; res error; exit 2 ;; esac
rm -rf -- "$dir" && mkdir -p -- "$dir" && G -C "$dir" init -q || { forget; res error; exit 2; }
if [ "$kind" = merge ]; then
  # The branches of the merge, in order: the job's, then the rest of a
  # batch. The daemon checked each name (no space, no glob character).
  set -- "$branch" "$@"
  specs="+refs/heads/next:refs/remotes/job/next"
  i=0
  for b in "$@"; do specs="$specs +refs/heads/$b:refs/remotes/job/card$i"; i=$((i + 1)); done
  set -f
  # shellcheck disable=SC2086
  if ! g -C "$dir" fetch -q -- "$url" $specs; then
    forget; echo "cannot fetch next and $* (does the repo have a next branch?)"; res error; exit 2
  fi
  set +f
  forget
  G -C "$dir" checkout -q -B next refs/remotes/job/next || { res error; exit 2; }
  # A branch that conflicts leaves the merge: its merge is aborted, and
  # the next branch merges onto the ones before it.
  i=0; merged=0
  for b in "$@"; do
    if out=$(G -C "$dir" merge -q --no-ff --no-edit -m "Merge $b onto next" "refs/remotes/job/card$i" 2>&1); then
      merged=$((merged + 1))
    else
      printf 'TERRARIUM_CONFLICT\t%s\n' "$i"
      G -C "$dir" diff --name-only --diff-filter=U
      # A merge that fails without a conflict (for example unrelated
      # histories) leaves no merge to abort: say why, and reset the tree,
      # so that only this branch leaves the merge.
      if ! G -C "$dir" merge --abort 2>/dev/null; then
        printf 'the merge failed without a conflict:\n%s\n' "$out"
        G -C "$dir" reset -q --hard HEAD || { res error; exit 2; }
      fi
    fi
    i=$((i + 1))
  done
  if [ "$merged" = 0 ]; then res conflict; exit 0; fi
  G -C "$dir" update-ref refs/job/merge HEAD || { res error; exit 2; }
else
  if ! g -C "$dir" fetch -q -- "$url" "+refs/heads/$branch:refs/remotes/job/card"; then
    forget; echo "cannot fetch $branch"; res error; exit 2
  fi
  # `next` gives the base of the changed files. A repo without it is no
  # error: the recipe then gets no list of changed files.
  g -C "$dir" fetch -q -- "$url" "+refs/heads/next:refs/remotes/job/next" > /dev/null 2>&1 || true
  forget
  if [ -n "$commit" ]; then
    if ! G -C "$dir" merge-base --is-ancestor "$commit" refs/remotes/job/card 2>/dev/null; then
      echo "commit $commit is not on $branch"; res error; exit 2
    fi
    G -C "$dir" checkout -q --detach "$commit" || { res error; exit 2; }
  else
    G -C "$dir" checkout -q --detach refs/remotes/job/card || { res error; exit 2; }
  fi
fi
# The changed files for the recipe (TER-100): the paths that differ
# between the base and HEAD, one per line, in .git/terrarium/changed-files.
# The base of a merge is `next` before the merge, of a validate the
# merge-base of the commit and `next`. No base: no file, and `base=`.
if [ "$kind" = merge ]; then
  base=$(G -C "$dir" rev-parse -q --verify "refs/remotes/job/next^{commit}" 2>/dev/null) || base=
else
  base=$(G -C "$dir" merge-base HEAD refs/remotes/job/next 2>/dev/null) || base=
fi
if [ -n "$base" ]; then
  mkdir -p -- "$dir/.git/terrarium" \
    && G -C "$dir" -c core.quotePath=false diff --no-ext-diff --no-textconv --name-only --no-renames "$base" HEAD \
      > "$dir/.git/terrarium/changed-files" \
    || { rm -f -- "$dir/.git/terrarium/changed-files"; base=; }
fi
printf 'TERRARIUM_JOB\toutcome=ok\thead=%s\tbase=%s\n' "$(G -C "$dir" rev-parse HEAD)" "$base"
"#;

/// `url branch`: the head of the branch on the remote (empty when the
/// remote has no such branch).
const LS_REMOTE_SCRIPT: &str = r#"url=$1; branch=$2
if out=$(g ls-remote -- "$url" "refs/heads/$branch"); then
  forget; res ok "$(printf '%s\n' "$out" | awk 'NR==1 {print $1}')"
else
  forget; res error; exit 2
fi
"#;

/// Step 3a, without the credential: `dir push sha`. A fresh bare repo at
/// `push` fetches `refs/job/merge` (made in step 1, before the recipe ran)
/// from the job clone over `file://`, and must find exactly the merge
/// commit `sha` that step 1 recorded. The fetch checks the object IDs, so
/// the content is the validated merge whatever the recipe did to the
/// clone; a moved or deleted ref fails the job. Nothing of the clone's
/// config reaches step 3b.
const COLLECT_SCRIPT: &str = r#"dir=$1; push=$2; sha=$3
case "$sha" in *[!0-9a-f]*|'') echo "bad merge commit"; res error; exit 2 ;; esac
case "$push" in /*/*) ;; *) echo "the push dir is not absolute"; res error; exit 2 ;; esac
rm -rf -- "$push" && G init -q --bare --template= "$push" || { echo "cannot make the push repo"; res error; exit 2; }
if ! G -C "$push" fetch -q --no-tags -- "file://$dir/.git" "+refs/job/merge:refs/heads/merge"; then
  echo "refs/job/merge is not in the job clone any more"; res error; exit 2
fi
if [ "$(G -C "$push" rev-parse --verify -q "refs/heads/merge^{commit}")" != "$sha" ]; then
  echo "refs/job/merge of the job clone is not the merge commit $sha"; res error; exit 2
fi
res ok "$sha"
"#;

/// Step 3b: `url push sha`. Push exactly the merge commit `sha` to `next`
/// from the fresh repo of step 3a, not forced. The push repo must still
/// hold `sha` as `refs/heads/merge`.
const PUSH_SCRIPT: &str = r#"url=$1; push=$2; sha=$3
case "$sha" in *[!0-9a-f]*|'') forget; echo "bad merge commit"; res error; exit 2 ;; esac
if [ "$(G -C "$push" rev-parse --verify -q "refs/heads/merge^{commit}")" != "$sha" ]; then
  forget; echo "the push repo does not hold the merge commit $sha"; res error; exit 2
fi
if g -C "$push" push -q -- "$url" "$sha:refs/heads/next"; then forget; res ok; else forget; res error; exit 2; fi
"#;

/// The end: `dir`. Remove the clone, the push repo, and the ssh dir.
const CLEAN_SCRIPT: &str =
    r#"case "$1" in /*/*) rm -rf -- "$1" "$1.push" "$1.ssh" ;; *) exit 2 ;; esac"#;

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

    /// Review of TER-71, major 1: ssh in the git steps reads no config
    /// file, keeps its host keys and the key file in the job's 0700 dir,
    /// and `forget` removes the key. ssh finds `~/.ssh/config` through the
    /// passwd home, not `$HOME`, so a test cannot plant one without
    /// touching the real file; `ssh -G` prints the config that a
    /// connection would use instead.
    #[test]
    fn ssh_reads_no_config_and_keeps_the_key_in_the_job_dir() {
        if std::process::Command::new("ssh")
            .arg("-V")
            .output()
            .is_err()
        {
            eprintln!("no ssh on this machine: skipped");
            return;
        }
        let tmp = tempfile::tempdir().unwrap();
        let dir = tmp.path().join("job.ssh");
        let d = dir.display().to_string();
        let script = format!(
            "{GIT_PRELUDE}{}",
            r#"ls -ld "$sshdir" | cut -c1-10
ls -l "$keyfile" | cut -c1-10
cat "$sshdir/known_hosts"
eval "$GIT_SSH_COMMAND -G forge.example" | grep -E '^(hostname|proxycommand|userknownhostsfile|identityfile) '
forget
if [ -e "$sshdir/key" ]; then echo key-left; else echo key-gone; fi
echo "after: $GIT_SSH_COMMAND"
"#
        );
        let out = std::process::Command::new("sh")
            .arg("-c")
            .arg(script)
            .env_clear()
            .env("PATH", std::env::var("PATH").unwrap())
            .env("TERRARIUM_GIT_SSH_DIR", &dir)
            .env("TERRARIUM_GIT_SSH_KEY", "not a key")
            .env(
                "TERRARIUM_GIT_KNOWN_HOSTS",
                "forge.example ssh-ed25519 AAAAtest",
            )
            .output()
            .unwrap();
        let text = String::from_utf8_lossy(&out.stdout);
        assert!(
            out.status.success(),
            "{text}\n{}",
            String::from_utf8_lossy(&out.stderr)
        );
        let lines: Vec<&str> = text.lines().collect();
        assert_eq!(lines[0], "drwx------", "{text}");
        assert_eq!(lines[1], "-rw-------", "{text}");
        assert_eq!(lines[2], "forge.example ssh-ed25519 AAAAtest", "{text}");
        assert!(text.contains("hostname forge.example\n"), "{text}");
        assert!(
            text.contains(&format!("userknownhostsfile {d}/known_hosts")),
            "{text}"
        );
        assert!(text.contains(&format!("identityfile {d}/key")), "{text}");
        assert!(
            !text
                .lines()
                .any(|l| l.starts_with("proxycommand ") && l != "proxycommand none"),
            "{text}"
        );
        assert!(text.contains("key-gone"), "{text}");
        assert!(text.contains("after: ssh -F /dev/null "), "{text}");
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
    fn batch_conflicts_are_read_by_index() {
        let out = b"TERRARIUM_CONFLICT\t1\nREADME\nsrc/a.rs\nTERRARIUM_CONFLICT\t3\nb.txt\nTERRARIUM_JOB\toutcome=ok\thead=abc\n";
        let found = conflicts(out);
        assert_eq!(found.len(), 2);
        assert_eq!(found[&1], "README\nsrc/a.rs");
        assert_eq!(found[&3], "b.txt");
        let card = |c: &str| BatchCard {
            card: c.into(),
            branch: c.to_lowercase(),
            by_agent: None,
            outcome: None,
            note: None,
        };
        let batch = vec![card("T-1"), card("T-2"), card("T-3"), card("T-4")];
        let marked = mark_conflicts(&batch, out);
        let outcomes: Vec<Option<&str>> = marked.iter().map(|c| c.outcome.as_deref()).collect();
        assert_eq!(outcomes, [None, Some("conflict"), None, Some("conflict")]);
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
