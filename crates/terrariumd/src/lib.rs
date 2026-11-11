//! `terrariumd`: the Terrarium daemon, as a library so that the binary and
//! the integration tests start it the same way.
#![warn(clippy::unwrap_used, clippy::expect_used)]

pub mod account_swap;
pub mod accounts;
pub mod actor;
pub mod agent_rpc;
pub mod agent_tools;
pub mod api;
pub mod assets;
pub mod backup;
pub mod bound;
pub mod calendar;
pub mod card_delivery;
pub mod card_dispatcher;
pub mod card_reminder;
pub mod cloud_budget;
pub mod config_repo;
pub mod conflict_fixes;
pub mod conflict_hints;
pub mod db;
pub mod defaults;
pub mod display;
mod docker_tunnel;
pub mod error;
pub mod events;
pub mod executor;
pub mod exedev;
pub mod fleet;
pub mod git;
pub mod git_identity;
pub mod git_proxy;
pub mod github_issues;
pub mod harness_auto_update;
pub mod harness_tools;
pub mod harness_versions;
pub mod hosts;
pub mod idle_alert;
pub mod ingest;
pub mod input_watchdog;
pub mod job_logs;
pub mod jobs;
pub mod leases;
pub mod lifecycle;
pub mod links;
pub mod login_session;
pub mod models;
pub mod monitors;
pub mod owner;
pub mod password;
pub mod pipelines;
pub mod policy;
pub mod probes;
pub mod prompt;
pub mod question_guard;
pub mod quota_notices;
pub mod quota_policy;
pub mod remotes;
pub mod repo_browse;
pub mod required_templates;
pub mod research_helpers;
pub mod reservations;
pub mod reset;
pub mod review_dispatcher;
pub mod rule_script;
pub mod rules_engine;
pub mod rules_verbs;
pub mod run_kill;
pub mod runner_supervisor;
pub mod sandbox;
pub mod scheduler;
pub mod secrets;
pub mod self_update;
pub mod session_archive;
pub mod settings;
pub mod skills;
pub mod spawn;
pub mod spawn_rights;
pub mod spawn_selection;
pub mod ssh_agent;
pub mod store;
pub mod stream_limit;
pub mod supervisor;
pub mod system_prompts;
pub mod target_roots;
pub mod target_test;
pub mod template_files;
pub mod test_results;
pub mod thread_agent;
pub mod thread_close;
pub mod time_param;
pub mod tool_run;
pub mod transcripts;
pub mod trash;
pub mod trouble;
pub mod usage_collector;
pub mod usage_limit;
pub mod waits;
pub mod webhooks;
pub mod workspace;

use std::path::PathBuf;
use std::sync::{Arc, RwLock};
use std::time::Instant;

use anyhow::Context;
use serde_json::{Value, json};
use terrarium_core::config::Definitions;
use terrarium_core::{AgentStatus, Event};
use tokio::sync::broadcast::error::RecvError;
use tokio::task::JoinHandle;
use tracing::{error, info, warn};

use crate::api::{AppState, Shared};
use crate::db::{Db, Tx};
use crate::error::ApiResult;
use crate::events::{EventDraft, EventHub};
use crate::store::agents;
use crate::store::sync::SyncReport;
use crate::supervisor::{Recovery, Supervisor, SupervisorEvent, SupervisorEventKind};

/// What [`start`] needs.
pub struct DaemonConfig {
    pub socket: PathBuf,
    /// The TCP address. It is used only with a token.
    pub listen: Option<String>,
    pub token: Option<String>,
    pub state_dir: PathBuf,
    pub config_dir: PathBuf,
    /// The work root (`TERRARIUM_WORK_DIR`): agent workspaces are
    /// `<work_dir>/<project>/<agent>`.
    pub work_dir: PathBuf,
    /// `TERRARIUM_MAX_TARGETS_PER_PROJECT`: the most targets of one project.
    pub max_targets_per_project: usize,
    /// `TERRARIUM_REPOS_DIR`: the local bare repos. A `lab_repo` with a URL
    /// under it is a local repo for the host agent (its `repos_dir`).
    pub repos_dir: PathBuf,
    pub supervisor: Arc<dyn Supervisor>,
    /// Trust `X-Terrarium-Agent` on the Unix socket. Tests and development
    /// only.
    pub insecure_agent_header: bool,
    /// The TCP address of the artifact serving origin (PLAN §15.2), for
    /// example `0.0.0.0:8081`. `None` or empty: no listener of its own, and
    /// the dashboard port serves `/a/...`.
    pub artifact_listen: Option<String>,
    /// The origins, WebSocket auth, and sessions
    /// ([`settings::Settings::web`]). The routers and the mail code read
    /// these, not the env.
    pub web: settings::WebConfig,
}

/// A running daemon.
pub struct Daemon {
    pub state: Shared,
    /// The `flock` on [`STATE_LOCK`], held until the daemon drops.
    _state_lock: nix::fcntl::Flock<std::fs::File>,
    pub socket: PathBuf,
    pub tcp_addr: Option<std::net::SocketAddr>,
    /// The bound address of the artifact origin, when it listens.
    pub artifact_addr: Option<std::net::SocketAddr>,
    /// The bound address of the git proxy, when it listens.
    pub git_proxy_addr: Option<std::net::SocketAddr>,
    tasks: Vec<JoinHandle<()>>,
}

impl Daemon {
    /// Stop serving and remove the socket. The stored status of each agent
    /// stays. Runner processes keep their harnesses, and the next start
    /// connects to them again; in-process harnesses end, and the next start
    /// launches them again.
    pub fn shutdown(self) {
        self.state.executor.hosts.shutdown();
        self.state.job_queue.stop();
        for t in &self.tasks {
            t.abort();
        }
        self.state.supervisor.shutdown();
        if let Err(e) = std::fs::remove_file(&self.socket) {
            warn!(socket = %self.socket.display(), "cannot remove socket: {e}");
        }
    }

