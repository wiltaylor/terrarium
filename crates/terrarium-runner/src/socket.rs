//! The runner socket server.

use std::path::Path;
use std::sync::Arc;

use serde_json::{Value, json};
use tokio::io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader};
use tokio::net::{UnixListener, UnixStream};

use crate::error::{Result, RunnerError};
use crate::protocol::{Message, RpcError};
use crate::runner::Inner;

/// A line longer than this closes the connection. An artifact upload
/// through the `http` method carries up to 32 MiB as base64 in one line
/// (`docs/agent-rpc.md`), so the limit is above 32 MiB * 4/3.
const MAX_LINE: usize = 64 * 1024 * 1024;

pub(crate) fn bind(path: &Path) -> Result<UnixListener> {
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir).map_err(|e| RunnerError::io("create", dir, e))?;
    }
    // A stale socket from an earlier runner blocks the bind.
    match std::fs::remove_file(path) {
        Ok(()) => {}
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
        Err(e) => return Err(RunnerError::io("remove", path, e)),
    }
    let listener = UnixListener::bind(path).map_err(|e| RunnerError::io("bind", path, e))?;
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600))
            .map_err(|e| RunnerError::io("chmod", path, e))?;
    }
    Ok(listener)
}

pub(crate) async fn serve(listener: UnixListener, inner: Arc<Inner>) {
    loop {
        match listener.accept().await {
            Ok((stream, _)) => {
                let inner = inner.clone();
                tokio::spawn(async move {
                    if let Err(e) = connection(stream, inner).await {
                        tracing::debug!("runner socket connection: {e}");
                    }
                });
            }
            Err(e) => {
                tracing::warn!("runner socket accept: {e}");
                tokio::time::sleep(std::time::Duration::from_millis(100)).await;
            }
        }
    }
}

async fn connection(stream: UnixStream, inner: Arc<Inner>) -> std::io::Result<()> {
    let (read, mut write) = stream.into_split();
    let mut reader = BufReader::new(read);
    let mut line = String::new();
    loop {
        line.clear();
        let n = (&mut reader)
            .take(MAX_LINE as u64)
            .read_line(&mut line)
            .await?;
        if n == 0 {
            return Ok(());
        }
        if n >= MAX_LINE && !line.ends_with('\n') {
            let reply = Message::response(
                Value::Null,
                Err(RpcError::parse_error("the line is too long")),
            );
            send(&mut write, &reply).await?;
            return Ok(());
        }
        let text = line.trim();
        if text.is_empty() {
            continue;
        }
        let reply = match serde_json::from_str::<Message>(text) {
            Ok(Message::Hook { hook, payload }) => {
                inner.on_hook(hook, payload);
                None
            }
            Ok(Message::Request { id, method, params }) => Some(Message::response(
                id,
                request(&inner, &method, params).await,
            )),
            Ok(Message::Response { .. }) => Some(Message::response(
                Value::Null,
                Err(RpcError::new(
                    "invalid_request",
                    "the runner does not take responses",
                )),
            )),
            Err(e) => Some(Message::response(
                Value::Null,
                Err(RpcError::parse_error(e.to_string())),
            )),
        };
        if let Some(reply) = reply {
            send(&mut write, &reply).await?;
        }
    }
}

async fn request(inner: &Inner, method: &str, params: Value) -> Result<Value, RpcError> {
    let ctx = inner.request_context();
    match method {
        "ping" => Ok(json!("pong")),
        "whoami" => Ok(json!({
            "agent": ctx.agent,
            "name": ctx.name,
            "project": ctx.project,
            "harness": ctx.harness,
            "roles": ctx.roles,
            "status": ctx.status,
        })),
        _ => inner.handler().handle(&ctx, method, params).await,
    }
}

async fn send(write: &mut tokio::net::unix::OwnedWriteHalf, msg: &Message) -> std::io::Result<()> {
    let mut bytes = serde_json::to_vec(msg).map_err(std::io::Error::other)?;
    bytes.push(b'\n');
    write.write_all(&bytes).await
}
