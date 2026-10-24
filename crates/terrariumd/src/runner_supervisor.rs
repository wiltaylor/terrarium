//! The real supervisor: one `terrarium_runner::Runner` per running agent.
//!
//! How the parts connect:
//!
//! ```text
//!  API handler ──start/stop/send──▶ RunnerSupervisor ──spawn──▶ Runner (PTY, socket, status)
//!                                        │   ▲                      │
//!                     SupervisorEvent ◀──┘   └── forward task ◀─────┘ EventRecord broadcast
//!                          │
//!                    lib.rs pump ──▶ store (agents.status, sessions, events) ──▶ EventHub
//!                                                                             │
//!  push delivery ◀── hub listener (mail, card) ◀──────────────────────────────┘
//!  runner socket ──▶ AgentRpc ──▶ api::router (Actor::Agent) ──▶ store
//! ```
//!
//! - `start` renders `prompt.md`, writes the adapter's per-agent files, asks
//!   the adapter for the `LaunchSpec`, builds a clean env, wraps it in the
//!   sandbox, and spawns the runner. A forward task sends each runner event
//!   to the daemon as a [`SupervisorEvent`]; the pump in `lib.rs` writes it
//!   to the store, which broadcasts it to the API streams.
//! - When the harness exits, the supervisor applies the restart policy of
//!   PLAN §18: exponential backoff, crash-loop detection, and a fresh session
//!   with a recap turn when a resume fails.
//! - A hub listener sees new mail and card assignments. It types the push
//!   pointer of PLAN §12 and §13 when the agent is idle, else after the next
//!   turn ends. One pending pointer covers all new items.

use std::collections::{HashMap, VecDeque};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, MutexGuard, OnceLock, Weak};
use std::time::{Duration, Instant};

use serde_json::Value;
use terrarium_adapter_claude::{ClaudeAdapter, ClaudeOptions};
use terrarium_core::config::RestartPolicy;
use terrarium_core::{
    Adapter, Agent, AgentId, AgentStatus, Event, EventRecord, HarnessId, HarnessSessionId,
    LaunchSpec,
};
use terrarium_runner::{ReadyDetector, Runner, RunnerConfig};
use tokio::sync::broadcast::error::RecvError;
use tokio::sync::{broadcast, mpsc};
use tracing::{debug, info, warn};

use crate::agent_rpc::AgentRpc;
use crate::api::{AppState, Shared};
use crate::prompt::{self, Peer, PromptContext};
use crate::sandbox::{self, AgentLayout, HarnessHome, SandboxConfig, SandboxMode};
use crate::supervisor::{
    AnswerInput, BoxFuture, PtyAttachment, PtyInput, Supervisor, SupervisorError, SupervisorEvent,
    SupervisorEventKind,
};

/// One harness adapter and its prompt-ready detector.
#[derive(Clone)]
pub struct AdapterEntry {
    pub adapter: Arc<dyn Adapter>,
    /// For a harness whose `screen_hint` cannot tell.
    pub ready_detector: Option<Arc<dyn ReadyDetector>>,
}

impl AdapterEntry {
    pub fn new(adapter: Arc<dyn Adapter>) -> Self {
        Self {
            adapter,
            ready_detector: None,
        }
    }
}

/// The restart policy numbers (PLAN §18).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RestartTuning {
    /// The first backoff. Each restart in the window doubles it.
    pub backoff_base: Duration,
    pub backoff_max: Duration,
    /// A crash loop is `crash_limit` restarts inside `crash_window`.
    pub crash_window: Duration,
    pub crash_limit: usize,
}

impl Default for RestartTuning {
    fn default() -> Self {
        Self {
            backoff_base: Duration::from_secs(1),
            backoff_max: Duration::from_secs(60),
            crash_window: Duration::from_secs(10 * 60),
            crash_limit: 5,
        }
    }
}

/// The runner settings that the supervisor passes on.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RunnerTuning {
    pub rows: u16,
    pub cols: u16,
    pub tick: Duration,
    pub submit_timeout: Duration,
    pub submit_attempts: u32,
    pub stall_timeout: Duration,
}

impl Default for RunnerTuning {
    fn default() -> Self {
        let d = RunnerConfig::new(AgentId::new(), "/", "/");
        Self {
            rows: d.rows,
            cols: d.cols,
            tick: d.tick,
            submit_timeout: d.submit_timeout,
            submit_attempts: d.submit_attempts,
            stall_timeout: d.stall_timeout,
        }
    }
}

/// The settings of a [`RunnerSupervisor`].
#[derive(Clone)]
pub struct RunnerSupervisorConfig {
    /// `/state`: agent dirs are `<state>/agents/<ulid>`.
    pub state_dir: PathBuf,
    /// `/run/terrarium`: runner sockets are `<run>/agents/<ulid>.sock`.
    pub runtime_dir: PathBuf,
    /// The prompt templates, `<prompts>/<name>.md`.
    pub prompts_dir: PathBuf,
    pub sandbox: SandboxConfig,
    /// The shared harness home of each harness on the host.
    pub homes: HashMap<HarnessId, PathBuf>,
    /// The adapters that can run. A harness that is not here gets
    /// `SupervisorError::NotReady`.
    pub adapters: HashMap<HarnessId, AdapterEntry>,
    pub restart: RestartTuning,
    pub runner: RunnerTuning,
}

