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
use terrarium_core::{Agent, AgentId, AgentStatus, Answer, Event, HarnessId, HarnessSessionId};
use tokio::sync::{broadcast, mpsc};

use crate::api::Shared;

#[cfg(any(test, feature = "fake-supervisor"))]
pub use fake::{FakeCompact, InMemorySupervisor};

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
    /// The runner cannot do this call: it is older than the daemon.
    #[error("{0}")]
    Unsupported(String),
    #[error("{0}")]
    Other(String),
}

/// The message for a runner that has no `screen` control call.
pub const OLD_RUNNER: &str = "runner too old; restart the agent";

/// The `NotReady` message of [`Supervisor::interrupt`] when no turn runs.
pub const NO_TURN: &str = "the agent runs no turn: there is nothing to interrupt";
/// The `NotReady` message of [`Supervisor::interrupt`] during a compaction.
pub const INTERRUPT_COMPACTING: &str =
    "the agent compacts its session; wait for the end of the compaction";

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
    /// a failed resume). End the current session with `outcome` and open
    /// the session of the relaunch: the row of `resume`, the harness
    /// session it resumes, or a new row
    /// (`crate::store::agents::reopen_session`).
    NewSession {
        outcome: String,
        resume: Option<HarnessSessionId>,
    },
}

/// Why an agent's new turns wait (PLAN §16.2): the provider refused a turn
/// with a usage-limit error, or its project spent a daily budget.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TurnHold {
    /// When the hold ends by itself (the reset), if known.
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

/// The status reason of an agent while its session compacts (PLAN §18,
/// "Auto compact"). The agent is `working`.
pub const REASON_COMPACTING: &str = "compacting";

/// How a compaction ended ([`Supervisor::compact`]).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CompactEnd {
    /// The harness reported a compaction that did not fail.
    pub ok: bool,
    /// Why it is not ok: the harness error, no end signal in time, or a
    /// stop of the agent.
    pub reason: Option<String>,
    /// Work went to the agent at the end: turns that waited during the
    /// compaction, the recap turn, or a push pointer.
    pub work: bool,
}

/// The end of a compaction that runs ([`Supervisor::compact`]).
pub type CompactWait = BoxFuture<'static, CompactEnd>;

/// The prompt to send after a compaction that an actor requested
/// ([`Supervisor::compact`], `crate::reset`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CompactThen {
    /// Who asked: `self`, `operator`, or `project/agent`.
    pub by: String,
    /// The prompt, as the actor wrote it.
    pub text: String,
}

impl CompactThen {
    /// The turn after a compaction that ended with `end`: the tag, a note
    /// when the compaction did not complete, and the prompt.
    pub fn turn(&self, end: &CompactEnd) -> String {
        let failed = (!end.ok).then(|| end.reason.as_deref().unwrap_or("no reason given"));
        crate::reset::then_turn(&self.by, &self.text, failed)
    }
}

