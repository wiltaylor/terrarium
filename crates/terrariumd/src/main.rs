//! `terrariumd`: the Terrarium daemon.
#![warn(clippy::unwrap_used, clippy::expect_used)]

use std::collections::HashMap;
use std::sync::Arc;

use anyhow::Context;
use clap::Parser;
use terrariumd::harness_tools::ToolsConfig;
use terrariumd::runner_supervisor::{RunnerMode, RunnerSupervisor, RunnerSupervisorConfig};
use terrariumd::sandbox::{self, SandboxConfig};
use terrariumd::settings::{
    Command, ConfigCommand, DefaultsCommand, HarnessCommand, PasswordCommand, SecretsCommand,
    Settings,
};
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
    if let Some(Command::Password { command }) = &settings.command {
        return password_command(&settings, command);
    }
    if let Some(Command::Secrets { command }) = &settings.command {
        return secrets_command(&settings, command);
    }
    if let Some(Command::Defaults { command }) = &settings.command {
        return defaults_command(&settings, command);
    }
    if let Some(Command::Config { command }) = &settings.command {
        return config_command(command);
    }
    // TERRARIUM_PASSWORD(_FILE): set the first password now, then start
    // again without the two variables, so no child process (a harness, git,
    // npm, a login) ever inherits them. The same restart puts the tools dir
    // first on PATH: the adapters and the runners find the harnesses there.
    initial_password(&settings)?;
    restart_clean(tools.as_ref().map(ToolsConfig::bin_dir).as_deref());
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
        let mut c = RunnerSupervisorConfig::new(
            settings.state_dir.clone(),
            settings.runtime_dir.clone(),
            settings.prompts_dir(),
            sandbox,
            homes.clone(),
        );
        c.config_repo = Some(settings.config_repo.clone());
        c
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
    // The config repo (PLAN §8.3): the packaged defaults go in, and it is
    // a git repo. A failure does not stop the start: the agents then run
    // with what the repo has (or the built-in prompt).
    seed_config_repo(&settings);
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
        auto_swap: terrariumd::accounts::auto_swap_from_env(),
        ..UsageConfig::default()
    };
    // The web settings, from the flags and the env, in one place.
    let artifact_listen = settings.artifact_listen();
    let mut web = settings.web();
    // The dial-in listener of the host agents (docs/host-protocol.md
    // "TLS"). TERRARIUM_HOST_TLS_CERT and TERRARIUM_HOST_TLS_KEY turn on
    // TLS and bind 0.0.0.0:8082; without them it is plaintext on
    // 127.0.0.1:8082. TERRARIUM_HOST_LISTEN sets the address; empty turns
    // it off. The dashboard port does not serve /hosts/connect. The host
    // installer (GET /install/host.sh) takes its URL and pin from it.
    let host_listen = terrariumd::hosts::HostListen::from_env()?;
    web.install.host_listen = host_listen.clone();
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
    if let Some(t) = tools {
        daemon.state.harness_tools.configure(t);
        // The start reconciled the live agents, so the supervisor knows the
        // version that each runs: remove the version dirs that nothing
        // uses, and the flat layout when no agent started on it.
        let state = daemon.state.clone();
        tokio::spawn(async move {
            let Some(cfg) = state.harness_tools.config() else {
                return;
            };
            let _busy = state.harness_tools.busy.lock().await;
            let removed = terrariumd::api::system::collect_garbage(&state, cfg).await;
            if !removed.is_empty() {
                info!(removed = ?removed, "removed unused harness versions");
            }
        });
    }
    // The harness auto-update (TERRARIUM_HARNESS_AUTO_UPDATE). Without a
    // tools dir it has nothing to update.
    let auto_update_task = daemon.state.harness_tools.config().map(|_| {
        terrariumd::harness_auto_update::spawn(daemon.state.clone(), settings.harness_auto_update)
    });
    let auto_sync = terrariumd::config_repo::AutoSync::from_env();
    let mut repo = terrariumd::config_repo::ConfigRepo::new(
        settings.config_repo.clone(),
        daemon.state.state_dir.clone(),
    );
    repo.auto_sync = auto_sync.as_str().to_owned();
    // The agent templates are files in the config repo (PLAN §5b): the
    // first load writes the files of the templates from before them.
    terrariumd::template_files::configure(&daemon.state, repo).await;
    let template_watch = terrariumd::template_files::spawn_watch(daemon.state.clone());
    let sync_task = terrariumd::api::spawn_config_sync(
        daemon.state.clone(),
        auto_sync,
        settings.config_sync_interval,
    );
    let host_task = match host_listen {
        Some(listen) => {
            let (task, _) = listen.serve(daemon.state.clone()).await?;
            Some(task)
        }
        None => None,
    };
    // The webhook listener of the event sources (docs/api.md §20). Off
    // when TERRARIUM_INGEST_ADDR is not set.
    let ingest_task = terrariumd::ingest::serve_from_env(daemon.state.clone())
        .await?
        .map(|(task, _)| task);

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
    if let Some(t) = ingest_task {
        t.abort();
    }
    if let Some(t) = sync_task {
        t.abort();
    }
    template_watch.abort();
    if let Some(t) = auto_update_task {
        t.abort();
    }
    daemon.shutdown();
    Ok(())
}

