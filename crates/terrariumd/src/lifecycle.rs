//! The agent lifecycle: wake on demand, the idle stop, and the pause (PLAN
//! §18, "Wake, idle stop, and pause").
//!
//! - **Wake.** Work for an agent that does not run starts it: new mail, a
//!   turn (`POST /api/agents/{id}/send`), a card that another actor
//!   assigned to it or that is unblocked, and a lead nudge. The wake is a
//!   start with resume of the last harness session, as the relaunch after
//!   a daemon start. The work waits until the harness is ready: queued
//!   mail in the store, card and nudge lines in `pending_pointers`, and
//!   turns in `queued_turns`. One wake at a time for each agent: a wake
//!   that comes while another one runs does nothing.
//! - **No wake.** A paused agent, an agent in `error`, an agent with a
//!   quota or budget hold, and every agent while the daemon drains for a
//!   self-update (`crate::self_update`) do not wake; their work waits. An
//!   unpause, the end of the last hold, the end of a drain, and the daemon
//!   start wake an agent that has work. A wake whose start fails puts the agent in `error` with the
//!   reason; the operator clears it (`start`, `restart`, or `stop`).
//! - **Idle stop.** An agent at rest (`idle` or `waiting`) for its idle
//!   stop (`idle_stop`, else `TERRARIUM_IDLE_STOP`, default 10 minutes)
//!   stops gracefully. Its status is `stopped` with the reason `idle`; the
//!   restart policy does not apply. An agent with a live exec session (a
//!   shell or a `terrarium run` job), an open `process` wait, queued work,
//!   or a wake on the way does not stop. A target display operation of the
//!   agent starts its clock again. Open waits, leases, and grants stay.
//! - **Pause** (`crate::api::agents`): the `paused` flag. A pause stops a
//!   running agent with the reason `paused`. The wake refuses a paused
//!   agent.
//! - **Park** (`crate::bound`, TER-74): a bound ephemeral agent whose
//!   object waits is parked. The wake refuses it, and the idle pass stops
//!   it as soon as it is at rest (no idle time), with the reason
//!   `parked`. A live exec session or an open `process` wait keeps it
//!   running; queued work does not (it waits for the unpark). The stop
//!   checks the park again while it holds the wake (TER-111), and the
//!   agent holds its place in the owner's `spawn.max_concurrent` until it
//!   is stopped.
//! - **Auto compact.** With `compact_on_idle` (else
//!   `TERRARIUM_COMPACT_ON_IDLE`, default off), an agent at its idle stop
//!   compacts its session first, and then stops. An agent with no idle
//!   stop compacts once after the same rest (the global idle stop, else 10
//!   minutes) and does not stop. With `compact_max_age` (else
//!   `TERRARIUM_COMPACT_MAX_AGE`, default off), an agent at rest whose
//!   session context is older than the maximum compacts, and goes on. The
//!   age counts from the last compaction, else from the start of the
//!   session. A compaction runs only after a turn that is newer than the
//!   last compaction and the start of the session. Not compacted: a
//!   paused or held agent, an agent that is not at rest, and an agent of a
//!   harness with no compaction ([`HarnessId::compact_command`]). The
//!   supervisor types the command; turns wait for the end. Each
//!   compaction writes `agent.compacted` (`{cause, ok, duration_ms}`).
//! - **Reset on request** (`crate::reset`): a compaction or a new session
//!   that an actor requested waits in `agents.pending_reset` and runs when
//!   the agent comes to rest (and in each idle pass), before its other
//!   work. A wake of an agent with a pending new session starts it with no
//!   resume.

use std::collections::{HashMap, HashSet};
use std::sync::{Arc, Mutex, MutexGuard, Weak};
use std::time::{Duration, Instant};

use serde_json::{Value, json};
use terrarium_core::config::{CompactMaxAge, IdleStop};
use terrarium_core::{AgentId, AgentStatus, HarnessId};
use tokio::sync::broadcast::error::RecvError;
use tokio::task::JoinHandle;
use tracing::{debug, info, warn};

use crate::api::{AppState, Shared};
use crate::error::Code;
use crate::events::EventDraft;
use crate::store::agents;
use crate::supervisor::{CompactEnd, SupervisorError, SupervisorEvent, SupervisorEventKind};

/// `TERRARIUM_IDLE_STOP`: the idle stop of an agent that sets none.
/// `off` (or `0`) turns it off.
pub const ENV_IDLE_STOP: &str = "TERRARIUM_IDLE_STOP";
/// The default of [`ENV_IDLE_STOP`].
pub const DEFAULT_IDLE_STOP: Duration = Duration::from_secs(10 * 60);

/// `TERRARIUM_COMPACT_ON_IDLE`: compact the session of an agent that sets
/// no `compact_on_idle` at its idle stop. Default off.
pub const ENV_COMPACT_ON_IDLE: &str = "TERRARIUM_COMPACT_ON_IDLE";
/// `TERRARIUM_COMPACT_MAX_AGE`: the maximum age of the session context of
/// an agent that sets no `compact_max_age`. Default `off`.
pub const ENV_COMPACT_MAX_AGE: &str = "TERRARIUM_COMPACT_MAX_AGE";
/// How long the daemon waits for the end signal of a compaction.
pub const DEFAULT_COMPACT_WAIT: Duration = Duration::from_secs(5 * 60);

/// The causes of a compaction (`agent.compacted` `cause`).
pub const CAUSE_IDLE: &str = "idle";
pub const CAUSE_MAX_AGE: &str = "max_age";
pub const CAUSE_MANUAL: &str = "manual";

/// The status reason of an idle stop.
pub const REASON_IDLE: &str = "idle";
/// The status reason of the stop of a pause.
pub const REASON_PAUSED: &str = "paused";
/// The status reason of the stop of a parked bound agent (TER-74).
pub const REASON_PARKED: &str = "parked";

