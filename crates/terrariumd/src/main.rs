//! `terrariumd`: the Terrarium daemon.
#![warn(clippy::unwrap_used, clippy::expect_used)]

use std::collections::HashMap;
use std::sync::Arc;

use anyhow::Context;
use clap::Parser;
use terrariumd::harness_tools::ToolsConfig;
use terrariumd::runner_supervisor::{RunnerMode, RunnerSupervisor, RunnerSupervisorConfig};
use terrariumd::sandbox::{self, SandboxConfig};
use terrariumd::settings::{Command, HarnessCommand, Settings};
use terrariumd::usage_collector::UsageConfig;
use terrariumd::{DaemonConfig, start_with_usage};
use tracing::{error, info, warn};
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
    let tools = settings.tools_config();
    if let Some(Command::Harness { command }) = &settings.command {
        return harness_command(tools, command).await;
    }
    if let Some(t) = &tools {
        // The adapters and the runners find the harnesses on PATH.
        ensure_first_on_path(&t.bin_dir());
    }
    let mut sandbox = SandboxConfig {
        mode: settings.sandbox,
        cli: settings.cli_path(),
        new_session: settings.sandbox_new_session,
        repos_dir: settings.repos_dir.clone(),
        tools_dir: tools.as_ref().map(|t| t.dir.clone()),
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
    if let Some(t) = &tools {
        if settings.no_harness_install {
            info!(tools = %t.dir.display(), "harness install skipped (--no-harness-install)");
        } else {
            log_install(&t.install().await);
        }
    }
    let mut cfg = supervisor_config(sandbox);
    cfg.mode = mode;
    cfg.version_policy = settings.harness_version_policy;
    let supervisor = Arc::new(RunnerSupervisor::new(cfg));
    let usage = UsageConfig {
        homes: homes.clone(),
        detect_on_start: true,
        // Sandbox mode `none` binds no per-agent overlay, so the session
        // files are in the shared home.
        per_agent_sessions: settings.sandbox == sandbox::SandboxMode::Bwrap,
        ..UsageConfig::default()
    };
    // The web settings, from the flags and the env, in one place.
    let artifact_listen = settings.artifact_listen();
    let web = settings.web();
    let daemon = start_with_usage(
        DaemonConfig {
            socket: settings.socket,
            listen: Some(settings.listen),
            token: settings.token,
            state_dir: settings.state_dir,
            config_dir: settings.config_dir,
            work_dir: settings.work_dir,
            max_targets_per_project: settings.max_targets_per_project,
            repos_dir: settings.repos_dir.clone(),
            supervisor,
            insecure_agent_header: settings.insecure_agent_header,
            artifact_listen,
            web,
        },
        usage,
    )
    .await?;
    // The dial-in listener of the host agents (docs/host-protocol.md
    // "TLS"). TERRARIUM_HOST_TLS_CERT and TERRARIUM_HOST_TLS_KEY turn on
    // TLS and bind 0.0.0.0:8082; without them it is plaintext on
    // 127.0.0.1:8082. TERRARIUM_HOST_LISTEN sets the address; empty turns
    // it off. The dashboard port does not serve /hosts/connect.
    if let Some(t) = tools {
        daemon.state.harness_tools.configure(t);
    }
    let host_task = match terrariumd::hosts::HostListen::from_env()? {
        Some(listen) => {
            let (task, _) = listen.serve(daemon.state.clone()).await?;
            Some(task)
        }
        None => None,
    };

    // Check the installed harness versions once at start, in the
    // background, so the log shows a harness outside its tested range
    // before an agent starts on it.
    let versions = daemon.state.supervisor.clone();
    tokio::spawn(async move {
        let report = versions.harness_versions(true).await;
        for h in report.harnesses {
            let installed = h.installed.as_deref().unwrap_or("not found");
            match h.in_range {
                Some(true) => {
                    info!(harness = %h.harness, installed, tested = %h.tested, "harness version")
                }
                Some(false) => warn!(
                    harness = %h.harness, installed, tested = %h.tested, pinned = %h.pinned,
                    policy = %report.policy,
                    "harness version outside the tested range: update it (`terrarium admin harness update <harness> --version <pinned>`)"
                ),
                None => {
                    warn!(harness = %h.harness, error = h.error.as_deref().unwrap_or(""), "harness version not known")
                }
            }
        }
    });

    shutdown_signal().await;
    info!("shutting down");
    if let Some(t) = host_task {
        t.abort();
    }
    daemon.shutdown();
    Ok(())
}