impl RunnerSupervisorConfig {
    /// The config with the real adapters: Claude Code. The codex, copilot,
    /// and pi adapters are stubs, so they are not registered.
    pub fn new(
        state_dir: PathBuf,
        runtime_dir: PathBuf,
        prompts_dir: PathBuf,
        sandbox: SandboxConfig,
        homes: HashMap<HarnessId, PathBuf>,
    ) -> Self {
        let adapters = default_adapters(&sandbox, &homes);
        Self {
            state_dir,
            runtime_dir,
            prompts_dir,
            sandbox,
            homes,
            adapters,
            restart: RestartTuning::default(),
            runner: RunnerTuning::default(),
        }
    }

    /// The host home of `harness`: the configured one, else `/auth/<harness>`.
    pub fn home(&self, harness: HarnessId) -> PathBuf {
        self.homes
            .get(&harness)
            .cloned()
            .unwrap_or_else(|| PathBuf::from(harness.auth_dir()))
    }
}

/// The adapters that work in this phase, set up for the sandbox mode.
pub fn default_adapters(
    sandbox: &SandboxConfig,
    homes: &HashMap<HarnessId, PathBuf>,
) -> HashMap<HarnessId, AdapterEntry> {
    let host = homes
        .get(&HarnessId::Claude)
        .cloned()
        .unwrap_or_else(|| PathBuf::from(HarnessId::Claude.auth_dir()));
    let inside = sandbox.home_inside(&HarnessHome {
        harness: HarnessId::Claude,
        host: host.clone(),
    });
    let options = ClaudeOptions {
        config_dir: inside,
        host_config_dir: Some(host),
        harness_workspace: match sandbox.mode {
            SandboxMode::Bwrap => Some(sandbox.home.join("work")),
            SandboxMode::None => None,
        },
        hook_program: sandbox.hook_program(),
        ..ClaudeOptions::default()
    };
    let mut map = HashMap::new();
    map.insert(
        HarnessId::Claude,
        AdapterEntry::new(Arc::new(ClaudeAdapter::new(options))),
    );
    map
}

/// The message for a harness with no working adapter.
pub fn not_ready(harness: HarnessId) -> SupervisorError {
    SupervisorError::NotReady(format!(
        "the {harness} adapter is not ready yet: its hook mapping and launch flags are \
         not verified (docs/compat.md). Only `claude` agents can start in this phase"
    ))
}

// ---- push pointers ----

#[derive(Debug, Clone, PartialEq, Eq)]
struct MailItem {
    message_id: String,
    from: String,
    thread: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum CardChange {
    Assigned,
    Unblocked,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct CardItem {
    card: String,
    change: CardChange,
}

/// The items one pointer turn covers.
#[derive(Debug, Default, Clone, PartialEq, Eq)]
struct Pointer {
    mail: Vec<MailItem>,
    cards: Vec<CardItem>,
}

impl Pointer {
    fn is_empty(&self) -> bool {
        self.mail.is_empty() && self.cards.is_empty()
    }

    fn add_mail(&mut self, m: MailItem) {
        if !self.mail.iter().any(|x| x.message_id == m.message_id) {
            self.mail.push(m);
        }
    }

    fn add_card(&mut self, c: CardItem) {
        if !self.cards.contains(&c) {
            self.cards.push(c);
        }
    }

    fn merge(&mut self, other: Pointer) {
        for m in other.mail {
            self.add_mail(m);
        }
        for c in other.cards {
            self.add_card(c);
        }
    }

    /// The pointer turn. One line per thread and per card, in the words of
    /// PLAN §12 and §13. The turn names the sender, the thread, and the
    /// command only: never the body, because mail is untrusted input.
    fn text(&self) -> String {
        let mut lines = Vec::new();
        let mut threads: Vec<&str> = Vec::new();
        for m in &self.mail {
            if threads.contains(&m.thread.as_str()) {
                continue;
            }
            threads.push(&m.thread);
            lines.push(format!(
                "New mail from {} (thread {}). Run `terrarium mail read --thread {}`.",
                m.from, m.thread, m.thread
            ));
        }
        for c in &self.cards {
            lines.push(match c.change {
                CardChange::Assigned => format!(
                    "Card {} was assigned to you. Run `terrarium card show {}`.",
                    c.card, c.card
                ),
                CardChange::Unblocked => format!(
                    "Card {} is unblocked. Run `terrarium card show {}`.",
                    c.card, c.card
                ),
            });
        }
        lines.join("\n")
    }

    fn message_ids(&self) -> Vec<String> {
        self.mail.iter().map(|m| m.message_id.clone()).collect()
    }
}

// ---- per-agent state ----

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Phase {
    Running,
    /// Between an exit and the next launch.
    Restarting,
    /// Not running, with the last status.
    Down(AgentStatus),
}

struct Slot {
    agent: Agent,
    runner: Option<Runner>,
    phase: Phase,
    /// Bumped by every `start` and `stop`, so a stale task does nothing.
    epoch: u64,
    stop_requested: bool,
    restart_requested: bool,
    restarts: VecDeque<Instant>,
    /// The session this launch resumes.
    resume: Option<HarnessSessionId>,
    /// The session the harness reported (`SessionStarted`).
    session: Option<HarnessSessionId>,
    reached_idle: bool,
    /// A recap turn to type at the first idle after a failed resume.
    recap: Option<String>,
    pointer: Pointer,
    pointer_busy: bool,
}

impl Slot {
    fn new(agent: Agent) -> Self {
        Self {
            agent,
            runner: None,
            phase: Phase::Down(AgentStatus::Stopped),
            epoch: 0,
            stop_requested: false,
            restart_requested: false,
            restarts: VecDeque::new(),
            resume: None,
            session: None,
            reached_idle: false,
            recap: None,
            pointer: Pointer::default(),
            pointer_busy: false,
        }
    }

    fn live(&self) -> bool {
        self.runner.is_some() || self.phase == Phase::Restarting
    }
}

struct Inner {
    cfg: RunnerSupervisorConfig,
    slots: Mutex<HashMap<AgentId, Slot>>,
    tx: broadcast::Sender<SupervisorEvent>,
    state: OnceLock<Weak<AppState>>,
    shutting_down: AtomicBool,
}

/// Runs each agent in a [`Runner`]. Cheap to clone.
#[derive(Clone)]
pub struct RunnerSupervisor {
    inner: Arc<Inner>,
}

fn lock<T>(m: &Mutex<T>) -> MutexGuard<'_, T> {
    m.lock().unwrap_or_else(std::sync::PoisonError::into_inner)
}

fn other(e: impl std::fmt::Display) -> SupervisorError {
    SupervisorError::Other(e.to_string())
}

/// What the start needs from the store.
#[derive(Default)]
struct StoreView {
    peers: Vec<Peer>,
    queued: Vec<MailItem>,
}

impl RunnerSupervisor {
    pub fn new(cfg: RunnerSupervisorConfig) -> Self {
        let (tx, _) = broadcast::channel(4096);
        Self {
            inner: Arc::new(Inner {
                cfg,
                slots: Mutex::new(HashMap::new()),
                tx,
                state: OnceLock::new(),
                shutting_down: AtomicBool::new(false),
            }),
        }
    }

