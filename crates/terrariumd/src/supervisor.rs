//! The seam between the daemon and the agent runners.
//!
//! The daemon calls a [`Supervisor`] to start, stop, and talk to agents. The
//! supervisor reports what happens as [`SupervisorEvent`]s; the daemon
//! writes each one to the event log and keeps `agents.status` in step.
//!
//! [`crate::runner_supervisor::RunnerSupervisor`] is the real one: one
//! agent runner per running agent, in a `terrarium-runner` process or in
//! the daemon process. `InMemorySupervisor` is a
//! fake for tests. It is built only for unit tests and with the
//! `fake-supervisor` feature (the integration tests turn it on).

use std::future::Future;
use std::pin::Pin;

use terrarium_core::config::RestartPolicy;
use terrarium_core::{Agent, AgentId, AgentStatus, Answer, Event, HarnessSessionId};
use tokio::sync::{broadcast, mpsc};

use crate::api::Shared;

#[cfg(any(test, feature = "fake-supervisor"))]
pub use fake::InMemorySupervisor;

pub type BoxFuture<'a, T> = Pin<Box<dyn Future<Output = T> + Send + 'a>>;

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum SupervisorError {
    #[error("agent {0} is not running")]
    NotRunning(AgentId),
    #[error("agent {0} is running already")]
    AlreadyRunning(AgentId),
    /// The agent cannot start: no adapter is registered for its harness, a
    /// file it needs is missing, or a policy hook refuses it. The message
    /// says what to do.
    #[error("{0}")]
    NotReady(String),
    #[error("{0}")]
    Other(String),
}

/// Input for a PTY.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PtyInput {
    /// Bytes typed into the terminal.
    Bytes(Vec<u8>),
    /// The client terminal changed size.
    Resize { rows: u16, cols: u16 },
}

/// An answer to a question the harness asks (`Event::Question`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AnswerInput {
    /// The adapter types the keys for this answer to `request_id`.
    Answer { request_id: String, answer: Answer },
    /// Raw keys, then Enter, for a dialog the adapter does not know.
    Keys(String),
}

/// One client attached to an agent's PTY. Dropping it detaches.
pub struct PtyAttachment {
    /// Escape sequences that draw the current screen. Send them first.
    pub snapshot: Vec<u8>,
    /// Terminal output after the snapshot. It ends when the agent stops.
    pub output: mpsc::Receiver<Vec<u8>>,
    pub input: mpsc::Sender<PtyInput>,
}

/// What a supervisor reports.
#[derive(Debug, Clone, PartialEq)]
pub enum SupervisorEventKind {
    /// The agent status changed.
    Status {
        status: AgentStatus,
        reason: Option<String>,
    },
    /// A normalised harness event: hooks, session ID, exit.
    Harness(Event),
    /// The supervisor started the harness again on its own (restart policy,
    /// a failed resume). End the current session with `outcome` and open a
    /// new one.
    NewSession { outcome: String },
}

/// Why an agent's new turns wait (PLAN §16.2): its plan is at 100 percent,
/// or its project spent a daily budget.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TurnHold {
    /// When the hold ends by itself (the window reset), if known.
    pub until: Option<chrono::DateTime<chrono::Utc>>,
    /// For the `blocked` status reason.
    pub reason: String,
}

#[derive(Debug, Clone, PartialEq)]
pub struct SupervisorEvent {
    pub agent: AgentId,
    pub kind: SupervisorEventKind,
    /// The runner seq of the event (`events.jsonl` line number), when the
    /// event comes from a runner. The event pump records the event once:
    /// it drops a seq at or below the last one that it recorded.
    pub seq: Option<u64>,
}

/// What [`Supervisor::recover`] found for an agent after a daemon start.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Recovery {
    /// A runner process still holds the harness. The supervisor connected
    /// to it and forwards its events again. The harness did not restart.
    Attached,
    /// The harness ended while the daemon was down, and the restart policy
    /// does not start it again. The supervisor reported the final status.
    Ended,
    /// No runner holds the agent. Launch it again with resume.
    Gone,
}

/// Does `policy` start a harness again after an exit? `clean`: the exit
/// code was 0, with no signal.
pub fn wants_restart(policy: RestartPolicy, clean: bool) -> bool {
    match policy {
        RestartPolicy::Always => true,
        RestartPolicy::OnFailure => !clean,
        RestartPolicy::Never => false,
    }
}