    /// [`Daemon::shutdown`], but it waits until the tasks have ended and no
    /// database call of them still runs, before it lets the state lock go.
    /// For a start again on the same state dir in the same process (tests):
    /// an aborted task ends at its next await, but a database call that it
    /// started runs on to its end on the blocking pool (TER-646).
    pub async fn shutdown_and_wait(mut self) {
        self.state.executor.hosts.shutdown();
        for task in self.state.job_queue.stop() {
            let _ = task.await;
        }
        let tasks = std::mem::take(&mut self.tasks);
        for t in &tasks {
            t.abort();
        }
        for t in tasks {
            let _ = t.await;
        }
        // The connection mutex: a call on the blocking pool holds it.
        let _ = self.state.db.read(|_| Ok(())).await;
        self.shutdown();
    }
}

/// The lock file in the state dir: the running daemon holds an exclusive
/// `flock` on it.
pub const STATE_LOCK: &str = "daemon.lock";

/// Take the exclusive lock of `state_dir`, or fail when another daemon
/// holds it. The CLI restore holds it too, so that no daemon starts on the
/// state dir while it is replaced.
pub fn lock_state_dir(
    state_dir: &std::path::Path,
) -> anyhow::Result<nix::fcntl::Flock<std::fs::File>> {
    use nix::fcntl::{Flock, FlockArg};
    use std::os::unix::fs::OpenOptionsExt as _;
    let path = state_dir.join(STATE_LOCK);
    let file = std::fs::OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .mode(0o600)
        .open(&path)
        .with_context(|| format!("cannot open the state lock {}", path.display()))?;
    match Flock::lock(file, FlockArg::LockExclusiveNonblock) {
        Ok(lock) => Ok(lock),
        Err((_, nix::errno::Errno::EWOULDBLOCK)) => anyhow::bail!(
            "another daemon uses the state dir {} ({} is locked): stop it first",
            state_dir.display(),
            path.display()
        ),
        Err((_, e)) => {
            Err(std::io::Error::from(e)).with_context(|| format!("cannot lock {}", path.display()))
        }
    }
}

fn log_definitions(defs: &Definitions, errors: &[String], dir: &std::path::Path) {
    for e in errors {
        error!("config: {e}");
    }
    info!(
        dir = %dir.display(),
        projects = defs.projects.len(),
        agents = defs.agents.len(),
        targets = defs.targets.len(),
        subscriptions = defs.subscriptions.len(),
        errors = errors.len(),
        "definitions loaded"
    );
}

/// Open the store, sync the definitions, and serve the API. The usage
/// subsystem takes its settings from the environment
/// ([`usage_collector::UsageConfig::from_env`]).
pub async fn start(cfg: DaemonConfig) -> anyhow::Result<Daemon> {
    start_with_executor(cfg, executor::Executor::default()).await
}

/// Start with an explicit executor, avoiding process-global SSH overrides in tests.
pub async fn start_with_executor(
    cfg: DaemonConfig,
    executor: executor::Executor,
) -> anyhow::Result<Daemon> {
    start_with_usage_and_executor(cfg, usage_collector::UsageConfig::from_env(), executor).await
}

/// [`start`] with the settings of usage accounting, quota probes, and login
/// sessions (PLAN §16).
pub async fn start_with_usage(
    cfg: DaemonConfig,
    usage: usage_collector::UsageConfig,
) -> anyhow::Result<Daemon> {
    start_with_usage_and_executor(cfg, usage, executor::Executor::default()).await
}

