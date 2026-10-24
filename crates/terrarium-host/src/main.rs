//! `terrarium-host`: the host agent (PLAN §19).
//!
//! The host agent dials out to the daemon over WebSocket and runs the
//! executor protocol on this machine. This build has the argument skeleton
//! only. Networking comes in Phase 5.

use clap::Parser;

/// The Terrarium host agent.
#[derive(Debug, Parser)]
#[command(name = "terrarium-host", version, about)]
struct Args {
    /// The daemon URL to dial, for example `ws://terrarium.lan:8082`.
    #[arg(long, env = "TERRARIUM_DAEMON_URL")]
    daemon_url: Option<String>,

    /// The per-host token that the daemon minted for this host.
    #[arg(long, env = "TERRARIUM_HOST_TOKEN", hide_env_values = true)]
    token: Option<String>,

    /// The name of this host, as targets refer to it. The default is the hostname.
    #[arg(long, env = "TERRARIUM_HOST_NAME")]
    name: Option<String>,
}

fn main() -> anyhow::Result<()> {
    let args = Args::parse();
    println!("terrarium-host {}", terrarium_core::VERSION);
    match &args.daemon_url {
        Some(url) => println!("daemon: {url}"),
        None => println!("daemon: not set (use --daemon-url)"),
    }
    println!(
        "token: {}",
        if args.token.is_some() {
            "set"
        } else {
            "not set"
        }
    );
    if let Some(name) = &args.name {
        println!("name: {name}");
    }
    println!("The host agent does not connect yet. Networking comes in Phase 5.");
    Ok(())
}