/// The global idle stop from [`ENV_IDLE_STOP`]. A value that does not
/// parse gives the default, with a warning.
pub fn idle_stop_from_env() -> IdleStop {
    let default = IdleStop::After(DEFAULT_IDLE_STOP);
    match std::env::var(ENV_IDLE_STOP) {
        Ok(v) if !v.trim().is_empty() => v.parse().unwrap_or_else(|e| {
            warn!("{ENV_IDLE_STOP}: {e}; the idle stop is {default}");
            default
        }),
        _ => default,
    }
}

/// The global `compact_on_idle` from [`ENV_COMPACT_ON_IDLE`]. A value that
/// does not parse gives off, with a warning.
pub fn compact_on_idle_from_env() -> bool {
    match std::env::var(ENV_COMPACT_ON_IDLE) {
        Ok(v) => crate::settings::parse_switch(&v).unwrap_or_else(|e| {
            warn!("{ENV_COMPACT_ON_IDLE}: {e}; the compaction on idle is off");
            false
        }),
        Err(_) => false,
    }
}

/// The global `compact_max_age` from [`ENV_COMPACT_MAX_AGE`]. A value that
/// does not parse gives `off`, with a warning.
pub fn compact_max_age_from_env() -> CompactMaxAge {
    match std::env::var(ENV_COMPACT_MAX_AGE) {
        Ok(v) if !v.trim().is_empty() => v.parse().unwrap_or_else(|e| {
            warn!("{ENV_COMPACT_MAX_AGE}: {e}; the compact max age is off");
            CompactMaxAge::Off
        }),
        _ => CompactMaxAge::Off,
    }
}

/// The message for an agent of a harness with no compaction.
pub fn compact_unsupported(harness: HarnessId) -> String {
    format!("auto compact not supported for {harness}")
}

/// The times of the lifecycle. Tests make them short.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Tuning {
    /// The idle stop of an agent that sets none.
    pub idle_stop: IdleStop,
    /// How often the daemon looks for agents to stop.
    pub tick: Duration,
    /// The `compact_on_idle` of an agent that sets none.
    pub compact_on_idle: bool,
    /// The `compact_max_age` of an agent that sets none.
    pub compact_max_age: CompactMaxAge,
    /// How long a compaction may take before the daemon stops to wait.
    pub compact_wait: Duration,
}

impl Default for Tuning {
    fn default() -> Self {
        Self {
            idle_stop: idle_stop_from_env(),
            tick: Duration::from_secs(15),
            compact_on_idle: compact_on_idle_from_env(),
            compact_max_age: compact_max_age_from_env(),
            compact_wait: DEFAULT_COMPACT_WAIT,
        }
    }
}

/// The lifecycle state of the daemon (`AppState::lifecycle`).
#[derive(Default)]
pub struct Lifecycle {
    tuning: Mutex<Tuning>,
    /// Wakes the timer when the tuning changes, so a new tick applies at
    /// once, not after the sleep of the old one ([`spawn`]).
    retuned: tokio::sync::Notify,
    /// The agents with a wake on the way.
    waking: Mutex<HashSet<AgentId>>,
    /// Since when each agent rests (`idle` or `waiting`).
    resting: Mutex<HashMap<AgentId, Instant>>,
    /// The reason of the next `stopped` status of an agent that the
    /// daemon stops itself (`idle`, `paused`).
    stop_reasons: Mutex<HashMap<AgentId, String>>,
    /// The agents with a compaction on the way.
    compacting: Mutex<HashSet<AgentId>>,
    /// Ephemeral agents (PLAN §18, "Ephemeral agents"): the close guard
    /// and the times (`crate::spawn`).
    pub spawns: crate::spawn::Runtime,
    /// The agents with a run of a pending reset on the way
    /// (`crate::reset::run_pending`).
    pub resets: crate::reset::Runs,
    /// The card reminder (`crate::card_reminder`).
    pub reminders: crate::card_reminder::Reminders,
    /// The reminder and the stale close of untouched Inbox threads
    /// (`crate::thread_close`).
    pub threads: crate::thread_close::Runtime,
    /// One lock for each agent around the hand-over of one queued turn
    /// (TER-54): a remove of that turn comes before it or after it, so a
    /// removed turn is never typed and a typed turn is never shown removed.
    turn_locks: Mutex<HashMap<AgentId, std::sync::Arc<tokio::sync::Mutex<()>>>>,
}

fn guard<T>(m: &Mutex<T>) -> MutexGuard<'_, T> {
    m.lock().unwrap_or_else(std::sync::PoisonError::into_inner)
}

impl Lifecycle {
    /// The queued-turn lock of agent `id` ([`Lifecycle::turn_locks`]).
    pub fn turn_lock(&self, id: AgentId) -> std::sync::Arc<tokio::sync::Mutex<()>> {
        guard(&self.turn_locks).entry(id).or_default().clone()
    }

    pub fn tuning(&self) -> Tuning {
        *guard(&self.tuning)
    }

    /// Change the times. For tests.
    pub fn set_tuning(&self, t: Tuning) {
        *guard(&self.tuning) = t;
        self.retuned.notify_one();
    }

    /// The agent did something that is not a turn (a target display
    /// operation): its idle clock starts again.
    pub fn touch(&self, agent: &str) {
        let Ok(id) = agent.parse::<AgentId>() else {
            return;
        };
        if let Some(since) = guard(&self.resting).get_mut(&id) {
            *since = Instant::now();
        }
    }

    /// Give the next `stopped` status of `agent` the reason `reason`.
    pub fn expect_stop(&self, agent: AgentId, reason: &str) {
        guard(&self.stop_reasons).insert(agent, reason.to_owned());
    }