/// Runs agents. Every method returns quickly; the work happens in the
/// background and shows up as events.
pub trait Supervisor: Send + Sync + 'static {
    /// Start the agent. `resume` is the harness session to continue.
    fn start(
        &self,
        agent: Agent,
        resume: Option<HarnessSessionId>,
    ) -> BoxFuture<'_, Result<(), SupervisorError>>;
    fn stop(&self, agent: AgentId) -> BoxFuture<'_, Result<(), SupervisorError>>;
    /// [`Supervisor::stop`] for a caller that is not the operator: `by`
    /// goes into `runner-exit.json` (`stopped_by`). The default ignores
    /// `by`.
    fn stop_by(
        &self,
        agent: AgentId,
        by: terrarium_runner::StoppedBy,
    ) -> BoxFuture<'_, Result<(), SupervisorError>> {
        let _ = by;
        self.stop(agent)
    }
    fn restart(&self, agent: Agent) -> BoxFuture<'_, Result<(), SupervisorError>>;
    /// Clear an agent in `error` for an operator stop: end a stray runner
    /// that still holds the agent, if one exists, and record the agent as
    /// `stopped` in the supervisor. It does nothing to a live agent (stop
    /// it with [`Supervisor::stop`]). It emits no status event: the daemon
    /// writes the status. The default does nothing.
    fn clear_error(&self, agent: AgentId) -> BoxFuture<'_, ()> {
        let _ = agent;
        Box::pin(async {})
    }
    /// Type `text` into the harness as one turn.
    fn send_turn(&self, agent: AgentId, text: String)
    -> BoxFuture<'_, Result<(), SupervisorError>>;
    /// Answer a question the harness asked (the agent is `blocked`).
    fn answer(
        &self,
        agent: AgentId,
        answer: AnswerInput,
    ) -> BoxFuture<'_, Result<(), SupervisorError>>;
    fn attach_pty(&self, agent: AgentId) -> BoxFuture<'_, Result<PtyAttachment, SupervisorError>>;
    /// The live status, or `None` when the supervisor does not run the agent.
    fn status(&self, agent: AgentId) -> BoxFuture<'_, Option<AgentStatus>>;
    /// Every event from now on.
    fn subscribe(&self) -> broadcast::Receiver<SupervisorEvent>;
    /// Hold (`Some`) or release (`None`) the agent's new turns, for the
    /// quota policy (`crate::quota_policy`). While a hold lasts, the agent
    /// is `blocked` with the reason, and turns (from `send_turn` and from
    /// mail and card delivery) wait in order. A release sends them and
    /// returns the agent to its status. A hold on an agent that does not
    /// run applies when it starts. The default cannot hold: it answers
    /// `NotReady`, and the policy records the hold as advice only.
    fn hold_turns(
        &self,
        agent: AgentId,
        hold: Option<TurnHold>,
    ) -> BoxFuture<'_, Result<(), SupervisorError>> {
        let _ = (agent, hold);
        Box::pin(async {
            Err(SupervisorError::NotReady(
                "this supervisor cannot hold turns".into(),
            ))
        })
    }
    /// The installed version of each harness against its tested range
    /// (`GET /api/harnesses`). `refresh` runs the binaries again. The
    /// default knows only the pins.
    fn harness_versions(
        &self,
        refresh: bool,
    ) -> BoxFuture<'_, crate::harness_versions::HarnessReport> {
        let _ = refresh;
        Box::pin(async {
            crate::harness_versions::pins_only(crate::harness_versions::VersionPolicy::default())
        })
    }
    /// After a daemon start, find the runner of an agent that the store
    /// shows as live (PLAN §18). `events_since` is the last runner seq that
    /// the store recorded for it. The default finds nothing: [`Recovery::Gone`].
    fn recover(
        &self,
        agent: Agent,
        events_since: u64,
    ) -> BoxFuture<'_, Result<Recovery, SupervisorError>> {
        let _ = (agent, events_since);
        Box::pin(async { Ok(Recovery::Gone) })
    }
    /// A lease of `agent` on `target` was granted, and the lifecycle step
    /// of the grant is done (docs/api.md §17.7): queue the grant pointer.
    /// An agent that does not run gets it after its next start. The
    /// pointer reads the target state and the expiry when it goes out, and
    /// is dropped when lease `lease` ended before then. The default does
    /// nothing.
    fn lease_granted(&self, agent: AgentId, target: String, lease: String) {
        let _ = (agent, target, lease);
    }
    /// The daemon calls this once, after it builds its state and before it
    /// serves. A supervisor that needs the store, the event hub, or the
    /// router keeps a weak reference.
    fn bind(&self, _state: &Shared) {}
    /// The daemon shuts down. Keep the stored status. A harness in a
    /// runner process keeps running, and the next daemon start connects to
    /// it again. An in-process harness ends, and the next daemon start
    /// launches it again with resume (PLAN §18).
    fn shutdown(&self) {}
}

