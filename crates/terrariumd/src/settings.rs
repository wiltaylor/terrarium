use std::path::PathBuf;

use clap::{Parser, Subcommand, ValueEnum};
use terrarium_core::{HarnessId, paths};

use crate::runner_supervisor::RunnerMode;
use crate::sandbox::SandboxMode;

/// The Terrarium daemon. Every option also reads an env var.
#[derive(Debug, Parser)]
#[command(name = "terrariumd", version, about)]
pub struct Settings {
    /// The Unix socket for the operator CLI. No token is needed on it.
    #[arg(long, env = paths::ENV_SOCKET, default_value_os_t = paths::api_socket())]
    pub socket: PathBuf,

    /// The TCP address for the dashboard and the remote API.
    #[arg(long, env = paths::ENV_LISTEN, default_value = paths::DEFAULT_LISTEN)]
    pub listen: String,

    /// The bearer token for the TCP listener. Without it the TCP listener stays off.
    #[arg(long, env = paths::ENV_TOKEN, hide_env_values = true)]
    pub token: Option<String>,

    /// SQLite, workspaces, logs, and artifacts.
    #[arg(long, env = paths::ENV_STATE_DIR, default_value = paths::DEFAULT_STATE_DIR)]
    pub state_dir: PathBuf,

    /// Definitions: projects/, agents/, targets/, subscriptions/.
    #[arg(long, env = paths::ENV_CONFIG_DIR, default_value = paths::DEFAULT_CONFIG_DIR)]
    pub config_dir: PathBuf,

    /// The TCP address of the artifact serving origin (PLAN §15.2). Empty:
    /// no listener of its own, and the dashboard port serves `/a/...`
    /// instead. With a listener (the default), the dashboard port does not
    /// serve artifacts (`docs/security-review.md`, F5). Set it with the env
    /// var: the API router reads the env (see [`artifact_listen`]).
    #[arg(
        long,
        env = "TERRARIUM_ARTIFACT_LISTEN",
        default_value = "0.0.0.0:8081"
    )]
    pub artifact_listen: String,

    /// Runner sockets go in `<dir>/agents/<ulid>.sock`.
    #[arg(long, env = "TERRARIUM_RUNTIME_DIR", default_value_os_t = default_runtime_dir())]
    pub runtime_dir: PathBuf,

    /// The prompt templates, `<dir>/<name>.md`.
    #[arg(long, env = "TERRARIUM_PROMPTS_DIR", default_value = "/prompts")]
    pub prompts_dir: PathBuf,

    /// How harnesses run: `bwrap` (the sandbox of PLAN §11) or `none`
    /// (development and tests only).
    #[arg(long, env = "TERRARIUM_SANDBOX", default_value = "bwrap")]
    pub sandbox: SandboxMode,

    /// Pass `--new-session` to bwrap. Off by default, because the harness
    /// then misses terminal resizes (docs/compat.md).
    #[arg(long, env = "TERRARIUM_SANDBOX_NEW_SESSION")]
    pub sandbox_new_session: bool,

    /// The shared home of a harness on this machine, as `<harness>=<dir>`.
    /// The default is `/auth/<harness>`. Repeat it, or use commas.
    #[arg(
        long = "harness-home",
        env = "TERRARIUM_HARNESS_HOME",
        value_delimiter = ',',
        value_parser = parse_home
    )]
    pub harness_homes: Vec<(HarnessId, PathBuf)>,

    /// The `terrarium` CLI that the sandbox binds for hooks and agent
    /// commands. The default is `terrarium` next to this binary, else on PATH.
    #[arg(long, env = "TERRARIUM_CLI")]
    pub cli: Option<PathBuf>,

    /// How agent runners run: `process` (one `terrarium-runner` process per
    /// agent; agents live through a daemon restart) or `inprocess` (in the
    /// daemon; the agents end with it; tests and development only).
    #[arg(long, env = "TERRARIUM_RUNNER_MODE", value_enum, default_value_t = RunnerModeName::Process)]
    pub runner_mode: RunnerModeName,

    /// The `terrarium-runner` binary for `--runner-mode process`. The
    /// default is `terrarium-runner` next to this binary, else on PATH.
    #[arg(long, env = "TERRARIUM_RUNNER")]
    pub runner: Option<PathBuf>,

    /// Trust the `X-Terrarium-Agent` header on the Unix socket. Any local
    /// caller can then act as any agent. For tests and development only.
    #[arg(long, env = "TERRARIUM_INSECURE_AGENT_HEADER")]
    pub insecure_agent_header: bool,

    #[command(subcommand)]
    pub command: Option<Command>,
}

/// The values of `--runner-mode`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, ValueEnum)]
pub enum RunnerModeName {
    Process,
    Inprocess,
}

/// Debug commands. Without one, the daemon serves.
#[derive(Debug, Subcommand)]
pub enum Command {
    /// Print the full sandbox command for an agent, and exit. It writes the
    /// agent's prompt and harness files, as a start does, but starts nothing.
    SandboxDryRun {
        /// The agent: a ULID, `project/name`, or a unique name.
        agent: String,
    },
}

fn default_runtime_dir() -> PathBuf {
    std::env::var_os("XDG_RUNTIME_DIR")
        .filter(|v| !v.is_empty())
        .map(|d| PathBuf::from(d).join("terrarium"))
        .unwrap_or_else(|| PathBuf::from("/run/terrarium"))
}