    pub fn config(&self) -> &RunnerSupervisorConfig {
        &self.inner.cfg
    }

    fn emit(&self, agent: AgentId, kind: SupervisorEventKind) {
        let _ = self.inner.tx.send(SupervisorEvent { agent, kind });
    }

    fn emit_status(&self, agent: AgentId, status: AgentStatus, reason: impl Into<String>) {
        self.emit(
            agent,
            SupervisorEventKind::Status {
                status,
                reason: Some(reason.into()),
            },
        );
    }

    fn state(&self) -> Option<Shared> {
        self.inner.state.get().and_then(Weak::upgrade)
    }

    fn entry(&self, harness: HarnessId) -> Result<AdapterEntry, SupervisorError> {
        self.inner
            .cfg
            .adapters
            .get(&harness)
            .cloned()
            .ok_or_else(|| not_ready(harness))
    }

    /// The runner of a running agent, for tests and debugging.
    pub fn runner(&self, agent: AgentId) -> Option<Runner> {
        lock(&self.inner.slots)
            .get(&agent)
            .and_then(|s| s.runner.clone())
    }

    /// The paths of `agent` on the host.
    pub fn layout(&self, agent: &Agent) -> AgentLayout {
        let cfg = &self.inner.cfg;
        let agent_dir = terrarium_runner::agent_dir(&cfg.state_dir, agent.id);
        AgentLayout {
            agent: agent.id,
            workspace: agent_dir.join("workspace"),
            agent_dir,
            socket: terrarium_runner::socket_path(&cfg.runtime_dir, agent.id),
            homes: vec![HarnessHome {
                harness: agent.harness,
                host: cfg.home(agent.harness),
            }],
            overlays: Vec::new(),
        }
    }

    /// Prepare the agent's files and build the final launch spec, with the
    /// sandbox and the clean env. `sandbox-dry-run` prints this.
    pub async fn launch_spec(
        &self,
        agent: &Agent,
        resume: Option<&HarnessSessionId>,
    ) -> Result<LaunchSpec, SupervisorError> {
        let view = self.store_view(agent).await;
        self.build_spec(agent, resume, &view).await
    }

    async fn store_view(&self, agent: &Agent) -> StoreView {
        let Some(state) = self.state() else {
            return StoreView::default();
        };
        let id = agent.id.to_string();
        let project = agent.project.clone();
        let result = state
            .db
            .read(move |c| {
                let page = crate::store::Page {
                    limit: Some(crate::store::Page::MAX),
                    cursor: None,
                };
                let peers = crate::store::agents::list(c, Some(&project), &page)?
                    .items
                    .into_iter()
                    .filter(|v| v.agent.id.to_string() != id)
                    .map(|v| Peer {
                        name: v.agent.name,
                        harness: v.agent.harness.to_string(),
                        status: v.agent.status.to_string(),
                        roles: v.agent.roles,
                    })
                    .collect();
                let f = crate::store::mail::MailFilter {
                    to: Some(id.clone()),
                    state: Some("queued".into()),
                    ..Default::default()
                };
                let queued = crate::store::mail::list(c, &f, &page)?
                    .items
                    .into_iter()
                    .map(|m| MailItem {
                        message_id: m.id,
                        from: m.from_name,
                        thread: m.thread_id,
                    })
                    .collect();
                Ok(StoreView { peers, queued })
            })
            .await;
        result.unwrap_or_else(|e| {
            warn!(agent = %agent.id, "cannot read the prompt context: {e}");
            StoreView::default()
        })
    }

