//! `terrariumd`: the Terrarium daemon.
#![warn(clippy::unwrap_used, clippy::expect_used)]

use std::sync::Arc;
use std::time::Duration;

use clap::Parser;
use terrariumd::settings::Settings;
use terrariumd::supervisor::InMemorySupervisor;
use terrariumd::{DaemonConfig, start};
use tracing::{error, info};
use tracing_subscriber::EnvFilter;

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

    // The real runner plugs in here in the next phase.
    let supervisor = Arc::new(InMemorySupervisor::new(Duration::from_millis(
        settings.fake_delay_ms,
    )));
    let daemon = start(DaemonConfig {
        socket: settings.socket,
        listen: Some(settings.listen),
        token: settings.token,
        state_dir: settings.state_dir,
        config_dir: settings.config_dir,
        supervisor,
    })
    .await?;

    shutdown_signal().await;
    info!("shutting down");
    daemon.shutdown();
    Ok(())
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
