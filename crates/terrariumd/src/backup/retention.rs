//! Deletion belongs to the updater, never to the daemon or offline CLI.
use terrarium_updater::protocol::{Request, Response};

pub async fn request(release: Option<String>) -> Result<serde_json::Value, String> {
    let socket = std::env::var_os(terrarium_updater::protocol::ENV_SOCKET)
        .map(std::path::PathBuf::from)
        .unwrap_or_else(|| terrarium_updater::protocol::DEFAULT_SOCKET.into());
    match terrarium_updater::client::call(&socket, &Request::Retention { release }).await {
        Ok(Response::Retained { detail }) => Ok(detail),
        Ok(Response::Error { message, .. }) => Err(message),
        Ok(_) => Err("unexpected retention answer".into()),
        Err(e) => Err(format!("no retention performed: {e}")),
    }
}

pub async fn after_backup() {
    if let Err(e) = request(None).await {
        tracing::warn!("backup retention deferred: {e}");
    }
}

/// First install pass, then bounded catch-up passes. Each call acquires the
/// updater run lock and the backup-directory lock before it deletes anything.
pub fn spawn() -> tokio::task::JoinHandle<()> {
    tokio::spawn(async {
        let mut tick = tokio::time::interval(std::time::Duration::from_secs(60));
        loop {
            tick.tick().await;
            after_backup().await;
        }
    })
}