/// Run this binary again when its env needs a change: `bin` (the tools
/// dir) first on `PATH` (`just daemon::run`; the image sets `PATH`
/// itself), and no `TERRARIUM_PASSWORD` or `TERRARIUM_PASSWORD_FILE`. The
/// process cannot remove a variable from its own env (`unsafe` in Rust
/// 2024), so it starts again without them.
fn restart_clean(bin: Option<&std::path::Path>) {
    use std::os::unix::process::CommandExt;
    let path = std::env::var("PATH").unwrap_or_default();
    let new_path = bin
        .filter(|b| path.split(':').next() != Some(b.to_string_lossy().as_ref()))
        .map(|b| terrariumd::harness_tools::prepend_path(b, &path));
    let secrets: Vec<&str> = terrariumd::password::SECRET_ENV
        .into_iter()
        .filter(|k| std::env::var_os(k).is_some())
        .collect();
    if new_path.is_none() && secrets.is_empty() {
        return;
    }
    let Ok(exe) = std::env::current_exe() else {
        warn!("cannot find this binary to start again; the env stays as it is");
        return;
    };
    let mut cmd = std::process::Command::new(exe);
    cmd.args(std::env::args_os().skip(1));
    if let Some(p) = &new_path {
        info!(path = %p, "starting again with the tools dir first on PATH");
        cmd.env("PATH", p);
    }
    for k in &secrets {
        cmd.env_remove(k);
    }
    if !secrets.is_empty() {
        info!("starting again without TERRARIUM_PASSWORD(_FILE) in the env");
    }
    let err = cmd.exec();
    warn!(
        "cannot start again ({err}); the env stays as it is, and the child processes strip \
         TERRARIUM_PASSWORD(_FILE) themselves"
    );
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

/// Copy the packaged defaults into the config repo, and make it a git
/// repo. Each failure is a warning.
fn seed_config_repo(settings: &Settings) {
    use terrariumd::defaults::{self, SeedAction};
    let target = &settings.config_repo;
    if settings.defaults_dir.join(defaults::MANIFEST).is_file() {
        match defaults::Defaults::load(&settings.defaults_dir)
            .and_then(|d| defaults::seed(&d, target))
        {
            Ok(report) => {
                let count = |a: SeedAction| report.iter().filter(|r| r.action == a).count();
                info!(
                    repo = %target.display(),
                    seeded = count(SeedAction::Seeded),
                    upgraded = count(SeedAction::Upgraded),
                    pending = count(SeedAction::Pending),
                    "config repo defaults"
                );
                for r in report.iter().filter(|r| r.action == SeedAction::Pending) {
                    warn!(file = %r.path, "a new default waits as {}.new: accept or keep it (Config page, `terrarium admin config defaults`)", r.path);
                }
            }
            Err(e) => error!("cannot copy the defaults into the config repo: {e}"),
        }
    } else {
        info!(dir = %settings.defaults_dir.display(), "no packaged defaults: the config repo is not seeded");
    }
    let repo = terrariumd::config_repo::ConfigRepo::new(target.clone(), settings.state_dir.clone());
    if let Err(e) = repo.ensure() {
        warn!(repo = %target.display(), "cannot make the config repo a git repo: {}", e.message);
    }
}

/// `terrariumd defaults seed|apply|manifest`.
fn defaults_command(settings: &Settings, command: &DefaultsCommand) -> anyhow::Result<()> {
    use terrariumd::defaults;
    match command {
        DefaultsCommand::Seed => {
            let d = defaults::Defaults::load(&settings.defaults_dir).map_err(anyhow::Error::msg)?;
            for r in defaults::seed(&d, &settings.config_repo).map_err(anyhow::Error::msg)? {
                println!("{:<9} {}", format!("{:?}", r.action).to_lowercase(), r.path);
            }
        }
        DefaultsCommand::Apply => {
            let d = defaults::Defaults::load(&settings.defaults_dir).map_err(anyhow::Error::msg)?;
            let backup = defaults::apply(
                &d,
                &settings.config_repo,
                &settings.state_dir.join("backup"),
            )
            .map_err(anyhow::Error::msg)?;
            println!("backup: {}", backup.display());
            println!(
                "wrote {} default file(s) into {}",
                d.manifest.files.len(),
                settings.config_repo.display()
            );
        }
        DefaultsCommand::Manifest { dir } => {
            let dir = dir.clone().unwrap_or_else(|| settings.defaults_dir.clone());
            let m = defaults::make_manifest(&dir).map_err(anyhow::Error::msg)?;
            let path = dir.join(defaults::MANIFEST);
            std::fs::write(&path, defaults::manifest_text(&m))
                .with_context(|| format!("cannot write {}", path.display()))?;
            println!("{}: {} file(s)", path.display(), m.files.len());
        }
    }
    Ok(())
}

/// `terrariumd config scan`.
fn config_command(command: &ConfigCommand) -> anyhow::Result<()> {
    let ConfigCommand::Scan {
        staged,
        repo,
        files,
    } = command;
    let mut found = Vec::new();
    if *staged {
        let repo = repo
            .clone()
            .unwrap_or_else(|| std::path::PathBuf::from("."));
        found.extend(terrarium_core::secret_scan::scan_staged(&repo).map_err(anyhow::Error::msg)?);
    }
    for f in files {
        let bytes = std::fs::read(f).with_context(|| format!("cannot read {}", f.display()))?;
        found.extend(terrarium_core::secret_scan::scan(
            &f.to_string_lossy(),
            &bytes,
        ));
    }
    if found.is_empty() {
        return Ok(());
    }
    for f in &found {
        eprintln!("secret scan: {f}");
    }
    eprintln!(
        "secret scan: {} finding(s); the commit is blocked. Remove the secret, or for a false hit add `{}` to the line.",
        found.len(),
        terrarium_core::secret_scan::ALLOW_MARKER
    );
    std::process::exit(1);
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
            let mut r = t.install().await;
            // This command does not know the live agents: keep the newest
            // versions (the daemon's GC at its start knows them).
            r.removed = t.gc(None);
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
                if !r.removed.is_empty() {
                    println!("removed: {}", r.removed.join(", "));
                }
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
                        "versions": t.versions(&p.name),
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
                        "{:<16} {:<34} pinned {:<10} installed {:<10} on disk {}",
                        p.name,
                        p.package,
                        p.version,
                        r["installed"].as_str().unwrap_or("-"),
                        t.versions(&p.name).join(" ")
                    );
                }
            }
        }
    }
    Ok(())
}

