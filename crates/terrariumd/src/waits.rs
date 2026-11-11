//! The run time of waits and watches (PLAN §12d, docs/api.md §6.7). The
//! rows and the rules that need a transaction are in
//! [`crate::store::waits`].
//!
//! - The timer ticks each [`Tuning::tick`]: it ends the `time` waits that
//!   are due and the waits past their timeout, checks the `card` waits and
//!   the `agent` waits (a wait whose agent was done at the create resolves
//!   after [`Tuning::settle`]), sends the wake mail, and fires the watches
//!   whose debounce window ended.
//! - Wake mail: each wait that ends (resolved, timed out, failed, or
//!   cancelled by someone other than the waiter) gets a mail from
//!   `system` to the waiter. The ends of one window of
//!   [`Tuning::wake_debounce`] from the first of them share one mail, which
//!   lists each wait with its own result; there is no other merge. The
//!   mail wakes the agent as all mail does (the push pointer of PLAN §12),
//!   also when other waits are still open.
//! - A process run (`POST /api/agents/{id}/run`, `terrarium run`) is an
//!   exec session that the daemon reads itself: it keeps the tail of the
//!   output for `terrarium run log`, and the end resolves the wait. The
//!   run does not belong to a turn: it lives through the end of the turn
//!   and a stop or restart of the agent. A daemon restart loses the
//!   session: the wait fails (`lost`).
//! - Watches: a listener on the event hub sees each `status` event. A
//!   watch without debounce fires at once. A watch with debounce waits
//!   [`Tuning::watch_window`] (or [`Tuning::flap_window`] for `idle`,
//!   `waiting`, and `working`), then fires only when the agent is still in
//!   one of the statuses of the watch.

use std::collections::{HashMap, VecDeque};
use std::sync::{Arc, Mutex, MutexGuard, Weak};
use std::time::{Duration, Instant};

use base64::Engine;
use serde::Serialize;
use serde_json::{Value, json};
use terrarium_core::AgentStatus;
use tokio::sync::broadcast::error::RecvError;
use tokio::task::JoinHandle;
use tracing::{debug, warn};

use crate::api::{AppState, Shared};
use crate::error::ApiResult;
use crate::executor::{End, ExecSession, MAX_READ, Stream};
use crate::store::waits::{self, RESULT_TAIL, Wait, WaitKind, WaitState};

/// `TERRARIUM_WAIT_DEFAULT_TIMEOUT`: the timeout of an `agent`, `process`,
/// or `card` wait that names none. Default 24 h.
pub const ENV_DEFAULT_TIMEOUT: &str = "TERRARIUM_WAIT_DEFAULT_TIMEOUT";
/// `TERRARIUM_WAIT_MAX_TIMEOUT`: the longest timeout (and `time` wait).
/// Default 7 d.
pub const ENV_MAX_TIMEOUT: &str = "TERRARIUM_WAIT_MAX_TIMEOUT";
/// The output of a run that `terrarium run log` can show.
pub const LOG_TAIL: usize = 64 * 1024;
/// The finished runs whose log tail stays in memory.
const KEEP_TAILS: usize = 64;

/// The times of the wait engine. Tests make them short.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Tuning {
    pub tick: Duration,
    /// An `agent` wait whose agent is done at the create resolves after
    /// this time, so that a card or mail pointer on its way can start the
    /// agent first.
    pub settle: Duration,
    /// The wake mail of a waiter goes out this long after the first end
    /// that has had no mail, with every end of that window.
    pub wake_debounce: Duration,
    pub watch_window: Duration,
    pub flap_window: Duration,
    pub default_timeout: Duration,
    pub max_timeout: Duration,
}

fn env_duration(name: &str, default: Duration) -> Duration {
    std::env::var(name)
        .ok()
        .and_then(|v| crate::settings::parse_duration(&v).ok())
        .unwrap_or(default)
}

impl Default for Tuning {
    fn default() -> Self {
        Self {
            tick: Duration::from_secs(1),
            settle: Duration::from_secs(5),
            wake_debounce: Duration::from_secs(2),
            watch_window: Duration::from_secs(5),
            flap_window: Duration::from_secs(10),
            default_timeout: env_duration(ENV_DEFAULT_TIMEOUT, Duration::from_secs(86_400)),
            max_timeout: env_duration(ENV_MAX_TIMEOUT, Duration::from_secs(7 * 86_400)),
        }
    }
}