    /// Forget the reason of [`Lifecycle::expect_stop`]: the stop did not
    /// happen.
    pub fn forget_stop(&self, agent: AgentId) {
        guard(&self.stop_reasons).remove(&agent);
    }

    /// `ev` with the reason of [`Lifecycle::expect_stop`], when it is the
    /// `stopped` status of that agent. The event pump calls it.
    pub fn with_stop_reason(&self, mut ev: SupervisorEvent) -> SupervisorEvent {
        if let SupervisorEventKind::Status { status, reason } = &mut ev.kind {
            match status {
                AgentStatus::Stopped => {
                    if let Some(r) = guard(&self.stop_reasons).remove(&ev.agent) {
                        *reason = Some(r);
                    }
                }
                AgentStatus::Error => {
                    guard(&self.stop_reasons).remove(&ev.agent);
                }
                _ => {}
            }
        }
        ev
    }

    fn waking(&self, agent: AgentId) -> bool {
        guard(&self.waking).contains(&agent)
    }

    /// True while a compaction of `agent` is on the way.
    pub fn compacting(&self, agent: AgentId) -> bool {
        guard(&self.compacting).contains(&agent)
    }

    /// The agent changed its status: keep its rest clock.
    fn note_status(&self, agent: AgentId, status: AgentStatus) {
        let mut resting = guard(&self.resting);
        if status.is_resting() {
            resting.entry(agent).or_insert_with(Instant::now);
        } else {
            resting.remove(&agent);
        }
    }
}

/// Holds the wake of one agent; the drop lets the next wake go. Public
/// for the tests that hold a wake on the way.
/// This serializes starts, not queued-turn delivery: a Busy wake still
/// drains DB-queued turns to a running supervisor under the turn lock.
/// Turns refused while starting stay queued for the start or timer pass.
pub struct WakeGuard {
    lifecycle: Arc<Lifecycle>,
    agent: AgentId,
}

impl WakeGuard {
    /// The guard of `agent`, or `None` while a wake of it runs.
    pub fn take(lifecycle: &Arc<Lifecycle>, agent: AgentId) -> Option<Self> {
        guard(&lifecycle.waking).insert(agent).then(|| Self {
            lifecycle: lifecycle.clone(),
            agent,
        })
    }
}

impl Drop for WakeGuard {
    fn drop(&mut self) {
        guard(&self.lifecycle.waking).remove(&self.agent);
    }
}

/// Holds the compaction of one agent; the drop lets the next one go.
struct CompactGuard {
    lifecycle: Arc<Lifecycle>,
    agent: AgentId,
}

impl CompactGuard {
    fn take(lifecycle: &Arc<Lifecycle>, agent: AgentId) -> Option<Self> {
        guard(&lifecycle.compacting).insert(agent).then(|| Self {
            lifecycle: lifecycle.clone(),
            agent,
        })
    }
}

impl Drop for CompactGuard {
    fn drop(&mut self) {
        guard(&self.lifecycle.compacting).remove(&self.agent);
    }
}

/// What [`wake`] did.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Woke {
    /// The agent starts now, with resume (or in a new session, for a
    /// pending new session, `crate::reset`).
    Started,
    /// The agent runs (or restarts) already. The queued turns went to it.
    Running,
    /// Another wake of the agent is on the way.
    Busy,
    /// The agent is paused: its work waits for the unpause.
    Paused,
    /// The agent is a parked bound agent (`crate::bound`): its work waits
    /// for the unpark.
    Parked,
    /// The agent is in `error`: its work waits for the operator.
    Error,
    /// The daemon drains for a self-update (`crate::self_update`): the
    /// work waits for the new daemon, or for the end of the drain.
    Draining,
    /// A quota or budget hold lasts: the work waits for its end.
    Held,
    /// The subscription of the agent is disabled: the work waits for the
    /// enable (`POST /api/subscriptions/{name}/enable`).
    Disabled,
    /// No such agent.
    Gone,
    /// The start failed. The agent is in `error` with this reason.
    Failed(String),
}

/// The live statuses of the supervisor, and `restarting`.
async fn runs(state: &Shared, id: AgentId) -> bool {
    state
        .supervisor
        .status(id)
        .await
        .is_some_and(|s| s.is_live() || s == AgentStatus::Restarting)
}

