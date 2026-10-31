//! `terrarium-host`: the host agent binary (PLAN §19).
//!
//! `run` dials the daemon and serves sessions until a signal stops it.
//! `check` prints the detected capabilities and the TLS mode; `--probe`
//! also does a TLS handshake with the daemon. `token-file` writes a token
//! from stdin to a file with mode 0600. `cert` makes a self-signed
//! certificate for the daemon's host listener; `fingerprint` prints the
//! SHA-256 of a certificate to pin. `rdp-forget` removes the pinned
//! certificate of an RDP target.

use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::time::Duration;

use anyhow::{Context, bail};
use clap::{Parser, Subcommand};
use terrarium_host::config::{self, Config, DEFAULT_CONFIG_PATH};
use terrarium_host::{Client, ClientOptions, capabilities, tls};

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
        /// Also dial the daemon and do the TLS handshake (no upgrade, no
        /// token, no hello). Exits 1 when the handshake fails.
        #[arg(long)]
        probe: bool,
    },
    /// Read a token from stdin and write it to PATH with mode 0600.
    TokenFile {
        /// Where to write the token, for example /etc/terrarium-host/token.
        path: PathBuf,
    },
    /// Make a self-signed certificate and key for the daemon's host
    /// listener (TERRARIUM_HOST_TLS_CERT, TERRARIUM_HOST_TLS_KEY), and print
    /// the SHA-256 fingerprint to pin.
    Cert {
        /// The DNS names or IP addresses that host agents dial. The first
        /// is the common name.
        #[arg(required = true)]
        names: Vec<String>,
        /// The directory for host-cert.pem and host-key.pem.
        #[arg(long, default_value = "config/tls")]
        out: PathBuf,
        /// Replace existing files.
        #[arg(long)]
        force: bool,
    },
    /// Print the SHA-256 fingerprint of the first certificate in a PEM file.
    Fingerprint {
        /// A PEM certificate, for example config/tls/host-cert.pem.
        cert: PathBuf,
    },
    /// Remove the pinned TLS certificate of an RDP target, after the
    /// server got a new certificate on purpose. The next connection pins
    /// the new one. With no address: list the pins.
    RdpForget {
        /// The target's address as the pin shows it, for example
        /// 10.0.0.5:3389.
        address: Option<String>,
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

fn write_token_file(path: &Path) -> anyhow::Result<()> {
    let mut token = String::new();
    std::io::stdin()
        .read_to_string(&mut token)
        .context("read the token from stdin")?;
    let token = token.trim();
    if token.is_empty() {
        bail!("no token on stdin");
    }
    config::write_secret(path, token).with_context(|| format!("write {}", path.display()))?;
    eprintln!("wrote {}", path.display());
    Ok(())
}

fn write_cert(names: &[String], out: &Path, force: bool) -> anyhow::Result<()> {
    let cert = out.join("host-cert.pem");
    let key = out.join("host-key.pem");
    if !force && (cert.exists() || key.exists()) {
        bail!(
            "{} or {} exists; pass --force to replace it",
            cert.display(),
            key.display()
        );
    }
    let ss = tls::self_signed(names)?;
    std::fs::create_dir_all(out).with_context(|| format!("create {}", out.display()))?;
    std::fs::write(&cert, &ss.cert_pem).with_context(|| format!("write {}", cert.display()))?;
    config::write_secret(&key, &ss.key_pem).with_context(|| format!("write {}", key.display()))?;
    let mut o = std::io::stdout().lock();
    writeln!(o, "wrote {} and {}", cert.display(), key.display())?;
    writeln!(o, "names: {}", names.join(", "))?;
    writeln!(o, "server_fingerprint_sha256 = \"{}\"", ss.fingerprint)?;
    Ok(())
}

fn rdp_forget(cfg: &Config, address: Option<&str>) -> anyhow::Result<()> {
    let store = terrarium_host::display::rdp::PinStore::new(cfg.display.rdp_known_hosts_path());
    let path = store.path().display().to_string();
    let mut o = std::io::stdout().lock();
    let Some(address) = address else {
        let pins = store.list().with_context(|| format!("read {path}"))?;
        if pins.is_empty() {
            writeln!(o, "no RDP certificate pins in {path}")?;
        }
        for (a, f) in pins {
            writeln!(o, "{a} {f}")?;
        }
        return Ok(());
    };
    // The pins use the address with its port.
    let address = terrarium_host::display::remote::address(address, 3389)
        .map_err(|e| anyhow::anyhow!("{e}"))?;
    if store
        .forget(&address)
        .with_context(|| format!("update {path}"))?
    {
        writeln!(o, "removed the pin of {address} from {path}")?;
    } else {
        writeln!(o, "{address} has no pin in {path}")?;
    }
    Ok(())
}

fn fingerprint(cert: &Path) -> anyhow::Result<()> {
    writeln!(std::io::stdout(), "{}", tls::cert_file_fingerprint(cert)?)?;
    Ok(())
}

async fn check(cfg: &Config, json: bool, probe: bool) -> anyhow::Result<()> {
    let caps = capabilities::detect_with(&cfg.docker_bin, &cfg.vmlab_bin, &cfg.display).await;
    let paths = cfg.paths();
    let transport = cfg.daemon_url.as_ref().map(|_| cfg.transport());
    let (tls_mode, tls_error) = match &transport {
        Some(Ok(t)) => (Some(t.mode.clone()), None),
        Some(Err(e)) => (None, Some(e.to_string())),
        None => (None, None),
    };
    let probed = match (&transport, probe) {
        (Some(Ok(t)), true) => Some(tls::probe(t, Duration::from_secs(10)).await),
        _ => None,
    };
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
        "work_dir": paths.work_dir,
        "instances_dir": paths.instances_dir,
        "repos_dir": paths.repos_dir,
        "max_running_targets": cfg.max_running_targets,
        "rdp_known_hosts": cfg.display.rdp_known_hosts_path(),
        "capabilities": caps,
        "tls": tls_mode,
        "tls_error": tls_error,
        "probe": probed,
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
        let d = &caps.display;
        writeln!(
            out,
            "display:    desktop {}{}; ocr {}; tools {}",
            if d.enabled { "on" } else { "off" },
            if d.allowed_displays.is_empty() {
                String::new()
            } else {
                format!(" ({})", d.allowed_displays.join(", "))
            },
            if d.tesseract { "tesseract" } else { "none" },
            if d.tools.is_empty() {
                "none".to_owned()
            } else {
                d.tools.join(" ")
            }
        )?;
        match &cfg.allowed_cwd_roots {
            Some(r) => writeln!(out, "cwd roots:  {r:?}")?,
            None => writeln!(out, "cwd roots:  any")?,
        }
        let mark = |p: &std::path::Path| if p.is_dir() { "" } else { " (missing)" };
        writeln!(
            out,
            "work dir:   {}{}",
            paths.work_dir.display(),
            mark(&paths.work_dir)
        )?;
        writeln!(
            out,
            "instances:  {}{}",
            paths.instances_dir.display(),
            mark(&paths.instances_dir)
        )?;
        writeln!(
            out,
            "repos dir:  {}{}",
            paths.repos_dir.display(),
            mark(&paths.repos_dir)
        )?;
        writeln!(out, "max running targets: {}", cfg.max_running_targets)?;
        writeln!(
            out,
            "rdp pins:   {}",
            cfg.display.rdp_known_hosts_path().display()
        )?;
        match (&tls_mode, &tls_error) {
            (Some(m), _) => writeln!(out, "tls:        {}", m.describe())?,
            (None, Some(e)) => writeln!(out, "tls:        refused: {e}")?,
            (None, None) => writeln!(out, "tls:        no daemon_url")?,
        }
        if let Some(p) = &probed {
            if p.ok {
                let what = match (&p.tls_version, &p.cipher_suite) {
                    (Some(v), Some(c)) => format!("TLS ok ({v}, {c})"),
                    _ => "TCP ok (no TLS)".to_owned(),
                };
                writeln!(
                    out,
                    "probe:      {} {what} in {} ms",
                    p.address, p.elapsed_ms
                )?;
                if let Some(fp) = &p.server_fingerprint_sha256 {
                    writeln!(out, "server:     SHA-256 {fp}")?;
                }
            } else {
                writeln!(
                    out,
                    "probe:      {} failed: {}",
                    p.address,
                    p.error.as_deref().unwrap_or("unknown error")
                )?;
            }
        } else if probe && tls_error.is_none() && tls_mode.is_none() {
            writeln!(out, "probe:      skipped: no daemon_url")?;
        }
    }
    if probe && !probed.as_ref().is_some_and(|p| p.ok) {
        drop(out);
        std::process::exit(1);
    }
    Ok(())
}

