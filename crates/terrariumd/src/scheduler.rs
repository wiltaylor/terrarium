//! The schedule timer (docs/api.md §19). Every [`TICK`] it finds the due
//! schedules, moves each one's `next_run_at` on, and starts its run. At a
//! daemon start, [`startup`] ends the runs that the old daemon left
//! `running` and applies `catch_up` to the runs it missed while it was
//! down. Both take the time as an argument, so tests drive them with a
//! fake clock.
//!
//! The monitor tick (`crate::monitors::tick`) is in the same task, with
//! its own period ([`crate::monitors::TICK`]), and the pipeline tick
//! (`crate::pipelines::tick`) runs with it. The calendar notice tick
//! (`crate::calendar::tick`) runs with the schedule tick.
//!
//! The start-of-day schedules (spec `day_start`, GitHub #29) are on the
//! same timer. [`day_start_pass`] plans their run at each detected start:
//! the daemon start, a resume (a gap in the timer above [`RESUME_GAP`]),
//! the reconnect of the host agent of an `exec` target, or a change of day
//! while the daemon is up. The stored day of the last run keeps it to one
//! run per day, whatever the starts.
//!
//! The trigger engine (PLAN §12c) is in the same task. It reads the event
//! log from `trigger_cursor`, not only the live hub: a new event on the
//! hub only wakes it, and [`trigger_pass`] reads the rows after the
//! cursor and moves the cursor in the same write. So a restart or a lagged
//! hub loses no event. At a start, [`trigger_startup`] gives the last
//! event from before the start: a trigger with `catch_up = skip` ignores
//! the events up to it, one with `once` runs them.

use std::collections::{BTreeMap, HashMap, HashSet};
use std::sync::{Arc, LazyLock};
use std::time::Duration;

use chrono::{DateTime, Utc};
use tokio::sync::broadcast::error::TryRecvError;
use tokio::task::JoinHandle;
use tracing::{info, warn};

use crate::api::Shared;
use crate::error::ApiResult;
use crate::events::{self, EventFilter, StoredEvent};
use crate::store::schedules::{self, Decision, Run, Schedule, StartCause};
use terrarium_core::schedule::CatchUp;

/// The period of the timer.
pub const TICK: Duration = Duration::from_secs(10);

/// One started run.
pub type Started = JoinHandle<ApiResult<Run>>;

fn start(state: &Shared, id: String, trigger: &'static str, run_as: String) -> Started {
    let st = state.clone();
    tokio::spawn(async move {
        let r = crate::api::run_schedule(&st, &id, trigger, run_as).await;
        if let Err(e) = &r {
            warn!(schedule = %id, "a schedule run failed to record: {e}");
        }
        r
    })
}

/// Start the runs of the schedules due at `now`. Each due schedule's
/// `next_run_at` moves past `now` first, in one write, so the next tick
/// does not start it again. Returns the started runs.
pub async fn tick(state: &Shared, now: DateTime<Utc>) -> Vec<Started> {
    let due = state
        .db
        .write("system", move |t| {
            let due = schedules::due(t, now)?;
            for s in &due {
                schedules::advance(t, &s.id, now)?;
                if s.spec.is_day_start() {
                    schedules::mark_day_started(t, s, now)?;
                }
            }
            Ok(due)
        })
        .await;
    match due {
        Ok(due) => due
            .into_iter()
            .map(|s| {
                let by = s.run_label();
                let trigger = if s.spec.is_day_start() {
                    "day_start"
                } else {
                    "cron"
                };
                start(state, s.id, trigger, by)
            })
            .collect(),
        Err(e) => {
            warn!("the schedule tick cannot read the due schedules: {e}");
            Vec::new()
        }
    }
}

/// At a daemon start: fail the runs left `running`, then, for each
/// enabled schedule whose next run passed more than one tick ago (a run
/// missed while the daemon was down), compute the next run from `now`.
/// With `catch_up = once`, start one run with the trigger `catch_up`. A
/// `day_start` schedule has no catch-up: its planned run is dropped, and
/// the daemon-start [`day_start_pass`] plans the run of the day again.
pub async fn startup(state: &Shared, now: DateTime<Utc>) -> Vec<Started> {
    let before = now - chrono::Duration::from_std(TICK).unwrap_or_default();
    let missed = state
        .db
        .write("system", move |t| {
            let stale = schedules::fail_stale_runs(t, now)?;
            if stale > 0 {
                info!(runs = stale, "schedule runs ended with the old daemon");
            }
            let missed = schedules::missed(t, before)?;
            for s in &missed {
                schedules::advance(t, &s.id, now)?;
            }
            Ok(missed)
        })
        .await;
    match missed {
        Ok(missed) => missed
            .into_iter()
            .filter(|s| s.catch_up == CatchUp::Once && !s.spec.is_day_start())
            .map(|s| {
                let by = s.run_label();
                start(state, s.id, "catch_up", by)
            })
            .collect(),
        Err(e) => {
            warn!("the schedule catch-up failed: {e}");
            Vec::new()
        }
    }
}