/// Wake agent `id` for `cause` (`mail`, `turn`, `card`, `nudge`,
/// `unpause`, `hold_end`, `daemon_start`, `reset`, ...): start it with
/// resume when it does not run, then give it its queued turns. A pending
/// new session (`crate::reset`) starts it with no resume instead. See the
/// module doc for the agents that do not wake.
pub async fn wake(state: &Shared, id: AgentId, cause: &str) -> Woke {
    let wake_guard = WakeGuard::take(&state.lifecycle, id);
    let key = id.to_string();
    let read = state
        .db
        .read(move |c| {
            let v = agents::get(c, &key)?;
            let holds = crate::store::usage::holds_of(c, &key)?.len();
            let disabled = match v.agent.subscription.as_deref() {
                Some(n) => crate::store::subscriptions::find(c, n)?.is_some_and(|s| !s.enabled),
                None => false,
            };
            let parked = v.ephemeral && crate::store::spawns::is_parked(c, &key)?;
            Ok((v, holds, disabled, parked))
        })
        .await;
    let (view, holds, disabled, parked) = match read {
        Ok(r) => r,
        Err(e) if e.code == Code::NotFound => return Woke::Gone,
        Err(e) => {
            warn!(agent = %id, "cannot read the agent for a wake: {e}");
            return Woke::Failed(e.to_string());
        }
    };
    if view.paused {
        return Woke::Paused;
    }
    if parked {
        return Woke::Parked;
    }
    if runs(state, id).await {
        deliver_turns(state, id).await;
        return if wake_guard.is_some() {
            Woke::Running
        } else {
            Woke::Busy
        };
    }
    if wake_guard.is_none() {
        return Woke::Busy;
    }
    if state.self_update.draining() {
        return Woke::Draining;
    }
    if view.agent.status == AgentStatus::Error {
        return Woke::Error;
    }
    if holds > 0 {
        return Woke::Held;
    }
    // A disabled subscription: a stopped agent stays stopped, and its
    // work waits (PLAN §16). The status reason says why.
    if disabled {
        if view.agent.status == AgentStatus::Stopped
            && view.status_reason.as_deref() != Some(crate::store::subscriptions::REASON_DISABLED)
        {
            let key = id.to_string();
            if let Err(e) = state
                .db
                .write("system", move |t| {
                    agents::set_status(
                        t,
                        &key,
                        AgentStatus::Stopped,
                        Some(crate::store::subscriptions::REASON_DISABLED),
                    )
                    .map(|_| ())
                })
                .await
            {
                warn!(agent = %id, "cannot record the reason of a refused wake: {e}");
            }
        }
        return Woke::Disabled;
    }
    let (key, why, queued) = (id.to_string(), cause.to_owned(), view.queued);
    let begun = state
        .db
        .write("system", move |t| {
            let plan = crate::reset::take_new_session(t, &key)?;
            let resume = if plan.is_some() {
                None
            } else {
                agents::begin_resume(t, &key, "superseded")?
            };
            let v = agents::get(t, &key)?;
            t.emit(
                EventDraft::new(
                    "agent.woken",
                    format!("{}: woken ({why})", v.agent.name),
                    json!({"cause": why, "queued": queued}),
                )
                .agent(key.clone(), v.agent.name.clone())
                .project(v.agent.project.clone()),
            )?;
            Ok((v, resume, plan))
        })
        .await;
    let (view, resume, plan) = match begun {
        Ok(r) => r,
        Err(e) => {
            warn!(agent = %id, "cannot begin the session of a wake: {e}");
            return Woke::Failed(e.to_string());
        }
    };
    info!(agent = %id, name = %view.agent.name, %cause, resume = ?resume, "waking the agent");
    let started = match plan {
        Some(plan) => state.supervisor.new_session(view.agent, plan).await,
        None => state.supervisor.start(view.agent, resume).await,
    };
    match started {
        Ok(()) => {
            deliver_turns(state, id).await;
            Woke::Started
        }
        Err(SupervisorError::AlreadyRunning(_)) => {
            deliver_turns(state, id).await;
            Woke::Running
        }
        Err(e) => {
            let reason = format!("wake failed: {e}");
            warn!(agent = %id, "{reason}");
            let (key, r) = (id.to_string(), reason.clone());
            if let Err(e) = state
                .db
                .write("system", move |t| {
                    agents::end_session(t, &key, "start_failed")?;
                    crate::apply_status(t, &key, AgentStatus::Error, Some(&r))
                })
                .await
            {
                warn!(agent = %id, "cannot record the failed wake: {e}");
            }
            Woke::Failed(reason)
        }
    }
}

/// [`wake`] in a task of its own.
pub fn wake_soon(state: &Shared, id: AgentId, cause: &'static str) {
    let weak = Arc::downgrade(state);
    tokio::spawn(async move {
        if let Some(state) = weak.upgrade() {
            let woke = wake(&state, id, cause).await;
            debug!(agent = %id, %cause, ?woke, "wake");
        }
    });
}

/// Wake each agent of `only` (every agent when `None`) that has work
/// waiting for it ([`agents::queued_count`]).
pub async fn wake_with_work(state: &Shared, only: Option<AgentId>, cause: &'static str) {
    let ids = match state.db.read(agents::with_queued_work).await {
        Ok(ids) => ids,
        Err(e) => {
            warn!("cannot find the agents with queued work: {e}");
            return;
        }
    };
    for id in ids.iter().filter_map(|i| i.parse::<AgentId>().ok()) {
        if only.is_none_or(|o| o == id) {
            let woke = wake(state, id, cause).await;
            debug!(agent = %id, %cause, ?woke, "wake");
        }
    }
}

/// Give the queued turns of a running agent to the supervisor, in order,
/// and delete them. A turn that the supervisor refuses stays, with the
/// turns after it. Each turn goes under the agent's turn lock
/// ([`Lifecycle::turn_lock`]): a turn that was removed meanwhile is
/// skipped, and the row of a turn goes only after the supervisor took it,
/// so a later turn cannot pass it (TER-25). A turn that the supervisor
/// keeps for later ([`Supervisor::send_queued_turn`]) keeps its row: the
/// supervisor deletes it when it types the turn, and a daemon restart
/// before then delivers it again (TER-542).
///
/// [`Supervisor::send_queued_turn`]: crate::supervisor::Supervisor::send_queued_turn
async fn deliver_turns(state: &Shared, id: AgentId) {
    let key = id.to_string();
    let turns = match state.db.read(move |c| agents::queued_turns(c, &key)).await {
        Ok(t) => t,
        Err(e) => {
            warn!(agent = %id, "cannot read the queued turns: {e}");
            return;
        }
    };
    let lock = state.lifecycle.turn_lock(id);
    for t in turns {
        let _held = lock.lock().await;
        let (key, turn) = (id.to_string(), t.id);
        match state
            .db
            .read(move |c| agents::queued_turn_waits(c, &key, turn))
            .await
        {
            Ok(true) => {}
            Ok(false) => continue,
            Err(e) => {
                warn!(agent = %id, "cannot read the queued turns: {e}");
                return;
            }
        }
        match state.supervisor.send_queued_turn(id, turn, t.text).await {
            Ok(false) => {}
            Ok(true) => continue,
            Err(e) => {
                debug!(agent = %id, "a queued turn waits: {e}");
                break;
            }
        }
        // Only this row: a row before it may wait in the supervisor.
        let key = id.to_string();
        if let Err(e) = state
            .db
            .write("system", move |tx| {
                agents::remove_queued_turn(tx, &key, turn).map(|_| ())
            })
            .await
        {
            warn!(agent = %id, "cannot delete the delivered turns: {e}");
        }
    }
}

