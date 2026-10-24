//! The seam between the daemon and the agent runners.
//!
//! The daemon calls a [`Supervisor`] to start, stop, and talk to agents. The
//! supervisor reports what happens as [`SupervisorEvent`]s; the daemon
//! writes each one to the event log and keeps `agents.status` in step.
//!
//! [`crate::runner_supervisor::RunnerSupervisor`] is the real one: one
//! `terrarium_runner::Runner` per running agent. `InMemorySupervisor` is a
//! fake for tests. It is built only for unit tests and with the
//! `fake-supervisor` feature (the integration tests turn it on).

use std::future::Future;
use std::pin::Pin;

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
    /// The agent cannot start: its adapter is a stub, or a file it needs
    /// is missing. The message says what to do.
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

#[derive(Debug, Clone, PartialEq)]
pub struct SupervisorEvent {
    pub agent: AgentId,
    pub kind: SupervisorEventKind,
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
    fn restart(&self, agent: Agent) -> BoxFuture<'_, Result<(), SupervisorError>>;
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
    /// The daemon calls this once, after it builds its state and before it
    /// serves. A supervisor that needs the store, the event hub, or the
    /// router keeps a weak reference.
    fn bind(&self, _state: &Shared) {}
    /// The daemon shuts down. End every harness, but keep the stored status,
    /// so the next daemon start launches the agents again with resume
    /// (PLAN §18).
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
        SupervisorEvent, SupervisorEventKind,
    };

    #[derive(Default)]
    struct FakeAgent {
        status: AgentStatus,
        /// Bumped on every start and stop, so a stale timer does nothing.
        epoch: u64,
        viewers: Vec<mpsc::Sender<Vec<u8>>>,
        size: Option<(u16, u16)>,
    }

    #[derive(Default)]
    struct Inner {
        agents: HashMap<AgentId, FakeAgent>,
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
            let _ = self.tx.send(SupervisorEvent { agent, kind });
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
                me.set_status(agent, AgentStatus::Idle, None);
            });
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
            });
            Ok(())
        }

        /// The last size a PTY client reported. For tests.
        pub fn pty_size(&self, agent: AgentId) -> Option<(u16, u16)> {
            self.lock().agents.get(&agent).and_then(|a| a.size)
        }
    }

    impl Supervisor for InMemorySupervisor {
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
            Box::pin(async move { self.turn(agent, text) })
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