// ------------------------------------------------- the start of the day

/// A gap between two ticks above it is a resume: the machine slept, or
/// the daemon was stopped (`SIGSTOP`).
pub const RESUME_GAP: Duration = Duration::from_secs(60);

/// What the timer keeps between two [`day_start_pass`]es: the time of
/// the last pass, and the `day_start` schedules that wait for the host
/// agent of their target.
#[derive(Debug, Default)]
pub struct DayStartWatch {
    last: Option<DateTime<Utc>>,
    waiting: HashSet<String>,
}

impl DayStartWatch {
    /// The start that a tick at `now` detects: a resume after a gap above
    /// [`RESUME_GAP`], else a change of day.
    pub fn tick_cause(&self, now: DateTime<Utc>) -> StartCause {
        let gap = chrono::Duration::from_std(RESUME_GAP).unwrap_or_default();
        match self.last {
            Some(prev) if now - prev > gap => StartCause::Resume,
            _ => StartCause::DayChange,
        }
    }
}

/// Can the action of `s` run now? An `exec` needs its target, and the
/// host agent of a host target online. An unknown target, or a failed
/// lookup, is not reachable. Other actions can always run.
pub fn reachable(
    state: &Shared,
) -> impl Fn(&rusqlite::Connection, &Schedule) -> bool + Send + 'static {
    let state = state.clone();
    move |conn, s| {
        let terrarium_core::schedule::ScheduleAction::Exec { target, .. } = &s.action else {
            return true;
        };
        match crate::store::targets::find(conn, target) {
            Ok(Some(row)) => row
                .def
                .host
                .as_deref()
                .is_none_or(|h| state.executor.hosts.is_online(h)),
            _ => false,
        }
    }
}

/// One change of a [`day_start_pass`].
enum DayStartChange {
    /// Drop the planned run.
    Disarm(String),
    /// Plan the run of the day, for this start.
    Arm(String, StartCause),
}

/// Plan the runs of the `day_start` schedules for a start at `now`. A
/// planned run that is no longer good (see
/// [`schedules::day_start_armed_valid`], or its target is offline) is
/// dropped. A schedule with no planned run gets one when its day did not
/// run yet: the cause is `cause`, or `host_reconnect` when it waited for
/// its host agent. A tick then starts the run when it is due.
///
/// It reads first, and takes the write lock only when a plan changes, so
/// a tick with nothing to do writes nothing. Returns the planned IDs.
pub async fn day_start_pass<R>(
    state: &Shared,
    now: DateTime<Utc>,
    cause: StartCause,
    watch: &mut DayStartWatch,
    reachable: R,
) -> Vec<String>
where
    R: Fn(&rusqlite::Connection, &Schedule) -> bool + Send + 'static,
{
    watch.last = Some(now);
    let mut waiting = std::mem::take(&mut watch.waiting);
    let read = state
        .db
        .read(move |c| {
            let mut changes = Vec::new();
            let mut still = HashSet::new();
            for s in schedules::day_starts(c)? {
                let up = reachable(c, &s);
                if s.next_run_at.is_some() {
                    if up && schedules::day_start_armed_valid(&s, now)? {
                        continue;
                    }
                    changes.push(DayStartChange::Disarm(s.id.clone()));
                }
                if !up {
                    still.insert(s.id.clone());
                    continue;
                }
                let cause = if waiting.remove(&s.id) {
                    StartCause::HostReconnect
                } else {
                    cause
                };
                if schedules::day_start_plan(&s.spec, s.day_start_done.as_deref(), now)?.is_some() {
                    changes.push(DayStartChange::Arm(s.id.clone(), cause));
                }
            }
            Ok((changes, still))
        })
        .await;
    let changes = match read {
        Ok((changes, still)) => {
            watch.waiting = still;
            changes
        }
        Err(e) => {
            warn!("the start-of-day pass cannot read the schedules: {e}");
            return Vec::new();
        }
    };
    if changes.is_empty() {
        return Vec::new();
    }
    let r = state
        .db
        .write("system", move |t| {
            let mut armed = Vec::new();
            for change in changes {
                match change {
                    DayStartChange::Disarm(id) => schedules::disarm_day_start(t, &id)?,
                    DayStartChange::Arm(id, cause) => {
                        // Read again under the lock: an API write may have
                        // changed the schedule since the read.
                        let Ok(s) = schedules::get(t, &id) else {
                            continue;
                        };
                        if !s.enabled || !s.spec.is_day_start() || s.next_run_at.is_some() {
                            continue;
                        }
                        if schedules::arm_day_start(t, &s, cause, now)?.is_some() {
                            armed.push(id);
                        }
                    }
                }
            }
            Ok(armed)
        })
        .await;
    r.unwrap_or_else(|e| {
        warn!("the start-of-day pass failed: {e}");
        Vec::new()
    })
}

