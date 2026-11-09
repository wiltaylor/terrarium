//! The monitor runs (docs/api.md §30.3).
//!
//! The scheduler task (`crate::scheduler`) calls [`tick`] every [`TICK`].
//! A tick finds the due monitors and moves their next run on in one
//! write, as the schedule tick does, then starts one run for each. The
//! run checks the rights of the monitor's owner again (its `run_as` actor,
//! or the project's targets and grants, `crate::owner`), runs the
//! script with `sh -c` on the target through the executor
//! ([`crate::executor::start_cmd`], the path of `terrarium exec`), reads
//! the output by the output kind ([`parse`]), and keeps the result
//! (`crate::store::monitors::record`).
//!
//! Two limits keep the monitors from flooding the host agents:
//!
//! - One run of a monitor at a time. A tick that finds the run of a
//!   monitor still running records a skip (`skipped`) and starts nothing.
//! - At most [`DEFAULT_PARALLEL`] runs at once in the daemon
//!   ([`ENV_PARALLEL`]). A run waits for a free slot.
//!
//! A monitor is not an agent. An actor-owned monitor uses the target with
//! the rights of its `run_as` actor: the target's policy or a lease of
//! that agent (the operator may use any target). A project-owned monitor
//! uses the targets of its project and the global targets granted to the
//! project for monitors; a system-owned one any global target or one of
//! its project. Each run's result row carries the actor
//! `monitor:<project>/<name>`; a status change writes a `monitor.status`
//! audit row with that actor (`crate::store::monitors::record`). The runs
//! write no `target.exec` row: a 15 s monitor would add 5 760 rows a day to
//! an audit log with no prune. Its commands run in the resource budget
//! `<project>/monitors` of a `host` target with `limits`. A docker or
//! vmlab target that is stopped is not brought up for a monitor: the run
//! records `unknown` with "target down". An offline host agent is the
//! same.

pub mod layout;
pub mod parse;

use std::collections::HashSet;
use std::sync::{Arc, Mutex, MutexGuard};
use std::time::{Duration, Instant};

use chrono::{DateTime, Utc};
use serde::Serialize;
use terrarium_core::config::TargetKind;
use tokio::sync::{OwnedSemaphorePermit, Semaphore};
use tokio::task::JoinHandle;
use tracing::warn;

use crate::actor::Actor;
use crate::api::Shared;
use crate::error::ApiResult;
use crate::executor::{self, End, RemoteCmd};
use crate::owner::Owner;
use crate::policy;
use crate::store::monitors::{self, MonitorDef, MonitorResult};
use crate::store::target_grants::Purpose;
use crate::store::targets::TargetRow;
use parse::Parsed;

/// The period of the monitor tick.
pub const TICK: Duration = Duration::from_secs(5);
/// The most monitor runs at once in the daemon.
pub const DEFAULT_PARALLEL: usize = 8;
/// `TERRARIUM_MONITOR_PARALLEL`: another value for [`DEFAULT_PARALLEL`].
pub const ENV_PARALLEL: &str = "TERRARIUM_MONITOR_PARALLEL";
/// The bytes of stdout and of stderr that a run reads.
pub const OUTPUT_MAX: usize = 64 * 1024;
/// The bytes of stdout and of stderr that a run-now answer shows.
pub const SHOWN_MAX: usize = 8 * 1024;
/// The time past `timeout_secs` that the daemon waits for the target to
/// end the command itself.
const GRACE: Duration = Duration::from_secs(5);

/// The run state of the monitors: which monitors run now, and the slots.
pub struct Runtime {
    running: Mutex<HashSet<String>>,
    slots: Arc<Semaphore>,
    cap: usize,
}

impl Default for Runtime {
    fn default() -> Self {
        let cap = std::env::var(ENV_PARALLEL)
            .ok()
            .and_then(|v| v.trim().parse().ok())
            .filter(|n| *n > 0)
            .unwrap_or(DEFAULT_PARALLEL);
        Self::with_cap(cap)
    }
}