/// The output of one run, as the daemon reads it.
#[derive(Debug, Default)]
struct TailBuf {
    all: VecDeque<u8>,
    stdout: VecDeque<u8>,
    stderr: VecDeque<u8>,
    end: Option<End>,
}

fn keep_tail(buf: &mut VecDeque<u8>, bytes: &[u8], cap: usize) {
    buf.extend(bytes);
    let over = buf.len().saturating_sub(cap);
    buf.drain(..over);
}

fn text_of(buf: &VecDeque<u8>) -> String {
    let (a, b) = buf.as_slices();
    let mut v = Vec::with_capacity(a.len() + b.len());
    v.extend_from_slice(a);
    v.extend_from_slice(b);
    String::from_utf8_lossy(&v).into_owned()
}

/// The log of a run, for `GET /api/waits/{id}/log`.
#[derive(Debug, Clone, Serialize)]
pub struct RunLog {
    pub wait_id: String,
    pub session_id: String,
    pub target: String,
    pub running: bool,
    /// The last bytes of stdout and stderr together, as text. Untrusted.
    pub output: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub end: Option<End>,
}

#[derive(Debug, Clone)]
struct Pending {
    agent: String,
    due: Instant,
    status: AgentStatus,
    reason: Option<String>,
}

/// The wait engine's state in [`AppState`].
#[derive(Default)]
pub struct WaitRuntime {
    tuning: Mutex<Tuning>,
    tails: Mutex<HashMap<String, Arc<Mutex<TailBuf>>>>,
    tail_order: Mutex<VecDeque<String>>,
    pending: Mutex<HashMap<String, Pending>>,
}

fn guard<T>(m: &Mutex<T>) -> MutexGuard<'_, T> {
    m.lock().unwrap_or_else(|e| e.into_inner())
}

impl WaitRuntime {
    pub fn tuning(&self) -> Tuning {
        *guard(&self.tuning)
    }

    /// Change the times. For tests.
    pub fn set_tuning(&self, t: Tuning) {
        *guard(&self.tuning) = t;
    }

    fn new_tail(&self, wait: &str) -> Arc<Mutex<TailBuf>> {
        let t = Arc::new(Mutex::new(TailBuf::default()));
        guard(&self.tails).insert(wait.to_owned(), t.clone());
        let mut order = guard(&self.tail_order);
        order.push_back(wait.to_owned());
        while order.len() > KEEP_TAILS {
            if let Some(old) = order.pop_front() {
                guard(&self.tails).remove(&old);
            }
        }
        t
    }

    fn tail(&self, wait: &str) -> Option<Arc<Mutex<TailBuf>>> {
        guard(&self.tails).get(wait).cloned()
    }
}

fn stamp(at: chrono::DateTime<chrono::Utc>) -> String {
    at.to_rfc3339_opts(chrono::SecondsFormat::Millis, true)
}

fn ago(d: Duration) -> String {
    stamp(chrono::Utc::now() - chrono::Duration::from_std(d).unwrap_or_default())
}

/// The time `d` from now, in the stored form.
pub fn from_now(d: Duration) -> String {
    stamp(chrono::Utc::now() + chrono::Duration::from_std(d).unwrap_or_default())
}

// ------------------------------------------------------------- the timer

/// Start the engine: fail the process waits whose run the daemon lost,
/// then run the timer and the watch listener.
pub fn spawn(state: &Shared) -> Vec<JoinHandle<()>> {
    let weak: Weak<AppState> = Arc::downgrade(state);
    let timer = tokio::spawn(async move {
        if let Some(state) = weak.upgrade()
            && let Err(e) = reconcile_runs(&state).await
        {
            warn!("cannot check the process waits after the start: {e}");
        }
        loop {
            let tick = {
                let Some(state) = weak.upgrade() else { return };
                if let Err(e) = tick(&state).await {
                    warn!("the wait timer failed: {e}");
                }
                state.waits.tuning().tick
            };
            tokio::time::sleep(tick).await;
        }
    });
    vec![timer, spawn_watch_listener(state)]
}

/// The open process waits whose exec session the executor does not have:
/// fail with `lost`. Restored native sessions get a fresh output reader.
async fn reconcile_runs(state: &Shared) -> ApiResult<()> {
    let open = state
        .db
        .read(|c| waits::all_open(c, WaitKind::Process))
        .await?;
    for w in open {
        let gone = w.session().is_none_or(|s| state.executor.find(s).is_none());
        if gone {
            let id = w.id.clone();
            state
                .db
                .write("system", move |t| {
                    waits::finish(
                        t,
                        &id,
                        WaitState::Failed,
                        json!({"error": "lost", "message":
                            "the daemon restarted, so it cannot see the end of the run; \
                             the command can still run on the target"}),
                        true,
                        0,
                    )
                })
                .await?;
        } else if let Some(session) = w.session().and_then(|s| state.executor.find(s)) {
            spawn_run_reader(state, w.id, w.agent, session);
        }
    }
    Ok(())
}