#[cfg(any(test, feature = "fake-supervisor"))]
mod fake {
    use std::collections::HashMap;
    use std::sync::{Arc, Mutex};
    use std::time::Duration;

    use terrarium_core::{Agent, AgentId, AgentStatus, Event, HarnessSessionId};
    use tokio::sync::{broadcast, mpsc};

    use super::{
        AnswerInput, BoxFuture, PtyAttachment, PtyInput, Supervisor, SupervisorError,
        SupervisorEvent, SupervisorEventKind, TurnHold,
    };

    #[derive(Default)]
    struct FakeAgent {
        status: AgentStatus,
        /// Bumped on every start and stop, so a stale timer does nothing.
        epoch: u64,
        viewers: Vec<mpsc::Sender<Vec<u8>>>,
        size: Option<(u16, u16)>,
        /// The quota hold, and the turns that wait for its end.
        held: Option<TurnHold>,
        waiting: Vec<String>,
    }

    #[derive(Default)]
    struct Inner {
        agents: HashMap<AgentId, FakeAgent>,
        /// With mail push on: the mail of each agent that waits for its
        /// next idle (the message IDs).
        mail: HashMap<AgentId, Vec<String>>,
        mail_push: bool,
        /// The grant pointers, in order: the agent, the line, and when it
        /// came (RFC 3339, microseconds).
        lease_pointers: Vec<(AgentId, String, String)>,
        state: Option<std::sync::Weak<crate::api::AppState>>,
    }

    /// A fake supervisor. `start` goes `starting` -> `idle` after `delay`.
    /// `send_turn` and `answer` go `working` -> `idle` after `delay`, with
    /// `turn_start` and `turn_end` events. The PTY echoes its input.
    #[derive(Clone)]
    pub struct InMemorySupervisor {
        delay: Duration,
        inner: Arc<Mutex<Inner>>,
        tx: broadcast::Sender<SupervisorEvent>,
    }

    impl InMemorySupervisor {
        pub fn new(delay: Duration) -> Self {
            let (tx, _) = broadcast::channel(1024);
            Self {
                delay,
                inner: Arc::default(),
                tx,
            }
        }

        fn lock(&self) -> std::sync::MutexGuard<'_, Inner> {
            match self.inner.lock() {
                Ok(g) => g,
                Err(poisoned) => poisoned.into_inner(),
            }
        }

        fn emit(&self, agent: AgentId, kind: SupervisorEventKind) {
            let _ = self.tx.send(SupervisorEvent {
                agent,
                kind,
                seq: None,
            });
        }

        /// Set the status and emit the change. Returns the new epoch.
        fn set_status(&self, agent: AgentId, status: AgentStatus, reason: Option<&str>) -> u64 {
            let epoch = {
                let mut inner = self.lock();
                let a = inner.agents.entry(agent).or_default();
                a.status = status;
                if !status.is_live() {
                    a.viewers.clear();
                }
                a.epoch
            };
            self.emit(
                agent,
                SupervisorEventKind::Status {
                    status,
                    reason: reason.map(str::to_owned),
                },
            );
            epoch
        }

        fn current(&self, agent: AgentId) -> Option<(AgentStatus, u64)> {
            self.lock().agents.get(&agent).map(|a| (a.status, a.epoch))
        }

        /// After `delay`, if nothing restarted or stopped the agent meanwhile,
        /// run `then`.
        fn later(&self, agent: AgentId, epoch: u64, then: impl FnOnce(&Self) + Send + 'static) {
            let me = self.clone();
            tokio::spawn(async move {
                tokio::time::sleep(me.delay).await;
                if me
                    .current(agent)
                    .is_some_and(|(s, e)| e == epoch && s.is_live())
                {
                    then(&me);
                }
            });
        }

