//! Shared daemon/host logging policy for credential-bearing transports.

use tracing::{Level, Metadata, Subscriber};
use tracing_subscriber::{EnvFilter, fmt::MakeWriter, prelude::*};

const TRANSPORTS: &[&str] = &[
    "tungstenite",
    "tokio_tungstenite",
    "hyper",
    "hyper_util",
    "h2",
    "axum",
    "reqwest",
    "tower_http",
];

fn safe_transport_level(metadata: &Metadata<'_>) -> bool {
    // WebSocket/HTTP dependency TRACE logs can dump complete frames (including
    // reversible hex), which carry session passwords and authentication tokens.
    // This independent filter also gates the log-to-tracing bridge's enabled
    // check. A more-specific RUST_LOG directive must never override the cap.
    *metadata.level() <= Level::INFO
        || !TRANSPORTS.iter().any(|name| {
            metadata.target() == *name
                || metadata
                    .target()
                    .strip_prefix(*name)
                    .is_some_and(|tail| tail.starts_with("::"))
        })
}

/// Production subscriber. `SubscriberInitExt::init` installs it globally and
/// installs the log-to-tracing bridge, so dependency `log` records use the same
/// filters. Tests use this exact setup with RUST_LOG=trace in a child process.
pub fn subscriber<W>(writer: W, ansi: bool) -> impl Subscriber + Send + Sync
where
    W: for<'a> MakeWriter<'a> + Send + Sync + 'static,
{
    let mut env = EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("info"));
    for name in TRANSPORTS {
        // Fixed directives follow the environment filter. The independent
        // metadata filter above covers more-specific environment directives.
        env = env.add_directive(
            format!("{name}=info")
                .parse()
                .expect("fixed logging directive"),
        );
    }
    tracing_subscriber::registry()
        .with(tracing_subscriber::filter::filter_fn(safe_transport_level))
        .with(env)
        .with(
            tracing_subscriber::fmt::layer()
                .with_writer(writer)
                .with_ansi(ansi),
        )
}