async fn start_with_usage_and_executor(
    cfg: DaemonConfig,
    usage: usage_collector::UsageConfig,
    executor: executor::Executor,
) -> anyhow::Result<Daemon> {
    std::fs::create_dir_all(&cfg.state_dir)
        .with_context(|| format!("cannot create state dir {}", cfg.state_dir.display()))?;
    // One daemon per state dir, for its whole life: a second one stops
    // here, before it opens the database or clears the run dirs.
    let state_lock = lock_state_dir(&cfg.state_dir)?;
    let db_path = cfg.state_dir.join("terrarium.db");
    let conn = db::open(&db_path)?;
    info!(path = %db_path.display(), "database ready");
    // The run dirs of a daemon that stopped without its drops: before any
    // request or job can make a new one. The lock above makes sure that no
    // live daemon owns them.
    match secrets::RunDirs::clear_stale(&cfg.state_dir) {
        Ok(0) => {}
        Ok(n) => info!(count = n, "removed stale secret run dirs"),
        Err(e) => warn!(error = %e, "cannot clear the secret run dirs"),
    }

    let (defs, errors) = Definitions::load(&cfg.config_dir);
    let errors: Vec<String> = errors.iter().map(ToString::to_string).collect();
    log_definitions(&defs, &errors, &cfg.config_dir);

    let state: Shared = Arc::new(AppState {
        agent_calls: crate::agent_rpc::CallGate::with_probation(Some(
            cfg.state_dir.join("releases/probation"),
        )),
        started: Instant::now(),
        started_at: chrono::Utc::now(),
        db: Db::new(conn, EventHub::default()),
        definitions: RwLock::new(Definitions::default()),
        supervisor: cfg.supervisor.clone(),
        state_dir: cfg.state_dir.clone(),
        config_dir: cfg.config_dir.clone(),
        work_dir: cfg.work_dir.clone(),
        max_targets_per_project: cfg.max_targets_per_project,
        repos_dir: cfg.repos_dir.clone(),
        insecure_agent_header: cfg.insecure_agent_header,
        usage: Arc::new(usage_collector::UsageRuntime::new(usage)),
        executor: Arc::new(executor),
        harness_latest: Arc::default(),
        harness_tools: Arc::default(),
        agent_tools: Arc::default(),
        harness_auto_update: Arc::default(),
        self_update: Arc::default(),
        web: cfg.web.clone(),
        artifact_listen: cfg
            .artifact_listen
            .as_deref()
            .map(str::trim)
            .unwrap_or_default()
            .to_owned(),
        transcripts: Arc::default(),
        operator_auth: Arc::new(password::OperatorAuth::new(&cfg.state_dir)),
        waits: Arc::default(),
        config_repo: Arc::default(),
        lifecycle: Arc::default(),
        tool_rates: Arc::default(),
        lease_sweep: Arc::default(),
        rule_timers: Arc::default(),
        monitors: Arc::default(),
        webhooks: Arc::default(),
        pipelines: Arc::default(),
        git_proxy: Arc::default(),
        job_queue: Arc::default(),
        links: Arc::default(),
        remotes: Arc::default(),
    });
    // The instance ID of the links (TER-61): made at the first start.
    match state.links.instance_id(&state.state_dir) {
        Ok(id) => info!(instance = %id, "instance ID"),
        Err(e) => warn!("no instance ID: {}", e.message),
    }
    // No webhook may carry the API token (docs/security-review.md).
    state.webhooks.set_own_token(cfg.token.as_deref());
    if cfg.insecure_agent_header {
        warn!("--insecure-agent-header: any local caller can act as any agent");
    }
    // A probation daemon serves reads/health before activating background
    // effects that could escape the database snapshot (jobs, webhooks, etc.).
    let mut tasks = Vec::new();
    if state.agent_calls.has_probation_marker() {
        let activation_state = state.clone();
        tasks.push(tokio::spawn(async move {
            activation_state.agent_calls.wait_for_commit().await;
            match activate(activation_state.clone(), defs, errors).await {
                Ok(handles) => {
                    activation_state.agent_calls.activation_ready();
                    let _owned = ActivationTasks(handles);
                    std::future::pending::<()>().await;
                }
                Err(e) => {
                    error!(error = %e, "post-commit daemon activation failed");
                    activation_state.supervisor.shutdown();
                    activation_state
                        .agent_calls
                        .activation_failed(e.to_string());
                }
            }
        }));
    } else {
        tasks.extend(activate(state.clone(), defs, errors).await?);
        state.agent_calls.activation_ready();
    }

    tasks.push(api::serve_unix(state.clone(), &cfg.socket).await?);
    let mut tcp_addr = None;
    match (cfg.token.as_deref().filter(|t| !t.is_empty()), &cfg.listen) {
        (Some(token), Some(listen)) => {
            let (task, addr) = api::serve_tcp(state.clone(), listen, token.to_owned()).await?;
            tasks.push(task);
            state.webhooks.add_own_port(addr.port());
            tcp_addr = Some(addr);
        }
        (None, Some(listen)) => warn!(
            "TERRARIUM_TOKEN is not set: the TCP listener ({listen}) stays off; only the Unix socket serves"
        ),
        _ => {}
    }
    let mut artifact_addr = None;
    if let Some(addr) = Some(state.artifact_listen.as_str()).filter(|a| !a.is_empty()) {
        let listener = tokio::net::TcpListener::bind(addr)
            .await
            .with_context(|| format!("cannot bind the artifact origin {addr}"))?;
        let local = listener.local_addr()?;
        info!(addr = %local, "artifact origin listening on TCP");
        let app = api::artifact_router(state.clone());
        tasks.push(tokio::spawn(async move {
            if let Err(e) = axum::serve(listener, app).await {
                error!("artifact origin failed: {e}");
            }
        }));
        state.webhooks.add_own_port(local.port());
        artifact_addr = Some(local);
    }
    // The git proxy (issue #11): before the first checkout, so a checkout
    // of a proxied repo gets its proxy URL.
    let mut git_proxy_addr = None;
    if let Some(addr) = cfg.web.git_proxy_listen.as_deref() {
        if state.agent_calls.has_probation_marker() {
            let proxy_state = state.clone();
            let addr = addr.to_owned();
            tasks.push(tokio::spawn(async move {
                proxy_state.agent_calls.wait_for_commit().await;
                match git_proxy::serve(proxy_state.clone(), &addr).await {
                    Ok((task, _)) => {
                        let _owned = ActivationTasks(vec![task]);
                        std::future::pending::<()>().await;
                    }
                    Err(e) => {
                        error!(error = %e, "post-commit git proxy failed");
                        proxy_state.agent_calls.activation_failed(e.to_string());
                    }
                }
            }));
        } else {
            let (task, local) = git_proxy::serve(state.clone(), addr).await?;
            tasks.push(task);
            git_proxy_addr = Some(local);
        }
    }
    Ok(Daemon {
        state,
        _state_lock: state_lock,
        socket: cfg.socket,
        tcp_addr,
        artifact_addr,
        git_proxy_addr,
        tasks,
    })
}

// Aborting probation activation also stops the services it owns.
struct ActivationTasks(Vec<JoinHandle<()>>);
impl Drop for ActivationTasks {
    fn drop(&mut self) {
        for task in &self.0 {
            task.abort();
        }
    }
}

