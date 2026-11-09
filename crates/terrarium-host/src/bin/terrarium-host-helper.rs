//! `terrarium-host-helper`: the root helper of native GPU enforcement
//! (TER-345, docs/native-gpu-helper.md). It runs as a root systemd
//! service and serves only the host agent's user on a Unix socket. It
//! never runs a command as root: each session runs as its holder's user
//! from the workload pool, in a transient system service with a closed
//! device policy.

use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use anyhow::{Context, bail};
use clap::Parser;
use nix::unistd::User;
use terrarium_host::native::{
    self, Ctx, Helper, HelperConfig, HostDevices, HostWorkloadCheck, MIN_SYSTEMD, Sessions,
};

#[derive(Debug, Parser)]
#[command(name = "terrarium-host-helper", version = terrarium_version::FULL, about)]
struct Args {
    /// The root-owned config file.
    #[arg(long, default_value = native::DEFAULT_CONFIG)]
    config: PathBuf,
    /// Check the config, the users, and systemd, then exit.
    #[arg(long)]
    check: bool,
}

/// The path and every parent, and each symlink on the way: root-owned,
/// not writable by group or others. Returns the resolved path, which the
/// helper uses from then on.
fn root_chain(path: &Path) -> anyhow::Result<PathBuf> {
    native::check_root_chain(path).map_err(anyhow::Error::msg)
}

fn main() -> anyhow::Result<()> {
    let args = Args::parse();
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("info")),
        )
        .with_writer(std::io::stderr)
        .init();
    if !nix::unistd::geteuid().is_root() {
        bail!("terrarium-host-helper must run as root");
    }
    let config = root_chain(&args.config)?;
    let text =
        std::fs::read_to_string(&config).with_context(|| format!("read {}", config.display()))?;
    let cfg = HelperConfig::parse(&text).map_err(anyhow::Error::msg)?;

    let agent = User::from_name(&cfg.agent_user)
        .with_context(|| format!("look up user {}", cfg.agent_user))?
        .with_context(|| format!("no user {}", cfg.agent_user))?;
    let agent_uid = agent.uid.as_raw();
    let mut pool = Vec::new();
    for name in &cfg.workload_users {
        let (id, groups, linger) = native::resolve_user(name).map_err(anyhow::Error::msg)?;
        native::check_workload(agent_uid, &id, &groups, linger).map_err(anyhow::Error::msg)?;
        if pool
            .iter()
            .any(|w: &native::Identity| w.uid == id.uid || w.gid == id.gid)
        {
            bail!(
                "workload user {name} shares its UID or primary group with another pool user; \
                 each pool user needs its own UID and primary group"
            );
        }
        pool.push(id);
    }

    // The helper runs these as root: through the verified real path, so
    // the path as written in the config does not matter after this.
    let systemd_run = root_chain(&cfg.systemd_run)?;
    let systemctl = root_chain(&cfg.systemctl)?;
    // GPU sessions need a verified nvidia-smi.
    let (nvidia_smi, gpu) = if cfg.gpu.nvidia_smi.exists() {
        match native::check_root_chain(&cfg.gpu.nvidia_smi) {
            Ok(real) => (real, Ok(())),
            Err(e) => (cfg.gpu.nvidia_smi.clone(), Err(e)),
        }
    } else {
        (
            cfg.gpu.nvidia_smi.clone(),
            Err(format!("{} is missing", cfg.gpu.nvidia_smi.display())),
        )
    };
    if let Err(e) = &gpu {
        tracing::warn!(error = %e, "GPU sessions are off");
    }
    // A stop is confirmed from the unit's cgroup v2 `cgroup.events`.
    native::check_cgroup_v2(Path::new(native::CGROUP_ROOT)).map_err(anyhow::Error::msg)?;
    let version = std::process::Command::new(&systemctl)
        .arg("--version")
        .output()
        .context("systemctl --version")?;
    match native::parse_systemd_version(&String::from_utf8_lossy(&version.stdout)) {
        Some(v) if v >= MIN_SYSTEMD => {}
        v => bail!("systemd {v:?} is too old: the helper needs {MIN_SYSTEMD} or newer"),
    }
    let workspace_root = std::fs::canonicalize(&cfg.workspace_root)
        .with_context(|| format!("workspace_root {}", cfg.workspace_root.display()))?;
    let socket_dir = cfg
        .socket
        .parent()
        .context("the socket needs a directory")?;
    root_chain(socket_dir)?;
    // Before the helper creates or chmods it: a symlinked state_dir or a
    // writable parent would let another user aim root's writes.
    native::check_state_dir(&cfg.state_dir).map_err(anyhow::Error::msg)?;
    std::fs::create_dir_all(&cfg.state_dir)
        .with_context(|| format!("create {}", cfg.state_dir.display()))?;
    std::fs::set_permissions(
        &cfg.state_dir,
        <std::fs::Permissions as std::os::unix::fs::PermissionsExt>::from_mode(0o700),
    )?;
    let state_dir = root_chain(&cfg.state_dir)?;
    if args.check {
        println!(
            "ok: agent uid {agent_uid}, workload pool {:?}, workspace root {}, rights {:?}, GPU sessions {}",
            pool.iter()
                .map(|w| format!("{}={}", w.name, w.uid))
                .collect::<Vec<_>>(),
            workspace_root.display(),
            cfg.rights,
            match &gpu {
                Ok(()) => "on".to_owned(),
                Err(e) => format!("off ({e})"),
            }
        );
        return Ok(());
    }

    let helper = Arc::new(Helper {
        ctx: Ctx {
            workspace_root,
            state_dir,
            dev_dir: cfg.gpu.dev_dir.clone(),
            support_nodes: cfg.gpu.support_nodes.clone(),
        },
        agent_uid,
        rights: cfg.rights.clone(),
        pool,
        checker: Box::new(HostWorkloadCheck { agent_uid }),
        systemd_run,
        systemctl,
        nvidia_smi,
        cgroup_root: PathBuf::from(native::CGROUP_ROOT),
        gpu,
        nodes: Box::new(HostDevices),
        sessions: Mutex::new(Sessions::default()),
        stop_timeout: native::STOP_TIMEOUT,
        stop_retry: native::STOP_RETRY,
    });
    let rt = tokio::runtime::Runtime::new().context("start the runtime")?;
    rt.block_on(async move {
        // Fail closed: never serve while a leftover unit may hold a GPU.
        helper.kill_leftovers().await.map_err(anyhow::Error::msg)?;
        let listener = native::bind(&cfg.socket, helper.agent_uid)
            .with_context(|| format!("bind {}", cfg.socket.display()))?;
        tracing::info!(socket = %cfg.socket.display(), rights = ?cfg.rights, "serving");
        let mut term = tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())?;
        tokio::select! {
            r = native::serve(helper, listener) => r.context("accept")?,
            _ = term.recv() => tracing::info!("terminated"),
        }
        Ok(())
    })
}
