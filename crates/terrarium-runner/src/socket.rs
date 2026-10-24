//! The runner socket server.

use std::path::Path;
use std::sync::Arc;
use std::time::Duration;

use serde_json::{Value, json};
use tokio::io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader};
use tokio::net::UnixListener;
use tokio::net::unix::{OwnedReadHalf, OwnedWriteHalf};

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

/// How long the accept loop waits for the first line of a connection.
const FIRST_LINE: Duration = Duration::from_secs(1);

/// Serve the runner socket. Each `terrarium hook` call is its own
/// connection, and a harness runs its hooks one after the other, so the
/// accept loop reads the first line of each connection itself and runs a
/// hook at once, before it accepts the next connection. The hooks then
/// reach the status machine in the order the harness sent them. (Tasks per
/// connection could swap a `UserPromptSubmit` and the `Notification` after
/// it.) A client that sends nothing within `FIRST_LINE` gets its task with
/// no first line, so it cannot hold up the hooks for long.
pub(crate) async fn serve(listener: UnixListener, inner: Arc<Inner>) {
    loop {
        match listener.accept().await {
            Ok((stream, _)) => {
                let (read, write) = stream.into_split();
                let mut reader = BufReader::new(read);
                let mut first = String::new();
                let mut limited = (&mut reader).take(MAX_LINE as u64);
                let read_first = limited.read_line(&mut first);
                let eof = match tokio::time::timeout(FIRST_LINE, read_first).await {
                    Ok(Ok(0)) => true,
                    Ok(Ok(_)) | Err(_) => false,
                    Ok(Err(e)) => {
                        tracing::debug!("runner socket connection: {e}");
                        continue;
                    }
                };
                if first.ends_with('\n')
                    && let Ok(Message::Hook { hook, payload }) =
                        serde_json::from_str::<Message>(first.trim())
                {
                    inner.on_hook(hook, payload);
                    first.clear();
                }
                let inner = inner.clone();
                tokio::spawn(async move {
                    if let Err(e) = connection(reader, write, inner, first, eof).await {
                        tracing::debug!("runner socket connection: {e}");
                    }
                });
            }
            Err(e) => {
                tracing::warn!("runner socket accept: {e}");
                tokio::time::sleep(Duration::from_millis(100)).await;
            }
        }
    }
}

/// Serve one connection. `line` holds what the accept loop read already:
/// nothing, a whole line, or the start of a line. `eof`: the client has
/// closed its side.
async fn connection(
    mut reader: BufReader<OwnedReadHalf>,
    mut write: OwnedWriteHalf,
    inner: Arc<Inner>,
    mut line: String,
    mut eof: bool,
) -> std::io::Result<()> {
    loop {
        if !eof && !line.ends_with('\n') && line.len() < MAX_LINE {
            let room = (MAX_LINE - line.len()) as u64;
            if (&mut reader).take(room).read_line(&mut line).await? == 0 {
                eof = true;
            }
        }
        if line.is_empty() {
            return Ok(());
        }
        if line.len() >= MAX_LINE && !line.ends_with('\n') {
            let reply = Message::response(
                Value::Null,
                Err(RpcError::parse_error("the line is too long")),
            );
            send(&mut write, &reply).await?;
            return Ok(());
        }
        let text = line.trim();
        let reply = if text.is_empty() {
            None
        } else {
            match serde_json::from_str::<Message>(text) {
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
            }
        };
        if let Some(reply) = reply {
            send(&mut write, &reply).await?;
        }
        line.clear();
        if eof {
            return Ok(());
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

async fn send(write: &mut OwnedWriteHalf, msg: &Message) -> std::io::Result<()> {
    let mut bytes = serde_json::to_vec(msg).map_err(std::io::Error::other)?;
    bytes.push(b'\n');
    write.write_all(&bytes).await
}
