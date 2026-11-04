//! The updater socket: one JSON line in, one JSON line out
//! (`crate::protocol`).
//!
//! The socket dir is `0700` and the socket `0600`. A peer whose uid is not
//! the uid of the socket owner (the updater and the daemon share one uid)
//! gets an error and nothing else.

use std::os::unix::fs::{MetadataExt, PermissionsExt};
use std::path::Path;
use std::sync::Arc;

use tokio::io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader};
use tokio::net::{UnixListener, UnixStream};

use crate::machine::Updater;
use crate::protocol::{MAX_REQUEST, Request, Response};

/// Make the socket dir (`0700`) and bind the socket (`0600`). A stale
/// socket file is removed first.
pub fn bind(path: &Path) -> std::io::Result<UnixListener> {
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir)?;
        std::fs::set_permissions(dir, std::fs::Permissions::from_mode(0o700))?;
    }
    match std::fs::remove_file(path) {
        Ok(()) => {}
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
        Err(e) => return Err(e),
    }
    let l = UnixListener::bind(path)?;
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600))?;
    Ok(l)
}

/// Serve requests until the task ends.
pub async fn serve(listener: UnixListener, path: &Path, updater: Arc<Updater>) {
    let owner = std::fs::metadata(path).map(|m| m.uid()).ok();
    loop {
        let stream = match listener.accept().await {
            Ok((s, _)) => s,
            Err(e) => {
                tracing::warn!("cannot accept a connection: {e}");
                continue;
            }
        };
        let u = updater.clone();
        tokio::spawn(async move {
            if let Err(e) = one(stream, owner, &u).await {
                tracing::debug!("a request failed: {e}");
            }
        });
    }
}

async fn one(
    stream: UnixStream,
    owner: Option<u32>,
    updater: &Arc<Updater>,
) -> std::io::Result<()> {
    let peer = stream.peer_cred().map(|c| c.uid()).ok();
    let (read, mut write) = stream.into_split();
    let answer = if owner.is_none() || peer != owner {
        tracing::warn!(peer = ?peer, owner = ?owner, "refused a peer with another uid");
        Response::Error {
            message: "refused: the peer has another uid than the updater".into(),
            busy: false,
        }
    } else {
        let mut line = String::new();
        let mut r = BufReader::new(read.take(MAX_REQUEST as u64));
        r.read_line(&mut line).await?;
        match serde_json::from_str::<Request>(line.trim()) {
            Ok(req) => updater.handle(req).await,
            Err(e) => Response::Error {
                message: format!("bad request: {e}"),
                busy: false,
            },
        }
    };
    let mut out = serde_json::to_vec(&answer).map_err(std::io::Error::other)?;
    out.push(b'\n');
    write.write_all(&out).await?;
    write.shutdown().await
}