fn parse_home(s: &str) -> Result<(HarnessId, PathBuf), String> {
    let (h, dir) = s
        .split_once('=')
        .ok_or_else(|| format!("`{s}` is not `<harness>=<dir>`"))?;
    Ok((h.parse()?, PathBuf::from(dir)))
}

/// `program` next to the running binary, else on PATH.
fn sibling_or_path(program: &str) -> Option<PathBuf> {
    let sibling = std::env::current_exe()
        .ok()
        .and_then(|e| e.parent().map(|d| d.join(program)))
        .filter(|p| p.is_file());
    sibling.or_else(|| crate::sandbox::which(program, std::env::var("PATH").ok().as_deref()))
}

impl Settings {
    /// The runner mode. This is the one place that picks it
    /// (`TERRARIUM_RUNNER_MODE`, `TERRARIUM_RUNNER`).
    pub fn runner_mode(&self) -> Result<RunnerMode, String> {
        match self.runner_mode {
            RunnerModeName::Inprocess => Ok(RunnerMode::InProcess),
            RunnerModeName::Process => {
                let bin = match &self.runner {
                    Some(p) => p.clone(),
                    None => sibling_or_path("terrarium-runner").ok_or_else(|| {
                        "no `terrarium-runner` binary next to terrariumd or on PATH; build it \
                         (`cargo build -p terrarium-runner`), set TERRARIUM_RUNNER, or set \
                         TERRARIUM_RUNNER_MODE=inprocess for development"
                            .to_owned()
                    })?,
                };
                if !bin.is_file() {
                    return Err(format!(
                        "the runner binary {} does not exist; set TERRARIUM_RUNNER",
                        bin.display()
                    ));
                }
                Ok(RunnerMode::Process { bin })
            }
        }
    }

    /// The `terrarium` CLI to bind: `--cli`, else next to this binary, else
    /// on PATH.
    pub fn cli_path(&self) -> Option<PathBuf> {
        if let Some(p) = &self.cli {
            return Some(p.clone());
        }
        sibling_or_path("terrarium")
    }
}

// ---- settings that the daemon reads from the env where it needs them ----
//
// These are not `Settings` fields: the code that uses them (the API router,
// the mail bodies) does not get the parsed `Settings`. Each one reads its
// env var when it is called, so a test can pass a value in directly.

/// `TERRARIUM_PUBLIC_URL`: the dashboard as the operator opens it, for the
/// links in operator mail.
pub const ENV_PUBLIC_URL: &str = "TERRARIUM_PUBLIC_URL";
/// The default of [`ENV_PUBLIC_URL`].
pub const DEFAULT_PUBLIC_URL: &str = "http://localhost:8080";
/// `TERRARIUM_ARTIFACT_LISTEN`, also the `--artifact-listen` flag.
pub const ENV_ARTIFACT_LISTEN: &str = "TERRARIUM_ARTIFACT_LISTEN";
/// The default of [`ENV_ARTIFACT_LISTEN`].
pub const DEFAULT_ARTIFACT_LISTEN: &str = "0.0.0.0:8081";
/// `TERRARIUM_ARTIFACT_PUBLIC_URL`: the artifact origin as a browser opens
/// it, for signed artifact URLs.
pub const ENV_ARTIFACT_PUBLIC_URL: &str = "TERRARIUM_ARTIFACT_PUBLIC_URL";
/// `TERRARIUM_WS_QUERY_TOKEN`: `0` refuses the bearer token in `?token=`.
pub const ENV_WS_QUERY_TOKEN: &str = "TERRARIUM_WS_QUERY_TOKEN";

fn env_nonempty(key: &str) -> Option<String> {
    std::env::var(key)
        .ok()
        .map(|v| v.trim().to_owned())
        .filter(|v| !v.is_empty())
}

/// The dashboard URL for operator mail, with no trailing `/`.
pub fn public_url() -> String {
    env_nonempty(ENV_PUBLIC_URL)
        .unwrap_or_else(|| DEFAULT_PUBLIC_URL.to_owned())
        .trim_end_matches('/')
        .to_owned()
}

/// A dashboard link: [`public_url`] and `path` (which starts with `/`).
pub fn dashboard_link(path: &str) -> String {
    format!("{}{path}", public_url())
}

/// The artifact listener address: the env value (empty is allowed and
/// means "no listener of its own"), else the default.
pub fn artifact_listen() -> String {
    std::env::var(ENV_ARTIFACT_LISTEN)
        .map(|v| v.trim().to_owned())
        .unwrap_or_else(|_| DEFAULT_ARTIFACT_LISTEN.to_owned())
}

/// The explicit artifact origin URL, with no trailing `/`, when it is set.
pub fn artifact_public_url() -> Option<String> {
    env_nonempty(ENV_ARTIFACT_PUBLIC_URL).map(|u| u.trim_end_matches('/').to_owned())
}

/// May a WebSocket upgrade carry the bearer token in `?token=`? Yes,
/// unless `TERRARIUM_WS_QUERY_TOKEN=0` (or `false`, `no`, `off`).
pub fn ws_query_token_allowed() -> bool {
    !matches!(
        env_nonempty(ENV_WS_QUERY_TOKEN)
            .map(|v| v.to_ascii_lowercase())
            .as_deref(),
        Some("0" | "false" | "no" | "off")
    )
}