    async fn build_spec(
        &self,
        agent: &Agent,
        resume: Option<&HarnessSessionId>,
        view: &StoreView,
    ) -> Result<LaunchSpec, SupervisorError> {
        let entry = self.entry(agent.harness)?;
        let cfg = &self.inner.cfg;
        let mut layout = self.layout(agent);
        // The adapters find the agent dir from the workspace path.
        let mut agent = agent.clone();
        agent.workspace = layout.workspace.clone();

        let mut ctx = PromptContext::for_agent(&agent);
        ctx.peers = view.peers.clone();
        let template = prompt::load_template(&cfg.prompts_dir, agent.prompt.as_deref())
            .map_err(SupervisorError::NotReady)?;
        let text = prompt::render(&template, &ctx);

        let adapter = entry.adapter.clone();
        let home = cfg.home(agent.harness);
        let dirs = (layout.agent_dir.clone(), layout.workspace.clone());
        let a = agent.clone();
        let overlays = tokio::task::spawn_blocking(move || {
            for d in [&dirs.0, &dirs.1] {
                std::fs::create_dir_all(d)
                    .map_err(|e| format!("cannot create {}: {e}", d.display()))?;
            }
            let path = dirs.0.join("prompt.md");
            std::fs::write(&path, text)
                .map_err(|e| format!("cannot write {}: {e}", path.display()))?;
            if !home.is_dir() {
                return Err(format!(
                    "the {} home {} does not exist; log in first, or point the daemon at it \
                     with --harness-home {}=<dir>",
                    a.harness,
                    home.display(),
                    a.harness
                ));
            }
            adapter
                .materialize_config(&a, &home)
                .map_err(|e| format!("cannot write the {} config: {e}", a.harness))
        })
        .await
        .map_err(other)?
        .map_err(SupervisorError::NotReady)?;

        if cfg.sandbox.mode == SandboxMode::None && !overlays.is_empty() {
            warn!(
                agent = %agent.id,
                "sandbox mode `none` cannot bind {} per-agent file(s) over the shared home; \
                 the harness uses the shared files",
                overlays.len()
            );
        }
        layout.overlays = overlays;

        let spec = entry.adapter.launch(&agent, resume);
        let env = cfg
            .sandbox
            .child_env(&layout, &spec.env, &|k| std::env::var(k).ok());
        Ok(cfg.sandbox.wrap(spec, &layout, env))
    }

    /// Boxed, because a relaunch after an exit calls it again from the task
    /// that it spawned.
    fn launch(&self, id: AgentId, epoch: u64) -> BoxFuture<'_, Result<(), SupervisorError>> {
        Box::pin(self.launch_inner(id, epoch))
    }

    async fn launch_inner(&self, id: AgentId, epoch: u64) -> Result<(), SupervisorError> {
        let (agent, resume) = {
            let slots = lock(&self.inner.slots);
            let slot = slots.get(&id).ok_or(SupervisorError::NotRunning(id))?;
            (slot.agent.clone(), slot.resume.clone())
        };
        let entry = self.entry(agent.harness)?;
        let view = self.store_view(&agent).await;
        let spec = self.build_spec(&agent, resume.as_ref(), &view).await?;
        debug!(agent = %id, command = %sandbox::command_line(&spec), "launch");

        let cfg = &self.inner.cfg;
        let mut rc = RunnerConfig::new(id, &cfg.state_dir, &cfg.runtime_dir);
        rc.name = agent.name.clone();
        rc.project = agent.project.clone();
        rc.roles = agent
            .roles
            .iter()
            .filter_map(|r| serde_json::to_value(r).ok())
            .filter_map(|v| v.as_str().map(str::to_owned))
            .collect();
        let t = cfg.runner;
        rc.rows = t.rows;
        rc.cols = t.cols;
        rc.tick = t.tick;
        rc.submit_timeout = t.submit_timeout;
        rc.submit_attempts = t.submit_attempts;
        rc.stall_timeout = t.stall_timeout;
        rc.ready_detector = entry.ready_detector.clone();
        let handler = Arc::new(AgentRpc::new(
            self.inner.state.get().cloned().unwrap_or_default(),
        ));
        let program = spec.program.clone();
        let runner = Runner::spawn(rc, entry.adapter.clone(), spec, handler)
            .await
            .map_err(|e| SupervisorError::Other(format!("cannot start {program}: {e}")))?;
        let rx = runner.subscribe_events();

        {
            let mut slots = lock(&self.inner.slots);
            let current = slots
                .get_mut(&id)
                .filter(|s| s.epoch == epoch && !s.stop_requested);
            let Some(slot) = current else {
                drop(slots);
                let _ = runner.kill();
                return Ok(());
            };
            slot.runner = Some(runner.clone());
            slot.phase = Phase::Running;
            slot.reached_idle = false;
            slot.pointer_busy = false;
            for m in view.queued {
                slot.pointer.add_mail(m);
            }
        }
        let reason = match &resume {
            Some(s) => format!("resuming session {s}"),
            None => "new session".to_owned(),
        };
        info!(agent = %id, name = %agent.name, %reason, "agent started");
        self.emit_status(id, AgentStatus::Starting, reason);
        let now = runner.status();
        if now != AgentStatus::Starting {
            self.emit_status(id, now, "caught up");
            if now == AgentStatus::Idle {
                self.on_idle(id, epoch);
            }
        }
        let me = self.clone();
        tokio::spawn(async move { me.forward(id, epoch, runner, rx).await });
        Ok(())
    }