/// A new harness session that an actor requested
/// ([`Supervisor::new_session`], `crate::reset`).
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct NewSessionPlan {
    /// The status reason of the start (`new session: requested by <who>`).
    pub reason: String,
    /// The text of the recap turn. The supervisor adds the tag
    /// `[System:recap]` and the open questions. `None`: no recap.
    pub recap: Option<String>,
    /// The prompt turn after the recap, with its tag. It goes before the
    /// other turns that wait.
    pub then: Option<String>,
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

/// The harness version that a live agent runs
/// ([`Supervisor::harness_runs`]).
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct HarnessRun {
    pub agent: AgentId,
    /// `project/name`.
    pub name: String,
    pub harness: terrarium_core::HarnessId,
    /// The version dir that the launch resolved
    /// (`crate::harness_tools::active_version`). `None`: the harness came
    /// from `PATH` (no tools dir, or an agent that started before the
    /// version dirs).
    pub version: Option<String>,
}

/// The interrupt keys of each harness, from its adapter crate
/// (`Adapter::interrupt_keys`), for a supervisor with no adapter registry.
pub fn interrupt_keys(harness: HarnessId) -> Vec<Vec<u8>> {
    match harness {
        HarnessId::Claude => terrarium_adapter_claude::keys::interrupt(),
        HarnessId::Codex => terrarium_adapter_codex::keys::interrupt(),
        HarnessId::Copilot => terrarium_adapter_copilot::keys::interrupt(),
        HarnessId::Pi => terrarium_adapter_pi::keys::interrupt(),
    }
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

/// How a swap of an agent's account applied (`POST
/// /api/agents/{id}/subscription`, PLAN §16.3).
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SwapApplied {
    /// The running harness reads the new account at its next request
    /// (Claude Code, pi).
    Live,
    /// The agent is in a turn: it restarts with resume when the turn ends.
    RestartScheduled,
    /// The agent restarted with resume now.
    Restarted,
    /// The agent does not run: its next start uses the new account.
    NextStart,
}

impl SwapApplied {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Live => "live",
            Self::RestartScheduled => "restart_scheduled",
            Self::Restarted => "restarted",
            Self::NextStart => "next_start",
        }
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
    /// `agent` got a stream of its self-hosted subscription
    /// (`crate::stream_limit`): type the work that waited for it. The
    /// default does nothing.
    fn stream_granted(&self, agent: AgentId) {
        let _ = agent;
    }
    /// Type `text` as-is into the harness, then Enter, at once (D3, a
    /// leader's raw send): no escape of a leading `/` or `!`, no bracketed
    /// paste, and no wait for `idle`. A slash command or a bang command
    /// runs. The Enter goes after the adapter's exit-key pause
    /// (`Adapter::raw_turn_keys`, `exit_key_writes`). The default cannot.
    fn send_raw(&self, agent: AgentId, text: String) -> BoxFuture<'_, Result<(), SupervisorError>> {
        let _ = (agent, text);
        Box::pin(async {
            Err(SupervisorError::Unsupported(
                "this supervisor cannot type raw text".into(),
            ))
        })
    }
    /// Write `bytes` (key presses) to the agent's PTY at once (D3, `POST
    /// /api/agents/{id}/keys`). The default cannot.
    fn write_keys(
        &self,
        agent: AgentId,
        bytes: Vec<u8>,
    ) -> BoxFuture<'_, Result<(), SupervisorError>> {
        let _ = (agent, bytes);
        Box::pin(async {
            Err(SupervisorError::Unsupported(
                "this supervisor cannot send keys".into(),
            ))
        })
    }
    /// Interrupt the running turn of the agent (the dashboard's Stop,
    /// `POST /api/agents/{id}/interrupt`): type the interrupt keys of its
    /// harness (`Adapter::interrupt_keys`). The harness keeps running, and
    /// the agent goes `idle` with the reason `interrupted` when the screen
    /// settles (`terrarium_runner::Runner::interrupt`). `NotReady` with
    /// [`NO_TURN`]: the agent is not `working` or `blocked` in a turn;
    /// with [`INTERRUPT_COMPACTING`]: a compaction runs. `Unsupported`: the
    /// harness has no interrupt keys. The default cannot.
    fn interrupt(&self, agent: AgentId) -> BoxFuture<'_, Result<(), SupervisorError>> {
        let _ = agent;
        Box::pin(async {
            Err(SupervisorError::Unsupported(
                "this supervisor cannot interrupt a turn".into(),
            ))
        })
    }
    /// Answer a question the harness asked (the agent is `blocked`).
    fn answer(
        &self,
        agent: AgentId,
        answer: AnswerInput,
    ) -> BoxFuture<'_, Result<(), SupervisorError>>;
    fn attach_pty(&self, agent: AgentId) -> BoxFuture<'_, Result<PtyAttachment, SupervisorError>>;
    /// A read-only copy of the agent's parsed screen
    /// (`GET /api/agents/{id}/screen`). It does not attach and types
    /// nothing. The default cannot read a screen.
    fn screen(
        &self,
        agent: AgentId,
    ) -> BoxFuture<'_, Result<terrarium_runner::ScreenSnapshot, SupervisorError>> {
        let _ = agent;
        Box::pin(async {
            Err(SupervisorError::Unsupported(
                "this supervisor cannot read the screen".into(),
            ))
        })
    }
    /// Compact the session of an idle agent (PLAN §18, "Auto compact"):
    /// type the compact command of its harness as a raw command
    /// ([`terrarium_core::HarnessId::compact_command`]), show the agent
    /// `working` with the reason [`REASON_COMPACTING`], and keep new turns
    /// and pointers until the end. The answer comes when the compaction
    /// starts; its future ends when the harness reports the end, or after
    /// `wait`. Then the agent shows its status again, and the work that
    /// waited goes out. `NotRunning`: the agent does not run. `NotReady`:
    /// the agent is not idle, or compacts already. `Unsupported`: the
    /// harness has no compaction that the daemon can see the end of. The
    /// default cannot compact.
    ///
    /// `then`: the turn to type after the end, whatever the outcome
    /// ([`CompactThen::turn`]). It goes before the other work that waited.
    /// A compaction also opens the reset gate ([`Supervisor::reset_gate`]).
    fn compact(
        &self,
        agent: AgentId,
        wait: std::time::Duration,
        then: Option<CompactThen>,
    ) -> BoxFuture<'_, Result<CompactWait, SupervisorError>> {
        let _ = (agent, wait, then);
        Box::pin(async {
            Err(SupervisorError::Unsupported(
                "this supervisor cannot compact".into(),
            ))
        })
    }
    /// True when [`Supervisor::compact`] can compact agents of `harness`
    /// at all, so that a compaction request is refused before it is
    /// written (`crate::reset::request`). A `false` here is the
    /// `Unsupported` of `compact`. The default cannot compact.
    fn can_compact(&self, harness: HarnessId) -> bool {
        let _ = harness;
        false
    }
    /// Close (`on`) or open the reset gate of `agent` (`crate::reset`):
    /// while it is closed, the supervisor types no turn, recap, or push
    /// pointer into the agent; they wait. So a pending compaction or new
    /// session runs before the other work when the current turn ends. A
    /// compaction or a new session opens the gate. The gate stays over a
    /// stop. The default does nothing.
    fn reset_gate(&self, agent: AgentId, on: bool) {
        let _ = (agent, on);
    }
    /// Start a new harness session for `agent`, not a resume (`crate::reset`).
    /// A live agent ends its harness gracefully (the exit keys at rest,
    /// else a kill) and starts again with no resume. An agent that does
    /// not run starts in a new session now. The recap turn and the `then`
    /// turn of `plan` go out at the first idle, before the turns that
    /// wait. It opens the reset gate. `NotReady`: the agent restarts now.
    /// The default cannot.
    fn new_session(
        &self,
        agent: Agent,
        plan: NewSessionPlan,
    ) -> BoxFuture<'_, Result<(), SupervisorError>> {
        let _ = (agent, plan);
        Box::pin(async {
            Err(SupervisorError::Unsupported(
                "this supervisor cannot start a new session".into(),
            ))
        })
    }
    /// A signal of the harness that the daemon read from the session file
    /// of the agent, not from a hook (`crate::usage_collector`): the
    /// compaction end of Copilot CLI. `at` is the time of the
    /// line. A supervisor uses only the signals of a compaction that runs
    /// and that are not older than its start. The default ignores them.
    fn session_signal(
        &self,
        agent: AgentId,
        event: Event,
        at: Option<chrono::DateTime<chrono::Utc>>,
    ) {
        let _ = (agent, event, at);
    }
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
    /// The installed version of each harness (`GET /api/harnesses`). `refresh` runs the binaries again. The
    /// default knows only the pins.
    fn harness_versions(
        &self,
        refresh: bool,
    ) -> BoxFuture<'_, crate::harness_versions::HarnessReport> {
        let _ = refresh;
        Box::pin(async { crate::harness_versions::pins_only() })
    }
    /// The harness version of each agent that runs or starts now. The
    /// harness update lists the agents on an older version, and the GC of
    /// the tools dir keeps their version dirs. The default knows none.
    fn harness_runs(&self) -> Vec<HarnessRun> {
        Vec::new()
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
    /// Move the agent to the account of `agent.subscription` (PLAN §16.3).
    /// `agent` is the stored agent with the new subscription. Claude Code
    /// and pi swap live when the new account is bound in the sandbox; the
    /// other harnesses, and an account that is not bound, restart with
    /// resume, at once when the agent is not in a turn, else when the turn
    /// ends. The default records nothing: the next start uses the new
    /// account.
    fn swap_subscription(
        &self,
        agent: Agent,
    ) -> BoxFuture<'_, Result<SwapApplied, SupervisorError>> {
        let _ = agent;
        Box::pin(async { Ok(SwapApplied::NextStart) })
    }
    /// Run `f` while no agent of `harness` runs or starts, and keep every
    /// start of that harness waiting until it returns. False: an agent of
    /// the harness runs, and `f` did not run. The start-up migration of
    /// the shared logins uses it (`crate::accounts::migrate`). It blocks:
    /// call it off the async threads. The default runs `f`.
    fn with_quiet_harness(&self, harness: HarnessId, f: &mut dyn FnMut()) -> bool {
        let _ = harness;
        f();
        true
    }
    /// The daemon calls this once, after it builds its state and before it
    /// serves. A supervisor that needs the store, the event hub, or the
    /// router keeps a weak reference.
    fn bind(&self, _state: &Shared) {}
    /// Where the harness of `agent` sees its workspace: the sandbox shows
    /// it at `/work/<project>/<name>` (the path a target sees too). The
    /// real supervisor answers the host path in sandbox mode `none`.
    fn workspace_inside(&self, agent: &Agent) -> std::path::PathBuf {
        std::path::Path::new(crate::workspace::WORKSPACE_MOUNT_ROOT)
            .join(&agent.project)
            .join(&agent.name)
    }
    /// The daemon shuts down. Keep the stored status. A harness in a
    /// runner process keeps running, and the next daemon start connects to
    /// it again. An in-process harness ends, and the next daemon start
    /// launches it again with resume (PLAN §18).
    fn shutdown(&self) {}
}

