//! The job queue of a project (TER-71): the run time. The rows are in
//! [`crate::store::jobs`].
//!
//! - A tick (each [`TICK`], and at once after a queue or an end) starts
//!   the queued jobs that have a slot: first in, first out, at most the
//!   project's `slots`, one merge job per repo. The pick and the start are
//!   one write, so two ticks cannot overfill the slots.
//! - A job is three commands on the project's target, each its own run:
//!   1. prepare, with the repo credential in its env: a fresh clone in
//!      `<work_dir>/<project>/<job>`. A validate job checks out the card
//!      commit. A merge job checks out `next` and merges the card branch
//!      with `--no-ff`; a conflict ends the job (never resolved, never
//!      rerun).
//!   2. the recipe (`sh -c <recipe>` in the clone), without any
//!      credential: it runs the repo's code.
//!   3. a merge job only, when the recipe passed: push the merge commit
//!      to `next`. The recipe may have changed the clone (new commits, a
//!      moved HEAD, its `.git/config`), so the push never runs there:
//!      a. without any credential: a fresh bare repo `<clone>.push`
//!         fetches exactly the merge commit that step 1 recorded, over
//!         `file://`. A commit that is gone fails the job.
//!      b. with the credential: push that commit ID from the fresh repo
//!         to `next`. Not forced: when `next` moved, the push fails.
//!
//!   Then the clone and the push repo are removed.
//! - On the end the daemon moves the card itself (as the operator, with a
//!   comment that starts "Job queue:"): a pass moves a validate card to
//!   `review` and a merge card to `accepted`. A failure is rerun once
//!   (not a conflict). The second failure writes a "Validation failed"
//!   block at the top of the card body, moves the card to the top of
//!   `ready`, and mails the worker of its `by:<agent>` label. The final
//!   move (a pass, or the second failure) also clears the assignee.
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
use crate::store::jobs::{self, Job, JobEnd, JobKind};
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

