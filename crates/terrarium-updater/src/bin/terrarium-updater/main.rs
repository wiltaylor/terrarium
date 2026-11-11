//! `terrarium-updater`: the updater service (docs/api.md §34). It holds
//! docker.sock and listens on the updater socket. `serve` is the default;
//! `status`, `update`, and `rollback` talk to a running updater, for the
//! operator on the host (`docker compose exec terrarium-updater ...`).

use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use anyhow::Context;
use clap::{Parser, Subcommand};
use terrarium_updater::docker::Docker;
use terrarium_updater::machine::{Config, DEFAULT_REPOSITORY, Updater};
use terrarium_updater::protocol::{DEFAULT_SOCKET, ENV_SOCKET, Request, Response};
use terrarium_updater::registry::HttpRegistry;
use terrarium_updater::store::{DEFAULT_KEEP, Store};
use terrarium_updater::verify::{
    CommandVerifier, DEFAULT_COSIGN_KEY, DEFAULT_REPO, DEFAULT_WORKFLOW, ENV_COSIGN_KEY, ENV_REPO,
    ENV_VERIFY, ENV_WORKFLOW, Mode, Signer,
};
use terrarium_updater::{client, server};

#[derive(Debug, Parser)]
#[command(name = "terrarium-updater", version = terrarium_version::FULL, long_version = terrarium_version::LONG)]
struct Cli {
    /// The updater socket.
    #[arg(long, env = ENV_SOCKET, default_value = DEFAULT_SOCKET, global = true)]
    socket: PathBuf,
    #[command(subcommand)]
    command: Option<Cmd>,
}

#[derive(Debug, Subcommand)]
enum Cmd {
    /// Run the updater (the default).
    Serve(ServeArgs),
    /// Print the status of a running updater as JSON.
    Status,
    /// Ask a running updater for an update: a version, `latest`, or `pre`.
    Update { target: String },
    /// Ask a running updater to go back to the image before the last switch.
    Rollback,
}

#[derive(Debug, Default, clap::Args)]
struct ServeArgs {
    /// The Docker socket.
    #[arg(long, env = "TERRARIUM_UPDATER_DOCKER", default_value = terrarium_updater::docker::DEFAULT_SOCKET)]
    docker: PathBuf,
    /// The state dir: the history, the rollback target, and the image file.
    #[arg(long, env = "TERRARIUM_UPDATER_STATE", default_value = "/updater")]
    state: PathBuf,
    /// The name of the terrarium container. Default: the service
    /// `terrarium` of the compose project of this container.
    #[arg(long, env = "TERRARIUM_UPDATER_TARGET")]
    target: Option<String>,
    /// The image repository of the releases.
    #[arg(long, env = "TERRARIUM_UPDATE_IMAGE", default_value = DEFAULT_REPOSITORY)]
    image: String,
    /// The GitHub repo of the releases, for the signature identity.
    #[arg(long, env = ENV_REPO, default_value = DEFAULT_REPO)]
    repo: String,
    /// The image check: cosign (the release key), gh (the build provenance
    /// of the GitHub workflow), cosign-keyless, off, or command:<shell command>.
    #[arg(long, env = ENV_VERIFY, default_value = "cosign")]
    verify: String,
    /// The workflow in the repo that signed the releases (gh).
    #[arg(long, env = ENV_WORKFLOW, default_value = DEFAULT_WORKFLOW)]
    workflow: String,
    /// The release public key of the cosign check.
    #[arg(long, env = ENV_COSIGN_KEY, default_value = DEFAULT_COSIGN_KEY)]
    cosign_key: String,
    /// Seconds that the drain waits for the running turns.
    #[arg(long, env = "TERRARIUM_UPDATE_DRAIN_WAIT", default_value_t = 600)]
    drain_wait: u64,
    /// Seconds that the new container has to become healthy.
    #[arg(long, env = "TERRARIUM_UPDATE_HEALTH_TIMEOUT", default_value_t = 900)]
    health_timeout: u64,
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env().unwrap_or_else(|_| "info".into()),
        )
        .with_writer(std::io::stderr)
        .init();
    let cli = Cli::parse();
    match cli.command {
        None => serve(&cli.socket, ServeArgs::parse_env()?).await,
        Some(Cmd::Serve(a)) => serve(&cli.socket, a).await,
        Some(Cmd::Status) => ask(&cli.socket, Request::Status).await,
        Some(Cmd::Update { target }) => {
            ask(
                &cli.socket,
                Request::Update {
                    target,
                    actor: "operator:host".into(),
                },
            )
            .await
        }
        Some(Cmd::Rollback) => {
            ask(
                &cli.socket,
                Request::Rollback {
                    actor: "operator:host".into(),
                },
            )
            .await
        }
    }
}

impl ServeArgs {
    /// The serve arguments from the env alone (no subcommand).
    fn parse_env() -> anyhow::Result<Self> {
        #[derive(Parser)]
        struct Only {
            #[command(flatten)]
            a: ServeArgs,
        }
        Ok(Only::try_parse_from(["terrarium-updater"])?.a)
    }
}

async fn ask(socket: &std::path::Path, req: Request) -> anyhow::Result<()> {
    let r = client::call(socket, &req).await?;
    #[allow(clippy::print_stdout)]
    {
        println!("{}", serde_json::to_string_pretty(&r)?);
    }
    if let Response::Error { message, .. } = r {
        anyhow::bail!(message);
    }
    Ok(())
}

async fn serve(socket: &std::path::Path, a: ServeArgs) -> anyhow::Result<()> {
    let mode = Mode::parse(&a.verify).map_err(|e| anyhow::anyhow!("{ENV_VERIFY}: {e}"))?;
    if mode == Mode::Off {
        tracing::warn!("{ENV_VERIFY}=off: the updater does not check the signature of an image");
    }
    let updater = Arc::new(Updater::new(
        Arc::new(Docker {
            socket: a.docker,
            // compose.yaml passes an empty TERRARIUM_UPDATER_TARGET when .env has none.
            target: a.target.filter(|t| !t.trim().is_empty()),
        }),
        Arc::new(HttpRegistry::new().map_err(anyhow::Error::msg)?),
        Arc::new(CommandVerifier {
            mode,
            signer: Signer {
                repo: a.repo,
                workflow: a.workflow,
                key: a.cosign_key,
            },
        }),
        Store::new(a.state, DEFAULT_KEEP),
        Config {
            repository: a.image,
            drain_wait: Duration::from_secs(a.drain_wait),
            health_timeout: Duration::from_secs(a.health_timeout),
            health_poll: Duration::from_secs(2),
            version: terrarium_version::FULL.into(),
        },
    ));
    let listener =
        server::bind(socket).with_context(|| format!("cannot listen on {}", socket.display()))?;
    tracing::info!(socket = %socket.display(), version = terrarium_version::FULL, "the updater listens");
    // `docker stop` sends SIGTERM; a run in progress ends with the process
    // (the next start finds the container as the run left it).
    let mut term = tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())?;
    tokio::select! {
        () = server::serve(listener, socket, updater) => {}
        r = tokio::signal::ctrl_c() => { r?; }
        _ = term.recv() => {}
    }
    Ok(())
}