async fn activate(
    state: Shared,
    defs: Definitions,
    errors: Vec<String>,
) -> anyhow::Result<Vec<JoinHandle<()>>> {
    // The secret store (PLAN §16b): the master key, then the move of the
    // old credential files.
    state
        .db
        .read_now(|c| {
            secrets::ensure_key(c, &state.state_dir).map_err(crate::error::ApiError::from)
        })
        .map_err(|e| anyhow::anyhow!("secret store: {e}"))?;
    secrets::migrate::run_at_start(&state).await;
    state
        .executor
        .hosts
        .restore(&state.state_dir)
        .map_err(|e| anyhow::anyhow!("host reconnect journal: {e}"))?;
    // The redaction matcher (PLAN §16b.6), from the store as it is now.
    secrets::redact::rebuild(&state).await;
    executor::restore_sessions(&state).map_err(|e| anyhow::anyhow!("exec session restore: {e}"))?;
    // A project named `global` from before the name was reserved (S7): it
    // stays, but it has no secrets of its own.
    if let Ok(true) = state.db.read_now(store::projects::has_reserved_name) {
        warn!(
            "project `global` has a reserved name: its project_admin cannot manage the global \
             secrets, and it has no project secrets; rename it (make a new project and move \
             the agents)"
        );
    }

    let mut owned = ActivationTasks(Vec::new());
    let tasks = &mut owned.0;
    tasks.push(state.executor.hosts.spawn_resume_grace());
    tasks.push(spawn_pump(state.clone()));
    tasks.push(git_identity::spawn_refresh(state.clone()));
    tasks.push(usage_collector::spawn(state.clone()));
    tasks.push(tool_run::spawn_sweep(state.clone()));
    tasks.extend(question_guard::spawn(state.clone()));
    state.supervisor.bind(&state);

    let report = apply_definitions(&state, defs, errors, "system".into())
        .await
        .map_err(|e| anyhow::anyhow!("definition sync failed: {e}"))?;
    info!(
        projects_added = report.projects.added.len(),
        agents_added = report.agents.added.len(),
        agents_removed = report.agents.removed.len(),
        "definitions synced"
    );
    let work_dir = state.work_dir.clone();
    if let Err(e) = std::fs::create_dir_all(&work_dir) {
        warn!(dir = %work_dir.display(), "cannot create the work root: {e}");
    }
    let moved = state
        .db
        .write("system", move |t| {
            store::agents::set_workspaces(t, &work_dir)
        })
        .await
        .map_err(|e| anyhow::anyhow!("workspace paths: {e}"))?;
    if moved > 0 {
        info!(agents = moved, work_dir = %state.work_dir.display(),
              "agent workspaces now live under the work root; each moves at its next start");
    }
    reconcile(&state)
        .await
        .map_err(|e| anyhow::anyhow!("reconcile failed: {e}"))?;

    warn_direct_push_deny(&state).await;
    tasks.push(spawn_prune_timer(state.clone()));
    tasks.push(spawn_artifact_sweep(state.clone()));
    tasks.push(target_roots::spawn(state.clone()));
    tasks.push(leases::spawn_timer(&state));
    tasks.push(reservations::spawn_timer(&state));
    tasks.push(scheduler::spawn(state.clone()));
    // The board rule engine (TER-168): runs the outbox rows that the last
    // daemon left pending, then the new events.
    tasks.push(rules_engine::spawn(state.clone()));
    // The webhook deliveries (docs/api.md §19.5): the pending rows of the
    // old daemon are due again.
    tasks.push(webhooks::spawn(state.clone()));
    tasks.push(api::spawn_remote_checks(state.clone()));
    tasks.extend(waits::spawn(&state));
    // The job queue (TER-71): fails the runs that the last daemon lost.
    tasks.push(jobs::spawn(&state));
    // After the reconcile: the agents with queued work wake.
    tasks.extend(lifecycle::spawn(&state));
    // The card files of each assigned card, in the workspace (§8.4).
    tasks.push(card_delivery::spawn(&state));
    // The agents bound to cards: park, unpark, and end (TER-74).
    tasks.push(bound::spawn(&state));
    // The exe.dev sweep (TER-294): VMs past their life, unheld, or left.
    tasks.push(exedev::spawn(&state));
    tasks.extend(start_usage(&state).await);
    Ok(std::mem::take(&mut owned.0))
}

/// Log a warning for each repo with `push_deny` whose checkouts go
/// straight to the forge, where `push_deny` is only the client hook
/// (review P8).
async fn warn_direct_push_deny(state: &Shared) {
    let proxy_on = state.git_proxy.is_on();
    let found = state.db.read(store::repos::all).await;
    match found {
        Ok(all) => {
            for r in all {
                if let Some(why) = git_proxy::direct_reason(&r, proxy_on) {
                    warn!(repo = %format!("{}/{}", r.project, r.name), "{why}");
                }
            }
        }
        Err(e) => warn!("cannot read the repos: {e}"),
    }
}

/// Start the usage subsystem (PLAN §16): the probe scheduler, the quota
/// policy timer, the usage-limit listener, and the start-up login check. Login sessions that a
/// daemon stop left open become `expired`.
async fn start_usage(state: &Shared) -> Vec<JoinHandle<()>> {
    if let Err(e) = state
        .db
        .write("system", store::subscriptions::expire_open_logins)
        .await
    {
        warn!("cannot expire old login sessions: {e}");
    }
    vec![
        spawn_account_migration(state.clone()),
        probes::spawn_scheduler(state.clone()),
        quota_policy::spawn(state.clone()),
        usage_limit::spawn(state.clone()),
    ]
}

/// Move the shared login of each harness into the account of its
/// subscription (PLAN §16.3, "The migration"): at the start, and then on
/// each probe tick while a harness still has one. A harness migrates only
/// while none of its agents runs (`Supervisor::with_quiet_harness`): a
/// running agent reads the shared file. Each change writes a
/// `credentials.migrate` audit row and a `credentials.migrated` event; the
/// same result logs once.
///
/// The start-up login check (`login_session::detect_on_start`) runs after
/// the first pass, so it checks a moved login in its account and never
/// sees the shared home between the two.
fn spawn_account_migration(state: Shared) -> JoinHandle<()> {
    tokio::spawn(async move {
        let mut last: std::collections::HashMap<terrarium_core::HarnessId, String> =
            std::collections::HashMap::new();
        let mut first = true;
        loop {
            let subs = match state.db.read(store::subscriptions::list).await {
                Ok(l) => l,
                Err(e) => {
                    warn!("the account migration cannot list subscriptions: {e}");
                    tokio::time::sleep(state.usage.config.probe_tick).await;
                    continue;
                }
            };
            let mut moved = false;
            for h in accounts::harnesses() {
                let home = state.usage.config.home(h);
                let (dir, sup, list) = (
                    state.state_dir.clone(),
                    state.supervisor.clone(),
                    subs.clone(),
                );
                let run = tokio::task::spawn_blocking(move || {
                    if !accounts::migration_pending(h, &home) {
                        return Ok(None);
                    }
                    let mut out = None;
                    let ran = sup.with_quiet_harness(h, &mut || {
                        out = Some(accounts::migrate(&dir, h, &home, &list));
                    });
                    if ran { Ok(out) } else { Err(()) }
                })
                .await;
                let note = match &run {
                    Ok(Ok(None)) => String::new(),
                    Ok(Err(())) => "waiting".to_owned(),
                    Ok(Ok(Some(m))) => serde_json::to_string(m).unwrap_or_default(),
                    Err(e) => format!("failed: {e}"),
                };
                if last.get(&h) == Some(&note) {
                    continue;
                }
                last.insert(h, note);
                match run {
                    Ok(Ok(Some(m))) if !m.is_empty() => {
                        moved |= !m.moved.is_empty();
                        if m.moved.is_empty() && !m.pi_unlinked {
                            warn!(harness = %h, skipped = ?m.skipped, "the shared login stays in the shared home");
                        } else {
                            info!(harness = %h, moved = ?m.moved, pi_unlinked = m.pi_unlinked,
                                  backups = m.backups.len(), "moved the shared login into its account");
                        }
                        let detail = serde_json::json!(m);
                        let summary = if m.pi_unlinked {
                            "pi: the shared auth.json is a plain file again (pi uses API keys only)"
                                .to_owned()
                        } else {
                            format!(
                                "{h}: shared login moved to {}",
                                if m.moved.is_empty() {
                                    "no account".to_owned()
                                } else {
                                    m.moved.join(", ")
                                }
                            )
                        };
                        let _ = state
                            .db
                            .write("system", move |t| {
                                t.audit("credentials.migrate", h.as_str(), None, detail.clone())?;
                                t.emit(events::EventDraft::new(
                                    "credentials.migrated",
                                    summary,
                                    detail,
                                ))
                            })
                            .await;
                    }
                    Ok(Err(())) => info!(
                        harness = %h,
                        "the shared {h} login moves into its account when no {h} agent runs"
                    ),
                    Err(e) => warn!(harness = %h, "the account migration failed: {e}"),
                    _ => {}
                }
            }
            // The default account of each harness (PLAN §16.3): the
            // backfill at the first pass, and the first account that the
            // migration moves.
            if first || moved {
                api::settle_account_defaults(&state, None).await;
            }
            if std::mem::take(&mut first) && state.usage.config.detect_on_start {
                let s = state.clone();
                tokio::spawn(async move {
                    if let Err(e) = login_session::detect_on_start(&s).await {
                        warn!("start-up login check failed: {e}");
                    }
                });
            }
            tokio::time::sleep(state.usage.config.probe_tick).await;
        }
    })
}

