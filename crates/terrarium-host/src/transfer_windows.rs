//! Each transfer owns a synchronous worker. All filesystem operations,
//! including cleanup and metadata, run under that worker's logon token.
//! No tokio filesystem worker is allowed to inherit service authority.

use super::*;
use crate::protocol::{CHUNK_SIZE, SessionId, StreamTag};
use std::io::{Read, Write};
use std::sync::Arc;
use terrarium_winapi::Identity;
use tokio::sync::{mpsc, oneshot};

type Receipt<T> = oneshot::Sender<Result<T, TransferError>>;
enum Message {
    Write(Vec<u8>, Receipt<()>),
    Finish(u64, String, Receipt<(u64, String)>),
    Abort(oneshot::Sender<()>),
}

#[derive(Debug)]
pub struct PushFile {
    tx: mpsc::Sender<Message>,
    path: PathBuf,
}

fn stopped(path: &Path) -> TransferError {
    io("worker", path)(std::io::Error::other("Windows transfer worker stopped"))
}

/// The temporary file is closed and removed before impersonation is reverted,
/// even when the async caller is cancelled and its channel closes.
struct Pending {
    file: Option<std::fs::File>,
    tmp: tempfile::TempPath,
    path: PathBuf,
    bytes: u64,
    hasher: Sha256,
}

impl Pending {
    fn open(path: &Path, mkdirs: bool) -> Result<Self, TransferError> {
        let parent = path.parent().filter(|_| path.file_name().is_some())
            .ok_or_else(|| TransferError::NotAFile { path: path.to_owned() })?;
        if mkdirs { std::fs::create_dir_all(parent).map_err(io("create", parent))?; }
        // Random create_new temporary names cannot open an existing symlink.
        let (file, tmp) = tempfile::Builder::new().prefix(".terrarium-").suffix(".part")
            .tempfile_in(parent).map_err(io("create", parent))?.into_parts();
        Ok(Self { file: Some(file), tmp, path: path.to_owned(), bytes: 0, hasher: Sha256::new() })
    }

    fn write(&mut self, data: &[u8]) -> Result<(), TransferError> {
        self.file.as_mut().expect("live transfer").write_all(data).map_err(io("write", &self.path))?;
        self.bytes += data.len() as u64;
        self.hasher.update(data);
        Ok(())
    }

    fn finish(&mut self, bytes: u64, sha256: &str) -> Result<(u64, String), TransferError> {
        let got = hex(&self.hasher.clone().finalize());
        if self.bytes != bytes || !got.eq_ignore_ascii_case(sha256) {
            return Err(TransferError::Mismatch {
                got_bytes: self.bytes, got, want_bytes: bytes, want: sha256.to_owned(),
            });
        }
        let file = self.file.take().expect("live transfer");
        file.sync_all().map_err(io("sync", &self.path))?;
        drop(file);
        std::fs::rename(&self.tmp, &self.path).map_err(io("rename", &self.path))?;
        Ok((self.bytes, got))
    }
}

impl Drop for Pending {
    fn drop(&mut self) {
        // Close before TempPath removes the file (Windows sharing rules).
        drop(self.file.take());
    }
}

fn as_user<T>(identity: Option<&Identity>, f: impl FnOnce() -> std::io::Result<T>) -> std::io::Result<T> {
    match identity { Some(identity) => identity.with_impersonation(f), None => f() }
}

impl PushFile {
    pub async fn open(path: &Path, mode: Option<u32>, mkdirs: bool, session: SessionId) -> Result<Self, TransferError> {
        Self::open_as(path, mode, mkdirs, session, None).await
    }

