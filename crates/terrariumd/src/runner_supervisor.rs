//! The real supervisor: one agent runner per running agent, behind
//! `terrarium_runner::RunnerHandle`.
//!
//! How the parts connect:
//!
//! ```text
//!  API handler ──start/stop/send──▶ RunnerSupervisor ──spawn──▶ runner (PTY, socket, status)
//!                                        │   ▲                      │
//!                     SupervisorEvent ◀──┘   └── forward task ◀─────┘ EventRecord (with seq)
//!                          │
//!                    lib.rs pump ──▶ store (agents.status, sessions, events) ──▶ EventHub
//!                                                                             │
//!  push delivery ◀── hub listener (mail, card) ◀──────────────────────────────┘
//!  runner socket ──▶ AgentRpc ──▶ api::router (Actor::Agent) ──▶ store
//! ```
//!
//! - [`RunnerMode`] picks the runner. `Process` (the daemon default) starts
//!   one `terrarium-runner` process per agent and talks to it with
//!   `RemoteRunner` over the control socket; the harness lives through a
//!   daemon restart (PLAN §6, §18). `InProcess` holds a `Runner` in the
//!   daemon, for tests and development. Only the spawn and the shutdown
//!   differ; the rest of the supervisor sees an `Arc<dyn RunnerHandle>`.
//! - `start` brings the ephemeral targets of the agent's policy up
//!   (PLAN §19), renders `prompt.md`, writes the adapter's per-agent files,
//!   asks the adapter for the `LaunchSpec`, builds a clean env, wraps it in
//!   the sandbox, and spawns the runner. A forward task sends each runner
//!   event to the daemon as a [`SupervisorEvent`] with its seq; the pump in
//!   `lib.rs` writes it to the store once, which broadcasts it to the API
//!   streams.
//! - After a daemon start, [`Supervisor::recover`] connects to each live
//!   runner process again, with the events from the stored seq on. A
//!   runner that ended while the daemon was down left `runner-exit.json`;
//!   the restart policy decides on it.
//! - A stop, or an exit that does not restart, removes the ephemeral
//!   targets (unless `keep = true`). A daemon shutdown removes nothing.
//! - When the harness exits, the supervisor applies the restart policy of
//!   PLAN §18: exponential backoff, crash-loop detection, and a fresh session
//!   with a recap turn when a resume fails.
//! - A hub listener sees new mail and card assignments. It types the push
//!   pointer of PLAN §12 and §13 when the agent is idle, else after the next
//!   turn ends. One pending pointer covers all new items.
//! - The same listener nudges the leads: a card with no assignee in
//!   `ready`, or a move to `review` or `blocked`, adds a line to the pointer
//!   of each `project_admin` agent of the project, except the agent that
//!   made the change. Per project, one batch goes out in each
//!   `lead_nudge_interval` (10 s); the changes in between go out together.
//! - A quota hold ([`Supervisor::hold_turns`]) shows an idle agent as
//!   `blocked` with the reason and the reset time. Operator turns, push
//!   pointers, and the recap turn wait. The release types the waiting turns
//!   in order, then the pending pointer.

use std::collections::{HashMap, VecDeque};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, MutexGuard, OnceLock, Weak};
use std::time::{Duration, Instant};

use serde_json::Value;
use terrarium_adapter_claude::{ClaudeAdapter, ClaudeOptions};
use terrarium_adapter_codex::{CodexAdapter, CodexOptions};
use terrarium_adapter_copilot::{CopilotAdapter, CopilotOptions};
use terrarium_adapter_pi::{PiAdapter, PiOptions};
use terrarium_core::config::SubscriptionDef;
use terrarium_core::{
    Adapter, Agent, AgentId, AgentStatus, Event, EventRecord, HarnessId, HarnessSessionId,
    LaunchSpec,
};
use terrarium_runner::safefs;
use terrarium_runner::{
    AdapterKind, AdapterOptions, AttachEvent, ConnectOptions, ExitStatus, LOST_SIGNAL,
    ReadyDetector, ReadyDetectorSpec, RemoteRunner, RequestHandler, Runner, RunnerConfig,
    RunnerHandle, RunnerSpec, SpawnOptions, StoppedBy, live_runner_pid, read_exit_record,
};
use tokio::sync::broadcast::error::RecvError;
use tokio::sync::{broadcast, mpsc, watch};
use tracing::{debug, info, warn};

use crate::agent_rpc::AgentRpc;
use crate::api::{AppState, Shared};
use crate::prompt::{self, Peer, PromptContext};
use crate::sandbox::{self, AgentLayout, HarnessHome, SandboxConfig, SandboxMode};
use crate::store::subscriptions::Subscription;
use crate::supervisor::{
    AnswerInput, BoxFuture, PtyAttachment, PtyInput, Recovery, Supervisor, SupervisorError,
    SupervisorEvent, SupervisorEventKind, TurnHold, wants_restart,
};

/// One harness adapter and its prompt-ready detector.
#[derive(Clone)]
pub struct AdapterEntry {
    pub adapter: Arc<dyn Adapter>,
    /// For a harness whose `screen_hint` cannot tell. In-process runners
    /// only: a runner process takes [`AdapterEntry::ready_spec`].
    pub ready_detector: Option<Arc<dyn ReadyDetector>>,
    /// The `DefaultReadyDetector` settings for a runner process.
    pub ready_spec: Option<ReadyDetectorSpec>,
    /// The adapter that a runner process picks (`RunnerSpec::adapter`).
    /// `Fake` in tests only.
    pub runner_adapter: AdapterKind,
    /// The harness home paths that the hook mapping of a runner process
    /// reads (`RunnerSpec::adapter_options`).
    pub adapter_options: AdapterOptions,
}

impl AdapterEntry {
    pub fn new(adapter: Arc<dyn Adapter>) -> Self {
        Self {
            adapter,
            ready_detector: None,
            ready_spec: None,
            runner_adapter: AdapterKind::Harness,
            adapter_options: AdapterOptions::default(),
        }
    }
}

/// How the agent runners run (PLAN §6). The daemon picks it in one place,
/// `Settings::runner_mode` (`TERRARIUM_RUNNER_MODE`, `TERRARIUM_RUNNER`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RunnerMode {
    /// One `terrarium-runner` process per agent, started from `bin`. The
    /// harness lives through a daemon restart (PLAN §18).
    Process { bin: PathBuf },
    /// One `Runner` in the daemon process. The harness ends with the
    /// daemon. For tests and development.
    InProcess,
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
    /// `InProcess` unless the caller sets it; the daemon binary sets it
    /// from its settings.
    pub mode: RunnerMode,
    /// The shortest time between two lead nudges of one project
    /// ([`DEFAULT_LEAD_NUDGE_INTERVAL`]). The nudges in between go out
    /// together at the end of it.
    pub lead_nudge_interval: Duration,
}

/// The default of [`RunnerSupervisorConfig::lead_nudge_interval`].
pub const DEFAULT_LEAD_NUDGE_INTERVAL: Duration = Duration::from_secs(10);

