//! `terrariumd`: the Terrarium daemon.
#![warn(clippy::unwrap_used, clippy::expect_used)]

mod api;
mod assets;
mod db;
mod settings;
mod store;

use std::sync::Arc;

use anyhow::Context;
use clap::Parser;
use tracing::{error, info, warn};
use tracing_subscriber::EnvFilter;

use crate::settings::Settings;

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("info")),
        )
        .with_ansi(std::io::IsTerminal::is_terminal(&std::io::stdout()))
        .init();

    let settings = Settings::parse();
    info!(version = terrarium_core::VERSION, "terrariumd starting");

    let definitions = load_definitions(&settings);

    std::fs::create_dir_all(&settings.state_dir)
        .with_context(|| format!("cannot create state dir {}", settings.state_dir.display()))?;
    let db_path = settings.state_dir.join("terrarium.db");
    let db = db::open(&db_path)?;
    info!(path = %db_path.display(), "database ready");

    let state = Arc::new(api::AppState::new(db, definitions));

    let unix = api::serve_unix(state.clone(), &settings.socket).await?;
    let tcp = match settings.token.as_deref().filter(|t| !t.is_empty()) {
        Some(token) => {
            Some(api::serve_tcp(state.clone(), &settings.listen, token.to_owned()).await?)
        }
        None => {
            warn!(
                "TERRARIUM_TOKEN is not set: the TCP listener ({}) stays off; only the Unix socket serves",
                settings.listen
            );
            None
        }
    };

    shutdown_signal().await;
    info!("shutting down");
    unix.abort();
    if let Some(tcp) = tcp {
        tcp.abort();
    }
    if let Err(e) = std::fs::remove_file(&settings.socket) {
        warn!(socket = %settings.socket.display(), "cannot remove socket: {e}");
    }
    Ok(())
}

fn load_definitions(settings: &Settings) -> terrarium_core::config::Definitions {
    let (defs, errors) = terrarium_core::config::Definitions::load(&settings.config_dir);
    for e in &errors {
        error!("config: {e}");
    }
    info!(
        dir = %settings.config_dir.display(),
        projects = defs.projects.len(),
        agents = defs.agents.len(),
        targets = defs.targets.len(),
        subscriptions = defs.subscriptions.len(),
        errors = errors.len(),
        "definitions loaded"
    );
    for p in &defs.projects {
        info!(project = %p.name, repos = p.repos.len(), "found project");
    }
    for a in &defs.agents {
        info!(agent = %a.name, project = %a.project, harness = %a.harness, "found agent");
    }
    for t in &defs.targets {
        info!(target = %t.name, kind = ?t.kind, "found target");
    }
    for s in &defs.subscriptions {
        info!(subscription = %s.name, provider = %s.provider, "found subscription");
    }
    defs
}

async fn shutdown_signal() {
    let ctrl_c = async {
        if let Err(e) = tokio::signal::ctrl_c().await {
            error!("cannot listen for ctrl-c: {e}");
            std::future::pending::<()>().await;
        }
    };
    let term = async {
        match tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate()) {
            Ok(mut s) => {
                s.recv().await;
            }
            Err(e) => {
                error!("cannot listen for SIGTERM: {e}");
                std::future::pending::<()>().await;
            }
        }
    };
    tokio::select! {
        () = ctrl_c => {},
        () = term => {},
    }
}
