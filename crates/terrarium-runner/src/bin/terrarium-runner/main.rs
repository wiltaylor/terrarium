//! `terrarium-runner`: one agent runner in its own process (PLAN §6). It
//! holds the harness PTY, serves the hook socket `<run>/agents/<ulid>.sock`
//! and the control socket `<run>/runners/<ulid>.sock`, and keeps running
//! when the daemon goes away. See `docs/runner.md`.

mod adapters;

use std::path::PathBuf;
use std::time::Duration;

use anyhow::{Context, Result};
use clap::Parser;
use terrarium_core::AgentId;
use terrarium_runner::RunnerError;
use terrarium_runner::process::{self, ProcessOptions};
use terrarium_runner::spec::RunnerSpec;

/// The exit code when a live runner already holds the agent.
const EXIT_ALREADY_RUNNING: i32 = 3;

#[derive(Debug, Parser)]
#[command(
    name = "terrarium-runner",
    version,
    about = "The Terrarium agent runner process"
)]
struct Args {
    /// The agent ULID.
    #[arg(long)]
    agent: AgentId,
    /// The state root: the runner writes to `<dir>/agents/<ulid>/`.
    #[arg(long)]
    state_dir: PathBuf,
    /// The runtime root: the sockets are under `<dir>/agents/` and
    /// `<dir>/runners/`.
    #[arg(long)]
    runtime_dir: PathBuf,
    /// The JSON spec file: the launch spec, the runner settings, the harness.
    #[arg(long)]
    spec: PathBuf,
    /// A JSON array of strings. It replaces the program and the arguments of
    /// the launch spec, for example with the full `bwrap` argv.
    #[arg(long)]
    sandbox_argv_file: Option<PathBuf>,
    /// How long to wait, after the harness exits, for the daemon to read
    /// the last frames.
    #[arg(long, default_value_t = 2000)]
    linger_ms: u64,
}

fn main() {
    let code = match real_main() {
        Ok(code) => code,
        Err(e) => {
            if let Some(RunnerError::AlreadyRunning { .. }) = e.downcast_ref::<RunnerError>() {
                eprintln!("terrarium-runner: {e:#}");
                EXIT_ALREADY_RUNNING
            } else {
                eprintln!("terrarium-runner: {e:#}");
                1
            }
        }
    };
    std::process::exit(code);
}

fn real_main() -> Result<i32> {
    let args = Args::parse();
    // Leave the session of the daemon, so that a signal to its process
    // group (Ctrl-C in a terminal) does not reach the runner. This fails
    // when the process already leads a group; that is fine.
    let _ = nix::unistd::setsid();
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_env("TERRARIUM_LOG")
                .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("info")),
        )
        .with_writer(std::io::stderr)
        .with_ansi(false)
        .init();

    let mut spec = RunnerSpec::read(&args.spec)?;
    if let Some(path) = &args.sandbox_argv_file {
        // No symlink follow: the file is in `private/`
        // (`docs/security-review.md`, F11).
        let text = read_nofollow(path).with_context(|| format!("read {}", path.display()))?;
        let argv: Vec<String> =
            serde_json::from_str(&text).with_context(|| format!("parse {}", path.display()))?;
        spec.set_argv(argv)?;
    }
    let adapter = adapters::select(&spec);
    let rt = tokio::runtime::Runtime::new().context("start the tokio runtime")?;
    let status = rt.block_on(process::run(
        ProcessOptions {
            agent: args.agent,
            state_dir: args.state_dir,
            runtime_dir: args.runtime_dir,
            spec,
            linger: Duration::from_millis(args.linger_ms),
        },
        adapter,
    ))?;
    // The exit code of the runner reflects the harness: its code, or 128
    // plus the signal. The full status is in `runner-exit.json`.
    tracing::info!(code = status.code, signal = ?status.signal, "runner done");
    Ok(status.exit_code())
}

/// Read `path` as text with `O_NOFOLLOW`: a symlink fails with `ELOOP`.
fn read_nofollow(path: &std::path::Path) -> std::io::Result<String> {
    use std::io::Read;
    use std::os::unix::fs::OpenOptionsExt;
    let mut text = String::new();
    std::fs::OpenOptions::new()
        .read(true)
        .custom_flags(nix::fcntl::OFlag::O_NOFOLLOW.bits())
        .open(path)?
        .read_to_string(&mut text)?;
    Ok(text)
}