impl RunnerSupervisorConfig {
    /// The config with the real adapters: Claude Code, Codex, Copilot CLI,
    /// and pi.
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
            mode: RunnerMode::InProcess,
            lead_nudge_interval: DEFAULT_LEAD_NUDGE_INTERVAL,
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

/// Every adapter, set up for the sandbox mode: each harness sees its home
/// at `$HOME/<rel>` in bwrap, or at the host path with no sandbox.
pub fn default_adapters(
    sandbox: &SandboxConfig,
    homes: &HashMap<HarnessId, PathBuf>,
) -> HashMap<HarnessId, AdapterEntry> {
    let host = |h: HarnessId| {
        homes
            .get(&h)
            .cloned()
            .unwrap_or_else(|| PathBuf::from(h.auth_dir()))
    };
    let inside = |h: HarnessId| {
        sandbox.home_inside(&HarnessHome {
            harness: h,
            host: host(h),
        })
    };
    let bwrap = sandbox.mode == SandboxMode::Bwrap;
    let work = bwrap.then(|| sandbox.home.join("work"));

    let claude = ClaudeOptions {
        config_dir: inside(HarnessId::Claude),
        host_config_dir: Some(host(HarnessId::Claude)),
        harness_workspace: work.clone(),
        hook_program: sandbox.hook_program(),
        ..ClaudeOptions::default()
    };
    let codex = CodexOptions {
        codex_home: inside(HarnessId::Codex),
        host_codex_home: Some(host(HarnessId::Codex)),
        harness_workspace: work.clone(),
        hook_program: sandbox.hook_program(),
        ..CodexOptions::default()
    };
    let copilot = CopilotOptions {
        config_dir: inside(HarnessId::Copilot),
        host_config_dir: Some(host(HarnessId::Copilot)),
        harness_workspace: work.clone(),
        cache_dir: bwrap.then(|| sandbox.home.join(".cache/copilot")),
        hook_program: sandbox.hook_program(),
        ..CopilotOptions::default()
    };
    let pi = PiOptions {
        agent_dir: inside(HarnessId::Pi),
        host_agent_dir: Some(host(HarnessId::Pi)),
        harness_workspace: work,
        ..PiOptions::default()
    };
    let mut map = HashMap::new();
    let entries: [(HarnessId, Arc<dyn Adapter>); 4] = [
        (HarnessId::Claude, Arc::new(ClaudeAdapter::new(claude))),
        (HarnessId::Codex, Arc::new(CodexAdapter::new(codex))),
        (HarnessId::Copilot, Arc::new(CopilotAdapter::new(copilot))),
        (HarnessId::Pi, Arc::new(PiAdapter::new(pi))),
    ];
    for (h, a) in entries {
        let mut entry = AdapterEntry::new(a);
        // A runner process builds its own adapter for the hook mapping; it
        // needs the same two home paths.
        entry.adapter_options = AdapterOptions {
            config_dir: Some(inside(h)),
            host_config_dir: Some(host(h)),
        };
        map.insert(h, entry);
    }
    map
}

/// The message for a harness with no registered adapter.
pub fn not_ready(harness: HarnessId) -> SupervisorError {
    SupervisorError::NotReady(format!(
        "the {harness} adapter is not registered in this daemon"
    ))
}

/// The definition of a stored subscription, for the adapter.
fn sub_def(s: &Subscription) -> SubscriptionDef {
    SubscriptionDef {
        name: s.name.clone(),
        provider: s.provider.clone(),
        plan: s.plan.clone(),
        harnesses: s.harnesses.clone(),
        probe: s.probe.clone(),
        alert_at: s.alert_at,
    }
}

/// How long a graceful stop waits for the harness to exit on its exit
/// keys before it kills the harness.
const GRACEFUL_STOP: Duration = Duration::from_secs(5);

/// The file that holds the SHA-256 of the prompt a harness session began
/// with (`<agent dir>/private/session-prompt.sha256`).
const SESSION_PROMPT_HASH: &str = "session-prompt.sha256";

/// The mark in the message of a launch that a symlink or a special file in
/// the agent dir refuses (`terrarium_runner::RunnerError::UnsafePath`,
/// `docs/security-review.md` F11). Such a launch ends in `error`, and the
/// restart policy does not try again: a planted entry does not go away.
const UNSAFE_MARK: &str = "unsafe agent dir: ";

/// The message of a file error at launch. An unsafe path gets
/// [`UNSAFE_MARK`] and what to do.
fn launch_fs_error(e: &terrarium_runner::RunnerError) -> String {
    match e {
        terrarium_runner::RunnerError::UnsafePath { .. } => {
            format!("{UNSAFE_MARK}{e}; remove it, then start the agent again")
        }
        _ => e.to_string(),
    }
}

/// True: the launch failed on an unsafe path ([`UNSAFE_MARK`]).
fn is_unsafe_launch(e: &SupervisorError) -> bool {
    matches!(e, SupervisorError::NotReady(m) | SupervisorError::Other(m) if m.contains(UNSAFE_MARK))
}

fn sha256_hex(text: &str) -> String {
    use sha2::Digest;
    sha2::Sha256::digest(text.as_bytes())
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect()
}

/// A launch spec, and what the launch does with a changed prompt.
struct Built {
    spec: LaunchSpec,
    /// The launch drops its resume: the prompt changed under a harness
    /// that freezes it.
    fresh_for_prompt: bool,
    /// The launch resumes with a changed prompt under a harness that
    /// takes the new one on a resume (Claude with
    /// `--system-prompt-snapshot off`, pi).
    prompt_updated: bool,
}

/// The `system` event of a resume that sends a changed prompt.
const PROMPT_UPDATED: &str = "prompt updated on resume";

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

/// A board change that a project lead acts on (a lead nudge).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum LeadChange {
    /// A card with no assignee came into `ready`.
    Ready,
    Review,
    Blocked,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct LeadItem {
    card: String,
    change: LeadChange,
    /// The card title, on one line.
    title: String,
    /// Who made the change.
    by: String,
    /// The comment of a move to `blocked`, on one line.
    comment: String,
}

impl LeadItem {
    fn line(&self) -> String {
        let c = &self.card;
        match self.change {
            LeadChange::Ready => format!(
                "New card {c} in ready: {}. Run `terrarium card ls --unassigned --column ready`.",
                self.title
            ),
            LeadChange::Review => format!(
                "{c} moved to review by {}. Run `terrarium card show {c}`.",
                self.by
            ),
            LeadChange::Blocked => format!(
                "{c} moved to blocked by {}: {}. Run `terrarium card show {c}`.",
                self.by, self.comment
            ),
        }
    }
}

/// The lead nudge of a card event, when the event is one: a card with no
/// assignee moved into `ready` (also a card made in `ready`), or a card
/// moved to `review` or `blocked`.
fn lead_item(ev: &crate::events::StoredEvent) -> Option<LeadItem> {
    let s = |k: &str| ev.data.get(k).and_then(Value::as_str).map(str::to_owned);
    if ev.kind != "card" || s("change").as_deref() != Some("moved") {
        return None;
    }
    let change = match s("to").as_deref()? {
        "ready" if s("assignee").is_none() => LeadChange::Ready,
        "review" => LeadChange::Review,
        "blocked" => LeadChange::Blocked,
        _ => return None,
    };
    Some(LeadItem {
        card: ev.card.clone()?,
        change,
        title: one_line(&s("title").unwrap_or_default()),
        by: s("actor_name")
            .or_else(|| ev.actor.clone())
            .unwrap_or_default(),
        comment: one_line(&s("comment").unwrap_or_default()),
    })
}

/// The lead nudges of one project that wait for the rate limit.
#[derive(Default)]
struct NudgeQueue {
    /// When the last batch went to the leads.
    last: Option<Instant>,
    pending: Vec<(AgentId, LeadItem)>,
    /// A task flushes `pending` at the end of the interval.
    armed: bool,
}

/// The items one pointer turn covers.
#[derive(Debug, Default, Clone, PartialEq, Eq)]
struct Pointer {
    mail: Vec<MailItem>,
    cards: Vec<CardItem>,
    leads: Vec<LeadItem>,
}

impl Pointer {
    fn is_empty(&self) -> bool {
        self.mail.is_empty() && self.cards.is_empty() && self.leads.is_empty()
    }

    /// One item for each card and change: a newer event of the same kind
    /// replaces the text.
    fn add_lead(&mut self, l: LeadItem) {
        match self
            .leads
            .iter_mut()
            .find(|x| x.card == l.card && x.change == l.change)
        {
            Some(x) => *x = l,
            None => self.leads.push(l),
        }
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
        for l in other.leads {
            self.add_lead(l);
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
        lines.extend(self.leads.iter().map(LeadItem::line));
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
    runner: Option<Arc<dyn RunnerHandle>>,
    /// The same runner, when it runs in this process: the shutdown ends it.
    local: Option<Runner>,
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
    /// The status of the last status event that the forward task passed
    /// on. A runner process reports its status on a stream of its own,
    /// which can lag behind the events; the push decision follows the
    /// events.
    seen: AgentStatus,
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
            local: None,
            phase: Phase::Down(AgentStatus::Stopped),
            epoch: 0,
            stop_requested: false,
            restart_requested: false,
            restarts: VecDeque::new(),
            resume: None,
            session: None,
            reached_idle: false,
            seen: AgentStatus::Starting,
            recap: None,
            pointer: Pointer::default(),
            pointer_busy: false,
        }
    }

    fn live(&self) -> bool {
        self.runner.is_some() || self.phase == Phase::Restarting
    }
}

/// A quota hold and the operator turns that wait for its end. It lives
/// outside the slot, so a hold on an agent that does not run applies when
/// it starts, and survives a restart of the harness.
struct Held {
    hold: TurnHold,
    waiting: Vec<String>,
}

/// The `blocked` reason of a hold: its reason, with the reset time when the
/// reason does not say it.
fn hold_reason(h: &TurnHold) -> String {
    match h.until {
        Some(u) if !h.reason.contains(&u.to_rfc3339()) => format!(
            "{}; turns wait until {}",
            h.reason,
            u.to_rfc3339_opts(chrono::SecondsFormat::Secs, true)
        ),
        _ => h.reason.clone(),
    }
}

struct Inner {
    cfg: RunnerSupervisorConfig,
    slots: Mutex<HashMap<AgentId, Slot>>,
    /// Never locked while `slots` is locked, and the other way round.
    holds: Mutex<HashMap<AgentId, Held>>,
    /// The lead nudges of each project that wait for the rate limit.
    nudges: Mutex<HashMap<String, NudgeQueue>>,
    /// The card pointers of agents that do not run: a card assigned (or
    /// unblocked) before the start. The next launch puts them into the
    /// pointer, which goes out at the first idle. Locked only inside
    /// `slots`, never the other way round.
    parked: Mutex<HashMap<AgentId, Vec<CardItem>>>,
    tx: broadcast::Sender<SupervisorEvent>,
    state: OnceLock<Weak<AppState>>,
    shutting_down: AtomicBool,
    /// True after the shutdown: the forward and attach tasks end, and drop
    /// their runner handles, so the control connections close.
    stop_tx: watch::Sender<bool>,
}

/// Runs each agent in a runner (see [`RunnerMode`]). Cheap to clone.
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
    /// The agent's subscription row: `None` when the agent names none, or
    /// when the supervisor has no store (unit tests).
    subscription: Option<SubscriptionCheck>,
    /// `<state>`: stored API keys are under `<state>/credentials`.
    state_dir: Option<PathBuf>,
    /// The URLs of the project's repos. The local ones under the repos dir
    /// are bound into the sandbox (`sandbox::local_repo_binds`).
    repo_urls: Vec<String>,
}

enum SubscriptionCheck {
    Found(Box<Subscription>),
    /// The agent names a subscription that does not exist.
    Missing(String),
}

/// PLAN §16, policy hooks: the harness must be able to spend the agent's
/// subscription. An agent with no subscription starts; the daemon logs it.
fn check_subscription(
    agent: &Agent,
    sub: Option<&SubscriptionCheck>,
) -> Result<Option<Subscription>, SupervisorError> {
    match sub {
        Some(SubscriptionCheck::Found(s)) if s.harnesses.contains(&agent.harness) => {
            Ok(Some((**s).clone()))
        }
        Some(SubscriptionCheck::Found(s)) => {
            let list: Vec<&str> = s.harnesses.iter().map(|h| h.as_str()).collect();
            Err(SupervisorError::NotReady(format!(
                "the {} harness cannot spend subscription `{}`: its harnesses are [{}]. Add `{}` \
                 to `harnesses` in the subscription, or give the agent another subscription",
                agent.harness,
                s.name,
                list.join(", "),
                agent.harness
            )))
        }
        Some(SubscriptionCheck::Missing(name)) => Err(SupervisorError::NotReady(format!(
            "agent `{}` names subscription `{name}`, but there is no such subscription; \
             define it, or clear it with PATCH /api/agents/{{id}} {{\"subscription\": null}}",
            agent.name
        ))),
        None => {
            if agent.subscription.is_none() {
                info!(
                    agent = %agent.id,
                    harness = %agent.harness,
                    "the agent has no subscription: no stored key goes into its env, \
                     and its usage is not attributed to a plan"
                );
            }
            Ok(None)
        }
    }
}

impl RunnerSupervisor {
    pub fn new(cfg: RunnerSupervisorConfig) -> Self {
        let (tx, _) = broadcast::channel(4096);
        Self {
            inner: Arc::new(Inner {
                cfg,
                slots: Mutex::new(HashMap::new()),
                holds: Mutex::new(HashMap::new()),
                nudges: Mutex::new(HashMap::new()),
                parked: Mutex::new(HashMap::new()),
                tx,
                state: OnceLock::new(),
                shutting_down: AtomicBool::new(false),
                stop_tx: watch::Sender::new(false),
            }),
        }
    }

    pub fn config(&self) -> &RunnerSupervisorConfig {
        &self.inner.cfg
    }

    fn emit(&self, agent: AgentId, kind: SupervisorEventKind) {
        self.emit_seq(agent, kind, None);
    }

