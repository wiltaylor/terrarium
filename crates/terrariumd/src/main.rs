//! `terrariumd`: the Terrarium daemon.
#![warn(clippy::unwrap_used, clippy::expect_used)]

use std::collections::HashMap;
use std::sync::Arc;

use anyhow::Context;
use clap::Parser;
use terrariumd::runner_supervisor::{RunnerMode, RunnerSupervisor, RunnerSupervisorConfig};
use terrariumd::sandbox::{self, SandboxConfig};
use terrariumd::settings::{Command, Settings};
use terrariumd::usage_collector::UsageConfig;
use terrariumd::{DaemonConfig, start_with_usage};
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
    let mut sandbox = SandboxConfig {
        mode: settings.sandbox,
        cli: settings.cli_path(),
        new_session: settings.sandbox_new_session,
        ..SandboxConfig::default()
    };
    let homes: HashMap<_, _> = settings.harness_homes.iter().cloned().collect();
    let supervisor_config = |sandbox: SandboxConfig| {
        RunnerSupervisorConfig::new(
            settings.state_dir.clone(),
            settings.runtime_dir.clone(),
            settings.prompts_dir.clone(),
            sandbox,
            homes.clone(),
        )
    };

    if let Some(Command::SandboxDryRun { agent }) = &settings.command {
        return dry_run(&settings, supervisor_config(sandbox), agent).await;
    }

    info!(version = terrarium_core::VERSION, sandbox = %sandbox.mode, "terrariumd starting");
    let bwrap = sandbox.check().map_err(anyhow::Error::msg)?;
    if sandbox.mode == sandbox::SandboxMode::Bwrap {
        info!(bwrap = %bwrap.display(), "sandbox ready");
        sandbox.bwrap = bwrap;
        if sandbox.cli.is_none() {
            error!("no `terrarium` CLI found: hooks inside the sandbox will fail; set --cli");
        }
    }
    let mode = settings.runner_mode().map_err(anyhow::Error::msg)?;
    match &mode {
        RunnerMode::Process { bin } => {
            info!(runner = %bin.display(), "agents run in runner processes");
        }
        RunnerMode::InProcess => {
            info!("agents run in the daemon process: they end when the daemon stops");
        }
    }
    let mut cfg = supervisor_config(sandbox);
    cfg.mode = mode;
    let supervisor = Arc::new(RunnerSupervisor::new(cfg));
    let usage = UsageConfig {
        homes: homes.clone(),
        detect_on_start: true,
        ..UsageConfig::default()
    };
    let daemon = start_with_usage(
        DaemonConfig {
            socket: settings.socket,
            listen: Some(settings.listen),
            token: settings.token,
            state_dir: settings.state_dir,
            config_dir: settings.config_dir,
            supervisor,
            insecure_agent_header: settings.insecure_agent_header,
            artifact_listen: Some(settings.artifact_listen),
        },
        usage,
    )
    .await?;
    // The dial-in listener of the host agents (docs/host-protocol.md). An
    // empty TERRARIUM_HOST_LISTEN turns it off; the main router still
    // serves /hosts/connect.
    let host_listen =
        std::env::var("TERRARIUM_HOST_LISTEN").unwrap_or_else(|_| "0.0.0.0:8082".into());
    let host_task = if host_listen.is_empty() {
        None
    } else {
        let (task, _) = terrariumd::hosts::serve(daemon.state.clone(), &host_listen).await?;
        Some(task)
    };

    shutdown_signal().await;
    info!("shutting down");
    if let Some(t) = host_task {
        t.abort();
    }
    daemon.shutdown();
    Ok(())
}

/// `terrariumd sandbox-dry-run <agent>`: print the launch command.
async fn dry_run(
    settings: &Settings,
    cfg: RunnerSupervisorConfig,
    key: &str,
) -> anyhow::Result<()> {
    let conn = terrariumd::db::open(&settings.state_dir.join("terrarium.db"))?;
    let view = terrariumd::store::agents::find(&conn, key, None)
        .map_err(|e| anyhow::anyhow!("{}", e.message))?;
    let resume = terrariumd::store::agents::last_harness_session(&conn, &view.agent.id.to_string())
        .map_err(|e| anyhow::anyhow!("{}", e.message))?;
    let sup = RunnerSupervisor::new(cfg);
    let spec = sup
        .launch_spec(&view.agent, resume.as_ref())
        .await
        .context("cannot build the launch command")?;
    #[allow(clippy::print_stdout)]
    {
        println!("{}", sandbox::command_line(&spec));
    }
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
