//! `terrarium`: the Terrarium CLI.
//!
//! Operator mode talks to the daemon over its Unix socket, or over TCP when
//! `TERRARIUM_URL` is set. Agent mode starts when `TERRARIUM_AGENT` is set:
//! the CLI then talks to the agent's runner socket (`TERRARIUM_SOCKET`).
//! Agent mode has only a `whoami` stub in this build.

mod client;

use std::path::PathBuf;
use std::process::ExitCode;

use clap::{Parser, Subcommand};
use serde::Deserialize;
use terrarium_core::{Agent, paths};

use crate::client::{Client, Endpoint};

#[derive(Debug, Parser)]
#[command(name = "terrarium", version, about = "The Terrarium CLI")]
struct Cli {
    /// The daemon URL, for example `http://localhost:8080`. Without it the CLI uses the Unix socket.
    #[arg(long, global = true, env = paths::ENV_URL)]
    url: Option<String>,

    /// The bearer token for a TCP daemon URL.
    #[arg(long, global = true, env = paths::ENV_TOKEN, hide_env_values = true)]
    token: Option<String>,

    /// The daemon's Unix socket.
    #[arg(long, global = true, env = paths::ENV_SOCKET)]
    socket: Option<PathBuf>,

    /// Print JSON instead of text.
    #[arg(long, global = true)]
    json: bool,

    #[command(subcommand)]
    command: Command,
}

#[derive(Debug, Subcommand)]
enum Command {
    /// Daemon commands
    Daemon {
        #[command(subcommand)]
        command: DaemonCommand,
    },
    /// Agent commands
    Agent {
        #[command(subcommand)]
        command: AgentCommand,
    },
    /// Show who this CLI acts as
    Whoami,
}

#[derive(Debug, Subcommand)]
enum DaemonCommand {
    /// Check that the daemon runs; exit non-zero when it does not
    Status,
}

#[derive(Debug, Subcommand)]
enum AgentCommand {
    /// List agents
    Ls {
        /// Only agents in this project
        #[arg(long)]
        project: Option<String>,
    },
}

/// The fields of `GET /api/health` that the CLI shows.
#[derive(Debug, Deserialize, serde::Serialize)]
struct Health {
    version: String,
    uptime: u64,
}

/// The agent-mode context, from the env that the runner sets.
#[derive(Debug)]
struct AgentContext {
    agent: String,
    socket: Option<PathBuf>,
}

impl AgentContext {
    fn from_env() -> Option<Self> {
        let agent = std::env::var(paths::ENV_AGENT)
            .ok()
            .filter(|v| !v.is_empty())?;
        Some(Self {
            agent,
            socket: std::env::var_os(paths::ENV_AGENT_SOCKET).map(PathBuf::from),
        })
    }
}

fn operator_client(cli: &Cli) -> anyhow::Result<Client> {
    let endpoint = match &cli.url {
        Some(url) if !url.is_empty() => Endpoint::from_url(url)?,
        _ => Endpoint::Unix(cli.socket.clone().unwrap_or_else(paths::api_socket)),
    };
    let token = cli.token.clone().filter(|t| !t.is_empty());
    Ok(Client::new(endpoint, token))
}

fn format_uptime(secs: u64) -> String {
    let (d, h, m, s) = (secs / 86_400, secs / 3600 % 24, secs / 60 % 60, secs % 60);
    match (d, h, m) {
        (0, 0, 0) => format!("{s}s"),
        (0, 0, _) => format!("{m}m{s}s"),
        (0, _, _) => format!("{h}h{m}m"),
        _ => format!("{d}d{h}h"),
    }
}

async fn run(cli: Cli) -> anyhow::Result<()> {
    let agent_ctx = AgentContext::from_env();
    match &cli.command {
        Command::Whoami => {
            match agent_ctx {
                Some(ctx) => {
                    // Agent mode. The runner socket answers this in Phase 2.
                    let socket = ctx
                        .socket
                        .map(|p| p.display().to_string())
                        .unwrap_or_else(|| "unset".into());
                    println!("agent: {} (runner socket: {socket})", ctx.agent);
                    println!("agent mode is a stub: project and harness come in Phase 2");
                }
                None => println!("operator"),
            }
            Ok(())
        }
        _ if agent_ctx.is_some() => {
            anyhow::bail!("this command is not available in agent mode yet")
        }
        Command::Daemon {
            command: DaemonCommand::Status,
        } => {
            let client = operator_client(&cli)?;
            let health: serde_json::Value = client.get_json("/api/health").await?;
            if cli.json {
                println!("{}", serde_json::to_string_pretty(&health)?);
            } else {
                let h: Health = serde_json::from_value(health)?;
                println!(
                    "terrariumd {} running at {} (up {})",
                    h.version,
                    client.endpoint,
                    format_uptime(h.uptime)
                );
            }
            Ok(())
        }
        Command::Agent {
            command: AgentCommand::Ls { project },
        } => {
            let client = operator_client(&cli)?;
            let path = match project {
                Some(p) => format!("/api/agents?project={}", encode_query(p)),
                None => "/api/agents".to_owned(),
            };
            let agents: Vec<Agent> = client.get_json(&path).await?;
            if cli.json {
                println!("{}", serde_json::to_string_pretty(&agents)?);
            } else if agents.is_empty() {
                println!("no agents");
            } else {
                println!(
                    "{:<26}  {:<16} {:<16} {:<8} STATUS",
                    "ID", "NAME", "PROJECT", "HARNESS"
                );
                for a in agents {
                    println!(
                        "{:<26}  {:<16} {:<16} {:<8} {}",
                        a.id, a.name, a.project, a.harness, a.status
                    );
                }
            }
            Ok(())
        }
    }
}

/// Percent-encode a query value. Project names are simple, but be safe.
fn encode_query(s: &str) -> String {
    s.bytes()
        .map(|b| match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => {
                (b as char).to_string()
            }
            _ => format!("%{b:02X}"),
        })
        .collect()
}

#[tokio::main(flavor = "current_thread")]
async fn main() -> ExitCode {
    let cli = Cli::parse();
    match run(cli).await {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => {
            eprintln!("terrarium: {e:#}");
            ExitCode::FAILURE
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn uptime_formats() {
        assert_eq!(format_uptime(5), "5s");
        assert_eq!(format_uptime(65), "1m5s");
        assert_eq!(format_uptime(3_700), "1h1m");
        assert_eq!(format_uptime(90_000), "1d1h");
    }

    #[test]
    fn query_encodes() {
        assert_eq!(encode_query("my proj/x"), "my%20proj%2Fx");
    }

    #[test]
    fn cli_parses() {
        use clap::CommandFactory;
        Cli::command().debug_assert();
    }
}