    /// Emit an event of the runner, with its seq, so the pump records it
    /// once.
    fn emit_seq(&self, agent: AgentId, kind: SupervisorEventKind, seq: Option<u64>) {
        let _ = self.inner.tx.send(SupervisorEvent { agent, kind, seq });
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

    /// The quota hold of `agent`, if any.
    fn held(&self, agent: AgentId) -> Option<TurnHold> {
        lock(&self.inner.holds).get(&agent).map(|h| h.hold.clone())
    }

    /// The operator turns that wait for the end of a quota hold. For tests.
    pub fn waiting_turns(&self, agent: AgentId) -> Vec<String> {
        lock(&self.inner.holds)
            .get(&agent)
            .map(|h| h.waiting.clone())
            .unwrap_or_default()
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
    pub fn runner(&self, agent: AgentId) -> Option<Arc<dyn RunnerHandle>> {
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
            workspace: terrarium_runner::workspace_dir(&cfg.state_dir, agent.id),
            harness_dir: terrarium_runner::harness_dir(&cfg.state_dir, agent.id),
            private_dir: terrarium_runner::private_dir(&cfg.state_dir, agent.id),
            agent_dir,
            socket: terrarium_runner::socket_path(&cfg.runtime_dir, agent.id),
            homes: vec![HarnessHome {
                harness: agent.harness,
                host: cfg.home(agent.harness),
            }],
            overlays: Vec::new(),
            repos: Vec::new(),
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
        Ok(self.build_spec(agent, resume, &view).await?.spec)
    }

    async fn store_view(&self, agent: &Agent) -> StoreView {
        let Some(state) = self.state() else {
            return StoreView::default();
        };
        let id = agent.id.to_string();
        let project = agent.project.clone();
        let sub_name = agent.subscription.clone();
        let state_dir = state.state_dir.clone();
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
                let repo_urls = crate::store::repos::all_for_project(c, &project)?
                    .into_iter()
                    .map(|r| r.url)
                    .collect();
                let subscription = match sub_name {
                    Some(n) => Some(match crate::store::subscriptions::find(c, &n)? {
                        Some(s) => SubscriptionCheck::Found(Box::new(s)),
                        None => SubscriptionCheck::Missing(n),
                    }),
                    None => None,
                };
                Ok(StoreView {
                    peers,
                    queued,
                    subscription,
                    state_dir: Some(state_dir),
                    repo_urls,
                })
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
    ) -> Result<Built, SupervisorError> {
        let entry = self.entry(agent.harness)?;
        let sub = check_subscription(agent, view.subscription.as_ref())?;
        let def = sub.as_ref().map(sub_def);
        if agent.harness == HarnessId::Pi
            && let Some(reason) = def.as_ref().and_then(terrarium_adapter_pi::refusal)
        {
            return Err(SupervisorError::NotReady(format!(
                "subscription `{}`: {reason}",
                agent.subscription.as_deref().unwrap_or_default()
            )));
        }
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
        let dirs = (
            layout.agent_dir.clone(),
            layout.harness_dir.clone(),
            layout.private_dir.clone(),
            layout.workspace.clone(),
        );
        let a = agent.clone();
        let state_dir = view.state_dir.clone();
        // A harness that keeps the prompt of a session gets a new session
        // when the prompt changed since that session began.
        let frozen = entry.adapter.prompt_frozen_on_resume();
        let check_prompt = resume.is_some() && agent.new_session_on_prompt_change && frozen;
        // A harness that takes the new prompt on a resume: the session now
        // has the new prompt, so the hash follows it.
        let follow_prompt = resume.is_some() && !frozen;
        let fresh = resume.is_none();
        let repo_urls = match cfg.sandbox.mode {
            SandboxMode::Bwrap => view.repo_urls.clone(),
            SandboxMode::None => Vec::new(),
        };
        let repos_dir = cfg.sandbox.repos_dir.clone();
        let (overlays, credentials, fresh_for_prompt, prompt_updated, repos) =
            tokio::task::spawn_blocking(move || {
                migrate_dir(&dirs.0);
                for d in [&dirs.1, &dirs.3] {
                    std::fs::create_dir_all(d)
                        .map_err(|e| format!("cannot create {}: {e}", d.display()))?;
                }
                terrarium_runner::create_private_dir(&dirs.2).map_err(|e| launch_fs_error(&e))?;
                let hash = sha256_hex(&text);
                let hash_path = dirs.2.join(SESSION_PROMPT_HASH);
                // No symlink follow (F11): the daemon reads and writes in
                // `private/` with the runner's no-follow opens.
                let old = match safefs::read_to_string(&hash_path) {
                    Ok(h) => Some(h),
                    Err(e @ terrarium_runner::RunnerError::UnsafePath { .. }) => {
                        return Err(launch_fs_error(&e));
                    }
                    Err(_) => None,
                };
                let changed = old.as_deref().is_some_and(|h| h.trim() != hash);
                let fresh_for_prompt = check_prompt && changed;
                let prompt_updated = follow_prompt && changed;
                if fresh || fresh_for_prompt || prompt_updated {
                    safefs::write_private(&hash_path, hash.as_bytes()).map_err(|e| {
                        format!(
                            "cannot write {}: {}",
                            hash_path.display(),
                            launch_fs_error(&e)
                        )
                    })?;
                }
                let path = dirs.1.join("prompt.md");
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
                let overlays = adapter
                    .materialize_config(&a, &home)
                    .map_err(|e| format!("cannot write the {} config: {e}", a.harness))?;
                // The stored API key of the subscription (pi providers, the
                // Copilot PAT as `COPILOT_GITHUB_TOKEN`). Secrets: never logged.
                let credentials = match (&sub, &state_dir) {
                    (Some(s), Some(dir)) => crate::login_session::credential_env_for(dir, s),
                    _ => Vec::new(),
                };
                let repos =
                    sandbox::local_repo_binds(repo_urls.iter().map(String::as_str), &repos_dir);
                Ok((
                    overlays,
                    credentials,
                    fresh_for_prompt,
                    prompt_updated,
                    repos,
                ))
            })
            .await
            .map_err(other)?
            .map_err(SupervisorError::NotReady)?;

        // An overlay of a path over itself (a writable part of `harness/`)
        // changes nothing without a sandbox.
        let over_home = overlays.iter().filter(|o| o.source != o.target).count();
        if cfg.sandbox.mode == SandboxMode::None && over_home > 0 {
            warn!(
                agent = %agent.id,
                "sandbox mode `none` cannot bind {over_home} per-agent file(s) over the shared \
                 home; the harness uses the shared files"
            );
        }
        layout.overlays = overlays;
        layout.repos = repos;

        let resume = if fresh_for_prompt {
            info!(
                agent = %agent.id,
                "the prompt changed and the {} harness keeps the prompt of a session; \
                 starting a new session instead of a resume",
                agent.harness
            );
            None
        } else {
            resume
        };
        let mut spec = entry.adapter.launch_for(&agent, resume, def.as_ref());
        if !credentials.is_empty() {
            debug!(
                agent = %agent.id,
                vars = ?credentials.iter().map(|(k, _)| k.as_str()).collect::<Vec<_>>(),
                "stored credentials go into the env"
            );
        }
        spec.env.extend(credentials);
        // A commit in the sandbox needs an identity: the home has no
        // `.gitconfig`, and the host name gives no mail domain.
        let email = format!("{}.{}@terrarium.invalid", agent.name, agent.project);
        for (k, v) in [
            ("GIT_AUTHOR_NAME", agent.name.as_str()),
            ("GIT_COMMITTER_NAME", agent.name.as_str()),
            ("GIT_AUTHOR_EMAIL", email.as_str()),
            ("GIT_COMMITTER_EMAIL", email.as_str()),
        ] {
            spec.env.entry(k.to_owned()).or_insert_with(|| v.to_owned());
        }
        let env = cfg
            .sandbox
            .child_env(&layout, &spec.env, &|k| std::env::var(k).ok());
        Ok(Built {
            spec: cfg.sandbox.wrap(spec, &layout, env),
            fresh_for_prompt,
            prompt_updated,
        })
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
        let Built {
            spec,
            fresh_for_prompt,
            prompt_updated,
        } = self.build_spec(&agent, resume.as_ref(), &view).await?;
        let resume = if fresh_for_prompt {
            // The old session stays in the history; this launch begins a
            // new one, and a later restart resumes that.
            if let Some(slot) = lock(&self.inner.slots).get_mut(&id) {
                slot.resume = None;
                slot.session = None;
            }
            None
        } else {
            resume
        };
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
        let program = spec.program.clone();
        // The seq before the launch: a runner process replays the events
        // from here, so the ones before the connect are not lost.
        let base =
            events_seq(&terrarium_runner::private_dir(&cfg.state_dir, id).join("events.jsonl"));
        self.clamp_seq(id, base).await;
        let (runner, local): (Arc<dyn RunnerHandle>, Option<Runner>) = match &cfg.mode {
            RunnerMode::InProcess => {
                let r = Runner::spawn(rc, entry.adapter.clone(), spec, self.rpc_handler())
                    .await
                    .map_err(|e| {
                        SupervisorError::Other(format!(
                            "cannot start {program}: {}",
                            launch_fs_error(&e)
                        ))
                    })?;
                (Arc::new(r.clone()), Some(r))
            }
            RunnerMode::Process { bin } => {
                // After an exit, the old runner process can still be
                // cleaning up (docs/runner.md, "The runner process", step 6).
                let deadline = Instant::now() + OLD_RUNNER_WAIT;
                while live_runner_pid(&cfg.runtime_dir, id).is_some() && Instant::now() < deadline {
                    tokio::time::sleep(Duration::from_millis(20)).await;
                }
                let mut rs = RunnerSpec::from_config(&rc, agent.harness, spec);
                rs.adapter = entry.runner_adapter;
                rs.adapter_options = entry.adapter_options.clone();
                rs.ready_detector = entry.ready_spec.clone();
                if rs.ready_detector.is_none() && entry.ready_detector.is_some() {
                    warn!(
                        agent = %id,
                        "the {} adapter has a ready detector in code only; the runner process \
                         uses the default detector",
                        agent.harness
                    );
                }
                let mut so = SpawnOptions::new(id, &cfg.state_dir, &cfg.runtime_dir, rs);
                so.connect = ConnectOptions {
                    handler: self.rpc_handler(),
                    events_since: Some(base),
                    ..ConnectOptions::default()
                };
                let r = RemoteRunner::spawn_process(bin, so).await.map_err(|e| {
                    SupervisorError::Other(format!(
                        "cannot start {program} in a runner process ({}): {e}",
                        bin.display()
                    ))
                })?;
                info!(agent = %id, pid = r.hello().runner_pid, "runner process started");
                (Arc::new(r), None)
            }
        };
        let rx = runner.subscribe_events();

        let installed = {
            let mut slots = lock(&self.inner.slots);
            match slots
                .get_mut(&id)
                .filter(|s| s.epoch == epoch && !s.stop_requested)
            {
                Some(slot) => {
                    slot.runner = Some(runner.clone());
                    slot.local = local;
                    slot.phase = Phase::Running;
                    slot.reached_idle = false;
                    slot.seen = runner.status();
                    slot.pointer_busy = false;
                    for m in view.queued {
                        slot.pointer.add_mail(m);
                    }
                    for c in lock(&self.inner.parked).remove(&id).unwrap_or_default() {
                        slot.pointer.add_card(c);
                    }
                    true
                }
                None => false,
            }
        };
        if !installed {
            // A stop or a new start came while the harness launched.
            let _ = runner.kill().await;
            return Ok(());
        }
        let reason = match &resume {
            Some(s) => format!("resuming session {s}"),
            None if fresh_for_prompt => "new session: the prompt changed".to_owned(),
            None => "new session".to_owned(),
        };
        info!(agent = %id, name = %agent.name, %reason, "agent started");
        self.emit_status(id, AgentStatus::Starting, reason);
        if prompt_updated && resume.is_some() {
            info!(agent = %id, "{PROMPT_UPDATED}");
            self.emit(
                id,
                SupervisorEventKind::Harness(Event::System {
                    message: PROMPT_UPDATED.to_owned(),
                }),
            );
        }
        let now = runner.status();
        if now != AgentStatus::Starting {
            match self.held(id) {
                Some(h) if now == AgentStatus::Idle => {
                    self.emit_status(id, AgentStatus::Blocked, hold_reason(&h));
                }
                _ => self.emit_status(id, now, "caught up"),
            }
            if now == AgentStatus::Idle {
                self.on_idle(id, epoch);
            }
        }
        let me = self.clone();
        tokio::spawn(async move { me.forward(id, epoch, runner, rx).await });
        Ok(())
    }

    /// The handler for the agent RPC requests of a runner.
    fn rpc_handler(&self) -> Arc<dyn RequestHandler> {
        Arc::new(AgentRpc::new(
            self.inner.state.get().cloned().unwrap_or_default(),
        ))
    }

    /// Lower the stored runner seq of `id` to `seq` (see
    /// `store::agents::clamp_runner_seq`).
    async fn clamp_seq(&self, id: AgentId, seq: u64) {
        let Some(state) = self.state() else { return };
        let key = id.to_string();
        let result = state
            .db
            .write("system", move |t| {
                crate::store::agents::clamp_runner_seq(t, &key, seq)
            })
            .await;
        if let Err(e) = result {
            warn!(agent = %id, "cannot reset the runner seq: {e}");
        }
    }

    fn is_process_mode(&self) -> bool {
        matches!(self.inner.cfg.mode, RunnerMode::Process { .. })
    }

    /// Connect to the live runner process of `id`. `since`: replay the
    /// events after this seq.
    async fn connect_runner(
        &self,
        id: AgentId,
        since: Option<u64>,
    ) -> Result<RemoteRunner, SupervisorError> {
        RemoteRunner::connect_with(
            &self.inner.cfg.runtime_dir,
            id,
            ConnectOptions {
                handler: self.rpc_handler(),
                events_since: since,
                ..ConnectOptions::default()
            },
        )
        .await
        .map_err(|e| SupervisorError::Other(format!("cannot connect to the runner of {id}: {e}")))
    }

    /// Take over a connected runner process: the slot of epoch `epoch`
    /// holds it, and a forward task passes its events on. The harness does
    /// not restart. False: the slot moved on (a stop or a new start).
    fn adopt(&self, id: AgentId, epoch: u64, remote: RemoteRunner, since: Option<u64>) -> bool {
        let hello = remote.hello().clone();
        let runner: Arc<dyn RunnerHandle> = Arc::new(remote);
        let rx = runner.subscribe_events();
        {
            let mut slots = lock(&self.inner.slots);
            let Some(slot) = slots
                .get_mut(&id)
                .filter(|s| s.epoch == epoch && !s.stop_requested)
            else {
                return false;
            };
            slot.runner = Some(runner.clone());
            slot.local = None;
            slot.phase = Phase::Running;
            slot.reached_idle = hello.status != AgentStatus::Starting;
            slot.seen = hello.status;
            slot.pointer_busy = false;
            for c in lock(&self.inner.parked).remove(&id).unwrap_or_default() {
                slot.pointer.add_card(c);
            }
            if let Some(s) = &hello.session {
                // A later crash restarts with resume of this session.
                slot.session = Some(s.clone());
                slot.resume = Some(s.clone());
            }
        }
        let reason = format!("connected to runner pid {}", hello.runner_pid);
        info!(agent = %id, seq = hello.seq, since = ?since, status = %hello.status, "{reason}");
        // With a replay, the replayed status changes bring the stored
        // status up to date. With none, report the status now.
        let replay = since.is_some_and(|s| s < hello.seq);
        if !replay {
            let now = runner.status();
            match self.held(id) {
                Some(h) if now == AgentStatus::Idle => {
                    self.emit_status(id, AgentStatus::Blocked, hold_reason(&h));
                }
                _ => self.emit_status(id, now, reason),
            }
            if now == AgentStatus::Idle {
                self.on_idle(id, epoch);
            }
        }
        let me = self.clone();
        tokio::spawn(async move { me.forward(id, epoch, runner, rx).await });
        true
    }

    /// Pass one runner event on. True: it is the exit of the harness.
    fn pass_on(&self, id: AgentId, epoch: u64, rec: EventRecord) -> bool {
        match &rec.event {
            // The supervisor decides the final status itself.
            Event::StatusChanged {
                to: AgentStatus::Stopped | AgentStatus::Error,
                ..
            } => return false,
            Event::SessionStarted {
                harness_session, ..
            } => {
                if let Some(slot) = lock(&self.inner.slots).get_mut(&id) {
                    slot.session = Some(harness_session.clone());
                }
            }
            Event::StatusChanged { to, .. } => {
                if let Some(slot) = lock(&self.inner.slots)
                    .get_mut(&id)
                    .filter(|s| s.epoch == epoch)
                {
                    slot.seen = *to;
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
        match self.held(id) {
            // A held agent is `blocked`, not `idle`.
            Some(h) if idle => self.emit_seq(
                id,
                SupervisorEventKind::Status {
                    status: AgentStatus::Blocked,
                    reason: Some(hold_reason(&h)),
                },
                rec.seq,
            ),
            _ => self.emit_seq(id, SupervisorEventKind::Harness(rec.event), rec.seq),
        }
        if idle {
            self.on_idle(id, epoch);
        }
        exit
    }

    /// Send the runner's events on, until the harness exits. A shutdown
    /// ends the task at once. When the control connection of a live runner
    /// process drops, the task connects again.
    async fn forward(
        self,
        id: AgentId,
        epoch: u64,
        runner: Arc<dyn RunnerHandle>,
        mut rx: broadcast::Receiver<EventRecord>,
    ) {
        let mut stop = self.inner.stop_tx.subscribe();
        let mut last = 0u64;
        let mut note = |rec: &EventRecord| {
            if let Some(seq) = rec.seq {
                last = last.max(seq);
            }
        };
        let mut exited = None;
        {
            let wait = runner.wait();
            tokio::pin!(wait);
            loop {
                tokio::select! {
                    biased;
                    () = stopped(&mut stop) => return,
                    r = rx.recv() => match r {
                        Ok(rec) => {
                            note(&rec);
                            if self.pass_on(id, epoch, rec) {
                                break;
                            }
                        }
                        Err(RecvError::Lagged(n)) => {
                            warn!(agent = %id, "the supervisor missed {n} runner events");
                        }
                        Err(RecvError::Closed) => break,
                    },
                    status = &mut wait => {
                        exited = Some(status);
                        break;
                    }
                }
            }
        }
        if exited.is_some() {
            // The exit can come before the last events: take what is still
            // on the way.
            while let Ok(Ok(rec)) = tokio::time::timeout(EXIT_DRAIN, rx.recv()).await {
                note(&rec);
                if self.pass_on(id, epoch, rec) {
                    break;
                }
            }
        }
        let status = match exited {
            Some(s) => s,
            None => runner.wait().await,
        };
        if *stop.borrow() {
            return;
        }
        drop(runner);
        if status.signal.as_deref() == Some(LOST_SIGNAL)
            && self.is_process_mode()
            && let Some(pid) = live_runner_pid(&self.inner.cfg.runtime_dir, id)
        {
            warn!(agent = %id, pid, "the control connection to the runner dropped; connecting again");
            match self.connect_runner(id, Some(last)).await {
                Ok(remote) => {
                    if !self.adopt(id, epoch, remote, Some(last)) {
                        debug!(agent = %id, "the agent moved on while the supervisor connected again");
                    }
                    return;
                }
                Err(e) => warn!(agent = %id, "{e}"),
            }
        }
        self.on_exit(id, epoch, status).await;
    }

    fn on_idle(&self, id: AgentId, epoch: u64) {
        {
            let mut slots = lock(&self.inner.slots);
            let Some(slot) = slots.get_mut(&id).filter(|s| s.epoch == epoch) else {
                return;
            };
            slot.reached_idle = true;
        }
        self.deliver_pending(id);
    }

    /// Type the recap turn, else the pending pointer. Nothing goes out
    /// while a quota hold lasts.
    fn deliver_pending(&self, id: AgentId) {
        if self.held(id).is_some() {
            return;
        }
        let recap = {
            let mut slots = lock(&self.inner.slots);
            let Some(slot) = slots.get_mut(&id) else {
                return;
            };
            match slot.runner.clone() {
                Some(r) => slot.recap.take().map(|t| (t, r)),
                None => None,
            }
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

    async fn on_exit(&self, id: AgentId, epoch: u64, status: ExitStatus) {
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
        let clean = status.success();
        let how = exit_how(&status);
        let mut ended_agent = None;
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
                let dir = terrarium_runner::private_dir(&self.inner.cfg.state_dir, id);
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
                let wanted = wants_restart(slot.agent.restart, clean);
                let now = Instant::now();
                while slot
                    .restarts
                    .front()
                    .is_some_and(|t| now.duration_since(*t) > tuning.crash_window)
                {
                    slot.restarts.pop_front();
                }
                if !wanted {
                    let (s, reason) = final_status(&status);
                    Next::Done(s, reason)
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
            slot.local = None;
            match &next {
                Next::Done(s, _) => {
                    slot.phase = Phase::Down(*s);
                    ended_agent = Some(slot.agent.clone());
                }
                Next::Relaunch { .. } => slot.phase = Phase::Restarting,
            }
            next
        };
        match next {
            Next::Done(s, reason) => {
                info!(agent = %id, status = %s, %reason, "agent ended");
                self.emit_status(id, s, reason);
                if let Some(a) = ended_agent {
                    self.targets_down(&a);
                }
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
                        let agent = lock(&me.inner.slots).get_mut(&id).map(|slot| {
                            slot.phase = Phase::Down(AgentStatus::Error);
                            slot.agent.clone()
                        });
                        me.emit_status(id, AgentStatus::Error, format!("restart failed: {e}"));
                        if let Some(a) = agent {
                            me.targets_down(&a);
                        }
                    }
                });
            }
        }
    }

    /// Bring the ephemeral targets of the agent's policy up (PLAN §19). A
    /// target that does not come up does not stop the start; the agent
    /// gets a notification event.
    async fn targets_up(&self, agent: &Agent) {
        if agent.policy.targets.is_empty() {
            return;
        }
        let Some(state) = self.state() else { return };
        for (t, e) in crate::executor::on_agent_start(&state, &agent.policy.targets).await {
            self.emit(
                agent.id,
                SupervisorEventKind::Harness(Event::Notification {
                    message: format!("the ephemeral target `{t}` did not come up: {e}"),
                    question: false,
                }),
            );
        }
    }

    /// Remove the ephemeral targets of the agent's policy, unless
    /// `keep = true` (PLAN §19). In the background: a stop does not wait.
    fn targets_down(&self, agent: &Agent) {
        if agent.policy.targets.is_empty() {
            return;
        }
        let Some(state) = self.state() else { return };
        let targets = agent.policy.targets.clone();
        tokio::spawn(async move {
            crate::executor::on_agent_stop(&state, &targets).await;
        });
    }

    /// Mark the slot of `agent` as starting, with a new epoch. Fails when
    /// the agent is live.
    fn claim(
        &self,
        agent: &Agent,
        resume: Option<HarnessSessionId>,
    ) -> Result<u64, SupervisorError> {
        let mut slots = lock(&self.inner.slots);
        let slot = slots
            .entry(agent.id)
            .or_insert_with(|| Slot::new(agent.clone()));
        if slot.live() {
            return Err(SupervisorError::AlreadyRunning(agent.id));
        }
        slot.agent = agent.clone();
        slot.epoch += 1;
        slot.stop_requested = false;
        slot.restart_requested = false;
        slot.restarts.clear();
        slot.resume = resume;
        slot.session = None;
        slot.recap = None;
        slot.pointer = Pointer::default();
        slot.phase = Phase::Running;
        Ok(slot.epoch)
    }

    /// Set the slot of `id` down, when it is still at `epoch`.
    fn release(&self, id: AgentId, epoch: u64, status: AgentStatus) {
        if let Some(slot) = lock(&self.inner.slots).get_mut(&id)
            && slot.epoch == epoch
        {
            slot.phase = Phase::Down(status);
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

    /// Queue a card pointer. An agent that does not run keeps it parked
    /// until its next launch (as queued mail waits in the store), so the
    /// order "assign, then start" still gives the agent its pointer.
    fn add_card_pointer(&self, id: AgentId, c: CardItem) {
        {
            let mut slots = lock(&self.inner.slots);
            match slots.get_mut(&id).filter(|s| s.runner.is_some()) {
                Some(slot) => slot.pointer.add_card(c),
                None => {
                    let mut parked = lock(&self.inner.parked);
                    let cards = parked.entry(id).or_default();
                    if !cards.contains(&c) {
                        cards.push(c);
                    }
                    return;
                }
            }
        }
        self.maybe_push(id);
    }

    /// Type the pending pointer when the agent is idle and no pointer is on
    /// the way. A `working` agent gets it after its next stop hook.
    fn maybe_push(&self, id: AgentId) {
        if self.held(id).is_some() {
            return;
        }
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
                || slot.seen != AgentStatus::Idle
            {
                return;
            }
            slot.pointer_busy = true;
            (runner, std::mem::take(&mut slot.pointer))
        };
        let me = self.clone();
        tokio::spawn(async move {
            // The board can change between the event and the idle: drop
            // the card lines that no longer apply.
            let pointer = me.still_current(id, pointer).await;
            if pointer.is_empty() {
                if let Some(slot) = lock(&me.inner.slots).get_mut(&id) {
                    slot.pointer_busy = false;
                }
                // Items that came in meanwhile go out now.
                me.maybe_push(id);
                return;
            }
            let delivery = runner.type_turn(pointer.text());
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

    /// `pointer` without the card and lead lines that the board no longer
    /// supports: a card in `ready` that has an assignee now, a card that
    /// left `review` or `blocked`, a card assigned to another agent, or a
    /// card that is gone. Mail lines stay. On a read error, all stay.
    async fn still_current(&self, id: AgentId, mut pointer: Pointer) -> Pointer {
        if pointer.cards.is_empty() && pointer.leads.is_empty() {
            return pointer;
        }
        let Some(state) = self.state() else {
            return pointer;
        };
        let mut keys: Vec<String> = pointer.cards.iter().map(|c| c.card.clone()).collect();
        keys.extend(pointer.leads.iter().map(|l| l.card.clone()));
        keys.sort();
        keys.dedup();
        let cards = state
            .db
            .read(move |c| {
                let mut out = HashMap::new();
                for k in keys {
                    match crate::store::board::get_card(c, &k) {
                        Ok(card) => {
                            out.insert(k, Some((card.column, card.assignee, card.archived)));
                        }
                        Err(e) if e.code == crate::error::Code::NotFound => {
                            out.insert(k, None);
                        }
                        Err(e) => return Err(e),
                    }
                }
                Ok(out)
            })
            .await;
        let cards = match cards {
            Ok(c) => c,
            Err(e) => {
                debug!(agent = %id, "cannot check the pointer cards: {e}");
                return pointer;
            }
        };
        let me = id.to_string();
        let now = |k: &str| cards.get(k).cloned().flatten().filter(|c| !c.2);
        pointer.cards.retain(|c| match now(&c.card) {
            None => false,
            Some((column, assignee, _)) => {
                column != "done"
                    && (c.change != CardChange::Assigned || assignee.as_deref() == Some(&me))
            }
        });
        pointer.leads.retain(|l| match now(&l.card) {
            None => false,
            Some((column, assignee, _)) => match l.change {
                LeadChange::Ready => column == "ready" && assignee.is_none(),
                LeadChange::Review => column == "review",
                LeadChange::Blocked => column == "blocked",
            },
        });
        pointer
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

    /// Queue a lead nudge for each `project_admin` agent of the card's
    /// project, except the agent that made the change. Not more than one
    /// batch per project goes out in each `lead_nudge_interval`; the items
    /// in between go out together at its end. Each batch joins the pending
    /// pointer of the lead, which goes out when the lead is idle.
    async fn nudge_leads(&self, ev: &crate::events::StoredEvent, item: LeadItem) {
        let (Some(state), Some(project)) = (self.state(), ev.project.clone()) else {
            return;
        };
        let cause = ev
            .actor
            .as_deref()
            .and_then(|a| a.strip_prefix("agent:"))
            .map(str::to_owned);
        let p = project.clone();
        let leads = state
            .db
            .read(move |c| {
                let page = crate::store::Page {
                    limit: Some(crate::store::Page::MAX),
                    cursor: None,
                };
                Ok(crate::store::agents::list(c, Some(&p), &page)?
                    .items
                    .into_iter()
                    .filter(|v| v.agent.roles.contains(&terrarium_core::Role::ProjectAdmin))
                    .map(|v| v.agent.id)
                    .filter(|id| cause.as_deref() != Some(id.to_string().as_str()))
                    .collect::<Vec<AgentId>>())
            })
            .await;
        let leads = match leads {
            Ok(l) if !l.is_empty() => l,
            Ok(_) => return,
            Err(e) => {
                warn!(project = %project, "cannot find the project leads: {e}");
                return;
            }
        };
        let interval = self.inner.cfg.lead_nudge_interval;
        let wait = {
            let mut all = lock(&self.inner.nudges);
            let q = all.entry(project.clone()).or_default();
            q.pending
                .extend(leads.into_iter().map(|id| (id, item.clone())));
            if q.armed {
                return;
            }
            match q.last.map(|t| t.elapsed()) {
                Some(e) if e < interval => {
                    q.armed = true;
                    interval - e
                }
                _ => Duration::ZERO,
            }
        };
        if wait.is_zero() {
            self.flush_nudges(&project);
            return;
        }
        let me = self.clone();
        tokio::spawn(async move {
            tokio::time::sleep(wait).await;
            me.flush_nudges(&project);
        });
    }

    /// Put the waiting lead nudges of `project` into the leads' pointers.
    fn flush_nudges(&self, project: &str) {
        let pending = {
            let mut all = lock(&self.inner.nudges);
            let Some(q) = all.get_mut(project) else {
                return;
            };
            q.armed = false;
            q.last = Some(Instant::now());
            std::mem::take(&mut q.pending)
        };
        // One add for each lead: an idle lead gets the whole batch in one
        // pointer.
        let mut by_lead: Vec<(AgentId, Vec<LeadItem>)> = Vec::new();
        for (id, item) in pending {
            match by_lead.iter_mut().find(|(a, _)| *a == id) {
                Some((_, items)) => items.push(item),
                None => by_lead.push((id, vec![item])),
            }
        }
        for (id, items) in by_lead {
            self.add_pointer(id, |p| items.into_iter().for_each(|i| p.add_lead(i)));
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
                if let Some(item) = lead_item(&ev) {
                    me.nudge_leads(&ev, item).await;
                }
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
                        // The loop guard held the message (PLAN §12): it is
                        // in the inbox, but it types no pointer.
                        if ev.data.get("held").and_then(Value::as_bool) == Some(true) {
                            continue;
                        }
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
                        me.add_card_pointer(agent, CardItem { card, change });
                    }
                    _ => {}
                }
            }
        });
    }
}

/// How long a launch waits for the runner process of the last launch to
/// end. It lingers up to 2 s after the harness exits.
const OLD_RUNNER_WAIT: Duration = Duration::from_secs(5);

/// Resolves when the shutdown flag is set.
async fn stopped(rx: &mut watch::Receiver<bool>) {
    let _ = rx.wait_for(|s| *s).await;
}

/// How long the forward task waits for the last events after the harness
/// exits.
const EXIT_DRAIN: Duration = Duration::from_secs(1);

/// How a harness exit reads in a status reason.
pub fn exit_how(status: &ExitStatus) -> String {
    match &status.signal {
        Some(s) => format!("exited with code {} ({s})", status.code),
        None => format!("exited with code {}", status.code),
    }
}

/// The final status of an exit that does not restart: `stopped` (reason
/// `exited`) for a clean exit, else `error`.
fn final_status(status: &ExitStatus) -> (AgentStatus, String) {
    if status.success() {
        (AgentStatus::Stopped, "exited".into())
    } else {
        (AgentStatus::Error, exit_how(status))
    }
}

/// The seq of the last line of an `events.jsonl` (docs/runner.md, "Event
/// seq"): its number of lines. 0 when the file is missing.
fn events_seq(path: &Path) -> u64 {
    use std::io::Read;
    // No symlink follow (F11).
    let Ok(mut f) = safefs::open_read(path) else {
        return 0;
    };
    let mut buf = vec![0u8; 64 * 1024];
    let (mut lines, mut last) = (0u64, b'\n');
    loop {
        match f.read(&mut buf) {
            Ok(0) | Err(_) => break,
            Ok(n) => {
                lines += buf[..n].iter().filter(|b| **b == b'\n').count() as u64;
                last = buf[n - 1];
            }
        }
    }
    // A torn last line has no newline; it keeps its seq.
    if last != b'\n' {
        lines += 1;
    }
    lines
}

/// The files that the layout before the split kept at the top of the agent
/// dir and that now live in `private/` (`terrarium_runner::LEGACY_PRIVATE`).
const LEGACY_PRIVATE: &[&str] = &[
    "events.jsonl",
    "pty.log",
    "pty.log.1",
    "runner.json",
    "runner-argv.json",
    "runner-exit.json",
    "runner.log",
    "session-prompt.sha256",
];

/// The entries that the layout before the split kept at the top of the
/// agent dir and that now live in `harness/` (`terrarium_runner::LEGACY_HARNESS`).
const LEGACY_HARNESS: &[&str] = &["prompt.md", "claude", "codex", "copilot", "pi"];

/// The marker in the name of a quarantined agent dir:
/// `<ulid>.quarantine-<unix seconds>`.
const QUARANTINE_MARK: &str = ".quarantine-";

/// The first entry of the agent dir `dir` that makes the layout unsafe to
/// migrate or to launch from, or `None` (`docs/security-review.md` F11).
///
/// Before the split, the sandbox bound the whole agent dir read-write, so
/// an agent could plant a symlink where the daemon or the runner now writes
/// (`private/`, `harness/`, a record file). A symlink at the agent dir, at
/// `private`, `harness`, or `workspace`, at an entry that the split moves
/// (at the top or at its new place), or directly in an adapter dir, is
/// unsafe. So is an entry to
/// move that is not a regular file or a directory. Nothing follows a link.
fn unsafe_layout_entry(dir: &Path) -> Option<PathBuf> {
    let kind = |p: &Path| std::fs::symlink_metadata(p).ok().map(|m| m.file_type());
    let is_link = |p: &Path| kind(p).is_some_and(|t| t.is_symlink());
    if is_link(dir) {
        return Some(dir.to_path_buf());
    }
    if !kind(dir).is_some_and(|t| t.is_dir()) {
        return None;
    }
    for sub in [
        terrarium_runner::PRIVATE_DIR,
        terrarium_runner::HARNESS_DIR,
        terrarium_runner::WORKSPACE_DIR,
    ] {
        if is_link(&dir.join(sub)) {
            return Some(dir.join(sub));
        }
    }
    for (sub, names) in [
        (terrarium_runner::PRIVATE_DIR, LEGACY_PRIVATE),
        (terrarium_runner::HARNESS_DIR, LEGACY_HARNESS),
    ] {
        for name in names {
            let top = dir.join(name);
            if let Some(t) = kind(&top)
                && !(t.is_file() || t.is_dir())
            {
                return Some(top);
            }
            let placed = dir.join(sub).join(name);
            if is_link(&placed) {
                return Some(placed);
            }
            // The daemon writes the hook config and the harness files of
            // an adapter dir (`claude/settings.json`, ...): one level in.
            for d in [&top, &placed] {
                if !kind(d).is_some_and(|t| t.is_dir()) {
                    continue;
                }
                let Ok(entries) = std::fs::read_dir(d) else {
                    continue;
                };
                for e in entries.flatten() {
                    if e.file_type().is_ok_and(|t| t.is_symlink()) {
                        return Some(e.path());
                    }
                }
            }
        }
    }
    None
}

/// Rename the unsafe agent dir `dir` to `<ulid>.quarantine-<ts>` and make
/// a clean one in its place (`docs/security-review.md` F11). The workspace
/// comes back when it is a real directory: it is the agent's own work, and
/// the sandbox binds it read-write anyway. The rest stays in the
/// quarantine for the operator to read: the daemon needs none of it (the
/// sessions live in the harness home, and the next launch writes a new
/// prompt and a new private dir). Returns the quarantine path.
fn quarantine_dir(dir: &Path) -> std::io::Result<PathBuf> {
    let ts = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| d.as_secs());
    let name = dir
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_default();
    let mut to = dir.with_file_name(format!("{name}{QUARANTINE_MARK}{ts}"));
    let mut n = 1;
    while std::fs::symlink_metadata(&to).is_ok() {
        to = dir.with_file_name(format!("{name}{QUARANTINE_MARK}{ts}-{n}"));
        n += 1;
    }
    // `rename` moves a symlink itself, never its target.
    std::fs::rename(dir, &to)?;
    std::fs::create_dir(dir)?;
    let work = to.join(terrarium_runner::WORKSPACE_DIR);
    if std::fs::symlink_metadata(&work).is_ok_and(|m| m.file_type().is_dir()) {
        std::fs::rename(&work, dir.join(terrarium_runner::WORKSPACE_DIR))?;
    }
    Ok(to)
}

/// Move the files of an agent dir from the layout before the split into
/// `private/` and `harness/` (`terrarium_runner::migrate_legacy_layout`).
/// A dir with a planted symlink is not migrated: it goes to quarantine and
/// a clean dir takes its place ([`quarantine_dir`]). Idempotent; an error
/// is logged, not fatal.
fn migrate_dir(dir: &Path) {
    if let Some(bad) = unsafe_layout_entry(dir) {
        match quarantine_dir(dir) {
            Ok(to) => warn!(
                dir = %dir.display(),
                entry = %bad.display(),
                quarantine = %to.display(),
                "the agent dir has a symlink or a special file where the daemon writes; \
                 moved it to quarantine and made a clean one"
            ),
            Err(e) => warn!(
                dir = %dir.display(),
                entry = %bad.display(),
                "the agent dir has a symlink where the daemon writes, and cannot go to \
                 quarantine: {e}"
            ),
        }
        return;
    }
    match terrarium_runner::migrate_legacy_layout(dir) {
        Ok(0) => {}
        Ok(n) => info!(
            dir = %dir.display(),
            moved = n,
            "moved the agent files into private/ and harness/"
        ),
        Err(e) => warn!(dir = %dir.display(), "cannot move the agent files to the new layout: {e}"),
    }
}

/// Migrate every agent dir under `<state>/agents` once (see [`migrate_dir`]).
/// A quarantined dir stays as it is.
fn migrate_all(state_dir: &Path) {
    let Ok(entries) = std::fs::read_dir(state_dir.join("agents")) else {
        return;
    };
    for e in entries.flatten() {
        if e.file_name().to_string_lossy().contains(QUARANTINE_MARK) {
            continue;
        }
        if e.file_type().is_ok_and(|t| t.is_dir() || t.is_symlink()) {
            migrate_dir(&e.path());
        }
    }
}

/// Queue `text` on the runner and log a failed delivery.
fn type_detached(agent: AgentId, runner: &Arc<dyn RunnerHandle>, text: String) {
    let delivery = runner.type_turn(text);
    tokio::spawn(async move {
        if let Err(e) = delivery.wait().await {
            debug!(agent = %agent, "a turn was not delivered: {e}");
        }
    });
}

/// The recap turn after a resume failed: the last turns and tool calls of
/// the old session, from `events.jsonl` in `private_dir`, and the commands
/// to pick up work.
pub fn recap_turn(private_dir: &Path, old: Option<&HarnessSessionId>) -> String {
    // No symlink follow (F11).
    let lines: Vec<String> = safefs::read_to_string(&private_dir.join("events.jsonl"))
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
            let epoch = self.claim(&agent, resume)?;
            // A runner process that an earlier daemon started still holds
            // the agent: take it over. A second runner cannot start.
            if self.is_process_mode()
                && let Some(pid) = live_runner_pid(&self.inner.cfg.runtime_dir, id)
            {
                warn!(agent = %id, pid, "a runner process still holds the agent; taking it over");
                let remote = match self.connect_runner(id, None).await {
                    Ok(r) => r,
                    Err(e) => {
                        self.release(id, epoch, AgentStatus::Stopped);
                        return Err(e);
                    }
                };
                self.adopt(id, epoch, remote, None);
                return Ok(());
            }
            self.targets_up(&agent).await;
            let result = self.launch(id, epoch).await;
            match &result {
                Ok(()) => {}
                // A planted entry in the agent dir: `error` with the path,
                // so the operator sees it (F11).
                Err(e) if is_unsafe_launch(e) => {
                    warn!(agent = %id, "the launch refused the agent dir: {e}");
                    self.release(id, epoch, AgentStatus::Error);
                    self.emit_status(id, AgentStatus::Error, e.to_string());
                    self.targets_down(&agent);
                }
                Err(_) => {
                    self.release(id, epoch, AgentStatus::Stopped);
                    self.targets_down(&agent);
                }
            }
            result
        })
    }

    fn stop(&self, agent: AgentId) -> BoxFuture<'_, Result<(), SupervisorError>> {
        self.stop_by(agent, StoppedBy::Operator)
    }

