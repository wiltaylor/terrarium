use std::path::PathBuf;

use clap::Parser;
use terrarium_core::paths;

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
}