/// Remove the artifact blobs no row owns, once at start
/// ([`api::sweep_orphans`]).
fn spawn_artifact_sweep(state: Shared) -> JoinHandle<()> {
    tokio::spawn(async move {
        match api::sweep_orphans(&state).await {
            Ok(n) if n > 0 => info!(entries = n, "artifact orphan sweep done"),
            Ok(_) => {}
            Err(e) => warn!("artifact orphan sweep failed: {e}"),
        }
    })
}

/// Run the artifact retention prune (PLAN §15.2): first one hour after the
/// start, then every `[artifacts] prune_interval_hours` (default 24). The
/// same timer prunes the trash of deleted agents (`TERRARIUM_TRASH_DAYS`,
/// [`trash::prune_trash`]), the plain backups of the moved credentials
/// after the same days ([`secrets::migrate::prune_backups`]), the session archive of deleted agents
/// ([`session_archive::prune_archive`]), the revoked sessions past their `exp`, and the
/// schedule runs past the newest 500 of each schedule, and the webhook
/// delivery IDs older than 7 days, the turn attachments older than 7
/// days ([`api::attachments::prune`]), and the removed card files past
/// the trash days with their blobs ([`api::card_files::prune`]), and the
/// agent write answers past a day ([`agent_rpc::prune_answers`]).
fn spawn_prune_timer(state: Shared) -> JoinHandle<()> {
    tokio::spawn(async move {
        let mut wait = std::time::Duration::from_secs(3600);
        loop {
            tokio::time::sleep(wait).await;
            match api::run_prune(&state, chrono::Utc::now(), false, "system".into()).await {
                Ok(r) => info!(blobs = r.blobs.len(), "artifact prune done"),
                Err(e) => warn!("artifact prune failed: {e}"),
            }
            let dir = state.state_dir.clone();
            let max_age = std::time::Duration::from_secs(trash::trash_days() * 86_400);
            match tokio::task::spawn_blocking(move || {
                trash::prune_trash(&dir, std::time::SystemTime::now(), max_age)
            })
            .await
            {
                Ok(Ok(removed)) if !removed.is_empty() => {
                    info!(entries = removed.len(), "trash prune done");
                }
                Ok(Ok(_)) => {}
                Ok(Err(e)) => warn!("trash prune failed: {e}"),
                Err(e) => warn!("trash prune task failed: {e}"),
            }
            // The plain backups of the moved credentials (PLAN §16b.7).
            let dir = state.state_dir.clone();
            if let Err(e) =
                tokio::task::spawn_blocking(move || secrets::migrate::prune_backups_now(&dir)).await
            {
                warn!("credential backup prune task failed: {e}");
            }
            let work = state.work_dir.clone();
            match tokio::task::spawn_blocking(move || {
                api::attachments::prune(&work, std::time::SystemTime::now())
            })
            .await
            {
                Ok(n) if n > 0 => info!(files = n, "attachment prune done"),
                Ok(_) => {}
                Err(e) => warn!("attachment prune task failed: {e}"),
            }
            match api::card_files::prune(&state, std::time::SystemTime::now()).await {
                Ok((rows, blobs)) if rows + blobs > 0 => {
                    info!(rows, blobs, "card file prune done");
                }
                Ok(_) => {}
                Err(e) => warn!("card file prune failed: {e}"),
            }
            let dir = state.state_dir.clone();
            let max_age = std::time::Duration::from_secs(session_archive::archive_days() * 86_400);
            match tokio::task::spawn_blocking(move || {
                session_archive::prune_archive(&dir, std::time::SystemTime::now(), max_age)
            })
            .await
            {
                Ok(Ok(removed)) if !removed.is_empty() => {
                    info!(agents = removed.len(), "session archive prune done");
                }
                Ok(Ok(_)) => {}
                Ok(Err(e)) => warn!("session archive prune failed: {e}"),
                Err(e) => warn!("session archive prune task failed: {e}"),
            }
            let now = chrono::Utc::now().timestamp();
            if let Err(e) = state
                .db
                .write("system", move |t| {
                    t.execute("DELETE FROM revoked_sessions WHERE exp <= ?1", [now])?;
                    Ok(())
                })
                .await
            {
                warn!("cannot prune the revoked sessions: {e}");
            }
            match state
                .db
                .write("system", |t| {
                    store::schedules::prune_runs(t, store::schedules::KEEP_RUNS)
                })
                .await
            {
                Ok(n) if n > 0 => info!(runs = n, "schedule run prune done"),
                Ok(_) => {}
                Err(e) => warn!("cannot prune the schedule runs: {e}"),
            }
            match state
                .db
                .write("system", |t| {
                    store::event_sources::prune_deliveries(t, chrono::Utc::now())
                })
                .await
            {
                Ok(n) if n > 0 => info!(deliveries = n, "webhook delivery prune done"),
                Ok(_) => {}
                Err(e) => warn!("cannot prune the webhook deliveries: {e}"),
            }
            match agent_rpc::prune_answers(&state.db, chrono::Utc::now()).await {
                Ok(n) if n > 0 => info!(answers = n, "agent write answer prune done"),
                Ok(_) => {}
                Err(e) => warn!("cannot prune the agent write answers: {e}"),
            }
            let hours = store::artifacts::context(&state.state_dir, &state.config_dir)
                .map(|c| c.limits.prune_interval_hours)
                .unwrap_or(24)
                .max(1);
            wait = std::time::Duration::from_secs(hours * 3600);
        }
    })
}