/// Stop agent `id` for idleness: a graceful stop, with the status reason
/// `idle`.
async fn idle_stop(state: &Shared, id: AgentId) {
    guard(&state.lifecycle.resting).remove(&id);
    state.lifecycle.expect_stop(id, REASON_IDLE);
    info!(agent = %id, "the agent rested for its idle stop; stopping it");
    if let Err(e) = state.supervisor.stop_idle(id).await {
        state.lifecycle.forget_stop(id);
        debug!(agent = %id, "the idle stop did not stop the agent: {e}");
    }
}

/// Stop the parked bound agent `id` (TER-74), as an idle stop with the
/// reason `parked`. Public for the tests.
///
/// TER-111 (R2-1): the idle pass read the parked agents a while ago, so
/// an unpark may have landed since. The stop holds the wake of the agent
/// ([`WakeGuard`]) and checks the park again first: an agent that was
/// unparked runs on, and a wake on the way skips the stop. An unpark
/// that lands during the stop finds the wake held (`Woke::Busy`): it may
/// deliver DB-queued turns if the supervisor still runs. After the stop
/// the agent wakes again to deliver any turns that remain queued.
pub async fn park_stop(state: &Shared, id: AgentId) {
    let Some(wake) = WakeGuard::take(&state.lifecycle, id) else {
        return;
    };
    let key = id.to_string();
    let still = state
        .db
        .read(move |c| crate::store::spawns::is_parked(c, &key))
        .await
        .unwrap_or(false);
    if !still {
        debug!(agent = %id, "the agent was unparked; no park stop");
        return;
    }
    guard(&state.lifecycle.resting).remove(&id);
    state.lifecycle.expect_stop(id, REASON_PARKED);
    info!(agent = %id, "the bound agent is parked and at rest; stopping it");
    if let Err(e) = state
        .supervisor
        .stop_by(id, terrarium_runner::StoppedBy::Operator)
        .await
    {
        state.lifecycle.forget_stop(id);
        debug!(agent = %id, "the park did not stop the agent: {e}");
    }
    drop(wake);
    let key = id.to_string();
    let unparked = state
        .db
        .read(move |c| crate::store::spawns::find(c, &key))
        .await
        .ok()
        .flatten()
        .is_some_and(|sp| sp.running() && sp.parked_at.is_none());
    if unparked {
        info!(agent = %id, "an unpark landed during the park stop; waking the agent");
        wake_soon(state, id, "unpark");
    }
}

/// True when the parked agent `id` must not stop now: a wake on the way,
/// a live exec session, or an open `process` wait. Queued work does not
/// count: it waits for the unpark.
async fn keeps_running_parked(state: &Shared, id: AgentId) -> bool {
    let key = id.to_string();
    if state.lifecycle.waking(id) || state.executor.live_of(&key) > 0 {
        return true;
    }
    state
        .db
        .read(move |c| {
            let runs: i64 = c.query_row(
                "SELECT count(*) FROM waits
                 WHERE agent = ?1 AND state = 'open' AND kind = 'process'",
                [&key],
                |r| r.get(0),
            )?;
            Ok(runs > 0)
        })
        .await
        .unwrap_or(true)
}

/// True when agent `id` must not stop for idleness now: it has a live exec
/// session, an open `process` wait, queued work, or a wake on the way.
async fn keeps_running(state: &Shared, id: AgentId) -> bool {
    let key = id.to_string();
    // Work that waits for a stream of its subscription counts as queued
    // work (`crate::stream_limit`).
    if state.lifecycle.waking(id)
        || state.lifecycle.reminders.checking(id)
        || state.executor.live_of(&key) > 0
        || state.usage.streams.is_waiting(id)
    {
        return true;
    }
    let busy = state
        .db
        .read(move |c| {
            let runs: i64 = c.query_row(
                "SELECT count(*) FROM waits
                 WHERE agent = ?1 AND state = 'open' AND kind = 'process'",
                [&key],
                |r| r.get(0),
            )?;
            Ok(runs > 0 || agents::queued_count(c, &key)? > 0)
        })
        .await;
    busy.unwrap_or(true)
}

/// One agent at rest in the idle pass, with its settings.
struct Resting {
    id: AgentId,
    harness: Option<HarnessId>,
    paused: bool,
    /// Since when it rests.
    since: Instant,
    /// Its idle stop; `None` for `off`.
    stop_after: Option<Duration>,
    /// The rest before a compaction on idle: the idle stop, else the
    /// global one, else [`DEFAULT_IDLE_STOP`]. `None`: no compaction on
    /// idle.
    compact_after: Option<Duration>,
    /// Its compact max age; `None` for `off`.
    max_age: Option<Duration>,
}

/// One pass of the idle stop and the auto compact: keep the rest clock of
/// each agent in step with the store, compact each agent that is due for
/// a compaction, and stop each other agent past its idle stop.
pub async fn idle_pass(state: &Shared) {
    if let Some(read) = idle_read(state).await {
        idle_act(state, read).await;
    }
}

/// The agents at rest that one idle pass read ([`idle_read`]).
pub struct IdleRead {
    now: Instant,
    at_rest: Vec<Resting>,
}