    /// Send the runner's events on, until the harness exits.
    async fn forward(
        self,
        id: AgentId,
        epoch: u64,
        runner: Runner,
        mut rx: broadcast::Receiver<EventRecord>,
    ) {
        loop {
            let rec = match rx.recv().await {
                Ok(rec) => rec,
                Err(RecvError::Lagged(n)) => {
                    warn!(agent = %id, "the supervisor missed {n} runner events");
                    continue;
                }
                Err(RecvError::Closed) => break,
            };
            match &rec.event {
                // The supervisor decides the final status itself.
                Event::StatusChanged {
                    to: AgentStatus::Stopped | AgentStatus::Error,
                    ..
                } => continue,
                Event::SessionStarted {
                    harness_session, ..
                } => {
                    if let Some(slot) = lock(&self.inner.slots).get_mut(&id) {
                        slot.session = Some(harness_session.clone());
                    }
                }
                _ => {}
            }
            let exit = matches!(rec.event, Event::Exit { .. });
            let idle = matches!(
                rec.event,
                Event::StatusChanged {
                    to: AgentStatus::Idle,
                    ..
                }
            );
            self.emit(id, SupervisorEventKind::Harness(rec.event));
            if idle {
                self.on_idle(id, epoch);
            }
            if exit {
                break;
            }
        }
        let status = runner.wait().await;
        self.on_exit(id, epoch, status).await;
    }

    fn on_idle(&self, id: AgentId, epoch: u64) {
        let recap = {
            let mut slots = lock(&self.inner.slots);
            let Some(slot) = slots.get_mut(&id).filter(|s| s.epoch == epoch) else {
                return;
            };
            slot.reached_idle = true;
            slot.recap.take().zip(slot.runner.clone())
        };
        if let Some((text, runner)) = recap {
            let delivery = runner.type_turn(text);
            tokio::spawn(async move {
                if let Err(e) = delivery.wait().await {
                    warn!(agent = %id, "the recap turn was not delivered: {e}");
                }
            });
            return;
        }
        self.maybe_push(id);
    }

    async fn on_exit(&self, id: AgentId, epoch: u64, status: terrarium_runner::ExitStatus) {
        if self.inner.shutting_down.load(Ordering::SeqCst) {
            return;
        }
        enum Next {
            Done(AgentStatus, String),
            Relaunch {
                delay: Duration,
                reason: String,
                new_session: Option<String>,
            },
        }
        let code = status.code;
        let clean = status.success();
        let how = match &status.signal {
            Some(s) => format!("exited with code {code} ({s})"),
            None => format!("exited with code {code}"),
        };
        let next = {
            let mut slots = lock(&self.inner.slots);
            let Some(slot) = slots.get_mut(&id).filter(|s| s.epoch == epoch) else {
                return;
            };
            slot.runner = None;
            let tuning = self.inner.cfg.restart;
            let resume_failed = slot.resume.is_some() && !slot.reached_idle && !clean;
            let next = if slot.stop_requested {
                Next::Done(AgentStatus::Stopped, "stopped".into())
            } else if slot.restart_requested {
                slot.restart_requested = false;
                Next::Relaunch {
                    delay: Duration::ZERO,
                    reason: "restart".into(),
                    new_session: None,
                }
            } else if resume_failed {
                let old = slot.resume.take();
                let dir = terrarium_runner::agent_dir(&self.inner.cfg.state_dir, id);
                slot.recap = Some(recap_turn(&dir, old.as_ref()));
                slot.session = None;
                Next::Relaunch {
                    delay: Duration::ZERO,
                    reason: format!(
                        "the resume of session {} failed ({how}); starting a new session",
                        old.map(|s| s.0).unwrap_or_default()
                    ),
                    new_session: Some("resume_failed".into()),
                }
            } else {
                let wanted = match slot.agent.restart {
                    RestartPolicy::Always => true,
                    RestartPolicy::OnFailure => !clean,
                    RestartPolicy::Never => false,
                };
                let now = Instant::now();
                while slot
                    .restarts
                    .front()
                    .is_some_and(|t| now.duration_since(*t) > tuning.crash_window)
                {
                    slot.restarts.pop_front();
                }
                if !wanted {
                    if clean {
                        Next::Done(AgentStatus::Stopped, "exited".into())
                    } else {
                        Next::Done(AgentStatus::Error, how.clone())
                    }
                } else if slot.restarts.len() >= tuning.crash_limit {
                    Next::Done(
                        AgentStatus::Error,
                        format!(
                            "crash loop: {} restarts in {} s; last exit: {how}",
                            slot.restarts.len(),
                            tuning.crash_window.as_secs()
                        ),
                    )
                } else {
                    let n = u32::try_from(slot.restarts.len()).unwrap_or(u32::MAX);
                    slot.restarts.push_back(now);
                    let delay = tuning
                        .backoff_base
                        .saturating_mul(2u32.saturating_pow(n))
                        .min(tuning.backoff_max);
                    slot.resume = slot.session.clone().or(slot.resume.take());
                    Next::Relaunch {
                        delay,
                        reason: format!(
                            "{how}; restart {} in {} ms (restart = {})",
                            n + 1,
                            delay.as_millis(),
                            crate::store::agents::restart_str(slot.agent.restart)
                        ),
                        new_session: Some(if clean { "exited" } else { "crashed" }.into()),
                    }
                }
            };
            match &next {
                Next::Done(s, _) => slot.phase = Phase::Down(*s),
                Next::Relaunch { .. } => slot.phase = Phase::Restarting,
            }
            next
        };
        match next {
            Next::Done(s, reason) => {
                info!(agent = %id, status = %s, %reason, "agent ended");
                self.emit_status(id, s, reason);
            }
            Next::Relaunch {
                delay,
                reason,
                new_session,
            } => {
                info!(agent = %id, %reason, "agent restarting");
                self.emit_status(id, AgentStatus::Restarting, reason);
                if let Some(outcome) = new_session {
                    self.emit(id, SupervisorEventKind::NewSession { outcome });
                }
                let me = self.clone();
                tokio::spawn(async move {
                    tokio::time::sleep(delay).await;
                    let go = lock(&me.inner.slots)
                        .get(&id)
                        .is_some_and(|s| s.epoch == epoch && !s.stop_requested);
                    if !go {
                        return;
                    }
                    if let Err(e) = me.launch(id, epoch).await {
                        warn!(agent = %id, "restart failed: {e}");
                        if let Some(slot) = lock(&me.inner.slots).get_mut(&id) {
                            slot.phase = Phase::Down(AgentStatus::Error);
                        }
                        me.emit_status(id, AgentStatus::Error, format!("restart failed: {e}"));
                    }
                });
            }
        }
    }