/// `TERRARIUM_PASSWORD` or `TERRARIUM_PASSWORD_FILE`: set the operator
/// password at the start, when none is set. The value never reaches the
/// log.
fn initial_password(settings: &Settings) -> anyhow::Result<()> {
    match terrariumd::password::initial_password(
        &settings.state_dir,
        terrariumd::password::env_password().as_deref(),
        settings.password_file.as_deref(),
    )? {
        Some(true) => warn!(
            "set the operator password from TERRARIUM_PASSWORD(_FILE); remove the variable from \
             the env now, and use `terrariumd password set` to change the password"
        ),
        Some(false) => warn!(
            "TERRARIUM_PASSWORD(_FILE) is set, but a password exists already, so the daemon \
             ignores it; remove the variable from the env"
        ),
        None => {}
    }
    Ok(())
}

/// `terrariumd password set [--stdin]`: write `<state>/operator.password`.
fn password_command(settings: &Settings, command: &PasswordCommand) -> anyhow::Result<()> {
    let PasswordCommand::Set { stdin } = command;
    let password = if *stdin {
        let mut line = String::new();
        std::io::stdin()
            .read_line(&mut line)
            .context("cannot read the password from stdin")?;
        line.trim_end_matches(['\r', '\n']).to_owned()
    } else {
        let first = rpassword::prompt_password("New operator password: ")
            .context("cannot read the password from the terminal (use --stdin in a script)")?;
        terrariumd::password::check_new(&first)?;
        let again = rpassword::prompt_password("Again: ")
            .context("cannot read the password from the terminal")?;
        if first != again {
            anyhow::bail!("the two passwords are not the same");
        }
        first
    };
    // The daemon reads the file as its own user: a file of another owner
    // (root, from `docker compose exec -u root`) would lock it out.
    terrariumd::password::check_owner(&settings.state_dir)?;
    let store = terrariumd::password::PasswordStore::new(&settings.state_dir);
    let version = store.set(&password)?;
    #[allow(clippy::print_stdout)]
    {
        println!(
            "password set in {} (version {version}); every session has ended",
            store.path().display()
        );
    }
    Ok(())
}

/// `terrariumd secrets rotate-key`: a new master key, and each secret
/// sealed again with it (PLAN §16b.2). It refuses while a daemon answers
/// on the socket: the daemon reads the key file at each use.
fn secrets_command(settings: &Settings, command: &SecretsCommand) -> anyhow::Result<()> {
    let SecretsCommand::RotateKey = command;
    if std::os::unix::net::UnixStream::connect(&settings.socket).is_ok() {
        anyhow::bail!(
            "a daemon answers on {}: stop it first, then rotate the key",
            settings.socket.display()
        );
    }
    let mut conn = terrariumd::db::open(&settings.state_dir.join("terrarium.db"))?;
    terrariumd::secrets::ensure_key(&conn, &settings.state_dir)?;
    let n = terrariumd::secrets::migrate::rotate_key(&mut conn, &settings.state_dir)?;
    #[allow(clippy::print_stdout)]
    {
        println!(
            "new key in {}; {n} secret(s) sealed again",
            terrariumd::secrets::crypto::key_path(&settings.state_dir).display()
        );
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