/// The first half of [`idle_pass`]: read the agents at rest and keep their
/// rest clocks in step with the store. `None` when the store read fails.
/// Public for the tests, which start a turn between this read and
/// [`idle_act`] (TER-141).
pub async fn idle_read(state: &Shared) -> Option<IdleRead> {
    let rows = match state
        .db
        .read(|c| {
            let mut stmt = c.prepare(
                "SELECT id, status, idle_stop, harness, paused, compact_on_idle,
                        compact_max_age
                 FROM agents",
            )?;
            let rows = stmt
                .query_map([], |r| {
                    Ok((
                        r.get::<_, String>(0)?,
                        r.get::<_, String>(1)?,
                        r.get::<_, Option<String>>(2)?,
                        r.get::<_, String>(3)?,
                        r.get::<_, bool>(4)?,
                        r.get::<_, Option<bool>>(5)?,
                        r.get::<_, Option<String>>(6)?,
                    ))
                })?
                .collect::<Result<Vec<_>, _>>()?;
            Ok(rows)
        })
        .await
    {
        Ok(r) => r,
        Err(e) => {
            warn!("the idle stop cannot read the agents: {e}");
            return None;
        }
    };
    let tuning = state.lifecycle.tuning();
    let now = Instant::now();
    let mut at_rest = Vec::new();
    {
        let mut resting = guard(&state.lifecycle.resting);
        let mut seen = HashSet::new();
        for (id, status, own, harness, paused, on_idle, max_age) in rows {
            let (Ok(id), Ok(status)) = (
                id.parse::<AgentId>(),
                serde_json::from_value::<AgentStatus>(json!(status)),
            ) else {
                continue;
            };
            if !status.is_resting() {
                continue;
            }
            seen.insert(id);
            let since = *resting.entry(id).or_insert(now);
            let stop_after = own
                .and_then(|s| s.parse::<IdleStop>().ok())
                .unwrap_or(tuning.idle_stop)
                .duration();
            let compact_after = on_idle.unwrap_or(tuning.compact_on_idle).then(|| {
                stop_after
                    .or(tuning.idle_stop.duration())
                    .unwrap_or(DEFAULT_IDLE_STOP)
            });
            let max_age = max_age
                .and_then(|s| s.parse::<CompactMaxAge>().ok())
                .unwrap_or(tuning.compact_max_age)
                .duration();
            at_rest.push(Resting {
                id,
                harness: harness.parse().ok(),
                paused,
                since,
                stop_after,
                compact_after,
                max_age,
            });
        }
        resting.retain(|id, _| seen.contains(id));
    }
    Some(IdleRead { now, at_rest })
}

/// The second half of [`idle_pass`]: compact or stop the agents of `read`
/// that are due. Each stop checks first that the agent still rests as
/// `read` saw it ([`still_resting`]). Public for the tests.
pub async fn idle_act(state: &Shared, read: IdleRead) {
    let IdleRead { now, at_rest } = read;
    // A pending reset (`crate::reset`) comes before the rest.
    let resets: HashSet<AgentId> = state
        .db
        .read(agents::with_pending_reset)
        .await
        .unwrap_or_default()
        .iter()
        .filter_map(|i| i.parse().ok())
        .collect();
    let parked: HashSet<AgentId> = state
        .db
        .read(crate::store::spawns::parked_ids)
        .await
        .unwrap_or_default()
        .iter()
        .filter_map(|i| i.parse().ok())
        .collect();
    for r in at_rest {
        // A parked bound agent stops at once, before a reset or a
        // compaction: those run in the session after the unpark.
        if parked.contains(&r.id) {
            if !state.lifecycle.compacting(r.id) && !keeps_running_parked(state, r.id).await {
                park_stop(state, r.id).await;
            }
            continue;
        }
        let rested = now.duration_since(r.since);
        let stop_due = r.stop_after.is_some_and(|l| rested >= l);
        if state.lifecycle.compacting(r.id) {
            continue;
        }
        if resets.contains(&r.id) {
            let ran = crate::reset::run_pending(state, r.id).await;
            if ran != crate::reset::Ran::Nothing {
                continue;
            }
        }
        if let Some(cause) = compact_cause(state, &r, rested).await {
            // A compaction on idle comes before the idle stop.
            let then_stop = cause == CAUSE_IDLE && r.stop_after.is_some();
            match start_compaction(state, r.id, cause, then_stop, None).await {
                Ok(()) => continue,
                // The idle stop goes on without a compaction.
                Err(SupervisorError::Unsupported(e)) => {
                    debug!(agent = %r.id, "no compaction: {e}");
                }
                Err(e) => {
                    debug!(agent = %r.id, %cause, "the compaction waits: {e}");
                    continue;
                }
            }
        }
        if !stop_due || keeps_running(state, r.id).await || !still_resting(state, &r).await {
            continue;
        }
        idle_stop(state, r.id).await;
    }
}

/// True when agent `r` still rests as the pass read it: its rest clock
/// did not move, and its live status is at rest. The pass reads the
/// agents first and awaits the store after that. A turn that started and
/// ended in those reads moves the clock, and a turn that still runs shows
/// in the live status. A stop on the old read would end that turn, or
/// come before the card reminder of its idle (TER-141).
async fn still_resting(state: &Shared, r: &Resting) -> bool {
    let same = guard(&state.lifecycle.resting).get(&r.id) == Some(&r.since);
    same && state
        .supervisor
        .status(r.id)
        .await
        .is_some_and(AgentStatus::is_resting)
}