    /// Queue a pointer item for a running agent, then push if it is idle.
    fn add_pointer(&self, id: AgentId, add: impl FnOnce(&mut Pointer)) {
        {
            let mut slots = lock(&self.inner.slots);
            let Some(slot) = slots.get_mut(&id).filter(|s| s.runner.is_some()) else {
                return;
            };
            add(&mut slot.pointer);
        }
        self.maybe_push(id);
    }

    /// Type the pending pointer when the agent is idle and no pointer is on
    /// the way. A `working` agent gets it after its next stop hook.
    fn maybe_push(&self, id: AgentId) {
        let (runner, pointer) = {
            let mut slots = lock(&self.inner.slots);
            let Some(slot) = slots.get_mut(&id) else {
                return;
            };
            let Some(runner) = slot.runner.clone() else {
                return;
            };
            if slot.pointer_busy
                || slot.pointer.is_empty()
                || slot.recap.is_some()
                || runner.status() != AgentStatus::Idle
            {
                return;
            }
            slot.pointer_busy = true;
            (runner, std::mem::take(&mut slot.pointer))
        };
        let delivery = runner.type_turn(pointer.text());
        let me = self.clone();
        tokio::spawn(async move {
            let result = delivery.wait().await;
            match &result {
                Ok(()) => me.mark_delivered(pointer.message_ids()).await,
                Err(e) => debug!(agent = %id, "the push pointer was not delivered: {e}"),
            }
            let mut slots = lock(&me.inner.slots);
            if let Some(slot) = slots.get_mut(&id) {
                slot.pointer_busy = false;
                if result.is_err() {
                    // Keep the items for the next idle.
                    let newer = std::mem::replace(&mut slot.pointer, pointer);
                    slot.pointer.merge(newer);
                }
            }
        });
    }

    async fn mark_delivered(&self, ids: Vec<String>) {
        if ids.is_empty() {
            return;
        }
        let Some(state) = self.state() else { return };
        let result = state
            .db
            .write("system", move |t| {
                crate::store::mail::mark_delivered(t, &ids)
            })
            .await;
        if let Err(e) = result {
            warn!("cannot mark mail delivered: {e}");
        }
    }

    /// Watch the event hub for new mail and card changes (PLAN §12, §13).
    fn spawn_hub_listener(&self, state: &Shared) {
        let mut rx = state.db.hub().subscribe();
        let me = self.clone();
        tokio::spawn(async move {
            loop {
                let ev = match rx.recv().await {
                    Ok(ev) => ev,
                    Err(RecvError::Lagged(n)) => {
                        warn!("the push listener missed {n} events");
                        continue;
                    }
                    Err(RecvError::Closed) => return,
                };
                let Some(agent) = ev
                    .agent_id
                    .as_deref()
                    .and_then(|a| a.parse::<AgentId>().ok())
                else {
                    continue;
                };
                let s = |k: &str| ev.data.get(k).and_then(Value::as_str).map(str::to_owned);
                match ev.kind.as_str() {
                    "mail" => {
                        let (Some(message_id), Some(thread)) = (s("message_id"), s("thread_id"))
                        else {
                            continue;
                        };
                        let from = s("from_name").or_else(|| s("from")).unwrap_or_default();
                        me.add_pointer(agent, |p| {
                            p.add_mail(MailItem {
                                message_id,
                                from,
                                thread,
                            });
                        });
                    }
                    "card" => {
                        let Some(card) = ev.card.clone() else {
                            continue;
                        };
                        let self_made = ev.actor.as_deref() == Some(&format!("agent:{agent}"));
                        let change = match s("change").as_deref() {
                            Some("assigned")
                                if s("assignee").as_deref() == Some(&agent.to_string())
                                    && !self_made =>
                            {
                                CardChange::Assigned
                            }
                            Some("unblocked") => CardChange::Unblocked,
                            _ => continue,
                        };
                        me.add_pointer(agent, |p| p.add_card(CardItem { card, change }));
                    }
                    _ => {}
                }
            }
        });
    }
}

