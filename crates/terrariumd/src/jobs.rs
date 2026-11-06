//! The job queue of a project (TER-71): the run time. The rows are in
//! [`crate::store::jobs`].
//!
//! - A tick (each [`TICK`], and at once after a queue or an end) starts
//!   the queued jobs that have a place: first in, first out across
//!   projects, at most the host cap of the target's host (TER-97), then
//!   at most the project's `slots`; a merge job runs alone in its
//!   project. The pick and the start are one write, so two ticks cannot
//!   overfill the slots.
//! - A job is three commands on the project's target, each its own run:
//!   1. prepare, with the repo credential in its env: a fresh clone in
//!      `<work_dir>/<project>/<job>`. A validate job checks out the card
//!      commit. A merge job checks out `next` and merges the card branch
//!      with `--no-ff`; a conflict ends the job (never resolved, never
//!      rerun). A batch merge (TER-112) merges its card branches in
//!      order; a card that conflicts leaves the batch (its merge is
//!      aborted) and the others go on. When none merges, the job ends as
//!      a conflict.
//!   2. the recipe (`sh -c <recipe>` in the clone), without any
//!      credential: it runs the repo's code.
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

use std::collections::HashMap;
use std::sync::{Arc, Mutex, OnceLock};
use std::time::Duration;

use serde_json::Value;
use tokio::sync::mpsc;
use tokio::task::JoinHandle;
use tracing::{debug, warn};

use terrarium_core::config::DeliveryDef;

use crate::actor::Actor;
use crate::api::Shared;
use crate::db::{Tx, now};
use crate::error::{ApiError, ApiResult};
use crate::executor::{self, Collected, Control, End, RemoteCmd};
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

/// Kill the current step of a running job. The job then ends as
/// cancelled (its row is cancelled first, so the end does nothing).
pub fn kill(job: &str) {
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

/// Start a tick now, in the background.
pub fn kick(state: &Shared) {
    let state = state.clone();
    tokio::spawn(async move {
        if let Err(e) = tick(&state).await {
            warn!("job queue tick failed: {e}");
        }
    });
}

/// Start each queued job that has a place: on its host (the host cap,
/// across projects) and in its project (the slots). One write for all
/// projects, so the host counts hold.
pub async fn tick(state: &Shared) -> ApiResult<()> {
    let started = state
        .db
        .write("system", move |t| {
            let mut out = Vec::new();
            for (j, host, def) in jobs::startable(t)? {
                if jobs::start(t, &j.id, &host)? {
                    for c in j.cards() {
                        set_labels(t, &c.card, &[LABEL_RUNNING], &[LABEL_QUEUED])?;
                    }
                    out.push((jobs::get(t, &j.id)?, def));
                }
            }
            Ok(out)
        })
        .await?;
    for (job, def) in started {
        let state = state.clone();
        tokio::spawn(async move {
            let end = run(&state, &job, &def).await;
            set_control(&job.id, None);
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
            kick(&state);
        });
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
async fn step(
    state: &Shared,
    job: &str,
    def: &terrarium_core::config::TargetDef,
    cmd: &RemoteCmd,
    max: usize,
) -> ApiResult<Collected> {
    let running = executor::start_cmd(state, def, cmd)?;
    set_control(job, Some(running.ctl.clone()));
    Ok(running.collect(max).await)
}

/// Run a job to its end. Never fails: an error is a failed end.
async fn run(state: &Shared, job: &Job, def: &DeliveryDef) -> JobEnd {
    match run_inner(state, job, def).await {
        Ok(end) => end,
        Err(e) => error_end(format!("the job could not run: {}", e.message)),
    }
}

async fn run_inner(state: &Shared, job: &Job, def: &DeliveryDef) -> ApiResult<JobEnd> {
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
            // Keep only an answer: an exit code. An error, a signal, or a
            // timeout is checked again on the next job.
            if matches!(c.end, Some(End::Exit { code: Some(_), .. })) {
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

    /// Run a step at low IO and CPU priority ([`low_priority`]).
    async fn run(&self, cmd: &RemoteCmd, max: usize) -> ApiResult<Collected> {
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
    let recipe_cmd = RemoteCmd {
        argv: vec!["sh".into(), "-c".into(), recipe],
        cwd: Some(x.dir.to_owned()),
        timeout_secs: Some(timeout),
        holder: x.holder.clone(),
        ..RemoteCmd::default()
    };
    let c = x.run(&recipe_cmd, RECIPE_MAX).await?;
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
        s.push_str(&format!(
            ", batch of {}: {}",
            cards.len(),
            cards.join(" ")
        ));
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
            "the branch `{}` does not merge onto `next` and the cards before it in the batch; the queue does not resolve conflicts. Files:\n{}",
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
/// files for each branch that conflicts, then `outcome=ok head=<sha>`,
/// `outcome=conflict`, or `outcome=error`.
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
    if G -C "$dir" merge -q --no-ff --no-edit -m "Merge $b onto next" "refs/remotes/job/card$i"; then
      merged=$((merged + 1))
    else
      printf 'TERRARIUM_CONFLICT\t%s\n' "$i"
      G -C "$dir" diff --name-only --diff-filter=U
      G -C "$dir" merge --abort || { res error; exit 2; }
    fi
    i=$((i + 1))
  done
  if [ "$merged" = 0 ]; then res conflict; exit 0; fi
  G -C "$dir" update-ref refs/job/merge HEAD || { res error; exit 2; }
else
  if ! g -C "$dir" fetch -q -- "$url" "+refs/heads/$branch:refs/remotes/job/card"; then
    forget; echo "cannot fetch $branch"; res error; exit 2
  fi
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
res ok "$(G -C "$dir" rev-parse HEAD)"
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
}