#[cfg(any(test, feature = "fake-supervisor"))]
mod fake {
    use std::collections::{HashMap, HashSet};
    use std::sync::{Arc, Mutex};
    use std::time::Duration;

    use terrarium_core::{Agent, AgentId, AgentStatus, Event, HarnessId, HarnessSessionId};
    use tokio::sync::{broadcast, mpsc, oneshot};

    use super::{
        AnswerInput, BoxFuture, CompactEnd, CompactThen, CompactWait, INTERRUPT_COMPACTING,
        NO_TURN, NewSessionPlan, OLD_RUNNER, PtyAttachment, PtyInput, REASON_COMPACTING,
        Supervisor, SupervisorError, SupervisorEvent, SupervisorEventKind, TurnHold,
    };

    /// How the fake harness answers a compact command. For tests.
    #[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
    pub enum FakeCompact {
        /// `CompactFinished` with `ok` after the delay.
        #[default]
        Ok,
        /// `CompactFinished` with an error after the delay.
        Fail,
        /// No end signal: the caller waits for its time limit.
        Silent,
    }

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
        /// The bytes that the screen shows: the PTY echo and `feed_screen`.
        screen: Vec<u8>,
        /// `screen` answers as an old runner does.
        old_runner: bool,
        /// The bytes of `send_raw` and `write_keys`, in order.
        typed: Vec<u8>,
        /// Turns that came while the agent started, as the real supervisor
        /// keeps them for its first idle. One goes out at each idle.
        early: Vec<String>,
        /// The text of each turn that started, in order. For tests.
        prompts: Vec<String>,
        /// The harness of the last start.
        harness: Option<HarnessId>,
        /// The compaction that runs: its number, where its end goes, and
        /// the turn after it.
        compacting: Option<(
            u64,
            Option<oneshot::Sender<CompactEnd>>,
            Option<CompactThen>,
        )>,
        /// The reset gate ([`Supervisor::reset_gate`]): no early turn and
        /// no mail pointer goes out while it is closed.
        gated: bool,
        /// How the next compactions end.
        compact: FakeCompact,
        /// The compactions that started. For tests.
        compactions: usize,
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
        /// `stop` returns only after the store shows `stopped`.
        settle_stops: bool,
        /// Harnesses that the fake cannot compact, on top of the ones with
        /// no `compact_command`.
        no_compact: HashSet<HarnessId>,
        /// The number of the next compaction.
        compact_seq: u64,
        /// The session signals of each agent, in order
        /// ([`Supervisor::session_signal`]).
        signals: Vec<(AgentId, Event)>,
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
            self.boot_with(agent, resume, None);
        }

        /// [`Self::boot`] with the status reason of the start.
        fn boot_with(
            &self,
            agent: AgentId,
            resume: Option<HarnessSessionId>,
            reason: Option<&str>,
        ) {
            let epoch = {
                let mut inner = self.lock();
                let a = inner.agents.entry(agent).or_default();
                a.epoch += 1;
                a.compacting = None;
                a.epoch
            };
            self.set_status(agent, AgentStatus::Starting, reason);
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
                me.next_early(agent);
                me.push_mail(agent);
            });
        }

        /// Start the next early turn of an idle agent.
        fn next_early(&self, agent: AgentId) {
            let text = {
                let mut inner = self.lock();
                match inner.agents.get_mut(&agent) {
                    Some(a)
                        if a.status == AgentStatus::Idle
                            && a.held.is_none()
                            && !a.gated
                            && !a.early.is_empty() =>
                    {
                        a.early.remove(0)
                    }
                    _ => return,
                }
            };
            if self.turn(agent, text.clone()).is_err()
                && let Some(a) = self.lock().agents.get_mut(&agent)
            {
                a.early.insert(0, text);
            }
        }

        /// The text of each turn that started, in order. For tests.
        pub fn prompts(&self, agent: AgentId) -> Vec<String> {
            self.lock()
                .agents
                .get(&agent)
                .map(|a| a.prompts.clone())
                .unwrap_or_default()
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
            if let Some(a) = self.lock().agents.get_mut(&agent) {
                a.prompts.push(prompt.clone());
            }
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
                me.next_early(agent);
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
                    .is_some_and(|a| a.status == AgentStatus::Idle && a.held.is_none() && !a.gated);
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

        /// Write `bytes` to the agent's fake screen, as harness output. For
        /// tests.
        pub fn feed_screen(&self, agent: AgentId, bytes: &[u8]) {
            if let Some(a) = self.lock().agents.get_mut(&agent) {
                a.screen.extend_from_slice(bytes);
            }
        }

        /// Emit a harness event of `agent`, as a runner does. For tests.
        pub fn emit_harness(&self, agent: AgentId, event: Event) {
            self.emit(agent, SupervisorEventKind::Harness(event));
        }

        /// The bytes that `send_raw` and `write_keys` wrote to the agent.
        /// For tests.
        pub fn typed(&self, agent: AgentId) -> Vec<u8> {
            self.lock()
                .agents
                .get(&agent)
                .map(|a| a.typed.clone())
                .unwrap_or_default()
        }

        fn type_bytes(&self, agent: AgentId, bytes: &[u8]) -> Result<(), SupervisorError> {
            let mut inner = self.lock();
            match inner.agents.get_mut(&agent) {
                Some(a) if a.status.is_live() => {
                    a.typed.extend_from_slice(bytes);
                    a.screen.extend_from_slice(bytes);
                    Ok(())
                }
                _ => Err(SupervisorError::NotRunning(agent)),
            }
        }

        /// Open a turn that does not end by itself: `turn_start`, then
        /// `working` (a long turn, for the interrupt). For tests.
        pub fn open_turn(&self, agent: AgentId) {
            self.emit(
                agent,
                SupervisorEventKind::Harness(Event::TurnStart { prompt: None }),
            );
            self.set_status(agent, AgentStatus::Working, None);
        }

        /// Forget `agent`, as a daemon restart does when its runner process
        /// is gone: the supervisor no longer runs it. The store keeps the
        /// status it had. For tests (TER-45).
        pub fn lose_runner(&self, agent: AgentId) {
            self.lock().agents.remove(&agent);
        }

        /// Make `screen` answer as a runner without the `screen` call. For
        /// tests.
        pub fn set_old_runner(&self, agent: AgentId) {
            if let Some(a) = self.lock().agents.get_mut(&agent) {
                a.old_runner = true;
            }
        }

        /// With `on`, `stop` returns only after the store shows the agent
        /// `stopped` (up to 5 s), as a slow real stop can. Without it, the
        /// `stopped` status can go into the store before or after the
        /// caller of `stop` writes. For tests.
        pub fn settle_stops(&self, on: bool) {
            self.lock().settle_stops = on;
        }

        /// The last size a PTY client reported. For tests.
        pub fn pty_size(&self, agent: AgentId) -> Option<(u16, u16)> {
            self.lock().agents.get(&agent).and_then(|a| a.size)
        }

        /// Set how the compactions of `agent` end. For tests.
        pub fn set_compact(&self, agent: AgentId, mode: FakeCompact) {
            self.lock().agents.entry(agent).or_default().compact = mode;
        }

        /// End the open compaction of `agent` as its time limit does (no
        /// end signal came), so a test of a `Silent` compaction picks the
        /// moment instead of racing a short limit. False when none is
        /// open. For tests.
        pub fn time_out_compaction(&self, agent: AgentId) -> bool {
            let end = CompactEnd {
                ok: false,
                reason: Some("no end of the compaction".into()),
                work: false,
            };
            self.end_compaction(agent, None, end).is_some()
        }

        /// The fake cannot compact agents of `harness`. For tests.
        pub fn refuse_compact(&self, harness: HarnessId) {
            self.lock().no_compact.insert(harness);
        }

        /// The early turns of `agent` that wait (for the start, a
        /// compaction, or the reset gate). For tests.
        pub fn early_turns(&self, agent: AgentId) -> Vec<String> {
            self.lock()
                .agents
                .get(&agent)
                .map(|a| a.early.clone())
                .unwrap_or_default()
        }

        /// True while the reset gate of `agent` is closed. For tests.
        pub fn gated(&self, agent: AgentId) -> bool {
            self.lock().agents.get(&agent).is_some_and(|a| a.gated)
        }

        /// The number of compactions of `agent` that started. For tests.
        pub fn compactions(&self, agent: AgentId) -> usize {
            self.lock().agents.get(&agent).map_or(0, |a| a.compactions)
        }

        /// The session signals that the collector gave for `agent`, in
        /// order. For tests.
        pub fn session_signals(&self, agent: AgentId) -> Vec<Event> {
            self.lock()
                .signals
                .iter()
                .filter(|(a, _)| *a == agent)
                .map(|(_, e)| e.clone())
                .collect()
        }

        /// End compaction `token` of `agent` (any one when `None`): the
        /// status goes back to `idle` (or `blocked` under a hold), and the
        /// turns that waited start.
        fn end_compaction(
            &self,
            agent: AgentId,
            token: Option<u64>,
            mut end: CompactEnd,
        ) -> Option<CompactEnd> {
            let (done, held) = {
                let mut inner = self.lock();
                let a = inner.agents.get_mut(&agent)?;
                if !a
                    .compacting
                    .as_ref()
                    .is_some_and(|(t, _, _)| token.is_none_or(|w| w == *t))
                {
                    return None;
                }
                let (_, done, then) = a.compacting.take()?;
                if let Some(then) = then {
                    a.early.insert(0, then.turn(&end));
                }
                end.work = !a.early.is_empty();
                (done, a.held.clone())
            };
            match held {
                Some(h) => self.set_status(agent, AgentStatus::Blocked, Some(&h.reason)),
                None => {
                    let reason = if end.ok {
                        "compacted"
                    } else {
                        "compaction failed"
                    };
                    self.set_status(agent, AgentStatus::Idle, Some(reason))
                }
            };
            if let Some(tx) = done {
                let _ = tx.send(end.clone());
            }
            self.next_early(agent);
            self.push_mail(agent);
            Some(end)
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
                self.lock().agents.entry(agent.id).or_default().harness = Some(agent.harness);
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
                        // A compaction ends with the harness.
                        a.compacting = None;
                    }
                }
                self.emit(
                    agent,
                    SupervisorEventKind::Harness(Event::Exit { code: Some(0) }),
                );
                self.set_status(agent, AgentStatus::Stopped, Some("stopped"));
                let state = {
                    let inner = self.lock();
                    inner
                        .settle_stops
                        .then(|| inner.state.as_ref().and_then(std::sync::Weak::upgrade))
                        .flatten()
                };
                if let Some(state) = state {
                    let id = agent.to_string();
                    let deadline = tokio::time::Instant::now() + Duration::from_secs(5);
                    while tokio::time::Instant::now() < deadline {
                        let key = id.clone();
                        let stored = state
                            .db
                            .read(move |c| crate::store::agents::get(c, &key))
                            .await
                            .map(|a| a.agent.status);
                        if !matches!(stored, Ok(s) if s != AgentStatus::Stopped) {
                            break;
                        }
                        tokio::time::sleep(Duration::from_millis(10)).await;
                    }
                }
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
                    // A starting agent, one with early turns, or one that
                    // compacts: in order, after the start or the compaction.
                    if let Some(a) = inner.agents.get_mut(&agent)
                        && a.status.is_live()
                        && (a.status == AgentStatus::Starting
                            || !a.early.is_empty()
                            || a.gated
                            || a.compacting.is_some())
                    {
                        a.early.push(text);
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

        /// The text as the default `Adapter::raw_turn_keys` types it.
        fn send_raw(
            &self,
            agent: AgentId,
            text: String,
        ) -> BoxFuture<'_, Result<(), SupervisorError>> {
            let bytes = terrarium_core::adapter::raw_text_keys(&text);
            Box::pin(async move { self.type_bytes(agent, &bytes) })
        }

        fn write_keys(
            &self,
            agent: AgentId,
            bytes: Vec<u8>,
        ) -> BoxFuture<'_, Result<(), SupervisorError>> {
            Box::pin(async move { self.type_bytes(agent, &bytes) })
        }

        /// The interrupt keys of the agent's harness go to `typed`, then
        /// the turn ends at once with the reason `interrupted` (no
        /// `turn_end`, as Claude Code on Esc).
        fn interrupt(&self, agent: AgentId) -> BoxFuture<'_, Result<(), SupervisorError>> {
            Box::pin(async move {
                let harness = {
                    let mut inner = self.lock();
                    let a = match inner.agents.get_mut(&agent) {
                        Some(a) if a.status.is_live() => a,
                        _ => return Err(SupervisorError::NotRunning(agent)),
                    };
                    if a.compacting.is_some() {
                        return Err(SupervisorError::NotReady(INTERRUPT_COMPACTING.into()));
                    }
                    if !matches!(a.status, AgentStatus::Working | AgentStatus::Blocked)
                        || a.held.is_some()
                    {
                        return Err(SupervisorError::NotReady(NO_TURN.into()));
                    }
                    // The pending `turn_end` of the turn does not come.
                    a.epoch += 1;
                    a.harness.unwrap_or(HarnessId::Claude)
                };
                let keys: Vec<u8> = super::interrupt_keys(harness).concat();
                self.type_bytes(agent, &keys)?;
                self.emit(
                    agent,
                    SupervisorEventKind::Harness(Event::System {
                        message: "interrupt: the interrupt keys went to the harness; \
                                  the turn stops"
                            .into(),
                    }),
                );
                self.set_status(agent, AgentStatus::Idle, Some("interrupted"));
                self.next_early(agent);
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
                                me.feed_screen(agent, &bytes);
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

        fn screen(
            &self,
            agent: AgentId,
        ) -> BoxFuture<'_, Result<terrarium_runner::ScreenSnapshot, SupervisorError>> {
            Box::pin(async move {
                let inner = self.lock();
                match inner.agents.get(&agent) {
                    Some(a) if a.status.is_live() && a.old_runner => {
                        Err(SupervisorError::Unsupported(OLD_RUNNER.into()))
                    }
                    Some(a) if a.status.is_live() => {
                        let (rows, cols) = a.size.unwrap_or((24, 80));
                        Ok(terrarium_runner::ScreenSnapshot::parse(
                            rows, cols, &a.screen,
                        ))
                    }
                    _ => Err(SupervisorError::NotRunning(agent)),
                }
            })
        }

        fn status(&self, agent: AgentId) -> BoxFuture<'_, Option<AgentStatus>> {
            Box::pin(async move { self.current(agent).map(|(s, _)| s) })
        }

        /// Recorded for [`InMemorySupervisor::session_signals`].
        fn session_signal(
            &self,
            agent: AgentId,
            event: Event,
            _at: Option<chrono::DateTime<chrono::Utc>>,
        ) {
            self.lock().signals.push((agent, event));
        }

        /// A harness with a compact command, unless
        /// [`InMemorySupervisor::refuse_compact`] refused it.
        fn can_compact(&self, harness: HarnessId) -> bool {
            harness.compact_command().is_some() && !self.lock().no_compact.contains(&harness)
        }

        /// The compact command of the harness, as raw text. The end
        /// follows [`InMemorySupervisor::set_compact`].
        fn compact(
            &self,
            agent: AgentId,
            wait: Duration,
            then: Option<CompactThen>,
        ) -> BoxFuture<'_, Result<CompactWait, SupervisorError>> {
            Box::pin(async move {
                let (rx, token, epoch, command, mode) = {
                    let mut inner = self.lock();
                    let refused = inner.no_compact.clone();
                    inner.compact_seq += 1;
                    let token = inner.compact_seq;
                    let a = inner
                        .agents
                        .get_mut(&agent)
                        .filter(|a| a.status.is_live())
                        .ok_or(SupervisorError::NotRunning(agent))?;
                    let harness = a.harness.unwrap_or(HarnessId::Claude);
                    let command = harness
                        .compact_command()
                        .filter(|_| !refused.contains(&harness))
                        .ok_or_else(|| {
                            SupervisorError::Unsupported(crate::lifecycle::compact_unsupported(
                                harness,
                            ))
                        })?;
                    if a.compacting.is_some() {
                        return Err(SupervisorError::NotReady(
                            "the agent compacts already".into(),
                        ));
                    }
                    // Behind the reset gate, the early turns wait for the
                    // compaction.
                    if a.held.is_some()
                        || a.status != AgentStatus::Idle
                        || (!a.early.is_empty() && !a.gated)
                    {
                        return Err(SupervisorError::NotReady(format!(
                            "the agent is not idle ({})",
                            a.status
                        )));
                    }
                    let (tx, rx) = oneshot::channel();
                    a.compacting = Some((token, Some(tx), then));
                    a.gated = false;
                    a.compactions += 1;
                    (rx, token, a.epoch, command, a.compact)
                };
                self.set_status(agent, AgentStatus::Working, Some(REASON_COMPACTING));
                let bytes = terrarium_core::adapter::raw_text_keys(command);
                self.type_bytes(agent, &bytes)?;
                self.emit(
                    agent,
                    SupervisorEventKind::Harness(Event::CompactStarted {
                        trigger: Some("manual".into()),
                    }),
                );
                if mode != FakeCompact::Silent {
                    self.later(agent, epoch, move |me| {
                        let ok = mode == FakeCompact::Ok;
                        let message = (!ok).then(|| "Not enough messages to compact.".to_owned());
                        me.emit(
                            agent,
                            SupervisorEventKind::Harness(Event::CompactFinished {
                                ok,
                                trigger: Some("manual".into()),
                                message: message.clone(),
                            }),
                        );
                        me.end_compaction(
                            agent,
                            Some(token),
                            CompactEnd {
                                ok,
                                reason: message,
                                work: false,
                            },
                        );
                    });
                }
                let me = self.clone();
                let end: CompactWait = Box::pin(async move {
                    match tokio::time::timeout(wait, rx).await {
                        Ok(Ok(end)) => end,
                        Ok(Err(_)) => CompactEnd {
                            ok: false,
                            reason: Some("the agent stopped".into()),
                            work: false,
                        },
                        Err(_) => {
                            let end = CompactEnd {
                                ok: false,
                                reason: Some("no end of the compaction".into()),
                                work: false,
                            };
                            me.end_compaction(agent, Some(token), end.clone())
                                .unwrap_or(end)
                        }
                    }
                });
                Ok(end)
            })
        }

        fn reset_gate(&self, agent: AgentId, on: bool) {
            let open = {
                let mut inner = self.lock();
                let a = inner.agents.entry(agent).or_default();
                let was = a.gated;
                a.gated = on;
                was && !on
            };
            if open {
                self.next_early(agent);
                self.push_mail(agent);
            }
        }

        /// A new fake session: `restarting` for a live agent, then a boot
        /// with no resume. The recap (with its tag) and the `then` turn go
        /// first in the early turns.
        fn new_session(
            &self,
            agent: Agent,
            plan: NewSessionPlan,
        ) -> BoxFuture<'_, Result<(), SupervisorError>> {
            Box::pin(async move {
                let live = self.current(agent.id).is_some_and(|(s, _)| s.is_live());
                if live {
                    self.set_status(agent.id, AgentStatus::Restarting, Some("new session"));
                }
                {
                    let mut inner = self.lock();
                    let a = inner.agents.entry(agent.id).or_default();
                    a.harness = Some(agent.harness);
                    a.gated = false;
                    a.viewers.clear();
                    let mut first = Vec::new();
                    if let Some(r) = plan.recap {
                        first.push(terrarium_core::turn_tag::render(
                            &terrarium_core::turn_tag::TurnSource::system(
                                terrarium_core::turn_tag::SystemKind::Recap,
                            ),
                            &r,
                        ));
                    }
                    first.extend(plan.then);
                    a.early.splice(0..0, first);
                }
                self.boot_with(agent.id, None, Some(&plan.reason));
                Ok(())
            })
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