// ------------------------------------------------------ the trigger engine

/// The most events that one [`trigger_pass`] reads.
pub const TRIGGER_BATCH: usize = 500;
/// The ticks between two prunes of `trigger_state` (one day).
const PRUNE_TICKS: u64 = 8640;

/// The runs that one pass started for one trigger, in event order.
pub type TriggerRuns = JoinHandle<Vec<ApiResult<Run>>>;

/// One lock for each trigger: its runs go one after another, in event
/// order, also across passes. A trigger's ULID is unique, so one map
/// serves every daemon in the process (the tests start many).
fn trigger_lock(id: &str) -> Arc<tokio::sync::Mutex<()>> {
    static LOCKS: LazyLock<std::sync::Mutex<HashMap<String, Arc<tokio::sync::Mutex<()>>>>> =
        LazyLock::new(Default::default);
    let mut map = match LOCKS.lock() {
        Ok(m) => m,
        Err(poisoned) => poisoned.into_inner(),
    };
    map.entry(id.to_owned()).or_default().clone()
}

/// The cursor and the last event `seq` at the daemon start. [`spawn`]
/// reads them on the calling thread, before the listeners take events.
pub fn start_point(state: &Shared) -> (Option<i64>, i64) {
    state
        .db
        .read_now(|c| Ok((schedules::cursor(c)?, events::last_seq(c)?)))
        .unwrap_or_else(|e| {
            warn!("the trigger engine cannot read its cursor: {e}");
            (None, 0)
        })
}

/// At a daemon start, with the [`start_point`]: the first start sets the
/// cursor to the end of the log (old events start no run). A later start
/// returns the last event `seq` from before the start when the cursor is
/// behind it: the events up to it came while the daemon was down. Also
/// prunes `trigger_state`.
pub async fn trigger_startup(
    state: &Shared,
    (cursor, last): (Option<i64>, i64),
    now: DateTime<Utc>,
) -> Option<i64> {
    let r = state
        .db
        .write("system", move |t| {
            schedules::prune_trigger_state(t, now)?;
            if schedules::cursor(t)?.is_none() {
                schedules::set_cursor(t, last)?;
            }
            Ok(())
        })
        .await;
    if let Err(e) = r {
        warn!("the trigger engine cannot set its cursor: {e}");
    }
    cursor.filter(|c| *c < last).map(|_| last)
}

/// Read the events after the cursor (at most [`TRIGGER_BATCH`]), decide
/// for each enabled trigger, and move the cursor, in one write. A rate
/// limit records a `skipped` run in that write; each other decision
/// starts a run after it. `boundary` is the value of [`trigger_startup`].
/// Returns the started runs, and whether more events wait.
pub async fn trigger_pass(
    state: &Shared,
    boundary: Option<i64>,
    now: DateTime<Utc>,
) -> (Vec<TriggerRuns>, bool) {
    let planned = state
        .db
        .write("system", move |t| {
            let Some(cursor) = schedules::cursor(t)? else {
                let last = events::last_seq(t)?;
                schedules::set_cursor(t, last)?;
                return Ok((Vec::new(), false));
            };
            let evs = events::list(t, &EventFilter::default(), cursor, TRIGGER_BATCH)?;
            let Some(last) = evs.last().map(|e| e.seq) else {
                return Ok((Vec::new(), false));
            };
            let triggers = schedules::triggers(t)?;
            let mut plan: Vec<(String, StoredEvent, Decision)> = Vec::new();
            for e in &evs {
                for s in &triggers {
                    if boundary.is_some_and(|b| e.seq <= b) && s.catch_up == CatchUp::Skip {
                        continue;
                    }
                    match schedules::decide(t, s, e, now)? {
                        None => {}
                        Some(Decision::RateLimited) => {
                            schedules::record_trigger_skip(t, s, e, "rate limit", now)?;
                        }
                        Some(d) => plan.push((s.id.clone(), e.clone(), d)),
                    }
                }
            }
            schedules::set_cursor(t, last)?;
            Ok((plan, evs.len() == TRIGGER_BATCH))
        })
        .await;
    let (plan, more) = match planned {
        Ok(p) => p,
        Err(e) => {
            warn!("the trigger engine cannot read the event log: {e}");
            return (Vec::new(), false);
        }
    };
    let mut by_trigger: BTreeMap<String, Vec<(StoredEvent, Decision)>> = BTreeMap::new();
    for (id, e, d) in plan {
        by_trigger.entry(id).or_default().push((e, d));
    }
    let started = by_trigger
        .into_iter()
        .map(|(id, items)| {
            let st = state.clone();
            tokio::spawn(async move {
                let lock = trigger_lock(&id);
                let _held = lock.lock().await;
                let mut out = Vec::new();
                for (e, d) in items {
                    let r = crate::api::run_triggered(&st, &id, e, d).await;
                    if let Err(e) = &r {
                        warn!(schedule = %id, "a trigger run failed to record: {e}");
                    }
                    out.push(r);
                }
                out
            })
        })
        .collect();
    (started, more)
}

