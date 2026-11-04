//! The client of the updater socket, for the daemon.

use std::path::Path;
use std::time::Duration;

use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::net::UnixStream;

use crate::protocol::{Request, Response};

/// The longest wait for an answer. A request only reads state or starts a
/// run in the background, so the answer is fast.
pub const TIMEOUT: Duration = Duration::from_secs(10);

/// Why a call failed.
#[derive(Debug, thiserror::Error)]
pub enum CallError {
    /// No updater listens on the socket.
    #[error("no updater answers on {path}: {source}")]
    Unreachable {
        path: String,
        source: std::io::Error,
    },
    #[error("the updater did not answer in {0:?}")]
    Timeout(Duration),
    #[error("cannot talk to the updater: {0}")]
    Io(#[from] std::io::Error),
    #[error("the updater sent a bad answer: {0}")]
    Bad(String),
}

/// Send `req` to the updater at `socket` and read its answer.
pub async fn call(socket: &Path, req: &Request) -> Result<Response, CallError> {
    match tokio::time::timeout(TIMEOUT, call_inner(socket, req)).await {
        Ok(r) => r,
        Err(_) => Err(CallError::Timeout(TIMEOUT)),
    }
}

async fn call_inner(socket: &Path, req: &Request) -> Result<Response, CallError> {
    let mut stream =
        UnixStream::connect(socket)
            .await
            .map_err(|source| CallError::Unreachable {
                path: socket.display().to_string(),
                source,
            })?;
    let mut line = serde_json::to_vec(req).map_err(|e| CallError::Bad(e.to_string()))?;
    line.push(b'\n');
    stream.write_all(&line).await?;
    let mut reader = BufReader::new(stream);
    let mut answer = String::new();
    reader.read_line(&mut answer).await?;
    if answer.trim().is_empty() {
        return Err(CallError::Bad("an empty answer".into()));
    }
    serde_json::from_str(answer.trim()).map_err(|e| CallError::Bad(e.to_string()))
}