async fn run(cfg: Config) -> anyhow::Result<()> {
    let transport = cfg.transport()?;
    let token = cfg.resolve_token()?;
    let name = cfg.resolve_name();
    let caps = capabilities::detect_with(&cfg.docker_bin, &cfg.vmlab_bin, &cfg.display).await;
    let url = transport.url.clone();
    tracing::info!(%url, %name, tls = %transport.mode.describe(), docker = caps.docker.present, vmlab = caps.vmlab.present, desktop_display = caps.display.enabled, "starting");
    if caps.display.enabled {
        tracing::warn!(
            "[display] enabled = true: an agent with an exclusive lease on a desktop target \
             can see this machine's screen and use its mouse and keyboard as this user"
        );
    }
    match &caps.limits {
        Some(l) if l.systemd => {
            tracing::info!(controllers = %l.controllers.join(" "), "resource limits: systemd-run --user works");
            terrarium_host::limits::kill_leftovers(&name).await;
        }
        Some(l) => tracing::info!(
            reason = l.reason.as_deref().unwrap_or(""),
            "resource limits: off on host targets; a session on a host target with limits is refused"
        ),
        None => {}
    }
    if transport.mode == tls::TlsMode::PlainInsecure {
        tracing::warn!(
            %url,
            "allow_insecure = true: the host token and all exec traffic cross the network \
             in clear text, and anyone on the path can replay the token; use wss://"
        );
    }
    let mut opts = ClientOptions::new(url, token, name);
    opts.capabilities = caps;
    opts.max_sessions = cfg.max_sessions;
    opts.buffer_limit = cfg.buffer_limit_bytes;
    opts.resume_grace = Duration::from_secs(cfg.resume_grace_secs);
    opts.policy = cfg.policy();
    opts.docker_bin = cfg.docker_bin.clone();
    opts.vmlab_bin = cfg.vmlab_bin.clone();
    opts.git_bin = cfg.git_bin.clone();
    opts.paths = cfg.paths();
    opts.max_running_targets = cfg.max_running_targets;
    tracing::info!(work_dir = %opts.paths.work_dir.display(), instances_dir = %opts.paths.instances_dir.display(), repos_dir = %opts.paths.repos_dir.display(), "workspace and lab paths");
    opts.tls = transport.tls.clone();
    opts.allow_insecure = transport.mode == tls::TlsMode::PlainInsecure;
    opts.token_file = cfg.token_file.clone();
    opts.display = cfg.display.clone();
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
    match &args.command {
        Some(Cmd::TokenFile { path }) => return write_token_file(path),
        Some(Cmd::Cert { names, out, force }) => return write_cert(names, out, *force),
        Some(Cmd::Fingerprint { cert }) => return fingerprint(cert),
        _ => {}
    }
    let cfg = load_config(&args)?;
    if let Some(Cmd::RdpForget { address }) = &args.command {
        return rdp_forget(&cfg, address.as_deref());
    }
    let rt = tokio::runtime::Runtime::new().context("start the runtime")?;
    match args.command {
        Some(Cmd::Check { json, probe }) => rt.block_on(check(&cfg, json, probe)),
        Some(Cmd::Run) | None => rt.block_on(run(cfg)),
        Some(
            Cmd::TokenFile { .. }
            | Cmd::Cert { .. }
            | Cmd::Fingerprint { .. }
            | Cmd::RdpForget { .. },
        ) => Ok(()),
    }
}