/// Run passes until the log after the cursor is read.
async fn drain_triggers(state: &Shared, boundary: Option<i64>) {
    loop {
        let (started, more) = trigger_pass(state, boundary, Utc::now()).await;
        drop(started);
        if !more {
            break;
        }
    }
}

/// The timer and the trigger engine: [`startup`] and [`trigger_startup`],
/// then [`tick`] every [`TICK`], and a trigger pass after each wake-up (a
/// new event on the hub, or a tick).
pub fn spawn(state: Shared) -> JoinHandle<()> {
    // Read the start point and subscribe now, so an event after the start
    // is neither "from while down" nor without a wake-up.
    let point = start_point(&state);
    let mut rx = state.db.hub().subscribe();
    tokio::spawn(async move {
        drop(startup(&state, Utc::now()).await);
        let mut watch = DayStartWatch::default();
        day_start_pass(
            &state,
            Utc::now(),
            StartCause::DaemonStart,
            &mut watch,
            reachable(&state),
        )
        .await;
        let boundary = trigger_startup(&state, point, Utc::now()).await;
        drain_triggers(&state, boundary).await;
        let mut next_tick = tokio::time::Instant::now() + TICK;
        let mut next_monitor = tokio::time::Instant::now() + crate::monitors::TICK;
        let mut ticks: u64 = 0;
        loop {
            tokio::select! {
                // The monitors (docs/api.md §30) on the same timer, at
                // their own period: a monitor runs as often as every 15 s.
                () = tokio::time::sleep_until(next_monitor) => {
                    drop(crate::monitors::tick(&state, Utc::now()).await);
                    // The pipelines (docs/api.md §31) on the same timer.
                    drop(crate::pipelines::tick(&state, Utc::now()).await);
                    next_monitor = tokio::time::Instant::now() + crate::monitors::TICK;
                    continue;
                }
                () = tokio::time::sleep_until(next_tick) => {
                    ticks += 1;
                    let now = Utc::now();
                    let cause = watch.tick_cause(now);
                    day_start_pass(&state, now, cause, &mut watch, reachable(&state)).await;
                    drop(tick(&state, now).await);
                    // The calendar notices (docs/api.md §33.5) on the same timer.
                    crate::calendar::tick(&state, Utc::now()).await;
                    next_tick = tokio::time::Instant::now() + TICK;
                    if ticks.is_multiple_of(PRUNE_TICKS) {
                        let pruned = state
                            .db
                            .write("system", |t| schedules::prune_trigger_state(t, Utc::now()))
                            .await;
                        if let Err(e) = pruned {
                            warn!("the trigger state prune failed: {e}");
                        }
                        crate::monitors::prune(&state).await;
                        crate::webhooks::prune(&state).await;
                        crate::calendar::prune(&state).await;
                    }
                }
                r = rx.recv() => {
                    if matches!(r, Err(tokio::sync::broadcast::error::RecvError::Closed)) {
                        rx = state.db.hub().subscribe();
                    }
                }
            }
            // One pass reads every event that woke the engine.
            while let Ok(_) | Err(TryRecvError::Lagged(_)) = rx.try_recv() {}
            drain_triggers(&state, boundary).await;
        }
    })
}