/// Why agent `r` must compact now, if it must (PLAN §18, "Auto
/// compact"): `idle` at its idle stop, or `max_age` when its session
/// context is too old. Never for a paused or held agent, for a harness
/// with no compaction, or without a turn since the last compaction and
/// the start of the session.
async fn compact_cause(state: &Shared, r: &Resting, rested: Duration) -> Option<&'static str> {
    let idle_due = r.compact_after.is_some_and(|l| rested >= l);
    if r.paused
        || (!idle_due && r.max_age.is_none())
        || r.harness.and_then(HarnessId::compact_command).is_none()
    {
        return None;
    }
    let key = r.id.to_string();
    let read = state
        .db
        .read(move |c| {
            let holds = crate::store::usage::holds_of(c, &key)?.len();
            Ok((agents::compact_clock(c, &key)?, holds))
        })
        .await;
    let (clock, holds) = match read {
        Ok(v) => v,
        Err(e) => {
            warn!(agent = %r.id, "cannot read the compaction clock: {e}");
            return None;
        }
    };
    if holds > 0 || !clock.turned {
        return None;
    }
    if idle_due {
        return Some(CAUSE_IDLE);
    }
    let since = clock
        .since
        .and_then(|t| chrono::DateTime::parse_from_rfc3339(&t).ok())?;
    let age = (chrono::Utc::now() - since.with_timezone(&chrono::Utc))
        .to_std()
        .ok()?;
    r.max_age.filter(|m| age > *m).map(|_| CAUSE_MAX_AGE)
}

/// Start a compaction of agent `id` for `cause` (`idle`, `max_age`,
/// `manual`): the supervisor types the compact command, and a task waits
/// for the end, writes `agent.compacted`, and notes the time. With
/// `then_stop`, the agent stops for idleness after the end, unless work
/// came or it must keep running. `then`: the prompt turn after the end
/// (`crate::reset`). The error comes before the start: the agent compacts
/// already, does not run, is not idle (`NotReady`), or its harness has no
/// compaction (`Unsupported`).
pub async fn start_compaction(
    state: &Shared,
    id: AgentId,
    cause: &'static str,
    then_stop: bool,
    then: Option<crate::supervisor::CompactThen>,
) -> Result<(), SupervisorError> {
    let Some(held) = CompactGuard::take(&state.lifecycle, id) else {
        return Err(SupervisorError::NotReady(
            "the agent compacts already".into(),
        ));
    };
    let began = Instant::now();
    let wait = state.lifecycle.tuning().compact_wait;
    let end = state.supervisor.compact(id, wait, then).await?;
    info!(agent = %id, %cause, "the agent compacts its session");
    let weak = Arc::downgrade(state);
    tokio::spawn(async move {
        let end = end.await;
        let Some(state) = weak.upgrade() else { return };
        record_compaction(&state, id, cause, &end, began.elapsed()).await;
        drop(held);
        let rests = state
            .supervisor
            .status(id)
            .await
            .is_some_and(|s| s == AgentStatus::Idle);
        if then_stop && !end.work && rests && !keeps_running(&state, id).await {
            idle_stop(&state, id).await;
        }
    });
    Ok(())
}

/// Write the end of a compaction: `last_compact_at`, and the event
/// `agent.compacted` (`{cause, ok, duration_ms, reason}`).
async fn record_compaction(
    state: &Shared,
    id: AgentId,
    cause: &'static str,
    end: &CompactEnd,
    took: Duration,
) {
    let key = id.to_string();
    let (ok, reason) = (end.ok, end.reason.clone());
    let ms = u64::try_from(took.as_millis()).unwrap_or(u64::MAX);
    let written = state
        .db
        .write("system", move |t| {
            agents::note_compaction(t, &key, ok)?;
            let v = agents::get(t, &key)?;
            let mut data = json!({"cause": cause, "ok": ok, "duration_ms": ms});
            if let Some(r) = &reason {
                data["reason"] = json!(r);
            }
            let summary = match &reason {
                _ if ok => format!("{}: compacted ({cause})", v.agent.name),
                Some(r) => format!("{}: compaction failed ({cause}): {r}", v.agent.name),
                None => format!("{}: compaction failed ({cause})", v.agent.name),
            };
            t.emit(
                EventDraft::new("agent.compacted", summary, data)
                    .agent(key.clone(), v.agent.name.clone())
                    .project(v.agent.project.clone()),
            )
        })
        .await;
    if let Err(e) = written {
        warn!(agent = %id, "cannot record the compaction: {e}");
    }
}

/// The agent of a hub event, when it names one.
fn event_agent(ev: &crate::events::StoredEvent) -> Option<AgentId> {
    ev.agent_id.as_deref().and_then(|a| a.parse().ok())
}

/// The wake cause of a hub event, when the event gives its agent work
/// that wakes it: mail that is not held, a card that another actor
/// assigned to it, or an unblocked card.
fn wake_cause(ev: &crate::events::StoredEvent, agent: AgentId) -> Option<&'static str> {
    let s = |k: &str| ev.data.get(k).and_then(Value::as_str);
    match ev.kind.as_str() {
        "mail" if ev.data.get("held").and_then(Value::as_bool) != Some(true) => Some("mail"),
        "card" => {
            let me = agent.to_string();
            let self_made = ev.actor.as_deref() == Some(&format!("agent:{me}"));
            match s("change") {
                Some("assigned") if s("assignee") == Some(me.as_str()) && !self_made => {
                    Some("card")
                }
                Some("unblocked") => Some("card"),
                _ => None,
            }
        }
        _ => None,
    }
}

