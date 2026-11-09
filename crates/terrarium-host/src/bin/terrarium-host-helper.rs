//! `terrarium-host-helper`: the root helper of native GPU enforcement
//! (TER-345, docs/native-gpu-helper.md). It runs as a root systemd
//! service and serves only the host agent's user on a Unix socket. It
//! never runs a command as root: each session runs as the workload user,
//! in a transient system service with a closed device policy.

use std::ffi::CString;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use anyhow::{Context, bail};
use clap::Parser;
use nix::unistd::{Group, User};
use terrarium_host::native::{
    self, Ctx, Helper, HelperConfig, HostDevices, Identity, MIN_SYSTEMD, Sessions,
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

fn root_owned(path: &Path) -> anyhow::Result<()> {
    let meta = std::fs::metadata(path).with_context(|| format!("{}", path.display()))?;
    native::check_root_owned(path, &meta).map_err(anyhow::Error::msg)
}

fn user(name: &str) -> anyhow::Result<User> {
    User::from_name(name)
        .with_context(|| format!("look up user {name}"))?
        .with_context(|| format!("no user {name}"))
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
    root_owned(&args.config)?;
    let text = std::fs::read_to_string(&args.config)
        .with_context(|| format!("read {}", args.config.display()))?;
    let cfg = HelperConfig::parse(&text).map_err(anyhow::Error::msg)?;

    let agent = user(&cfg.agent_user)?;
    let workload = user(&cfg.workload_user)?;
    let id = Identity {
        name: workload.name.clone(),
        uid: workload.uid.as_raw(),
        gid: workload.gid.as_raw(),
    };
    let cname = CString::new(workload.name.as_str()).context("workload user name")?;
    let groups: Vec<(u32, String)> = nix::unistd::getgrouplist(&cname, workload.gid)
        .context("groups of the workload user")?
        .into_iter()
        .map(|g| {
            let name = Group::from_gid(g)
                .ok()
                .flatten()
                .map(|g| g.name)
                .unwrap_or_default();
            (g.as_raw(), name)
        })
        .collect();
    let linger = Path::new("/var/lib/systemd/linger")
        .join(&workload.name)
        .exists();
    native::check_workload(agent.uid.as_raw(), &id, &groups, linger).map_err(anyhow::Error::msg)?;

    for bin in [&cfg.systemd_run, &cfg.systemctl] {
        root_owned(bin)?;
    }
    if cfg.gpu.nvidia_smi.exists() {
        root_owned(&cfg.gpu.nvidia_smi)?;
    }
    let version = std::process::Command::new(&cfg.systemctl)
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
    root_owned(socket_dir)?;
    std::fs::create_dir_all(&cfg.state_dir)
        .with_context(|| format!("create {}", cfg.state_dir.display()))?;
    std::fs::set_permissions(
        &cfg.state_dir,
        <std::fs::Permissions as std::os::unix::fs::PermissionsExt>::from_mode(0o700),
    )?;
    root_owned(&cfg.state_dir)?;
    if args.check {
        println!(
            "ok: agent uid {}, workload {} ({}:{}), workspace root {}, rights {:?}",
            agent.uid,
            id.name,
            id.uid,
            id.gid,
            workspace_root.display(),
            cfg.rights
        );
        return Ok(());
    }

    let helper = Arc::new(Helper {
        ctx: Ctx {
            workload: id,
            workspace_root,
            state_dir: cfg.state_dir.clone(),
            dev_dir: cfg.gpu.dev_dir.clone(),
            support_nodes: cfg.gpu.support_nodes.clone(),
        },
        agent_uid: agent.uid.as_raw(),
        rights: cfg.rights.clone(),
        systemd_run: cfg.systemd_run.clone(),
        systemctl: cfg.systemctl.clone(),
        nvidia_smi: cfg.gpu.nvidia_smi.clone(),
        nodes: Box::new(HostDevices),
        sessions: Mutex::new(Sessions::default()),
    });
    let rt = tokio::runtime::Runtime::new().context("start the runtime")?;
    rt.block_on(async move {
        helper.kill_leftovers().await;
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