/// The recap turn after a resume failed: the last turns and tool calls of
/// the old session, from `events.jsonl`, and the commands to pick up work.
pub fn recap_turn(agent_dir: &Path, old: Option<&HarnessSessionId>) -> String {
    let lines: Vec<String> = std::fs::read_to_string(agent_dir.join("events.jsonl"))
        .unwrap_or_default()
        .lines()
        .filter_map(|l| serde_json::from_str::<EventRecord>(l).ok())
        .filter_map(|r| match r.event {
            Event::TurnStart {
                prompt: Some(p), ..
            } => Some(format!("- turn: {}", one_line(&p))),
            Event::ToolUse { tool, summary, .. } => Some(match summary {
                Some(s) => format!("- tool {tool}: {}", one_line(&s)),
                None => format!("- tool {tool}"),
            }),
            _ => None,
        })
        .collect();
    let recent = &lines[lines.len().saturating_sub(10)..];
    let mut text = format!(
        "Terrarium restarted you in a new session, because the previous session{} could not be resumed.",
        old.map(|s| format!(" {s}")).unwrap_or_default()
    );
    if recent.is_empty() {
        text.push_str(" No earlier activity is recorded.");
    } else {
        text.push_str(" The last recorded activity of that session:\n");
        text.push_str(&recent.join("\n"));
    }
    text.push_str(
        "\nRun `terrarium mail check` and `terrarium card ls --mine` to continue your work.",
    );
    text
}

fn one_line(s: &str) -> String {
    let first = s
        .lines()
        .find(|l| !l.trim().is_empty())
        .unwrap_or("")
        .trim();
    let mut out: String = first.chars().take(100).collect();
    if first.chars().count() > 100 || s.trim().lines().count() > 1 {
        out.push('…');
    }
    out
}