/// Sync definitions into the store, keep them for `/api/health`, and stop
/// the file agents whose files are gone and move their dirs to the trash.
pub async fn apply_definitions(
    state: &Shared,
    defs: Definitions,
    errors: Vec<String>,
    actor: String,
) -> ApiResult<SyncReport> {
    let work_dir = state.work_dir.clone();
    let d = defs.clone();
    let mut report = state
        .db
        .write(actor, move |t| store::sync::sync(t, &d, &work_dir))
        .await?;
    report.errors.splice(0..0, errors);
    // `openai-compatible` rows that migration 46 renamed get their server
    // from the key file, before a file sync needs it. Idempotent.
    let dir = state.state_dir.clone();
    match state
        .db
        .write("system", move |t| {
            store::subscriptions::migrate_openai_compatible(t, &dir)
        })
        .await
    {
        Ok(names) if !names.is_empty() => {
            info!(subscriptions = ?names, "openai-compatible subscriptions are now selfhosted");
        }
        Ok(_) => {}
        Err(e) => warn!("cannot migrate the openai-compatible subscriptions: {e}"),
    }
    let subscriptions = defs.subscriptions.clone();
    let changes = state
        .db
        .write("system", move |t| {
            store::subscriptions::sync_files(t, &subscriptions)
        })
        .await?;
    report.errors.extend(changes.errors);
    // The stream limits of the self-hosted subscriptions.
    stream_limit::refresh(state).await;
    let target_defs = defs.targets.clone();
    let (_, target_errors) = state
        .db
        .write("system", move |t| {
            store::targets::sync_files(t, &target_defs)
        })
        .await?;
    report.errors.extend(target_errors);
    // The tool files, after the targets and the secrets that they name.
    let tool_defs = defs.tools.clone();
    let tool_dirs: Vec<std::path::PathBuf> = state
        .harness_tools
        .config()
        .map(|c| vec![c.bin_dir()])
        .unwrap_or_default();
    let (_, tool_errors) = state
        .db
        .write("system", move |t| {
            let refs: Vec<&std::path::Path> =
                tool_dirs.iter().map(std::path::PathBuf::as_path).collect();
            store::tools::sync_files(t, &tool_defs, &refs)
        })
        .await?;
    report.errors.extend(tool_errors);
    agent_tools::refresh(state).await;
    // Schedules of the project files, after the agents and targets that
    // they name (docs/api.md §19), and the templates (§21).
    // The `[[templates]]` of a project file are an import into the
    // template files of the config repo (`crate::template_files`).
    let project_defs = defs.projects.clone();
    let gone = report.projects.removed.clone();
    let repo = template_files::repo_dir(state);
    let schedule_errors = state
        .db
        .write("system", move |t| {
            let now = chrono::Utc::now();
            let mut errors = Vec::new();
            for p in &project_defs {
                if store::projects::find(t, &p.name)?.is_some() {
                    errors.extend(store::schedules::sync_file(t, &p.name, &p.schedules, now)?);
                    errors.extend(store::templates::import_file(
                        t,
                        &p.name,
                        &p.templates,
                        repo.as_deref(),
                    )?);
                }
            }
            for p in &gone {
                store::schedules::detach_file(t, p)?;
                store::templates::detach_file(t, p)?;
            }
            Ok(errors)
        })
        .await?;
    report.errors.extend(schedule_errors);
    // The board rules files (`projects/<p>.rules.wcl`, TER-166), after the
    // projects and their boards: the check reads the columns. The parse
    // runs off the DB lock: plan under the lock, parse, then store.
    let rules_defs = defs.rules.clone();
    let (plans, mut rule_errors) = state
        .db
        .read(move |c| {
            let mut plans = Vec::new();
            let mut errors = Vec::new();
            for f in &rules_defs {
                if store::projects::find(c, &f.project)?.is_none() {
                    errors.push(format!(
                        "{}: no project `{}`",
                        store::rules::file_path(&f.project),
                        f.project
                    ));
                    continue;
                }
                plans.extend(store::rules::plan_file(c, &f.project, &f.text)?);
            }
            Ok((plans, errors))
        })
        .await?;
    let mut checked = Vec::new();
    for plan in plans {
        let file = store::rules::file_path(&plan.project);
        match store::rules::parse_off(plan.text.clone(), file, plan.ctx.clone()).await? {
            Ok(set) => checked.push((plan, set)),
            // A bad file keeps the last good rules.
            Err(e) => rule_errors.extend(e.to_string().lines().map(str::to_owned)),
        }
    }
    let present: Vec<String> = defs.rules.iter().map(|f| f.project.clone()).collect();
    let stored = state
        .db
        .write("system", move |t| {
            let mut errors = Vec::new();
            for (plan, set) in &checked {
                errors.extend(store::rules::store_file(t, plan, set)?);
            }
            // A rules file that is gone: its rules become API rules.
            for p in store::rules::file_projects(t)? {
                if !present.contains(&p) {
                    store::rules::file_gone(t, &p)?;
                }
            }
            Ok(errors)
        })
        .await?;
    rule_errors.extend(stored);
    // TER-243: merge_queue into the wait overrides of older migrations.
    rule_errors.extend(store::rules::add_merge_queue(&state.db).await);
    report.errors.extend(rule_errors);
    // The template files, before the fleets: a fleet agent may name one.
    match template_files::refresh(state).await {
        Ok(r) => report.errors.extend(r.errors),
        Err(e) => report
            .errors
            .push(format!("cannot load the template files: {}", e.message)),
    }
    report.errors.extend(fleet::apply_config_dir(state).await);
    state.set_definitions(defs);
    // Like `DELETE /api/agents/{id}`: stop, then the dir to the trash.
    for id in &report.removed_agent_ids {
        if let Ok(id) = id.parse() {
            trash::retire_agent(state, id).await;
        }
    }
    Ok(report)
}