/// One tick of the timer.
pub async fn tick(state: &Shared) -> ApiResult<()> {
    let tuning = state.waits.tuning();
    let busy: i64 = state
        .db
        .read(|c| {
            Ok(c.query_row(
                "SELECT count(*) FROM waits WHERE state = 'open'
                    OR (state <> 'open' AND notified_at IS NULL)",
                [],
                |r| r.get(0),
            )?)
        })
        .await?;
    if busy > 0 {
        let now_at = stamp(chrono::Utc::now());
        let settled = ago(tuning.settle);
        let late_runs = state
            .db
            .write("system", move |t| {
                let runs = waits::expire(t, &now_at)?;
                waits::check_card_waits(t)?;
                waits::sweep_agent_waits(t, &settled)?;
                Ok(runs)
            })
            .await?;
        for w in late_runs {
            kill_run(state, &w);
        }
        let debounce = tuning.wake_debounce;
        state
            .db
            .write("system", move |t| {
                for (agent, ws) in waits::pending_wakes(t, chrono::Utc::now(), debounce)? {
                    waits::send_wake(t, &agent, &ws)?;
                }
                Ok(())
            })
            .await?;
    }
    flush_watches(state).await;
    Ok(())
}

// -------------------------------------------------------------- the runs

/// Kill the exec session of a process wait, if it still runs.
pub fn kill_run(state: &Shared, w: &Wait) {
    if let Some(s) = w.session().and_then(|s| state.executor.find(s)) {
        s.kill();
    }
}

/// [`kill_run`], and wait for the report of the kill on the target
/// ([`crate::run_kill`]): `{pid, term, kill, left}` or `{error}`.
pub async fn kill_run_report(state: &Shared, w: &Wait) -> Option<Value> {
    let s = w.session().and_then(|s| state.executor.find(s))?;
    s.kill_and_report().await
}

/// The result of a process wait from the end of its run.
fn run_result(end: &End, started: Instant, tail: Option<&TailBuf>) -> (WaitState, Value) {
    let mut r = match end {
        End::Exit {
            code,
            signal,
            reason,
        } => json!({"exit_code": code, "signal": signal, "reason": reason}),
        End::Error { code, message } => json!({"error": code, "message": message}),
        End::Cp { bytes, sha256 } => json!({"bytes": bytes, "sha256": sha256}),
    };
    r["duration_secs"] = json!((started.elapsed().as_secs_f64() * 10.0).round() / 10.0);
    if let Some(t) = tail {
        r["stdout_tail"] = json!(text_of(&t.stdout));
        r["stderr_tail"] = json!(text_of(&t.stderr));
    }
    let state = match end {
        End::Exit { reason, .. } if reason == "timeout" => WaitState::TimedOut,
        End::Exit { .. } | End::Cp { .. } => WaitState::Resolved,
        End::Error { .. } => WaitState::Failed,
    };
    (state, r)
}

async fn finish_run(state: &Shared, wait: &str, st: WaitState, result: Value) {
    let id = wait.to_owned();
    let hosts = state.executor.hosts.clone();
    if let Err(e) = state
        .db
        .write("system", move |t| {
            // An old output reader can observe its local SSH pipe closing
            // during shutdown. Only the replacement daemon owns completion.
            if hosts.is_stopped() { return Ok(()); }
            waits::finish(t, &id, st, result, true, 0).map(|_| ())
        })
        .await
    {
        warn!(wait, "cannot end the process wait: {e}");
    }
}