impl Supervisor for RunnerSupervisor {
    fn start(
        &self,
        agent: Agent,
        resume: Option<HarnessSessionId>,
    ) -> BoxFuture<'_, Result<(), SupervisorError>> {
        Box::pin(async move {
            self.entry(agent.harness)?;
            let id = agent.id;
            let epoch = {
                let mut slots = lock(&self.inner.slots);
                let slot = slots.entry(id).or_insert_with(|| Slot::new(agent.clone()));
                if slot.live() {
                    return Err(SupervisorError::AlreadyRunning(id));
                }
                slot.agent = agent;
                slot.epoch += 1;
                slot.stop_requested = false;
                slot.restart_requested = false;
                slot.restarts.clear();
                slot.resume = resume;
                slot.session = None;
                slot.recap = None;
                slot.pointer = Pointer::default();
                slot.phase = Phase::Running;
                slot.epoch
            };
            let result = self.launch(id, epoch).await;
            if result.is_err()
                && let Some(slot) = lock(&self.inner.slots).get_mut(&id)
                && slot.epoch == epoch
            {
                slot.phase = Phase::Down(AgentStatus::Stopped);
            }
            result
        })
    }

    fn stop(&self, agent: AgentId) -> BoxFuture<'_, Result<(), SupervisorError>> {
        Box::pin(async move {
            let runner = {
                let mut slots = lock(&self.inner.slots);
                let slot = slots
                    .get_mut(&agent)
                    .filter(|s| s.live())
                    .ok_or(SupervisorError::NotRunning(agent))?;
                slot.stop_requested = true;
                match slot.runner.clone() {
                    Some(r) => Some(r),
                    None => {
                        // Waiting for a restart: cancel it.
                        slot.epoch += 1;
                        slot.phase = Phase::Down(AgentStatus::Stopped);
                        None
                    }
                }
            };
            match runner {
                Some(r) => tokio::task::spawn_blocking(move || r.kill())
                    .await
                    .map_err(other)?
                    .map_err(other)?,
                None => self.emit_status(agent, AgentStatus::Stopped, "stopped"),
            }
            Ok(())
        })
    }

    fn restart(&self, agent: Agent) -> BoxFuture<'_, Result<(), SupervisorError>> {
        Box::pin(async move {
            let id = agent.id;
            let runner = {
                let mut slots = lock(&self.inner.slots);
                match slots.get_mut(&id) {
                    Some(slot) if slot.runner.is_some() => {
                        slot.restart_requested = true;
                        slot.resume = agent.session.clone().or(slot.session.clone());
                        slot.agent = agent.clone();
                        slot.runner.clone()
                    }
                    _ => None,
                }
            };
            match runner {
                Some(r) => tokio::task::spawn_blocking(move || r.kill())
                    .await
                    .map_err(other)?
                    .map_err(other),
                None => {
                    let resume = agent.session.clone();
                    self.start(agent, resume).await
                }
            }
        })
    }

    fn send_turn(
        &self,
        agent: AgentId,
        text: String,
    ) -> BoxFuture<'_, Result<(), SupervisorError>> {
        Box::pin(async move {
            let runner = self
                .runner(agent)
                .ok_or(SupervisorError::NotRunning(agent))?;
            let delivery = runner.type_turn(text);
            tokio::spawn(async move {
                if let Err(e) = delivery.wait().await {
                    debug!(agent = %agent, "a turn was not delivered: {e}");
                }
            });
            Ok(())
        })
    }

    fn answer(
        &self,
        agent: AgentId,
        answer: AnswerInput,
    ) -> BoxFuture<'_, Result<(), SupervisorError>> {
        Box::pin(async move {
            let runner = self
                .runner(agent)
                .ok_or(SupervisorError::NotRunning(agent))?;
            match answer {
                AnswerInput::Answer { request_id, answer } => {
                    runner.answer(&request_id, &answer).await
                }
                AnswerInput::Keys(keys) => runner.answer_keys(&keys).await,
            }
            .map_err(other)
        })
    }

    fn attach_pty(&self, agent: AgentId) -> BoxFuture<'_, Result<PtyAttachment, SupervisorError>> {
        Box::pin(async move {
            let runner = self
                .runner(agent)
                .ok_or(SupervisorError::NotRunning(agent))?;
            let (snapshot, mut output) = runner.attach();
            let (out_tx, out_rx) = mpsc::channel::<Vec<u8>>(256);
            let (in_tx, mut in_rx) = mpsc::channel::<PtyInput>(256);
            tokio::spawn(async move {
                let exited = runner.wait();
                tokio::pin!(exited);
                loop {
                    tokio::select! {
                        chunk = output.recv() => match chunk {
                            Ok(c) => {
                                if out_tx.send(c.to_vec()).await.is_err() {
                                    return;
                                }
                            }
                            Err(RecvError::Lagged(n)) => debug!("a PTY viewer missed {n} chunks"),
                            Err(RecvError::Closed) => return,
                        },
                        input = in_rx.recv() => match input {
                            Some(PtyInput::Bytes(b)) => {
                                if let Err(e) = runner.write(&b).await {
                                    debug!("PTY input: {e}");
                                }
                            }
                            Some(PtyInput::Resize { rows, cols }) => {
                                if let Err(e) = runner.resize(rows, cols) {
                                    debug!("PTY resize: {e}");
                                }
                            }
                            None => return,
                        },
                        _ = &mut exited => return,
                    }
                }
            });
            Ok(PtyAttachment {
                snapshot: snapshot.formatted,
                output: out_rx,
                input: in_tx,
            })
        })
    }

    fn status(&self, agent: AgentId) -> BoxFuture<'_, Option<AgentStatus>> {
        Box::pin(async move {
            let slots = lock(&self.inner.slots);
            let slot = slots.get(&agent)?;
            Some(match (&slot.runner, slot.phase) {
                (Some(r), _) => r.status(),
                (None, Phase::Restarting) => AgentStatus::Restarting,
                (None, Phase::Down(s)) => s,
                (None, Phase::Running) => AgentStatus::Starting,
            })
        })
    }

    fn subscribe(&self) -> broadcast::Receiver<SupervisorEvent> {
        self.inner.tx.subscribe()
    }

    fn bind(&self, state: &Shared) {
        if self.inner.state.set(Arc::downgrade(state)).is_ok() {
            self.spawn_hub_listener(state);
        }
    }

    fn shutdown(&self) {
        self.inner.shutting_down.store(true, Ordering::SeqCst);
        let runners: Vec<Runner> = lock(&self.inner.slots)
            .values()
            .filter_map(|s| s.runner.clone())
            .collect();
        for r in runners {
            if let Err(e) = r.kill() {
                warn!(agent = %r.agent(), "cannot end the harness: {e}");
            }
        }
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::*;

    #[test]
    fn one_pointer_covers_many_items() {
        let mut p = Pointer::default();
        p.add_mail(MailItem {
            message_id: "m1".into(),
            from: "demo/alice".into(),
            thread: "T1".into(),
        });
        assert_eq!(
            p.text(),
            "New mail from demo/alice (thread T1). Run `terrarium mail read --thread T1`."
        );
        p.add_mail(MailItem {
            message_id: "m2".into(),
            from: "demo/alice".into(),
            thread: "T1".into(),
        });
        p.add_mail(MailItem {
            message_id: "m2".into(),
            from: "demo/alice".into(),
            thread: "T1".into(),
        });
        p.add_card(CardItem {
            card: "TER-42".into(),
            change: CardChange::Assigned,
        });
        assert_eq!(p.message_ids(), ["m1", "m2"]);
        assert_eq!(
            p.text(),
            "New mail from demo/alice (thread T1). Run `terrarium mail read --thread T1`.\n\
             Card TER-42 was assigned to you. Run `terrarium card show TER-42`."
        );
    }

    #[test]
    fn recap_lists_recent_activity() {
        let dir = tempfile::tempdir().unwrap();
        let id = AgentId::new();
        let lines: Vec<String> = [
            Event::TurnStart {
                prompt: Some("fix the parser\nplease".into()),
            },
            Event::ToolUse {
                tool: "Bash".into(),
                input: Value::Null,
                id: None,
                summary: Some("cargo test".into()),
            },
            Event::TurnEnd,
        ]
        .into_iter()
        .map(|e| serde_json::to_string(&EventRecord::now(id, e)).unwrap())
        .collect();
        std::fs::write(dir.path().join("events.jsonl"), lines.join("\n")).unwrap();
        let text = recap_turn(dir.path(), Some(&HarnessSessionId("s1".into())));
        assert!(text.contains("previous session s1"), "{text}");
        assert!(text.contains("- turn: fix the parser…"), "{text}");
        assert!(text.contains("- tool Bash: cargo test"), "{text}");
        assert!(text.contains("terrarium mail check"), "{text}");
    }
}