/// React to one hub event: wake the agent that has new work, keep the
/// rest clocks, and wake an agent after the end of its last hold or after
/// an idle stop when work came in meanwhile.
fn on_event(state: &Shared, ev: &crate::events::StoredEvent) {
    let Some(agent) = event_agent(ev) else {
        return;
    };
    if let Some(cause) = wake_cause(ev, agent) {
        wake_soon(state, agent, cause);
        return;
    }
    match ev.kind.as_str() {
        "status" => {
            let status = ev
                .data
                .get("status")
                .cloned()
                .and_then(|v| serde_json::from_value::<AgentStatus>(v).ok());
            let Some(status) = status else { return };
            let was_resting = guard(&state.lifecycle.resting).contains_key(&agent);
            state.lifecycle.note_status(agent, status);
            // Idle after a turn, with cards in the working column: the
            // card reminder, before any idle stop.
            if state.lifecycle.reminders.note_status(agent, status) {
                let state = state.clone();
                tokio::spawn(async move {
                    crate::card_reminder::check(&state, agent).await;
                });
            }
            let reason = ev.data.get("reason").and_then(Value::as_str);
            let state = state.clone();
            if status == AgentStatus::Stopped && reason == Some(REASON_IDLE) {
                // Mail that came during the stop waits in the store.
                tokio::spawn(async move {
                    wake_with_work(&state, Some(agent), "queued").await;
                });
            } else if status.is_resting() && !was_resting {
                // A pending reset runs first (`crate::reset`). A turn that
                // was queued while the agent stopped or started goes out
                // now.
                tokio::spawn(async move {
                    crate::reset::at_rest(&state, agent).await;
                    let key = agent.to_string();
                    let waiting = state
                        .db
                        .read(move |c| agents::queued_turns(c, &key))
                        .await
                        .is_ok_and(|t| !t.is_empty());
                    if waiting {
                        wake(&state, agent, "turn").await;
                    }
                });
            }
        }
        "quota.resumed" if ev.data.get("holds_left").and_then(Value::as_u64) == Some(0) => {
            let state = state.clone();
            tokio::spawn(async move {
                wake_with_work(&state, Some(agent), "hold_end").await;
            });
        }
        _ => {}
    }
}

/// Start the lifecycle tasks: the hub listener, the idle stop timer, and
/// the wake of the agents with queued work after the daemon start. Call it
/// after the reconcile of the live agents.
pub fn spawn(state: &Shared) -> Vec<JoinHandle<()>> {
    let weak: Weak<AppState> = Arc::downgrade(state);
    let mut rx = state.db.hub().subscribe();
    let listener = tokio::spawn(async move {
        loop {
            let ev = match rx.recv().await {
                Ok(ev) => ev,
                Err(RecvError::Lagged(n)) => {
                    warn!("the lifecycle listener missed {n} events; waking agents with work");
                    if let Some(state) = weak.upgrade() {
                        tokio::spawn(async move {
                            wake_with_work(&state, None, "queued").await;
                        });
                    }
                    continue;
                }
                Err(RecvError::Closed) => return,
            };
            let Some(state) = weak.upgrade() else { return };
            on_event(&state, &ev);
        }
    });
    let weak: Weak<AppState> = Arc::downgrade(state);
    // Not the state: the timer must not keep it alive while it sleeps.
    let lifecycle = state.lifecycle.clone();
    let timer = tokio::spawn(async move {
        if let Some(state) = weak.upgrade() {
            crate::reset::close_gates(&state).await;
            wake_with_work(&state, None, "daemon_start").await;
        }
        loop {
            let tick = lifecycle.tuning().tick;
            // A new tuning ends the sleep: its tick applies from now. (A
            // test sets a short tick after the start, while this sleep of
            // the default 15 s may already run.)
            let retuned = tokio::select! {
                () = tokio::time::sleep(tick) => false,
                () = lifecycle.retuned.notified() => true,
            };
            let Some(state) = weak.upgrade() else { return };
            if retuned {
                continue;
            }
            idle_pass(&state).await;
            // The idle alert to the leads (TER-67).
            crate::idle_alert::pass(&state).await;
            crate::trouble::pass(&state).await;
            // The lifetime and the owner of each ephemeral agent.
            crate::spawn::pass(&state).await;
            crate::card_dispatcher::pass(&state).await;
            crate::review_dispatcher::pass(&state).await;
        }
    });
    vec![listener, timer]
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::*;

    #[test]
    fn the_stop_reason_goes_to_the_next_stopped_status_only() {
        let l = Lifecycle::default();
        let a = AgentId::new();
        let status = |s: AgentStatus, r: &str| SupervisorEvent {
            agent: a,
            kind: SupervisorEventKind::Status {
                status: s,
                reason: Some(r.into()),
            },
            seq: None,
        };
        l.expect_stop(a, REASON_IDLE);
        let idle = l.with_stop_reason(status(AgentStatus::Idle, "x"));
        assert_eq!(idle, status(AgentStatus::Idle, "x"));
        let stopped = l.with_stop_reason(status(AgentStatus::Stopped, "stopped"));
        assert_eq!(stopped, status(AgentStatus::Stopped, REASON_IDLE));
        let again = l.with_stop_reason(status(AgentStatus::Stopped, "stopped"));
        assert_eq!(again, status(AgentStatus::Stopped, "stopped"));
        // An error clears the expected reason.
        l.expect_stop(a, REASON_PAUSED);
        l.with_stop_reason(status(AgentStatus::Error, "crash"));
        let later = l.with_stop_reason(status(AgentStatus::Stopped, "stopped"));
        assert_eq!(later, status(AgentStatus::Stopped, "stopped"));
    }

    #[test]
    fn the_rest_clock_follows_the_status() {
        let l = Lifecycle::default();
        let a = AgentId::new();
        l.note_status(a, AgentStatus::Idle);
        let first = guard(&l.resting)[&a];
        // `waiting` keeps the clock of `idle`.
        l.note_status(a, AgentStatus::Waiting);
        assert_eq!(guard(&l.resting)[&a], first);
        l.note_status(a, AgentStatus::Working);
        assert!(!guard(&l.resting).contains_key(&a));
        l.note_status(a, AgentStatus::Idle);
        std::thread::sleep(Duration::from_millis(5));
        l.touch(&a.to_string());
        assert!(guard(&l.resting)[&a] > first);
    }

    #[test]
    fn one_wake_at_a_time() {
        let l = Arc::new(Lifecycle::default());
        let a = AgentId::new();
        let g = WakeGuard::take(&l, a).unwrap();
        assert!(WakeGuard::take(&l, a).is_none());
        assert!(l.waking(a));
        drop(g);
        assert!(WakeGuard::take(&l, a).is_some());
    }
}