/// Start each queued job that has a slot.
pub async fn tick(state: &Shared) -> ApiResult<()> {
    let projects = state.db.read(jobs::projects_with_queued).await?;
    for p in projects {
        let project = p.clone();
        let started = state
            .db
            .write("system", move |t| {
                let def = jobs::delivery(t, &project)?;
                if !def.enabled() {
                    return Ok(Vec::new());
                }
                let mut out = Vec::new();
                for j in jobs::startable(t, &project, def.slots())? {
                    if jobs::start(t, &j.id)? {
                        set_labels(t, &j.card, &[LABEL_RUNNING], &[LABEL_QUEUED])?;
                        out.push((jobs::get(t, &j.id)?, def.clone()));
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
    let ctx = StepCtx {
        state,
        job,
        def: &row.def,
        url: &repo.url,
        dir: &dir,
        secret,
        holder: holder.clone(),
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

/// What the steps of one job share.
struct StepCtx<'a> {
    state: &'a Shared,
    job: &'a Job,
    def: &'a terrarium_core::config::TargetDef,
    url: &'a str,
    dir: &'a str,
    secret: Option<&'a crate::secrets::Secret>,
    holder: Option<String>,
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
        RemoteCmd {
            argv,
            env: executor::checkout_env(self.secret),
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

    async fn run(&self, cmd: &RemoteCmd, max: usize) -> ApiResult<Collected> {
        step(self.state, &self.job.id, self.def, cmd, max).await
    }
}

/// Prepare, run the recipe, and push `next` for a merge.
async fn steps(x: &StepCtx<'_>, recipe: String, timeout: u64) -> ApiResult<JobEnd> {
    let job = x.job;
    let prep = x.git_cmd(
        PREPARE_SCRIPT,
        &[
            x.url,
            x.dir,
            job.kind.as_str(),
            &job.branch,
            job.commit.as_deref().unwrap_or(""),
        ],
    );
    let c = x.run(&prep, STEP_MAX).await?;
    let r = result_line(&c.stdout);
    let head = match r.get("outcome").map(String::as_str) {
        Some("ok") => r.get("head").cloned().filter(|h| !h.is_empty()),
        Some("conflict") => {
            return Ok(JobEnd {
                passed: false,
                outcome: Some("conflict".into()),
                exit_code: exit_code(c.end.as_ref()),
                tail: Some(format!(
                    "the branch `{}` does not merge onto `next` without conflicts; the queue does not resolve them. Files:\n{}",
                    job.branch,
                    output_of(&c)
                )),
                head: None,
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
    let result = JobEnd {
        passed: ok,
        outcome: (!ok).then(|| "red".to_owned()),
        exit_code: exit_code(c.end.as_ref()),
        tail: Some(format!(
            "{}\n[{}]",
            output_of(&c).trim_end(),
            end_text(c.end.as_ref())
        )),
        head: Some(head.clone()),
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
    })
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
        env: executor::checkout_env(secret.as_ref().map(|(_, s)| s)),
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
            if j.state == jobs::JobState::Passed {
                set_labels(t, &j.card, &[], &[LABEL_RUNNING, LABEL_QUEUED])?;
                let to = match j.kind {
                    JobKind::Validate => COL_REVIEW,
                    JobKind::Merge => COL_ACCEPTED,
                };
                let note = end_comment(&j, "passed");
                move_or_comment(t, &j.card, to, None, &note)?;
                return unassign(t, &j.card);
            }
            let rerun = j.attempt < 2 && j.outcome.as_deref() != Some("conflict");
            if rerun {
                let next = jobs::rerun(t, &j)?;
                set_labels(t, &j.card, &[LABEL_QUEUED], &[LABEL_RUNNING])?;
                let note = format!(
                    "{} It runs once more as job {} (flaky tests).",
                    end_comment(&j, "failed"),
                    next.id
                );
                return comment(t, &j.card, &note);
            }
            set_labels(t, &j.card, &[], &[LABEL_RUNNING, LABEL_QUEUED])?;
            let card = match board::get_card(t, &j.card) {
                Ok(c) => c,
                Err(e) if e.code.status().is_client_error() => return Ok(()),
                Err(e) => return Err(e),
            };
            let body = format!("{}{}", failure_block(&j), card.body);
            let patch = CardPatch {
                body: Some(body),
                ..CardPatch::default()
            };
            board::update_card(t, &Actor::Operator, &j.card, patch)?;
            let note = end_comment(&j, "failed again");
            move_or_comment(t, &j.card, COL_READY, Some(0), &note)?;
            unassign(t, &j.card)?;
            if let Some(who) = &j.by_agent {
                notify_worker(t, &j, who)?;
            }
            Ok(())
        })
        .await?;
    kick(state);
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
        end_comment(j, "failed twice")
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
export GIT_SSH_COMMAND="ssh -o BatchMode=yes -o StrictHostKeyChecking=accept-new"
helper=
keyfile=
if [ -n "${TERRARIUM_GIT_PASSWORD:-}" ]; then
  helper='!f() { test "$1" = get || exit 0; echo "username=$TERRARIUM_GIT_USERNAME"; echo "password=$TERRARIUM_GIT_PASSWORD"; }; f'
fi
if [ -n "${TERRARIUM_GIT_SSH_KEY:-}" ]; then
  keyfile=$(mktemp)
  trap 'rm -f "$keyfile"' EXIT
  chmod 600 "$keyfile"
  printf '%s\n' "$TERRARIUM_GIT_SSH_KEY" > "$keyfile"
  export GIT_SSH_COMMAND="ssh -i $keyfile -o IdentitiesOnly=yes -o BatchMode=yes -o StrictHostKeyChecking=accept-new"
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
  export GIT_SSH_COMMAND="ssh -o BatchMode=yes"
}
res() { printf 'TERRARIUM_JOB\toutcome=%s\thead=%s\n' "$1" "${2:-}"; }
"#;

/// Step 1: `url dir kind branch commit`. Prints `outcome=ok head=<sha>`,
/// `outcome=conflict`, or `outcome=error`.
const PREPARE_SCRIPT: &str = r#"url=$1; dir=$2; kind=$3; branch=$4; commit=$5
case "$dir" in /*) ;; *) echo "the job dir is not absolute"; res error; exit 2 ;; esac
rm -rf -- "$dir" && mkdir -p -- "$dir" && G -C "$dir" init -q || { forget; res error; exit 2; }
if [ "$kind" = merge ]; then
  if ! g -C "$dir" fetch -q -- "$url" "+refs/heads/next:refs/remotes/job/next" "+refs/heads/$branch:refs/remotes/job/card"; then
    forget; echo "cannot fetch next and $branch (does the repo have a next branch?)"; res error; exit 2
  fi
  forget
  G -C "$dir" checkout -q -B next refs/remotes/job/next || { res error; exit 2; }
  if ! G -C "$dir" merge -q --no-ff --no-edit -m "Merge $branch onto next" refs/remotes/job/card; then
    G -C "$dir" diff --name-only --diff-filter=U
    G -C "$dir" merge --abort
    res conflict; exit 0
  fi
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
/// `push` gets exactly the merge commit `sha` (recorded in step 1, before
/// the recipe ran) from the job clone over `file://`. The fetch checks the
/// object IDs, so the content is the validated merge whatever the recipe
/// did to the clone. Nothing of the clone's config reaches step 3b.
const COLLECT_SCRIPT: &str = r#"dir=$1; push=$2; sha=$3
case "$sha" in *[!0-9a-f]*|'') echo "bad merge commit"; res error; exit 2 ;; esac
case "$push" in /*/*) ;; *) echo "the push dir is not absolute"; res error; exit 2 ;; esac
rm -rf -- "$push" && G init -q --bare --template= "$push" || { echo "cannot make the push repo"; res error; exit 2; }
if ! G -c uploadpack.allowAnySHA1InWant=true -C "$push" fetch -q --no-tags -- "file://$dir/.git" "+$sha:refs/heads/merge"; then
  echo "the merge commit $sha is not in the job clone any more"; res error; exit 2
fi
if [ "$(G -C "$push" rev-parse --verify -q "refs/heads/merge^{commit}")" != "$sha" ]; then
  echo "the push repo does not hold the merge commit $sha"; res error; exit 2
fi
res ok "$sha"
"#;

/// Step 3b: `url push sha`. Push exactly the merge commit `sha` to `next`
/// from the fresh repo of step 3a, not forced.
const PUSH_SCRIPT: &str = r#"url=$1; push=$2; sha=$3
case "$sha" in *[!0-9a-f]*|'') forget; echo "bad merge commit"; res error; exit 2 ;; esac
if g -C "$push" push -q -- "$url" "$sha:refs/heads/next"; then forget; res ok; else forget; res error; exit 2; fi
"#;

/// The end: `dir`. Remove the clone and the push repo.
const CLEAN_SCRIPT: &str = r#"case "$1" in /*/*) rm -rf -- "$1" "$1.push" ;; *) exit 2 ;; esac"#;

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;

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
    fn by_agent_label() {
        let labels = vec!["ci".to_owned(), "by:dash-ci".to_owned()];
        assert_eq!(by_agent_of(&labels).as_deref(), Some("dash-ci"));
        assert_eq!(by_agent_of(&[]), None);
    }
}