impl Runtime {
    /// A runtime with `cap` slots.
    pub fn with_cap(cap: usize) -> Self {
        let cap = cap.max(1);
        Self {
            running: Mutex::default(),
            slots: Arc::new(Semaphore::new(cap)),
            cap,
        }
    }

    fn set(&self) -> MutexGuard<'_, HashSet<String>> {
        self.running.lock().unwrap_or_else(|e| e.into_inner())
    }

    /// Mark monitor `id` as running. `None` when a run of it is running.
    pub fn claim(self: &Arc<Self>, id: &str) -> Option<Claim> {
        if !self.set().insert(id.to_owned()) {
            return None;
        }
        Some(Claim {
            rt: self.clone(),
            id: id.to_owned(),
        })
    }

    /// A run of monitor `id` is in progress.
    pub fn is_running(&self, id: &str) -> bool {
        self.set().contains(id)
    }

    /// The most runs at once.
    pub fn cap(&self) -> usize {
        self.cap
    }

    /// The slots in use now.
    pub fn busy(&self) -> usize {
        self.cap - self.slots.available_permits()
    }

    /// Wait for a free slot.
    pub async fn slot(&self) -> Option<OwnedSemaphorePermit> {
        self.slots.clone().acquire_owned().await.ok()
    }
}

/// The mark of a running monitor. Dropping it ends the mark.
pub struct Claim {
    rt: Arc<Runtime>,
    id: String,
}

impl Drop for Claim {
    fn drop(&mut self) {
        self.rt.set().remove(&self.id);
    }
}

/// The raw output of one run, for a run now and a test run.
#[derive(Debug, Clone, Default, PartialEq, Serialize)]
pub struct Output {
    pub exit_code: Option<i32>,
    pub stdout: String,
    pub stderr: String,
    /// How the command ended (`exit`, `error`).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub end: Option<End>,
}

/// One run: what it gave, its raw output (none when it did not start),
/// its start, and its duration.
#[derive(Debug, Clone)]
pub struct Execution {
    pub parsed: Parsed,
    pub output: Option<Output>,
    pub at: DateTime<Utc>,
    pub duration_ms: i64,
}

fn shown(b: &[u8]) -> String {
    let s = String::from_utf8_lossy(b);
    parse::cut_bytes(&s, SHOWN_MAX).0
}

/// Why a target is down for a monitor (or a pipeline poll), or `None`.
pub(crate) fn down(state: &Shared, row: &TargetRow) -> Option<String> {
    let name = &row.def.name;
    match row.def.kind {
        TargetKind::Docker | TargetKind::Vmlab
            if matches!(row.state.as_str(), "stopped" | "absent") =>
        {
            return Some(format!(
                "target `{name}` is {}; a monitor or a pipeline does not bring a target up",
                row.state
            ));
        }
        TargetKind::Ssh => return None,
        _ => {}
    }
    match &row.def.host {
        Some(h) if !state.executor.hosts.is_online(h) => Some(format!(
            "the host agent `{h}` of target `{name}` is offline"
        )),
        _ => None,
    }
}

/// The rights of one run: an actor's (an actor-owned monitor, a test run),
/// or those of a project- or system-owned monitor.
#[derive(Debug, Clone, Copy)]
pub enum Rights<'a> {
    Actor(&'a Actor),
    Owned(Owner),
}

/// The target of a run with `rights`, for `project`.
async fn target_for(
    state: &Shared,
    rights: Rights<'_>,
    project: &str,
    target: &str,
) -> ApiResult<TargetRow> {
    match rights {
        Rights::Actor(actor) => {
            policy::admin_project(actor, project, "running a monitor").map_err(|e| {
                crate::error::ApiError::forbidden(format!(
                    "{} (runs as {})",
                    e.message,
                    actor.label()
                ))
            })?;
            executor::usable(state, actor, target).await
        }
        Rights::Owned(owner) => {
            let (p, t) = (project.to_owned(), target.to_owned());
            state
                .db
                .read(move |c| {
                    crate::owner::owned_target(c, owner, Some(&p), &t, Purpose::Monitors)
                })
                .await
        }
    }
}