        fn boot(&self, agent: AgentId, resume: Option<HarnessSessionId>) {
            let epoch = {
                let mut inner = self.lock();
                let a = inner.agents.entry(agent).or_default();
                a.epoch += 1;
                a.epoch
            };
            self.set_status(agent, AgentStatus::Starting, None);
            self.later(agent, epoch, move |me| {
                let session =
                    resume.unwrap_or_else(|| HarnessSessionId(format!("fake-{}", AgentId::new())));
                me.emit(
                    agent,
                    SupervisorEventKind::Harness(Event::SessionStarted {
                        harness_session: session,
                        transcript_path: None,
                        source: Some("startup".into()),
                    }),
                );
                match me.held(agent) {
                    Some(h) => me.set_status(agent, AgentStatus::Blocked, Some(&h.reason)),
                    None => me.set_status(agent, AgentStatus::Idle, None),
                };
                me.push_mail(agent);
            });
        }

        fn held(&self, agent: AgentId) -> Option<TurnHold> {
            self.lock().agents.get(&agent).and_then(|a| a.held.clone())
        }

        /// The turns that wait for the end of a quota hold. For tests.
        pub fn waiting_turns(&self, agent: AgentId) -> Vec<String> {
            self.lock()
                .agents
                .get(&agent)
                .map(|a| a.waiting.clone())
                .unwrap_or_default()
        }

        fn turn(&self, agent: AgentId, prompt: String) -> Result<(), SupervisorError> {
            let Some((_, epoch)) = self.current(agent).filter(|(s, _)| s.is_live()) else {
                return Err(SupervisorError::NotRunning(agent));
            };
            self.emit(
                agent,
                SupervisorEventKind::Harness(Event::TurnStart {
                    prompt: Some(prompt),
                }),
            );
            self.set_status(agent, AgentStatus::Working, None);
            self.later(agent, epoch, move |me| {
                me.emit(agent, SupervisorEventKind::Harness(Event::TurnEnd));
                me.set_status(agent, AgentStatus::Idle, None);
                me.push_mail(agent);
            });
            Ok(())
        }

        /// Type the push pointer of PLAN §12, as the real supervisor does,
        /// for the tests of waits (PLAN §12d). Off by default, so that the
        /// mail of other tests stays `queued`.
        pub fn set_mail_push(&self, on: bool) {
            self.lock().mail_push = on;
        }

        /// With mail push on: type one pointer turn for the queued mail of
        /// an idle agent, and mark the mail `delivered`.
        fn push_mail(&self, agent: AgentId) {
            let (ids, state) = {
                let mut inner = self.lock();
                let idle = inner
                    .agents
                    .get(&agent)
                    .is_some_and(|a| a.status == AgentStatus::Idle && a.held.is_none());
                if !inner.mail_push || !idle {
                    return;
                }
                let ids = inner.mail.remove(&agent).unwrap_or_default();
                (ids, inner.state.as_ref().and_then(std::sync::Weak::upgrade))
            };
            if ids.is_empty() {
                return;
            }
            let text = format!(
                "New mail ({} messages). Run `terrarium mail read`.",
                ids.len()
            );
            if self.turn(agent, text).is_err() {
                self.lock().mail.entry(agent).or_default().extend(ids);
                return;
            }
            if let Some(state) = state {
                tokio::spawn(async move {
                    let _ = state
                        .db
                        .write("system", move |t| {
                            for id in &ids {
                                t.execute(
                                    "UPDATE messages SET state = 'delivered', delivered_at = ?2
                                     WHERE id = ?1 AND state = 'queued'",
                                    rusqlite::params![id, crate::db::now()],
                                )?;
                            }
                            Ok(())
                        })
                        .await;
                });
            }
        }

        /// The grant pointers of `agent`: the line and when it came. For
        /// tests.
        pub fn lease_pointers(&self, agent: AgentId) -> Vec<(String, String)> {
            self.lock()
                .lease_pointers
                .iter()
                .filter(|p| p.0 == agent)
                .map(|p| (p.1.clone(), p.2.clone()))
                .collect()
        }