/// Read the output of a run that the daemon owns, to its end. The read
/// acknowledges the output, so the session never fills its buffer, and it
/// keeps the session from the reaper of unread sessions.
pub fn spawn_run_reader(state: &Shared, wait: String, waiter: String, sess: Arc<ExecSession>) {
    let tail = state.waits.new_tail(&wait);
    let weak = Arc::downgrade(state);
    let started = Instant::now();
    tokio::spawn(async move {
        let mut since = 0u64;
        loop {
            let read = sess.read(since, Duration::from_secs(30), MAX_READ).await;
            let Some(state) = weak.upgrade() else { return };
            if state.executor.hosts.is_stopped() { return; }
            let out = match read {
                Ok(o) => o,
                Err(e) => {
                    let end = End::Error {
                        code: "lost".into(),
                        message: e.message,
                    };
                    let (st, r) = run_result(&end, started, Some(&*guard(&tail)));
                    guard(&tail).end = Some(end);
                    finish_run(&state, &wait, st, r).await;
                    return;
                }
            };
            {
                let mut t = guard(&tail);
                for c in &out.chunks {
                    let Ok(bytes) = base64::engine::general_purpose::STANDARD.decode(&c.data)
                    else {
                        continue;
                    };
                    keep_tail(&mut t.all, &bytes, LOG_TAIL);
                    match c.stream {
                        Stream::Stderr => keep_tail(&mut t.stderr, &bytes, RESULT_TAIL),
                        _ => keep_tail(&mut t.stdout, &bytes, RESULT_TAIL),
                    }
                }
            }
            since = out.next;
            if out.done {
                let end = out.end.unwrap_or(End::Error {
                    code: "lost".into(),
                    message: "the run ended without an end".into(),
                });
                let (st, r) = run_result(&end, started, Some(&*guard(&tail)));
                guard(&tail).end = Some(end);
                finish_run(&state, &wait, st, r).await;
                return;
            }
            // A deleted waiter: nobody reads the result. End the run.
            let key = waiter.clone();
            let gone = state
                .db
                .read(move |c| crate::store::agents::display_name(c, &key))
                .await
                .is_ok_and(|n| n.is_none());
            if gone {
                debug!(wait, "the waiter is gone; killing its run");
                sess.kill();
            }
        }
    });
}

/// Watch an exec session that someone else reads (`terrarium wait process
/// <session>`): poll it until it ends or is gone.
pub fn spawn_session_poller(state: &Shared, wait: String, session: String) {
    let weak = Arc::downgrade(state);
    let started = Instant::now();
    tokio::spawn(async move {
        loop {
            tokio::time::sleep(Duration::from_secs(1)).await;
            let Some(state) = weak.upgrade() else { return };
            let open = {
                let id = wait.clone();
                state
                    .db
                    .read(move |c| waits::get(c, &id))
                    .await
                    .is_ok_and(|w| w.state == WaitState::Open)
            };
            if !open {
                return;
            }
            let end = match state.executor.find(&session) {
                Some(s) => match s.end() {
                    Some(e) => e,
                    None => continue,
                },
                None => End::Error {
                    code: "lost".into(),
                    message: "the exec session is gone".into(),
                },
            };
            let (st, r) = run_result(&end, started, None);
            finish_run(&state, &wait, st, r).await;
            return;
        }
    });
}

/// The log of the run of process wait `w`.
pub fn run_log(state: &Shared, w: &Wait, max: usize) -> RunLog {
    let session = w.session().unwrap_or_default().to_owned();
    let target = w.exec_target().unwrap_or_default().to_owned();
    let running =
        w.state == WaitState::Open && state.executor.find(&session).is_some_and(|s| !s.is_done());
    let (output, end) = match state.waits.tail(&w.id) {
        Some(t) => {
            let t = guard(&t);
            (text_of(&t.all), t.end.clone())
        }
        None => {
            // The tail left memory (a daemon restart, or an old run): the
            // result keeps the last bytes of each stream.
            let r = w.result.clone().unwrap_or(Value::Null);
            let s = |k: &str| {
                r.get(k)
                    .and_then(Value::as_str)
                    .unwrap_or_default()
                    .to_owned()
            };
            let mut text = s("stdout_tail");
            let err = s("stderr_tail");
            if !err.is_empty() {
                if !text.is_empty() && !text.ends_with('\n') {
                    text.push('\n');
                }
                text.push_str(&err);
            }
            (text, None)
        }
    };
    // The output is what the run printed: redact it whole, then cut it
    // (PLAN §16b.6).
    // The labels that the agent of the wait sees (S5): the run is its own.
    let view = terrarium_core::redact::View::Scope(w.project.clone());
    let output = state.db.redactor().get().redact_string_as(output, &view).0;
    let max = max.clamp(1, LOG_TAIL);
    let output = if output.len() > max {
        let mut i = output.len() - max;
        while !output.is_char_boundary(i) {
            i += 1;
        }
        output[i..].to_owned()
    } else {
        output
    };
    RunLog {
        wait_id: w.id.clone(),
        session_id: session,
        target,
        running,
        output,
        end,
    }
}

// ----------------------------------------------------------------- watches

