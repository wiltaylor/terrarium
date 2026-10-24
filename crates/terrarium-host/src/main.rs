//! `terrarium-host`: the host agent binary (PLAN §19).
//!
//! `run` dials the daemon and serves sessions until a signal stops it.
//! `check` prints the detected capabilities. `token-file` writes a token
//! from stdin to a file with mode 0600.

use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use anyhow::{Context, bail};
use clap::{Parser, Subcommand};
use terrarium_host::config::{self, Config, DEFAULT_CONFIG_PATH};
use terrarium_host::{Client, ClientOptions, capabilities};

/// The Terrarium host agent.
#[derive(Debug, Parser)]
#[command(name = "terrarium-host", version, about)]
struct Args {
    /// The config file. The default is /etc/terrarium-host/config.toml when
    /// it exists.
    #[arg(long, short, env = "TERRARIUM_HOST_CONFIG", global = true)]
    config: Option<PathBuf>,

    /// The daemon URL, for example `wss://terrarium.lan:8082`. Overrides the file.
    #[arg(long, env = "TERRARIUM_DAEMON_URL", global = true)]
    daemon_url: Option<String>,

    /// The per-host token. Overrides the file. Prefer `token_file` in the file.
    #[arg(
        long,
        env = "TERRARIUM_HOST_TOKEN",
        hide_env_values = true,
        global = true
    )]
    token: Option<String>,

    /// The name of this host, as targets refer to it. Overrides the file.
    #[arg(long, env = "TERRARIUM_HOST_NAME", global = true)]
    name: Option<String>,

    #[command(subcommand)]
    command: Option<Cmd>,
}

#[derive(Debug, Subcommand)]
enum Cmd {
    /// Connect to the daemon and serve sessions (the default).
    Run,
    /// Print the detected capabilities and the resolved settings, then exit.
    Check {
        /// Print JSON.
        #[arg(long)]
        json: bool,
    },
    /// Read a token from stdin and write it to PATH with mode 0600.
    TokenFile {
        /// Where to write the token, for example /etc/terrarium-host/token.
        path: PathBuf,
    },
}

fn load_config(args: &Args) -> anyhow::Result<Config> {
    let mut cfg = match &args.config {
        Some(p) => Config::load(p)?,
        None if Path::new(DEFAULT_CONFIG_PATH).exists() => {
            Config::load(Path::new(DEFAULT_CONFIG_PATH))?
        }
        None => Config::default(),
    };
    if let Some(u) = &args.daemon_url {
        cfg.daemon_url = Some(u.clone());
    }
    if let Some(t) = &args.token {
        cfg.token = Some(t.clone());
        cfg.token_file = None;
    }
    if let Some(n) = &args.name {
        cfg.name = Some(n.clone());
    }
    Ok(cfg)
}

fn tls_config(ca_file: &Path) -> anyhow::Result<Arc<rustls::ClientConfig>> {
    use rustls::pki_types::CertificateDer;
    use rustls::pki_types::pem::PemObject;
    terrarium_host::client::ensure_crypto_provider();
    let mut roots = rustls::RootCertStore::empty();
    for cert in CertificateDer::pem_file_iter(ca_file)
        .with_context(|| format!("read {}", ca_file.display()))?
    {
        let cert = cert.with_context(|| format!("parse {}", ca_file.display()))?;
        roots.add(cert).context("add CA certificate")?;
    }
    if roots.is_empty() {
        bail!("{} holds no certificates", ca_file.display());
    }
    Ok(Arc::new(
        rustls::ClientConfig::builder()
            .with_root_certificates(roots)
            .with_no_client_auth(),
    ))
}

fn write_token_file(path: &Path) -> anyhow::Result<()> {
    use std::os::unix::fs::{DirBuilderExt, OpenOptionsExt};
    let mut token = String::new();
    std::io::stdin()
        .read_to_string(&mut token)
        .context("read the token from stdin")?;
    let token = token.trim();
    if token.is_empty() {
        bail!("no token on stdin");
    }
    if let Some(parent) = path.parent().filter(|p| !p.as_os_str().is_empty()) {
        std::fs::DirBuilder::new()
            .recursive(true)
            .mode(0o700)
            .create(parent)
            .with_context(|| format!("create {}", parent.display()))?;
    }
    let tmp = path.with_extension("tmp");
    let mut f = std::fs::OpenOptions::new()
        .write(true)
        .create(true)
        .truncate(true)
        .mode(0o600)
        .open(&tmp)
        .with_context(|| format!("create {}", tmp.display()))?;
    writeln!(f, "{token}").context("write the token")?;
    f.sync_all().context("sync the token file")?;
    drop(f);
    std::fs::rename(&tmp, path).with_context(|| format!("rename to {}", path.display()))?;
    eprintln!("wrote {}", path.display());
    Ok(())
}