    fn stop_by(&self, agent: AgentId, by: StoppedBy) -> BoxFuture<'_, Result<(), SupervisorError>> {
        Box::pin(async move {
            let (runner, def) = {
                let mut slots = lock(&self.inner.slots);
                let slot = slots
                    .get_mut(&agent)
                    .filter(|s| s.live())
                    .ok_or(SupervisorError::NotRunning(agent))?;
                // A stop is on the way already (for example `agent stop`,
                // then `agent rm --confirm`, which stops again): it types
                // the exit keys and kills on a timeout. A second set of
                // keys in its exit-key pause gave Codex `/exit/exit`.
                if slot.stop_requested && slot.runner.is_some() {
                    return Ok(());
                }
                slot.stop_requested = true;
                let def = slot.agent.clone();
                match slot.runner.clone() {
                    Some(r) => (Some(r), def),
                    None => {
                        // Waiting for a restart: cancel it.
                        slot.epoch += 1;
                        slot.phase = Phase::Down(AgentStatus::Stopped);
                        (None, def)
                    }
                }
            };
            let harness = def.harness;
            let Some(r) = runner else {
                self.emit_status(agent, AgentStatus::Stopped, "stopped");
                self.targets_down(&def);
                return Ok(());
            };
            // `runner-exit.json` records the stop and who asked for it
            // (the operator, or an agent), so a daemon that starts before
            // this one sees the exit does not launch the agent again.
            if let Err(e) = r.set_stopped_by(by).await {
                debug!(agent = %agent, "cannot record the stop reason: {e}");
            }
            // A graceful stop: an idle harness with known exit keys gets
            // them, and a kill only when it has not exited in time. With an
            // exit-key pause, the final CR goes after the pause.
            let (writes, pause) = self
                .entry(harness)
                .map(|e| {
                    let pause = e.adapter.exit_key_pause();
                    (
                        terrarium_core::adapter::exit_key_writes(&e.adapter.exit_keys(), pause),
                        pause,
                    )
                })
                .unwrap_or_default();
            let mut writes = writes.into_iter();
            if let Some(first) = writes.next()
                && r.status() == AgentStatus::Idle
                && r.write(&first).await.is_ok()
            {
                tokio::spawn(async move {
                    for w in writes {
                        tokio::time::sleep(pause).await;
                        if let Err(e) = r.write(&w).await {
                            debug!(agent = %agent, "cannot type the exit keys: {e}");
                        }
                    }
                    if tokio::time::timeout(GRACEFUL_STOP, r.wait()).await.is_err() {
                        debug!(agent = %agent, "the harness did not exit on its exit keys; killing it");
                        let _ = r.kill().await;
                    }
                });
                return Ok(());
            }
            r.kill().await.map_err(other)
        })
    }

    fn clear_error(&self, agent: AgentId) -> BoxFuture<'_, ()> {
        Box::pin(async move {
            {
                let mut slots = lock(&self.inner.slots);
                if let Some(slot) = slots.get_mut(&agent) {
                    if slot.live() {
                        return;
                    }
                    slot.epoch += 1;
                    slot.phase = Phase::Down(AgentStatus::Stopped);
                }
            }
            // A runner process that no slot holds (for example one that an
            // earlier daemon started) still runs the agent: end it.
            if self.is_process_mode()
                && let Some(pid) = live_runner_pid(&self.inner.cfg.runtime_dir, agent)
            {
                warn!(agent = %agent, pid, "a stray runner holds the agent in error; killing it");
                match self.connect_runner(agent, None).await {
                    Ok(r) => {
                        if let Err(e) = r.set_stopped_by(StoppedBy::Operator).await {
                            debug!(agent = %agent, "cannot record the stop reason: {e}");
                        }
                        if let Err(e) = r.kill().await {
                            warn!(agent = %agent, "cannot kill the stray runner: {e}");
                        }
                    }
                    Err(e) => warn!(agent = %agent, "{e}"),
                }
            }
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
                Some(r) => {
                    let _ = r.set_stopped_by(StoppedBy::Policy).await;
                    r.kill().await.map_err(other)
                }
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
            if let Some(h) = lock(&self.inner.holds).get_mut(&agent) {
                info!(agent = %agent, "a quota hold lasts; the turn waits");
                h.waiting.push(text);
                return Ok(());
            }
            type_detached(agent, &runner, text);
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
            let (snapshot, mut output) = runner.attach().await.map_err(other)?;
            let (out_tx, out_rx) = mpsc::channel::<Vec<u8>>(256);
            let (in_tx, mut in_rx) = mpsc::channel::<PtyInput>(256);
            let mut stop = self.inner.stop_tx.subscribe();
            tokio::spawn(async move {
                loop {
                    tokio::select! {
                        item = output.recv() => {
                            let bytes = match item {
                                Some(AttachEvent::Output(c)) => c.to_vec(),
                                // The stream fell behind and attached
                                // again: draw the new screen.
                                Some(AttachEvent::Resynced(snap)) => {
                                    debug!("a PTY viewer fell behind; redrawing the screen");
                                    let mut b = b"\x1b[H\x1b[2J".to_vec();
                                    b.extend_from_slice(&snap.formatted);
                                    b
                                }
                                // The harness exited.
                                None => return,
                            };
                            if out_tx.send(bytes).await.is_err() {
                                return;
                            }
                        },
                        input = in_rx.recv() => match input {
                            Some(PtyInput::Bytes(b)) => {
                                if let Err(e) = runner.write(&b).await {
                                    debug!("PTY input: {e}");
                                }
                            }
                            Some(PtyInput::Resize { rows, cols }) => {
                                if let Err(e) = runner.resize(rows, cols).await {
                                    debug!("PTY resize: {e}");
                                }
                            }
                            None => return,
                        },
                        () = stopped(&mut stop) => return,
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
            let held = || lock(&self.inner.holds).contains_key(&agent);
            Some(match (&slot.runner, slot.phase) {
                (Some(r), _) if r.status() == AgentStatus::Idle && held() => AgentStatus::Blocked,
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

    fn hold_turns(
        &self,
        agent: AgentId,
        hold: Option<TurnHold>,
    ) -> BoxFuture<'_, Result<(), SupervisorError>> {
        Box::pin(async move {
            let Some(hold) = hold else {
                let Some(held) = lock(&self.inner.holds).remove(&agent) else {
                    return Ok(());
                };
                let Some(runner) = self.runner(agent) else {
                    if !held.waiting.is_empty() {
                        warn!(
                            agent = %agent,
                            turns = held.waiting.len(),
                            "the quota hold ended, but the agent does not run; its waiting turns are dropped"
                        );
                    }
                    return Ok(());
                };
                info!(agent = %agent, turns = held.waiting.len(), "the quota hold ended");
                if runner.status() == AgentStatus::Idle {
                    self.emit_status(agent, AgentStatus::Idle, "quota hold ended");
                }
                for text in held.waiting {
                    type_detached(agent, &runner, text);
                }
                self.deliver_pending(agent);
                return Ok(());
            };
            let reason = hold_reason(&hold);
            {
                let mut holds = lock(&self.inner.holds);
                match holds.get_mut(&agent) {
                    Some(h) => h.hold = hold,
                    None => {
                        holds.insert(
                            agent,
                            Held {
                                hold,
                                waiting: Vec::new(),
                            },
                        );
                    }
                }
            }
            info!(agent = %agent, %reason, "quota hold");
            // A working agent ends its turn first; its next idle shows
            // `blocked`.
            if self
                .runner(agent)
                .is_some_and(|r| r.status() == AgentStatus::Idle)
            {
                self.emit_status(agent, AgentStatus::Blocked, reason);
            }
            Ok(())
        })
    }

    fn recover(
        &self,
        agent: Agent,
        events_since: u64,
    ) -> BoxFuture<'_, Result<Recovery, SupervisorError>> {
        Box::pin(async move {
            if !self.is_process_mode() {
                return Ok(Recovery::Gone);
            }
            let id = agent.id;
            let cfg = &self.inner.cfg;
            if live_runner_pid(&cfg.runtime_dir, id).is_none() {
                // The runner ended while the daemon was down. Its exit
                // record decides; with none (an in-process runner, or a
                // runner that never wrote one), launch again.
                migrate_dir(&terrarium_runner::agent_dir(&cfg.state_dir, id));
                let Some(record) = read_exit_record(&cfg.state_dir, id) else {
                    return Ok(Recovery::Gone);
                };
                let status = record.status;
                match record.stopped_by {
                    // The container stopped (docker/entrypoint.sh sent
                    // SIGTERM): the agent did not choose to end, so it
                    // comes back with resume, whatever the exit code.
                    Some(StoppedBy::Container) => {
                        info!(
                            agent = %id,
                            exit = %exit_how(&status),
                            "the container stopped the harness; launching it again with resume"
                        );
                        return Ok(Recovery::Gone);
                    }
                    // The operator (or an agent through the API) stopped
                    // it: it stays stopped.
                    Some(by) if by.is_requested() => {
                        let reason = match by {
                            StoppedBy::Actor(a) => format!("stopped by {a}"),
                            _ => "stopped by the operator".to_owned(),
                        };
                        lock(&self.inner.slots)
                            .entry(id)
                            .or_insert_with(|| Slot::new(agent.clone()))
                            .phase = Phase::Down(AgentStatus::Stopped);
                        info!(agent = %id, %reason, "agent ended");
                        self.emit_status(id, AgentStatus::Stopped, reason);
                        self.targets_down(&agent);
                        return Ok(Recovery::Ended);
                    }
                    Some(_) | None => {}
                }
                if wants_restart(agent.restart, status.success()) {
                    info!(
                        agent = %id,
                        exit = %exit_how(&status),
                        restart = crate::store::agents::restart_str(agent.restart),
                        "the harness ended while the daemon was down; the restart policy starts it again"
                    );
                    return Ok(Recovery::Gone);
                }
                let (s, reason) = final_status(&status);
                let reason = format!("{reason} while the daemon was down");
                lock(&self.inner.slots)
                    .entry(id)
                    .or_insert_with(|| Slot::new(agent.clone()))
                    .phase = Phase::Down(s);
                info!(agent = %id, status = %s, %reason, "agent ended");
                self.emit_status(id, s, reason);
                self.targets_down(&agent);
                return Ok(Recovery::Ended);
            }
            let epoch = match self.claim(&agent, None) {
                Ok(e) => e,
                Err(SupervisorError::AlreadyRunning(_)) => return Ok(Recovery::Attached),
                Err(e) => return Err(e),
            };
            let remote = match self.connect_runner(id, Some(events_since)).await {
                Ok(r) => r,
                Err(e) => {
                    self.release(id, epoch, AgentStatus::Error);
                    return Err(e);
                }
            };
            self.adopt(id, epoch, remote, Some(events_since));
            Ok(Recovery::Attached)
        })
    }

    fn bind(&self, state: &Shared) {
        if self.inner.state.set(Arc::downgrade(state)).is_ok() {
            // Once per daemon start, before `recover` reads the exit
            // records: the layout split of `docs/security-review.md` F1.
            migrate_all(&self.inner.cfg.state_dir);
            self.spawn_hub_listener(state);
        }
    }

    /// Drop every runner handle. A runner process keeps its harness: only
    /// the control connection closes, and the next daemon connects again.
    /// An in-process harness ends.
    fn shutdown(&self) {
        self.inner.shutting_down.store(true, Ordering::SeqCst);
        self.inner.stop_tx.send_replace(true);
        let taken: Vec<(Arc<dyn RunnerHandle>, Option<Runner>)> = lock(&self.inner.slots)
            .values_mut()
            .filter_map(|s| s.runner.take().map(|r| (r, s.local.take())))
            .collect();
        for (r, local) in taken {
            match local {
                Some(l) => {
                    if let Err(e) = l.kill() {
                        warn!(agent = %l.agent(), "cannot end the harness: {e}");
                    }
                }
                None => debug!(agent = %r.agent(), "leaving the runner process running"),
            }
        }
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::*;

    /// The isolation gap of the cross-harness run: each agent's session
    /// files (transcripts, session index, session store) come from its own
    /// harness dir. The sandbox binds them read-write over the session
    /// area of the shared home, after the bind of the home, so no agent
    /// sees the session files of another agent. No bind puts the shared
    /// session area at that path.
    #[tokio::test]
    async fn the_sandbox_binds_the_agent_session_dirs_over_the_shared_home() {
        let tmp = tempfile::tempdir().unwrap();
        let state = tmp.path().join("state");
        let homes: HashMap<HarnessId, PathBuf> =
            [HarnessId::Claude, HarnessId::Codex, HarnessId::Copilot]
                .into_iter()
                .map(|h| (h, tmp.path().join("auth").join(h.to_string())))
                .collect();
        for h in homes.values() {
            std::fs::create_dir_all(h).unwrap();
        }
        let sandbox = SandboxConfig {
            bwrap: PathBuf::from("/usr/bin/bwrap"),
            cli: Some(PathBuf::from("/usr/local/bin/terrarium")),
            ..SandboxConfig::default()
        };
        let sup = RunnerSupervisor::new(RunnerSupervisorConfig::new(
            state.clone(),
            tmp.path().join("run"),
            tmp.path().join("prompts"),
            sandbox,
            homes.clone(),
        ));
        let cases: [(HarnessId, &[&str]); 3] = [
            (
                HarnessId::Claude,
                &[
                    "projects/-home-agent-work",
                    "file-history",
                    "todos",
                    "session-env",
                ],
            ),
            (
                HarnessId::Codex,
                &["sessions", "session_index.jsonl", "shell_snapshots"],
            ),
            (
                HarnessId::Copilot,
                &[
                    "session-state",
                    "session-store.db",
                    "session-store.db-wal",
                    "session-store.db-shm",
                    "logs",
                ],
            ),
        ];
        for (harness, areas) in cases {
            let def: terrarium_core::config::AgentDef = serde_json::from_value(serde_json::json!({
                "name": format!("a-{harness}"), "project": "p", "harness": harness.to_string()
            }))
            .unwrap();
            let agent = Agent::from_def(&def, &state);
            let spec = sup.launch_spec(&agent, None).await.unwrap();
            let a = &spec.args;
            let binds: Vec<(usize, &str, &Path, &Path)> = a
                .iter()
                .enumerate()
                .filter(|(_, f)| f.as_str() == "--bind" || f.as_str() == "--ro-bind")
                .map(|(i, f)| (i, f.as_str(), Path::new(&a[i + 1]), Path::new(&a[i + 2])))
                .collect();
            let shared = &homes[&harness];
            let inside = Path::new(sandbox::SANDBOX_HOME).join(sandbox::home_rel(harness));
            let home_bind = binds
                .iter()
                .find(|b| b.2 == shared.as_path() && b.3 == inside.as_path())
                .unwrap_or_else(|| panic!("{harness}: no bind of the shared home: {a:?}"));
            let own = terrarium_runner::harness_dir(&state, agent.id).join(harness.to_string());
            for area in areas {
                let target = inside.join(area);
                let hits: Vec<_> = binds.iter().filter(|b| b.3 == target).collect();
                assert_eq!(hits.len(), 1, "{harness} {area}: {a:?}");
                let (i, flag, source, _) = *hits[0];
                assert_eq!(flag, "--bind", "{harness} {area}");
                assert_eq!(source, own.join(area), "{harness} {area}");
                assert!(i > home_bind.0, "{harness} {area}: bound before the home");
                assert!(
                    !binds.iter().any(|b| b.2 == shared.join(area)),
                    "{harness} {area}: the shared one is bound"
                );
            }
            if harness == HarnessId::Codex {
                let sqlite = own.join("sqlite");
                let want = format!("sqlite_home=\"{}\"", sqlite.display());
                assert!(a.contains(&want), "{a:?}");
                assert!(
                    binds
                        .iter()
                        .any(|b| b.1 == "--bind" && b.2 == sqlite && b.3 == sqlite),
                    "{a:?}"
                );
            }
        }
    }

    /// An old agent dir with the files at the top, as before the split.
    fn legacy_dir(state: &Path) -> PathBuf {
        let dir = state.join("agents").join("01K8AGENT0000000000000000A");
        std::fs::create_dir_all(dir.join("workspace")).unwrap();
        std::fs::write(dir.join("workspace/notes.txt"), "mine").unwrap();
        std::fs::write(dir.join("events.jsonl"), "").unwrap();
        std::fs::write(dir.join("runner.json"), "{}").unwrap();
        std::fs::create_dir_all(dir.join("claude")).unwrap();
        std::fs::write(dir.join("claude/claude.json"), "{}").unwrap();
        dir
    }

    fn quarantined(state: &Path) -> Vec<PathBuf> {
        std::fs::read_dir(state.join("agents"))
            .unwrap()
            .flatten()
            .map(|e| e.path())
            .filter(|p| p.to_string_lossy().contains(QUARANTINE_MARK))
            .collect()
    }

    #[test]
    fn a_clean_legacy_dir_migrates() {
        let tmp = tempfile::tempdir().unwrap();
        let dir = legacy_dir(tmp.path());
        migrate_all(tmp.path());
        assert!(dir.join("private/events.jsonl").is_file());
        assert!(dir.join("private/runner.json").is_file());
        assert!(dir.join("harness/claude/claude.json").is_file());
        assert!(quarantined(tmp.path()).is_empty());
    }

    /// F11: a `private` symlink that the agent planted into its workspace.
    #[test]
    fn a_planted_private_symlink_sends_the_dir_to_quarantine() {
        let tmp = tempfile::tempdir().unwrap();
        let dir = legacy_dir(tmp.path());
        let target = dir.join("workspace/loot");
        std::fs::create_dir_all(&target).unwrap();
        std::os::unix::fs::symlink(&target, dir.join("private")).unwrap();
        migrate_all(tmp.path());

        let q = quarantined(tmp.path());
        assert_eq!(q.len(), 1, "{q:?}");
        // Nothing moved through the link.
        assert!(!q[0].join("workspace").exists(), "the workspace came back");
        assert!(q[0].join("events.jsonl").is_file());
        assert!(q[0].join("private").is_symlink());
        // A clean dir, with the agent's work, and no link.
        let meta = std::fs::symlink_metadata(&dir).unwrap();
        assert!(meta.is_dir() && !meta.file_type().is_symlink());
        assert!(!dir.join("private").exists());
        assert!(dir.join("workspace/notes.txt").is_file());
        assert!(dir.join("workspace/loot").is_dir());
        assert!(
            std::fs::read_dir(dir.join("workspace/loot"))
                .unwrap()
                .next()
                .is_none()
        );
        // The next launch makes the private dir as a real dir.
        terrarium_runner::create_private_dir(&dir.join("private")).unwrap();
        assert!(!dir.join("private").is_symlink());
        // A second pass leaves both alone.
        migrate_all(tmp.path());
        assert_eq!(quarantined(tmp.path()).len(), 1);
    }

    /// F11: a moved record file, the harness dir, an adapter file, or the
    /// agent dir itself as a symlink.
    #[test]
    fn every_planted_symlink_is_refused() {
        type Plant = dyn Fn(&Path, &Path);
        let cases: &[&Plant] = &[
            &|dir, t| {
                std::fs::remove_file(dir.join("runner.json")).unwrap();
                std::os::unix::fs::symlink(t, dir.join("runner.json")).unwrap();
            },
            &|dir, t| std::os::unix::fs::symlink(t, dir.join("harness")).unwrap(),
            &|dir, t| {
                std::fs::remove_file(dir.join("claude/claude.json")).unwrap();
                std::os::unix::fs::symlink(t, dir.join("claude/claude.json")).unwrap();
            },
            &|dir, t| {
                std::fs::create_dir_all(dir.join("private")).unwrap();
                std::os::unix::fs::symlink(t, dir.join("private/runner-exit.json")).unwrap();
            },
            &|dir, t| {
                std::fs::remove_dir_all(dir.join("workspace")).unwrap();
                std::os::unix::fs::symlink(t, dir.join("workspace")).unwrap();
            },
        ];
        for (i, plant) in cases.iter().enumerate() {
            let tmp = tempfile::tempdir().unwrap();
            let dir = legacy_dir(tmp.path());
            let elsewhere = tmp.path().join("elsewhere");
            std::fs::create_dir_all(&elsewhere).unwrap();
            plant(&dir, &elsewhere);
            assert!(unsafe_layout_entry(&dir).is_some(), "case {i}");
            migrate_dir(&dir);
            assert_eq!(quarantined(tmp.path()).len(), 1, "case {i}");
            assert!(unsafe_layout_entry(&dir).is_none(), "case {i}");
            assert!(
                std::fs::read_dir(&elsewhere).unwrap().next().is_none(),
                "case {i}: something went through the link"
            );
        }

        // The agent dir itself.
        let tmp = tempfile::tempdir().unwrap();
        let real = tmp.path().join("real");
        std::fs::create_dir_all(&real).unwrap();
        std::fs::write(real.join("events.jsonl"), "").unwrap();
        std::fs::create_dir_all(tmp.path().join("agents")).unwrap();
        let dir = tmp.path().join("agents/01K8AGENT0000000000000000B");
        std::os::unix::fs::symlink(&real, &dir).unwrap();
        migrate_all(tmp.path());
        assert!(real.join("events.jsonl").is_file(), "the target moved");
        assert!(!real.join("private").exists());
        let meta = std::fs::symlink_metadata(&dir).unwrap();
        assert!(meta.is_dir() && !meta.file_type().is_symlink());
        assert_eq!(quarantined(tmp.path()).len(), 1);
    }

    #[test]
    fn every_harness_has_an_adapter() {
        let homes = HashMap::new();
        for mode in [SandboxMode::Bwrap, SandboxMode::None] {
            let sandbox = SandboxConfig {
                mode,
                ..SandboxConfig::default()
            };
            let map = default_adapters(&sandbox, &homes);
            for h in [
                HarnessId::Claude,
                HarnessId::Codex,
                HarnessId::Copilot,
                HarnessId::Pi,
            ] {
                assert_eq!(map.get(&h).map(|e| e.adapter.id()), Some(h), "{mode}");
            }
        }
    }

    #[test]
    fn hold_reason_names_the_reset_once() {
        let until = chrono::DateTime::parse_from_rfc3339("2030-01-01T05:00:00Z")
            .unwrap()
            .with_timezone(&chrono::Utc);
        let h = TurnHold {
            until: Some(until),
            reason: "budget used up".into(),
        };
        assert_eq!(
            hold_reason(&h),
            "budget used up; turns wait until 2030-01-01T05:00:00Z"
        );
        let h = TurnHold {
            until: Some(until),
            reason: format!("resets at {}", until.to_rfc3339()),
        };
        assert_eq!(hold_reason(&h), h.reason);
    }

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

    fn card_event(data: serde_json::Value) -> crate::events::StoredEvent {
        serde_json::from_value(serde_json::json!({
            "seq": 1, "id": "e1", "at": "2026-10-25T00:00:00Z", "kind": "card",
            "actor": "agent:01K8AGENT0000000000000000A", "project": "demo",
            "card": "DEM-3", "data": data
        }))
        .unwrap()
    }

    /// Which card events nudge a lead, and the words of the nudge. A
    /// newer change of the same kind on the same card replaces the line.
    #[test]
    fn lead_nudges_name_the_card_and_the_command() {
        let moved = |to: &str, extra: serde_json::Value| {
            let mut d = serde_json::json!({"change": "moved", "to": to, "title": "Fix\nthe parser",
                "actor_name": "demo/bob"});
            for (k, v) in extra.as_object().unwrap() {
                d[k] = v.clone();
            }
            lead_item(&card_event(d))
        };
        let ready = moved("ready", serde_json::json!({})).unwrap();
        assert_eq!(
            ready.line(),
            "New card DEM-3 in ready: Fix…. Run `terrarium card ls --unassigned --column ready`."
        );
        assert!(moved("ready", serde_json::json!({"assignee": "01K8X"})).is_none());
        assert!(moved("in_progress", serde_json::json!({})).is_none());
        assert!(moved("done", serde_json::json!({})).is_none());
        let created = card_event(serde_json::json!({"change": "created", "column": "ready"}));
        assert!(lead_item(&created).is_none());
        let review = moved("review", serde_json::json!({})).unwrap();
        assert_eq!(
            review.line(),
            "DEM-3 moved to review by demo/bob. Run `terrarium card show DEM-3`."
        );
        let blocked = moved(
            "blocked",
            serde_json::json!({"comment": "needs a key\nand more"}),
        )
        .unwrap();
        assert_eq!(
            blocked.line(),
            "DEM-3 moved to blocked by demo/bob: needs a key…. Run `terrarium card show DEM-3`."
        );
        let mut p = Pointer::default();
        p.add_lead(review.clone());
        p.add_lead(blocked);
        p.add_lead(review);
        assert_eq!(p.leads.len(), 2);
        assert!(!p.is_empty());
        assert_eq!(p.text().lines().count(), 2);
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