        /// The last size a PTY client reported. For tests.
        pub fn pty_size(&self, agent: AgentId) -> Option<(u16, u16)> {
            self.lock().agents.get(&agent).and_then(|a| a.size)
        }
    }

    impl Supervisor for InMemorySupervisor {
        fn bind(&self, state: &crate::api::Shared) {
            self.lock().state = Some(Arc::downgrade(state));
            let mut rx = state.db.hub().subscribe();
            let me = self.clone();
            tokio::spawn(async move {
                while let Ok(ev) = rx.recv().await {
                    if ev.kind != "mail"
                        || ev.data.get("held").and_then(|v| v.as_bool()) == Some(true)
                    {
                        continue;
                    }
                    let (Some(agent), Some(msg)) = (
                        ev.agent_id
                            .as_deref()
                            .and_then(|a| a.parse::<AgentId>().ok()),
                        ev.data.get("message_id").and_then(|v| v.as_str()),
                    ) else {
                        continue;
                    };
                    {
                        let mut inner = me.lock();
                        if !inner.mail_push {
                            continue;
                        }
                        inner.mail.entry(agent).or_default().push(msg.to_owned());
                    }
                    me.push_mail(agent);
                }
            });
        }

        /// Record the pointer line, as the real supervisor would type it.
        fn lease_granted(&self, agent: AgentId, target: String, lease: String) {
            let at = chrono::Utc::now().to_rfc3339_opts(chrono::SecondsFormat::Micros, true);
            let Some(state) = self
                .lock()
                .state
                .as_ref()
                .and_then(std::sync::Weak::upgrade)
            else {
                return;
            };
            let me = self.clone();
            tokio::spawn(async move {
                let line = state
                    .db
                    .read(move |c| crate::leases::pointer_line(c, &lease))
                    .await
                    .ok()
                    .flatten()
                    .unwrap_or_else(|| format!("(no pointer for {target})"));
                me.lock().lease_pointers.push((agent, line, at));
            });
        }

        fn start(
            &self,
            agent: Agent,
            resume: Option<HarnessSessionId>,
        ) -> BoxFuture<'_, Result<(), SupervisorError>> {
            Box::pin(async move {
                if self.current(agent.id).is_some_and(|(s, _)| s.is_live()) {
                    return Err(SupervisorError::AlreadyRunning(agent.id));
                }
                self.boot(agent.id, resume);
                Ok(())
            })
        }

        fn stop(&self, agent: AgentId) -> BoxFuture<'_, Result<(), SupervisorError>> {
            Box::pin(async move {
                if !self.current(agent).is_some_and(|(s, _)| s.is_live()) {
                    return Err(SupervisorError::NotRunning(agent));
                }
                {
                    let mut inner = self.lock();
                    if let Some(a) = inner.agents.get_mut(&agent) {
                        a.epoch += 1;
                    }
                }
                self.emit(
                    agent,
                    SupervisorEventKind::Harness(Event::Exit { code: Some(0) }),
                );
                self.set_status(agent, AgentStatus::Stopped, Some("stopped"));
                Ok(())
            })
        }

        fn clear_error(&self, agent: AgentId) -> BoxFuture<'_, ()> {
            Box::pin(async move {
                let mut inner = self.lock();
                if let Some(a) = inner.agents.get_mut(&agent)
                    && !a.status.is_live()
                {
                    a.epoch += 1;
                    a.status = AgentStatus::Stopped;
                }
            })
        }

        fn restart(&self, agent: Agent) -> BoxFuture<'_, Result<(), SupervisorError>> {
            Box::pin(async move {
                self.set_status(agent.id, AgentStatus::Restarting, Some("restart"));
                {
                    let mut inner = self.lock();
                    if let Some(a) = inner.agents.get_mut(&agent.id) {
                        a.viewers.clear();
                    }
                }
                self.boot(agent.id, agent.session.clone());
                Ok(())
            })
        }

        fn send_turn(
            &self,
            agent: AgentId,
            text: String,
        ) -> BoxFuture<'_, Result<(), SupervisorError>> {
            Box::pin(async move {
                {
                    let mut inner = self.lock();
                    if let Some(a) = inner.agents.get_mut(&agent)
                        && a.held.is_some()
                        && a.status.is_live()
                    {
                        a.waiting.push(text);
                        return Ok(());
                    }
                }
                self.turn(agent, text)
            })
        }

        fn hold_turns(
            &self,
            agent: AgentId,
            hold: Option<TurnHold>,
        ) -> BoxFuture<'_, Result<(), SupervisorError>> {
            Box::pin(async move {
                let (status, waiting) = {
                    let mut inner = self.lock();
                    let a = inner.agents.entry(agent).or_default();
                    let waiting = if hold.is_none() {
                        std::mem::take(&mut a.waiting)
                    } else {
                        Vec::new()
                    };
                    a.held = hold.clone();
                    (a.status, waiting)
                };
                match hold {
                    Some(h) if status.is_live() => {
                        self.set_status(agent, AgentStatus::Blocked, Some(&h.reason));
                    }
                    Some(_) => {}
                    None => {
                        if status == AgentStatus::Blocked {
                            self.set_status(agent, AgentStatus::Idle, Some("quota hold ended"));
                        }
                        if status.is_live() {
                            for text in waiting {
                                self.turn(agent, text)?;
                            }
                        }
                    }
                }
                Ok(())
            })
        }

        fn answer(
            &self,
            agent: AgentId,
            answer: AnswerInput,
        ) -> BoxFuture<'_, Result<(), SupervisorError>> {
            let text = match answer {
                AnswerInput::Answer { request_id, answer } => {
                    format!("answer {request_id}: {answer:?}")
                }
                AnswerInput::Keys(keys) => keys,
            };
            Box::pin(async move { self.turn(agent, text) })
        }

        fn attach_pty(
            &self,
            agent: AgentId,
        ) -> BoxFuture<'_, Result<PtyAttachment, SupervisorError>> {
            Box::pin(async move {
                let (out_tx, out_rx) = mpsc::channel(256);
                {
                    let mut inner = self.lock();
                    match inner.agents.get_mut(&agent) {
                        Some(a) if a.status.is_live() => a.viewers.push(out_tx),
                        _ => return Err(SupervisorError::NotRunning(agent)),
                    }
                }
                let (in_tx, mut in_rx) = mpsc::channel::<PtyInput>(256);
                let me = self.clone();
                tokio::spawn(async move {
                    while let Some(input) = in_rx.recv().await {
                        match input {
                            PtyInput::Bytes(bytes) => {
                                let viewers = me
                                    .lock()
                                    .agents
                                    .get(&agent)
                                    .map(|a| a.viewers.clone())
                                    .unwrap_or_default();
                                for v in viewers {
                                    let _ = v.send(bytes.clone()).await;
                                }
                            }
                            PtyInput::Resize { rows, cols } => {
                                if let Some(a) = me.lock().agents.get_mut(&agent) {
                                    a.size = Some((rows, cols));
                                }
                            }
                        }
                    }
                });
                Ok(PtyAttachment {
                    snapshot: b"\x1b[2J\x1b[H".to_vec(),
                    output: out_rx,
                    input: in_tx,
                })
            })
        }

        fn status(&self, agent: AgentId) -> BoxFuture<'_, Option<AgentStatus>> {
            Box::pin(async move { self.current(agent).map(|(s, _)| s) })
        }

        fn subscribe(&self) -> broadcast::Receiver<SupervisorEvent> {
            self.tx.subscribe()
        }
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::*;
    use std::path::Path;
    use std::time::Duration;
    use terrarium_core::config::AgentDef;

    fn agent() -> Agent {
        let def: AgentDef = serde_json::from_value(serde_json::json!({
            "name": "a", "project": "p", "harness": "claude"
        }))
        .unwrap();
        Agent::from_def(&def, Path::new("/tmp"))
    }

    async fn next_status(rx: &mut broadcast::Receiver<SupervisorEvent>) -> AgentStatus {
        loop {
            if let SupervisorEventKind::Status { status, .. } = rx.recv().await.unwrap().kind {
                return status;
            }
        }
    }

    #[tokio::test]
    async fn status_flow_and_echo() {
        let sup = InMemorySupervisor::new(Duration::from_millis(10));
        let mut rx = sup.subscribe();
        let a = agent();
        assert_eq!(
            sup.send_turn(a.id, "x".into()).await,
            Err(SupervisorError::NotRunning(a.id))
        );
        sup.start(a.clone(), None).await.unwrap();
        assert_eq!(next_status(&mut rx).await, AgentStatus::Starting);
        assert_eq!(next_status(&mut rx).await, AgentStatus::Idle);
        assert!(sup.start(a.clone(), None).await.is_err());
        sup.send_turn(a.id, "hi".into()).await.unwrap();
        assert_eq!(next_status(&mut rx).await, AgentStatus::Working);
        assert_eq!(next_status(&mut rx).await, AgentStatus::Idle);

        let mut pty = sup.attach_pty(a.id).await.unwrap();
        pty.input
            .send(PtyInput::Resize {
                rows: 30,
                cols: 100,
            })
            .await
            .unwrap();
        pty.input
            .send(PtyInput::Bytes(b"ls\r".to_vec()))
            .await
            .unwrap();
        assert_eq!(pty.output.recv().await.unwrap(), b"ls\r");
        assert_eq!(sup.pty_size(a.id), Some((30, 100)));

        sup.stop(a.id).await.unwrap();
        assert_eq!(next_status(&mut rx).await, AgentStatus::Stopped);
        assert!(pty.output.recv().await.is_none());
        assert_eq!(sup.status(a.id).await, Some(AgentStatus::Stopped));
    }
}