async fn check(cfg: &Config, json: bool) -> anyhow::Result<()> {
    let caps = capabilities::detect(&cfg.docker_bin, &cfg.vmlab_bin).await;
    let url = cfg
        .daemon_url
        .as_deref()
        .map(config::connect_url)
        .transpose()?;
    let report = serde_json::json!({
        "version": terrarium_core::VERSION,
        "name": cfg.resolve_name(),
        "os": std::env::consts::OS,
        "arch": std::env::consts::ARCH,
        "daemon_url": url,
        "token": cfg.resolve_token().is_ok(),
        "max_sessions": cfg.max_sessions,
        "allowed_cwd_roots": cfg.allowed_cwd_roots,
        "capabilities": caps,
    });
    let mut out = std::io::stdout().lock();
    if json {
        writeln!(out, "{}", serde_json::to_string_pretty(&report)?)?;
    } else {
        let tool = |t: &terrarium_core::protocol::ToolCapability| match (&t.present, &t.version) {
            (true, Some(v)) => format!("yes ({v})"),
            (true, None) => "yes".to_owned(),
            (false, _) => "no".to_owned(),
        };
        writeln!(out, "terrarium-host {}", terrarium_core::VERSION)?;
        writeln!(out, "name:       {}", cfg.resolve_name())?;
        writeln!(
            out,
            "os/arch:    {}/{}",
            std::env::consts::OS,
            std::env::consts::ARCH
        )?;
        writeln!(out, "daemon:     {}", url.as_deref().unwrap_or("not set"))?;
        writeln!(
            out,
            "token:      {}",
            if cfg.resolve_token().is_ok() {
                "set"
            } else {
                "not set"
            }
        )?;
        writeln!(out, "docker:     {}", tool(&caps.docker))?;
        writeln!(out, "vmlab:      {}", tool(&caps.vmlab))?;
        writeln!(
            out,
            "shell:      {}",
            caps.shell.as_deref().unwrap_or("none")
        )?;
        match &cfg.allowed_cwd_roots {
            Some(r) => writeln!(out, "cwd roots:  {r:?}")?,
            None => writeln!(out, "cwd roots:  any")?,
        }
    }
    Ok(())
}

async fn run(cfg: Config) -> anyhow::Result<()> {
    let url = config::connect_url(
        cfg.daemon_url
            .as_deref()
            .context("no daemon_url: set it in the config file or with --daemon-url")?,
    )?;
    let token = cfg.resolve_token()?;
    let name = cfg.resolve_name();
    let caps = capabilities::detect(&cfg.docker_bin, &cfg.vmlab_bin).await;
    tracing::info!(%url, %name, docker = caps.docker.present, vmlab = caps.vmlab.present, "starting");
    let mut opts = ClientOptions::new(url, token, name);
    opts.capabilities = caps;
    opts.max_sessions = cfg.max_sessions;
    opts.buffer_limit = cfg.buffer_limit_bytes;
    opts.resume_grace = Duration::from_secs(cfg.resume_grace_secs);
    opts.policy = cfg.policy();
    opts.docker_bin = cfg.docker_bin.clone();
    opts.vmlab_bin = cfg.vmlab_bin.clone();
    if let Some(ca) = &cfg.ca_file {
        opts.tls = Some(tls_config(ca)?);
    }
    let client = Client::new(opts);
    let mut term = tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())
        .context("install the SIGTERM handler")?;
    tokio::select! {
        () = client.run() => {}
        _ = tokio::signal::ctrl_c() => tracing::info!("interrupted"),
        _ = term.recv() => tracing::info!("terminated"),
    }
    Ok(())
}

fn main() -> anyhow::Result<()> {
    let args = Args::parse();
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("info")),
        )
        .with_writer(std::io::stderr)
        .with_ansi(std::io::IsTerminal::is_terminal(&std::io::stderr()))
        .init();
    if let Some(Cmd::TokenFile { path }) = &args.command {
        return write_token_file(path);
    }
    let cfg = load_config(&args)?;
    let rt = tokio::runtime::Runtime::new().context("start the runtime")?;
    match args.command {
        Some(Cmd::Check { json }) => rt.block_on(check(&cfg, json)),
        Some(Cmd::Run) | None => rt.block_on(run(cfg)),
        Some(Cmd::TokenFile { .. }) => Ok(()),
    }
}