    pub async fn open_as(path: &Path, _mode: Option<u32>, mkdirs: bool, _session: SessionId,
        identity: Option<Arc<Identity>>) -> Result<Self, TransferError> {
        let (tx, mut rx) = mpsc::channel(8);
        let (ready, opened) = oneshot::channel();
        let worker_path = path.to_owned();
        std::thread::Builder::new().name("terrarium-push".into()).spawn(move || {
            let mut ready = Some(ready);
            let result = as_user(identity.as_deref(), || {
                let mut file = match Pending::open(&worker_path, mkdirs) {
                    Ok(file) => file,
                    Err(error) => {
                        let _ = ready.take().expect("ready once").send(Err(error));
                        return Ok(());
                    }
                };
                if ready.take().expect("ready once").send(Ok(())).is_err() { return Ok(()); }
                while let Some(message) = rx.blocking_recv() {
                    match message {
                        Message::Write(data, reply) => {
                            if reply.send(file.write(&data)).is_err() { break; }
                        }
                        Message::Finish(bytes, sha, reply) => {
                            let _ = reply.send(file.finish(bytes, &sha));
                            break;
                        }
                        Message::Abort(reply) => { drop(file); let _ = reply.send(()); return Ok(()); }
                    }
                }
                // Pending drops here, on this thread, still impersonating.
                Ok(())
            });
            if let Err(error) = result && let Some(ready) = ready {
                let _ = ready.send(Err(io("impersonate", &worker_path)(error)));
            }
        }).map_err(io("worker", path))?;
        opened.await.map_err(|_| stopped(path))??;
        Ok(Self { tx, path: path.to_owned() })
    }

    pub async fn write(&mut self, data: &[u8]) -> Result<(), TransferError> {
        let (tx, rx) = oneshot::channel();
        self.tx.send(Message::Write(data.to_vec(), tx)).await.map_err(|_| stopped(&self.path))?;
        rx.await.map_err(|_| stopped(&self.path))?
    }

    pub async fn finish(self, bytes: u64, sha256: &str) -> Result<(u64, String), TransferError> {
        let (tx, rx) = oneshot::channel();
        self.tx.send(Message::Finish(bytes, sha256.to_owned(), tx)).await.map_err(|_| stopped(&self.path))?;
        rx.await.map_err(|_| stopped(&self.path))?
    }

    pub async fn abort(self) {
        let (tx, rx) = oneshot::channel();
        if self.tx.send(Message::Abort(tx)).await.is_ok() { let _ = rx.await; }
    }
}

pub async fn pull(path: &Path, outbox: &Outbox) -> Result<(u64, String), TransferError> {
    pull_as(path, outbox, None).await
}

pub async fn pull_as(path: &Path, outbox: &Outbox, identity: Option<Arc<Identity>>) -> Result<(u64, String), TransferError> {
    let (tx, mut rx) = mpsc::channel(8);
    let worker_path = path.to_owned();
    let (done, result) = oneshot::channel();
    std::thread::Builder::new().name("terrarium-pull".into()).spawn(move || {
        let transfer = as_user(identity.as_deref(), || {
            let result = (|| {
                let mut file = std::fs::File::open(&worker_path).map_err(io("open", &worker_path))?;
                if !file.metadata().map_err(io("stat", &worker_path))?.is_file() {
                    return Err(TransferError::NotAFile { path: worker_path.clone() });
                }
                let mut hasher = Sha256::new();
                let mut bytes = 0;
                loop {
                    let mut data = vec![0; CHUNK_SIZE];
                    let n = file.read(&mut data).map_err(io("read", &worker_path))?;
                    if n == 0 { break; }
                    data.truncate(n);
                    bytes += n as u64;
                    hasher.update(&data);
                    tx.blocking_send(data).map_err(|_| stopped(&worker_path))?;
                }
                Ok((bytes, hex(&hasher.finalize())))
            })();
            Ok(result)
        });
        let _ = done.send(transfer.map_err(io("impersonate", &worker_path)).and_then(|r| r));
        // Close channel only after file operations and impersonation have ended.
        drop(tx);
    }).map_err(io("worker", path))?;
    while let Some(data) = rx.recv().await {
        outbox.push(StreamTag::File, data).await?;
    }
    result.await.map_err(|_| stopped(path))?
}