/// Set an agent's status, with a `status` event. A stop or an error ends
/// the current session. An `idle` agent with an open wait is `waiting`
/// (PLAN §12d); the change then checks the waits on the agent.
pub fn apply_status(
    t: &mut Tx<'_>,
    id: &str,
    status: AgentStatus,
    reason: Option<&str>,
) -> ApiResult<()> {
    apply_status_at(t, id, status, reason, 0)
}

/// The status that the store shows for the runner status `status` of
/// agent `id`: `idle` with an open wait is `waiting`.
pub fn shown_status(
    conn: &rusqlite::Connection,
    id: &str,
    status: AgentStatus,
) -> ApiResult<AgentStatus> {
    if !status.is_resting() {
        return Ok(status);
    }
    Ok(status.with_waits(store::waits::open_count(conn, id)?))
}

/// [`apply_status`] in a chain of wait ends, `depth` deep.
pub fn apply_status_at(
    t: &mut Tx<'_>,
    id: &str,
    status: AgentStatus,
    reason: Option<&str>,
    depth: u8,
) -> ApiResult<()> {
    let status = shown_status(t, id, status)?;
    let a = agents::get(t, id)?;
    // The runner says `idle` again for an agent that waits (the end of a
    // quota hold, for example): no change.
    if status == AgentStatus::Waiting && a.agent.status == AgentStatus::Waiting {
        return Ok(());
    }
    let from = agents::set_status(t, id, status, reason)?;
    if matches!(status, AgentStatus::Stopped | AgentStatus::Error) {
        let outcome = match status {
            AgentStatus::Error => "error",
            _ => reason.unwrap_or("stopped"),
        };
        agents::end_session(t, id, outcome)?;
    }
    let session = agents::current_session(t, id)?;
    let waits = store::waits::open_count(t, id)?;
    let mut data = json!({"status": status, "from": from, "reason": reason, "waits": waits});
    if let Some(on) = store::waits::waiting_on(t, id)? {
        data["waiting_on"] = json!(on);
    }
    t.emit(
        EventDraft::new(
            "status",
            format!("{}: {from} -> {status}", a.agent.name),
            data,
        )
        .agent(id, a.agent.name.clone())
        .project(a.agent.project.clone())
        .session(session),
    )?;
    store::waits::after_status(t, id, depth)?;
    Ok(())
}

fn summary(name: &str, kind: &str, data: &Value) -> String {
    let short = |v: &Value| {
        let s = v.as_str().unwrap_or_default();
        let mut out: String = s.chars().take(60).collect();
        if s.chars().count() > 60 {
            out.push('…');
        }
        out
    };
    match kind {
        "turn_start" => format!("{name}: turn: {}", short(&data["prompt"])),
        "tool_use" => format!("{name}: tool {}", short(&data["tool"])),
        "notification" | "question" | "warning" | "usage_limit" => {
            format!(
                "{name}: {}",
                short(data.get("message").unwrap_or(&data["question"]))
            )
        }
        "exit" => format!("{name}: exit {}", data["code"]),
        _ => format!("{name}: {kind}"),
    }
}

/// Write one supervisor event to the store. A runner event (with a seq)
/// goes in once: a seq at or below the stored `runner_seq` is a double from
/// a replay, and the pump drops it.
fn record(t: &mut Tx<'_>, ev: &SupervisorEvent) -> ApiResult<()> {
    let id = ev.agent.to_string();
    if let Some(seq) = ev.seq
        && !agents::advance_runner_seq(t, &id, seq)?
    {
        return Ok(());
    }
    let event = match &ev.kind {
        SupervisorEventKind::Status { status, reason } => {
            return apply_status(t, &id, *status, reason.as_deref());
        }
        SupervisorEventKind::Harness(Event::StatusChanged { to, reason, .. }) => {
            // The runner's input-box watchdog: mail the leads (TER-208).
            // A failed mail must not lose the status change. The savepoint
            // keeps all of the mails and the event, or none of them (TER-224).
            if input_watchdog::is_stuck(*to, reason.as_deref())
                && let Err(e) = t.savepoint(|t| input_watchdog::notify(t, &id))
            {
                warn!(agent = %id, "cannot mail the leads about the input box: {e}");
            }
            // The runner's first record is `stopped -> starting` ("spawned
            // <program>"), but the supervisor has already set `starting`.
            // A change to the status the agent already has is no change: it
            // goes in as a `system` notice, not as `starting -> starting`.
            let a = agents::get(t, &id)?;
            let shown = shown_status(t, &id, *to)?;
            if a.agent.status == shown && !matches!(to, AgentStatus::Stopped | AgentStatus::Error) {
                let message = reason.clone().unwrap_or_else(|| format!("status {to}"));
                let session = agents::current_session(t, &id)?;
                t.emit(
                    EventDraft::new(
                        "system",
                        format!("{}: {message}", a.agent.name),
                        json!({"message": message, "status": to}),
                    )
                    .agent(id, a.agent.name.clone())
                    .project(a.agent.project.clone())
                    .session(session),
                )?;
                return Ok(());
            }
            return apply_status(t, &id, *to, reason.as_deref());
        }
        SupervisorEventKind::NewSession { outcome, resume } => {
            agents::reopen_session(t, &id, outcome, resume.as_ref().map(|h| h.0.as_str()))?;
            return Ok(());
        }
        SupervisorEventKind::Harness(e) => e,
    };
    let a = agents::get(t, &id)?;
    if let Event::SessionStarted {
        harness_session, ..
    } = event
    {
        agents::set_harness_session(t, &id, &harness_session.0)?;
    }
    if let Event::TurnStart { .. } = event {
        // Auto compact runs only after a turn (PLAN §18).
        agents::note_turn(t, &id)?;
    }
    let mut data = serde_json::to_value(event)?;
    let kind = data
        .as_object_mut()
        .and_then(|m| m.remove("type"))
        .and_then(|v| v.as_str().map(str::to_owned))
        .unwrap_or_else(|| "harness".into());
    let session = agents::current_session(t, &id)?;
    t.emit(
        EventDraft::new(kind.clone(), summary(&a.agent.name, &kind, &data), data)
            .agent(id, a.agent.name.clone())
            .project(a.agent.project.clone())
            .session(session),
    )?;
    Ok(())
}