fn spawn_watch_listener(state: &Shared) -> JoinHandle<()> {
    let mut rx = state.db.hub().subscribe();
    let weak = Arc::downgrade(state);
    tokio::spawn(async move {
        loop {
            let ev = match rx.recv().await {
                Ok(ev) => ev,
                Err(RecvError::Lagged(n)) => {
                    warn!("the watch listener missed {n} events");
                    continue;
                }
                Err(RecvError::Closed) => return,
            };
            if ev.kind != "status" {
                continue;
            }
            let Some(state) = weak.upgrade() else { return };
            let Some(agent) = ev.agent_id.clone() else {
                continue;
            };
            let Some(status) = ev
                .data
                .get("status")
                .and_then(|v| serde_json::from_value::<AgentStatus>(v.clone()).ok())
            else {
                continue;
            };
            let reason = ev
                .data
                .get("reason")
                .and_then(Value::as_str)
                .map(str::to_owned);
            on_status(&state, &agent, status, reason).await;
        }
    })
}

/// A status change of `agent`: fire or queue its watches.
async fn on_status(state: &Shared, agent: &str, status: AgentStatus, reason: Option<String>) {
    let key = agent.to_owned();
    let Ok(list) = state.db.read(move |c| waits::watches_on(c, &key)).await else {
        return;
    };
    let tuning = state.waits.tuning();
    for w in list.into_iter().filter(|w| w.on.contains(&status)) {
        if !w.debounce {
            fire(state, &w.id, status, reason.clone()).await;
            continue;
        }
        let window = match status {
            AgentStatus::Idle | AgentStatus::Waiting | AgentStatus::Working => tuning.flap_window,
            _ => tuning.watch_window,
        };
        let mut pending = guard(&state.waits.pending);
        let p = pending.entry(w.id.clone()).or_insert_with(|| Pending {
            agent: agent.to_owned(),
            due: Instant::now() + window,
            status,
            reason: None,
        });
        p.status = status;
        p.reason = reason.clone();
    }
}

async fn fire(state: &Shared, watch: &str, status: AgentStatus, reason: Option<String>) {
    let id = watch.to_owned();
    if let Err(e) = state
        .db
        .write("system", move |t| {
            waits::fire_watch(t, &id, status, reason.as_deref()).map(|_| ())
        })
        .await
    {
        warn!(watch, "cannot fire the watch: {e}");
    }
}

/// Fire the queued watches whose window ended, when the agent is still in
/// one of their statuses.
async fn flush_watches(state: &Shared) {
    let due: Vec<(String, Pending)> = {
        let mut pending = guard(&state.waits.pending);
        let now = Instant::now();
        let ids: Vec<String> = pending
            .iter()
            .filter(|(_, p)| p.due <= now)
            .map(|(id, _)| id.clone())
            .collect();
        ids.into_iter()
            .filter_map(|id| pending.remove(&id).map(|p| (id, p)))
            .collect()
    };
    for (id, p) in due {
        let (key, wid) = (p.agent.clone(), id.clone());
        let found = state
            .db
            .read(move |c| {
                let status = crate::store::agents::get(c, &key)?.agent.status;
                let watch = waits::get_watch(c, &wid)?;
                Ok((status, watch))
            })
            .await;
        let Ok((now, watch)) = found else { continue };
        if watch.on.contains(&now) {
            let reason = (now == p.status).then_some(p.reason).flatten();
            fire(state, &id, now, reason).await;
        }
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::*;

    #[test]
    fn tails_keep_the_last_bytes() {
        let mut b = VecDeque::new();
        keep_tail(&mut b, b"abcdef", 4);
        assert_eq!(text_of(&b), "cdef");
        keep_tail(&mut b, b"gh", 4);
        assert_eq!(text_of(&b), "efgh");
    }

    #[test]
    fn a_run_end_maps_to_a_state() {
        let t = Instant::now();
        let exit = |code: i32, reason: &str| End::Exit {
            code: Some(code),
            signal: None,
            reason: reason.into(),
        };
        let (s, r) = run_result(&exit(3, "exited"), t, None);
        assert_eq!(s, WaitState::Resolved);
        assert_eq!(r["exit_code"], 3);
        assert_eq!(
            run_result(&exit(137, "timeout"), t, None).0,
            WaitState::TimedOut
        );
        let lost = End::Error {
            code: "lost".into(),
            message: "gone".into(),
        };
        let (s, r) = run_result(&lost, t, None);
        assert_eq!(s, WaitState::Failed);
        assert_eq!(r["error"], "lost");
    }
}
