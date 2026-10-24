//! Default paths and the env vars that override them.

use std::path::PathBuf;

/// Env var: the daemon's Unix socket path.
pub const ENV_SOCKET: &str = "TERRARIUM_API_SOCKET";
/// Env var: the state directory (SQLite, workspaces, artifacts).
pub const ENV_STATE_DIR: &str = "TERRARIUM_STATE_DIR";
/// Env var: the definitions directory (projects/, agents/, targets/, subscriptions/).
pub const ENV_CONFIG_DIR: &str = "TERRARIUM_CONFIG_DIR";
/// Env var: the bearer token for the TCP listener.
pub const ENV_TOKEN: &str = "TERRARIUM_TOKEN";
/// Env var: the daemon URL for the CLI, for example `http://host:8080`.
pub const ENV_URL: &str = "TERRARIUM_URL";
/// Env var: the TCP listen address of the daemon.
pub const ENV_LISTEN: &str = "TERRARIUM_LISTEN";
/// Env var, agent mode: the agent name or ID.
pub const ENV_AGENT: &str = "TERRARIUM_AGENT";
/// Env var, agent mode: the runner socket of the agent.
pub const ENV_AGENT_SOCKET: &str = "TERRARIUM_SOCKET";

pub const DEFAULT_STATE_DIR: &str = "/state";
pub const DEFAULT_CONFIG_DIR: &str = "/etc/terrarium";
pub const DEFAULT_LISTEN: &str = "0.0.0.0:8080";

fn env_path(key: &str) -> Option<PathBuf> {
    std::env::var_os(key)
        .filter(|v| !v.is_empty())
        .map(PathBuf::from)
}

/// The daemon socket: `$TERRARIUM_API_SOCKET`, else
/// `$XDG_RUNTIME_DIR/terrarium/api.sock`, else `/run/terrarium/api.sock`.
pub fn api_socket() -> PathBuf {
    env_path(ENV_SOCKET).unwrap_or_else(|| {
        env_path("XDG_RUNTIME_DIR")
            .map(|d| d.join("terrarium").join("api.sock"))
            .unwrap_or_else(|| PathBuf::from("/run/terrarium/api.sock"))
    })
}

pub fn state_dir() -> PathBuf {
    env_path(ENV_STATE_DIR).unwrap_or_else(|| PathBuf::from(DEFAULT_STATE_DIR))
}

pub fn config_dir() -> PathBuf {
    env_path(ENV_CONFIG_DIR).unwrap_or_else(|| PathBuf::from(DEFAULT_CONFIG_DIR))
}
