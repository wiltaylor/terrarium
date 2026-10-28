//! `terrarium`: the Terrarium CLI.
//!
//! Operator mode talks to the daemon over its Unix socket, or over TCP when
//! `TERRARIUM_URL` is set. Agent mode starts when `TERRARIUM_AGENT` and
//! `TERRARIUM_SOCKET` are both set: every API call then goes through the
//! runner socket as the `http` method (`docs/agent-rpc.md`). The same
//! commands run in both modes; the daemon decides what each caller may do.
//! `terrarium hook <event>` forwards a harness hook to the runner socket
//! without an async runtime.

mod api;
mod attach;
mod cli;
mod client;
mod commands;
mod credentials;
mod ctx;
mod exec_env;
mod output;
mod runner_socket;
mod tls;
mod transport;

use std::process::ExitCode;
use std::sync::Arc;

use clap::{CommandFactory, FromArgMatches};
use terrarium_core::paths;

use crate::api::{ServerError, exit, exit_code_of};
use crate::cli::{Cli, Command};
use crate::ctx::{AgentEnv, Ctx};
use crate::transport::{Endpoint, HttpTransport, RunnerTunnelTransport, Transport};

/// Build the context for the mode that the env selects.
fn context(cli: &Cli, agent: Option<AgentEnv>) -> anyhow::Result<Ctx> {
    let mut ctx = match agent {
        Some(env) => {
            let t: Arc<dyn Transport> = Arc::new(RunnerTunnelTransport::new(env.socket.clone()));
            Ctx::new(t, None, Some(env))
        }
        None => {
            let endpoint = match &cli.url {
                Some(url) if !url.is_empty() => Endpoint::from_url(url)?,
                _ => Endpoint::Unix(cli.socket.clone().unwrap_or_else(paths::api_socket)),
            };
            // TERRARIUM_TOKEN (or --token) wins; else the stored session
            // of `terrarium login` for this URL.
            let store = credentials::Store::default_location();
            let (token, source) =
                credentials::resolve(cli.token.clone(), &endpoint, store.as_ref());
            let http = HttpTransport::new(endpoint, token);
            let mut ctx = Ctx::new(Arc::new(http.clone()), Some(http), None);
            ctx.credentials = store;
            ctx.token_source = source;
            ctx
        }
    };
    ctx.json = cli.json;
    ctx.project = cli.project.clone();
    Ok(ctx)
}

fn report(err: &anyhow::Error, json: bool, source: credentials::TokenSource) {
    // The remote command printed its own errors.
    if matches!(err.downcast_ref(), Some(api::CliError::RemoteExit(_))) {
        return;
    }
    if let Some(se) = err.downcast_ref::<ServerError>() {
        if json {
            eprintln!("{}", se.body);
            return;
        }
        // The message is verbatim, so that an agent learns the rule.
        eprintln!("terrarium: {} [{}]", se.message, se.code);
        // A stale wiki base printed its diff already.
        if let Some(d) = se.details.as_ref().filter(|d| d.get("diff").is_none()) {
            eprintln!("details: {d}");
        }
        if se.is_missing_route() {
            eprintln!("terrarium: awaiting server: this daemon does not serve this endpoint yet");
        }
        if se.status == 401 && source == credentials::TokenSource::Stored {
            eprintln!("terrarium: {}", credentials::EXPIRED_HINT);
        }
        return;
    }
    if json {
        eprintln!(
            "{}",
            serde_json::json!({"error": {"code": "cli", "message": format!("{err:#}")}})
        );
    } else {
        eprintln!("terrarium: {err:#}");
    }
}

fn main() -> ExitCode {
    let agent = AgentEnv::from_env();
    let matches = cli::hide_for_mode(Cli::command(), agent.is_some()).get_matches();
    let cli = match Cli::from_arg_matches(&matches) {
        Ok(c) => c,
        Err(e) => e.exit(),
    };
    // A hook runs on every harness event. Keep it fast: no async runtime.
    if let Command::Hook { event } = &cli.command {
        return runner_socket::hook_main(event);
    }
    let runtime = match tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
    {
        Ok(rt) => rt,
        Err(e) => {
            eprintln!("terrarium: cannot start the runtime: {e}");
            return ExitCode::from(exit::SERVER);
        }
    };
    let mut source = credentials::TokenSource::None;
    let result = runtime.block_on(async {
        let ctx = context(&cli, agent)?;
        // A failed login is no expired session.
        if !matches!(cli.command, Command::Login { .. }) {
            source = ctx.token_source;
        }
        commands::run(&ctx, &cli.command).await
    });
    // A blocked stdin read (attach) must not hold the exit.
    runtime.shutdown_background();
    match result {
        Ok(()) => ExitCode::from(exit::OK),
        Err(e) => {
            report(&e, cli.json, source);
            ExitCode::from(exit_code_of(&e))
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cli_parses() {
        Cli::command().debug_assert();
    }

    #[test]
    fn help_hides_by_mode() {
        let names = |agent| {
            cli::hide_for_mode(Cli::command(), agent)
                .get_subcommands()
                .filter(|s| !s.is_hide_set())
                .map(|s| s.get_name().to_owned())
                .collect::<Vec<_>>()
        };
        let op = names(false);
        let ag = names(true);
        assert!(op.contains(&"daemon".to_owned()) && !op.contains(&"admin".to_owned()));
        assert!(ag.contains(&"admin".to_owned()) && !ag.contains(&"daemon".to_owned()));
        assert!(!op.contains(&"hook".to_owned()) && !ag.contains(&"hook".to_owned()));
        assert!(op.contains(&"login".to_owned()) && !ag.contains(&"login".to_owned()));
    }
}