/// Run the script of `def` once on its target with `rights`, for
/// `project`. Never fails: a refused right, a target that is down, and a
/// timeout are each an `unknown` result with the error.
pub async fn execute(
    state: &Shared,
    rights: Rights<'_>,
    project: &str,
    def: &MonitorDef,
) -> Execution {
    let at = Utc::now();
    let started = Instant::now();
    let done = |parsed: Parsed, output: Option<Output>| Execution {
        parsed,
        output,
        at,
        duration_ms: i64::try_from(started.elapsed().as_millis()).unwrap_or(i64::MAX),
    };
    let row = match target_for(state, rights, project, &def.target).await {
        Ok(r) => r,
        Err(e) => return done(Parsed::unknown(e.message), None),
    };
    if let Some(p) = row.def.project.as_deref().filter(|p| *p != project) {
        return done(
            Parsed::unknown(format!(
                "target `{}` belongs to project `{p}`; a monitor uses global targets and those of its project",
                def.target
            )),
            None,
        );
    }
    if let Some(why) = down(state, &row) {
        return done(Parsed::unknown(format!("target down: {why}")), None);
    }
    let Some(_slot) = state.monitors.slot().await else {
        return done(Parsed::unknown("the monitor runtime is closed"), None);
    };
    let actor = match rights {
        Rights::Actor(a) => Some(a),
        Rights::Owned(_) => None,
    };
    let tdef = match executor::resolved_def(state, &row, actor, None).await {
        Ok(d) => d,
        Err(e) => return done(Parsed::unknown(e.message), None),
    };
    let cmd = RemoteCmd {
        argv: vec!["sh".into(), "-c".into(), def.script.clone()],
        timeout_secs: Some(def.timeout_secs),
        holder: Some(terrarium_host::native::service_holder(project, "monitors")),
        ..Default::default()
    };
    let run = match executor::start_cmd(state, &tdef, &cmd) {
        Ok(r) => r,
        Err(e) => return done(Parsed::unknown(e.message), None),
    };
    let limit = Duration::from_secs(def.timeout_secs) + GRACE;
    let timed_out = format!("timed out after {} s", def.timeout_secs);
    // Dropping the run on the timeout ends the command.
    let Ok(c) = tokio::time::timeout(limit, run.collect(OUTPUT_MAX)).await else {
        return done(Parsed::unknown(timed_out), None);
    };
    let exit_code = match &c.end {
        Some(End::Exit { code, .. }) => *code,
        _ => None,
    };
    let output = Output {
        exit_code,
        stdout: shown(&c.stdout),
        stderr: shown(&c.stderr),
        end: c.end.clone(),
    };
    let parsed = match &c.end {
        Some(End::Exit {
            reason, code: None, ..
        }) if reason == "timeout" => Parsed::unknown(timed_out),
        Some(End::Exit { .. }) => {
            let stdout = String::from_utf8_lossy(&c.stdout);
            parse::parse(def.kind, &def.thresholds(), &stdout, exit_code)
        }
        Some(End::Error { code, message }) => Parsed::unknown(format!("{code}: {message}")),
        Some(End::Cp { .. }) | None => Parsed::unknown("the command ended without an exit"),
    };
    done(parsed, Some(output))
}

pub(crate) fn actor_of(conn: &rusqlite::Connection, run_as: &str) -> ApiResult<Option<Actor>> {
    if run_as == "operator" {
        return Ok(Some(Actor::Operator));
    }
    match run_as.strip_prefix("agent:") {
        Some(id) => Ok(crate::store::agents::actor_for(conn, id)?.map(Actor::Agent)),
        None => Ok(None),
    }
}

