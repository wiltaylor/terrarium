//! `terrariumd`: the Terrarium daemon, as a library so that the binary and
//! the integration tests start it the same way.
#![warn(clippy::unwrap_used, clippy::expect_used)]

pub mod actor;
pub mod api;
pub mod assets;
pub mod db;
pub mod error;
pub mod events;
pub mod policy;
pub mod settings;
pub mod store;
pub mod supervisor;

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
use crate::supervisor::{Supervisor, SupervisorError, SupervisorEvent, SupervisorEventKind};

/// What [`start`] needs.
pub struct DaemonConfig {
    pub socket: PathBuf,
    /// The TCP address. It is used only with a token.
    pub listen: Option<String>,
    pub token: Option<String>,
    pub state_dir: PathBuf,
    pub config_dir: PathBuf,
    pub supervisor: Arc<dyn Supervisor>,
}

/// A running daemon.
pub struct Daemon {
    pub state: Shared,
    pub socket: PathBuf,
    pub tcp_addr: Option<std::net::SocketAddr>,
    tasks: Vec<JoinHandle<()>>,
}

impl Daemon {
    /// Stop serving and remove the socket.
    pub fn shutdown(self) {
        for t in &self.tasks {
            t.abort();
        }
        if let Err(e) = std::fs::remove_file(&self.socket) {
            warn!(socket = %self.socket.display(), "cannot remove socket: {e}");
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

/// Open the store, sync the definitions, and serve the API.
pub async fn start(cfg: DaemonConfig) -> anyhow::Result<Daemon> {
    std::fs::create_dir_all(&cfg.state_dir)
        .with_context(|| format!("cannot create state dir {}", cfg.state_dir.display()))?;
    let db_path = cfg.state_dir.join("terrarium.db");
    let conn = db::open(&db_path)?;
    info!(path = %db_path.display(), "database ready");

    let (defs, errors) = Definitions::load(&cfg.config_dir);
    let errors: Vec<String> = errors.iter().map(ToString::to_string).collect();
    log_definitions(&defs, &errors, &cfg.config_dir);

    let state: Shared = Arc::new(AppState {
        started: Instant::now(),
        started_at: chrono::Utc::now(),
        db: Db::new(conn, EventHub::default()),
        definitions: RwLock::new(Definitions::default()),
        supervisor: cfg.supervisor.clone(),
        state_dir: cfg.state_dir.clone(),
        config_dir: cfg.config_dir.clone(),
    });

    let mut tasks = Vec::new();
    tasks.push(spawn_pump(state.clone()));

    let report = apply_definitions(&state, defs, errors, "system".into())
        .await
        .map_err(|e| anyhow::anyhow!("definition sync failed: {e}"))?;
    info!(
        projects_added = report.projects.added.len(),
        agents_added = report.agents.added.len(),
        agents_removed = report.agents.removed.len(),
        "definitions synced"
    );
    reconcile(&state)
        .await
        .map_err(|e| anyhow::anyhow!("reconcile failed: {e}"))?;

    tasks.push(api::serve_unix(state.clone(), &cfg.socket).await?);
    let mut tcp_addr = None;
    match (cfg.token.as_deref().filter(|t| !t.is_empty()), &cfg.listen) {
        (Some(token), Some(listen)) => {
            let (task, addr) = api::serve_tcp(state.clone(), listen, token.to_owned()).await?;
            tasks.push(task);
            tcp_addr = Some(addr);
        }
        (None, Some(listen)) => warn!(
            "TERRARIUM_TOKEN is not set: the TCP listener ({listen}) stays off; only the Unix socket serves"
        ),
        _ => {}
    }
    Ok(Daemon {
        state,
        socket: cfg.socket,
        tcp_addr,
        tasks,
    })
}

/// Sync definitions into the store, keep them for `/api/health`, and stop
/// the file agents whose files are gone.
pub async fn apply_definitions(
    state: &Shared,
    defs: Definitions,
    errors: Vec<String>,
    actor: String,
) -> ApiResult<SyncReport> {
    let state_dir = state.state_dir.clone();
    let d = defs.clone();
    let mut report = state
        .db
        .write(actor, move |t| store::sync::sync(t, &d, &state_dir))
        .await?;
    report.errors.splice(0..0, errors);
    state.set_definitions(defs);
    for id in &report.removed_agent_ids {
        if let Ok(id) = id.parse() {
            match state.supervisor.stop(id).await {
                Ok(()) | Err(SupervisorError::NotRunning(_)) => {}
                Err(e) => warn!(agent = %id, "cannot stop a removed agent: {e}"),
            }
        }
    }
    Ok(report)
}

/// Set an agent's status, with a `status` event. A stop or an error ends
/// the current session.
pub fn apply_status(
    t: &mut Tx<'_>,
    id: &str,
    status: AgentStatus,
    reason: Option<&str>,
) -> ApiResult<()> {
    let a = agents::get(t, id)?;
    let from = agents::set_status(t, id, status)?;
    if matches!(status, AgentStatus::Stopped | AgentStatus::Error) {
        let outcome = match status {
            AgentStatus::Error => "error",
            _ => reason.unwrap_or("stopped"),
        };
        agents::end_session(t, id, outcome)?;
    }
    let session = agents::current_session(t, id)?;
    t.emit(
        EventDraft::new(
            "status",
            format!("{}: {from} -> {status}", a.agent.name),
            json!({"status": status, "from": from, "reason": reason}),
        )
        .agent(id, a.agent.name.clone())
        .project(a.agent.project.clone())
        .session(session),
    )?;
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
        "notification" | "question" => {
            format!(
                "{name}: {}",
                short(data.get("message").unwrap_or(&data["question"]))
            )
        }
        "exit" => format!("{name}: exit {}", data["code"]),
        _ => format!("{name}: {kind}"),
    }
}

/// Write one supervisor event to the store.
fn record(t: &mut Tx<'_>, ev: &SupervisorEvent) -> ApiResult<()> {
    let id = ev.agent.to_string();
    let event = match &ev.kind {
        SupervisorEventKind::Status { status, reason } => {
            return apply_status(t, &id, *status, reason.as_deref());
        }
        SupervisorEventKind::Harness(Event::StatusChanged { to, reason, .. }) => {
            return apply_status(t, &id, *to, reason.as_deref());
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
            let result = state.db.write("system", move |t| record(t, &ev)).await;
            if let Err(e) = result {
                warn!("cannot record a supervisor event: {e}");
            }
        }
    })
}

/// After a start, an agent the store thinks is live but the supervisor
/// does not run is `stopped`.
async fn reconcile(state: &Shared) -> ApiResult<()> {
    let all = state.db.read(agents::all_statuses).await?;
    for (id, stored) in all {
        if !stored.is_live() {
            continue;
        }
        let Ok(agent_id) = id.parse() else { continue };
        let live = state
            .supervisor
            .status(agent_id)
            .await
            .is_some_and(AgentStatus::is_live);
        if !live {
            state
                .db
                .write("system", move |t| {
                    apply_status(
                        t,
                        &id,
                        AgentStatus::Stopped,
                        Some("not running after daemon start"),
                    )
                })
                .await?;
        }
    }
    Ok(())
}