/// Run this binary again with `bin` first on `PATH`, when it is not, so
/// the daemon, the adapters, and the runners find the harnesses of the
/// tools dir (`just daemon::run`; the image sets `PATH` itself).
fn ensure_first_on_path(bin: &std::path::Path) {
    use std::os::unix::process::CommandExt;
    let path = std::env::var("PATH").unwrap_or_default();
    if path.split(':').next() == Some(bin.to_string_lossy().as_ref()) {
        return;
    }
    let Ok(exe) = std::env::current_exe() else {
        warn!("cannot find this binary; the tools dir is not on PATH");
        return;
    };
    let new = terrariumd::harness_tools::prepend_path(bin, &path);
    info!(bin = %bin.display(), "starting again with the tools dir first on PATH");
    let err = std::process::Command::new(exe)
        .args(std::env::args_os().skip(1))
        .env("PATH", new)
        .exec();
    warn!("cannot start again with the tools dir on PATH: {err}");
}

fn log_install(r: &terrariumd::harness_tools::InstallReport) {
    for i in &r.items {
        let version = i.version.as_deref().unwrap_or("-");
        match &i.error {
            None => {
                info!(harness = %i.name, package = %i.package, version, action = %i.action, "harness tools")
            }
            Some(e) => {
                error!(harness = %i.name, package = %i.package, "harness install failed: {e}");
                if !i.output.is_empty() {
                    error!("npm output:\n{}", i.output.trim_end());
                }
            }
        }
    }
    info!(extension = %r.extension, tools = %r.tools_dir.display(), "Copilot prompt extension");
}

/// `terrariumd harness install|status`.
async fn harness_command(
    tools: Option<ToolsConfig>,
    command: &HarnessCommand,
) -> anyhow::Result<()> {
    let Some(t) = tools else {
        anyhow::bail!("no tools dir: set TERRARIUM_TOOLS_DIR (or --tools-dir)");
    };
    match command {
        HarnessCommand::Install { json } => {
            let started = std::time::Instant::now();
            let r = t.install().await;
            #[allow(clippy::print_stdout)]
            if *json {
                println!("{}", serde_json::to_string_pretty(&r)?);
            } else {
                for i in &r.items {
                    println!(
                        "{:<16} {:<34} {:<10} {}{}",
                        i.name,
                        i.package,
                        i.version.as_deref().unwrap_or("-"),
                        i.action,
                        i.error
                            .as_deref()
                            .map(|e| format!(": {e}"))
                            .unwrap_or_default()
                    );
                    if i.error.is_some() && !i.output.is_empty() {
                        println!("{}", i.output.trim_end());
                    }
                }
                println!("extension: {}", r.extension);
                println!(
                    "tools dir: {} ({:.1} s)",
                    r.tools_dir.display(),
                    started.elapsed().as_secs_f64()
                );
            }
            if r.failed() > 0 {
                anyhow::bail!("{} of {} installs failed", r.failed(), r.items.len());
            }
        }
        HarnessCommand::Status { json } => {
            let manifest = t.read_manifest();
            let rows: Vec<serde_json::Value> = t
                .pins
                .iter()
                .map(|p| {
                    serde_json::json!({
                        "name": p.name, "package": p.package, "pinned": p.version,
                        "installed": t.present(p), "manifest": manifest.get(&p.name),
                    })
                })
                .collect();
            #[allow(clippy::print_stdout)]
            if *json {
                println!(
                    "{}",
                    serde_json::to_string_pretty(
                        &serde_json::json!({"tools_dir": t.dir, "harnesses": rows})
                    )?
                );
            } else {
                println!("tools dir: {}", t.dir.display());
                for (p, r) in t.pins.iter().zip(&rows) {
                    println!(
                        "{:<16} {:<34} pinned {:<10} installed {}",
                        p.name,
                        p.package,
                        p.version,
                        r["installed"].as_str().unwrap_or("-")
                    );
                }
            }
        }
    }
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