/// Run monitor `id` once and keep the result. `claim` is its mark: the
/// caller took it with [`Runtime::claim`].
pub async fn run(
    state: &Shared,
    id: &str,
    claim: Claim,
) -> ApiResult<(MonitorResult, Option<Output>)> {
    let key = id.to_owned();
    let (m, actor) = state
        .db
        .read(move |c| {
            let m = monitors::get(c, &key)?;
            let actor = match m.owner {
                Owner::Actor => actor_of(c, &m.run_as)?,
                _ => None,
            };
            Ok((m, actor))
        })
        .await?;
    let exec = match (m.owner, actor) {
        (Owner::Actor, Some(a)) => execute(state, Rights::Actor(&a), &m.project, &m.def).await,
        (owner @ (Owner::Project | Owner::System), _) => {
            execute(state, Rights::Owned(owner), &m.project, &m.def).await
        }
        (Owner::Actor, None) => Execution {
            parsed: Parsed::unknown(format!(
                "the monitor runs as `{}`, which no longer exists; take it over with an edit \
                 (--owner actor runs it as you, --owner project gives it to the project)",
                m.run_as
            )),
            output: None,
            at: Utc::now(),
            duration_ms: 0,
        },
    };
    let key = id.to_owned();
    let (parsed, at, ms) = (exec.parsed.clone(), exec.at, exec.duration_ms);
    let r = state
        .db
        .write(m.label(), move |t| {
            monitors::record(t, &key, &parsed, at, ms)
        })
        .await;
    drop(claim);
    Ok((r?, exec.output))
}

/// One started run.
pub type Started = JoinHandle<ApiResult<(MonitorResult, Option<Output>)>>;

/// Start the runs of the monitors due at `now`. A monitor whose run is
/// still running gets a skip. Returns the started runs.
pub async fn tick(state: &Shared, now: DateTime<Utc>) -> Vec<Started> {
    let due = state
        .db
        .write("system", move |t| {
            let due = monitors::due(t, now)?;
            for m in &due {
                monitors::advance(t, &m.id, now)?;
            }
            Ok(due)
        })
        .await;
    let due = match due {
        Ok(d) => d,
        Err(e) => {
            warn!("the monitor tick cannot read the due monitors: {e}");
            return Vec::new();
        }
    };
    let mut started = Vec::new();
    let mut skipped = Vec::new();
    for m in due {
        match state.monitors.claim(&m.id) {
            None => skipped.push(m.id),
            Some(claim) => {
                let st = state.clone();
                started.push(tokio::spawn(async move {
                    let r = run(&st, &m.id, claim).await;
                    if let Err(e) = &r {
                        warn!(monitor = %m.id, "a monitor run failed to record: {e}");
                    }
                    r
                }));
            }
        }
    }
    if !skipped.is_empty() {
        let r = state
            .db
            .write("system", move |t| {
                for id in &skipped {
                    monitors::add_skip(t, id)?;
                }
                Ok(())
            })
            .await;
        if let Err(e) = r {
            warn!("the monitor tick cannot count the skips: {e}");
        }
    }
    started
}

/// Delete the results past each monitor's retention (the daily prune).
pub async fn prune(state: &Shared) {
    let r = state
        .db
        .write("system", |t| monitors::prune_all(t, Utc::now()))
        .await;
    if let Err(e) = r {
        warn!("the monitor prune failed: {e}");
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;

    #[test]
    fn one_claim_per_monitor() {
        let rt = Arc::new(Runtime::with_cap(2));
        let a = rt.claim("a").unwrap();
        assert!(rt.claim("a").is_none());
        assert!(rt.is_running("a"));
        let b = rt.claim("b").unwrap();
        drop(a);
        assert!(!rt.is_running("a"));
        assert!(rt.claim("a").is_some());
        drop(b);
    }

    #[tokio::test]
    async fn slots_cap_the_parallel_runs() {
        let rt = Arc::new(Runtime::with_cap(2));
        assert_eq!(rt.cap(), 2);
        let s1 = rt.slot().await.unwrap();
        let _s2 = rt.slot().await.unwrap();
        assert_eq!(rt.busy(), 2);
        // A third run waits until a slot is free.
        let waiting = tokio::spawn({
            let rt = rt.clone();
            async move { rt.slot().await.map(|_| ()) }
        });
        tokio::time::sleep(Duration::from_millis(30)).await;
        assert!(!waiting.is_finished());
        drop(s1);
        tokio::time::timeout(Duration::from_secs(2), waiting)
            .await
            .unwrap()
            .unwrap()
            .unwrap();
        assert_eq!(Runtime::with_cap(0).cap(), 1);
    }
}