/// Feed supervisor events into the store.
fn spawn_pump(state: Shared) -> JoinHandle<()> {
    let mut rx = state.supervisor.subscribe();
    tokio::spawn(async move {
        loop {
            let ev = match rx.recv().await {
                Ok(ev) => ev,
                Err(RecvError::Lagged(n)) => {
                    warn!("the event pump missed {n} supervisor events");
                    continue;
                }
                Err(RecvError::Closed) => return,
            };
            // An idle stop and a pause give the `stopped` status their
            // reason (`crate::lifecycle`).
            let ev = state.lifecycle.with_stop_reason(ev);
            let provenance_agent =
                matches!(&ev.kind, SupervisorEventKind::NewSession { .. }).then_some(ev.agent);
            let ended = matches!(
                &ev.kind,
                SupervisorEventKind::Status {
                    status: AgentStatus::Stopped | AgentStatus::Error,
                    ..
                } | SupervisorEventKind::Harness(Event::StatusChanged {
                    to: AgentStatus::Stopped | AgentStatus::Error,
                    ..
                })
            )
            .then_some(ev.agent);
            let result = state.db.write("system", move |t| record(t, &ev)).await;
            if let Err(e) = result {
                warn!("cannot record a supervisor event: {e}");
            }
            if let Some(id) = provenance_agent {
                let state = state.clone();
                tokio::spawn(async move {
                    if let Err(e) = git_identity::refresh(&state, &id.to_string()).await {
                        warn!(agent = %id, "cannot refresh git provenance: {e}");
                    }
                });
            }
            // A parked agent that stops frees its owner's place (TER-111).
            if let Some(agent) = ended {
                let state = state.clone();
                tokio::spawn(async move { bound::on_stopped(&state, agent).await });
            }
        }
    })
}

/// After a start, reconcile every agent that the store thinks is live (or
/// restarting) but the supervisor does not run (PLAN §18):
///
/// 1. [`Supervisor::recover`] connects to a live runner process. The
///    harness keeps running, and the events that the daemon missed come
///    from the stored `runner_seq` on.
/// 2. When the harness ended while the daemon was down, the supervisor
///    applies the restart policy to its exit status. With no restart, it
///    reports `stopped` or `error`.
/// 3. Else the daemon launches the agent again, with `--resume` of its
///    last harness session. An agent that cannot start gets `error`, with
///    the reason.
///
/// A paused agent is never launched again (TER-45): a pause just before a
/// restart can leave a live status in the store, because the stop of the
/// pause writes `stopped` later. With no runner it gets `stopped` with the
/// reason `paused`; a runner that still lives is stopped.
async fn reconcile(state: &Shared) -> ApiResult<()> {
    // The default plan accounts first: an agent with no subscription
    // launches on its harness's default (PLAN §16.3). Idempotent; the
    // migration loop settles again after it moves a login.
    api::settle_account_defaults(state, None).await;
    let all = state.db.read(agents::all_statuses).await?;
    for (id, stored) in all {
        if !stored.is_live() && stored != AgentStatus::Restarting {
            continue;
        }
        let Ok(agent_id) = id.parse() else { continue };
        let live = state
            .supervisor
            .status(agent_id)
            .await
            .is_some_and(|s| s.is_live() || s == AgentStatus::Restarting);
        if live {
            continue;
        }
        let key = id.clone();
        let (view, since) = state
            .db
            .read(move |c| Ok((agents::get(c, &key)?, agents::runner_seq(c, &key)?)))
            .await?;
        let paused = view.paused;
        match state.supervisor.recover(view.agent, since).await {
            Ok(Recovery::Attached) if paused => {
                info!(agent = %id, "connected to the runner of a paused agent; stopping it");
                state
                    .lifecycle
                    .expect_stop(agent_id, lifecycle::REASON_PAUSED);
                if let Err(e) = state.supervisor.stop(agent_id).await {
                    state.lifecycle.forget_stop(agent_id);
                    warn!(agent = %id, "cannot stop the runner of a paused agent: {e}");
                }
                continue;
            }
            Ok(Recovery::Attached) => {
                info!(agent = %id, since, "connected to the live runner again");
                continue;
            }
            Ok(Recovery::Gone) if paused => {
                info!(agent = %id, "a paused agent stays stopped after the daemon start");
                let key = id.clone();
                state
                    .db
                    .write("system", move |t| {
                        apply_status(
                            t,
                            &key,
                            AgentStatus::Stopped,
                            Some(lifecycle::REASON_PAUSED),
                        )
                    })
                    .await?;
                continue;
            }
            Ok(Recovery::Ended) => continue,
            Ok(Recovery::Gone) => {}
            Err(e) => {
                warn!(agent = %id, "cannot connect to the runner after daemon start: {e}");
                let reason = format!("cannot connect to the runner after daemon start: {e}");
                state
                    .db
                    .write("system", move |t| {
                        apply_status(t, &id, AgentStatus::Error, Some(&reason))
                    })
                    .await?;
                continue;
            }
        }
        let key = id.clone();
        let (view, resume) = state
            .db
            .write("system", move |t| {
                let resume = agents::begin_resume(t, &key, "daemon_restart")?;
                Ok((agents::get(t, &key)?, resume))
            })
            .await?;
        info!(agent = %id, name = %view.agent.name, resume = ?resume, "launching again after daemon start");
        if let Err(e) = state.supervisor.start(view.agent, resume).await {
            warn!(agent = %id, "cannot launch again after daemon start: {e}");
            let reason = format!("cannot launch again after daemon start: {e}");
            state
                .db
                .write("system", move |t| {
                    apply_status(t, &id, AgentStatus::Error, Some(&reason))
                })
                .await?;
        }
    }
    Ok(())
}
