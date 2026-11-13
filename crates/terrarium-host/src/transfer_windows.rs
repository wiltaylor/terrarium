//! Each transfer owns a synchronous worker. All filesystem operations,
//! including cleanup and metadata, run under that worker's logon token.
//! No tokio filesystem worker is allowed to inherit service authority.
//!
//! A path in an account workspace (TER-274, `[windows_workspaces]`) goes
//! through `terrarium_winapi::Workspace`: the service provisions the root,
//! then the worker walks the inner components as the account with no
//! reparse-point following, and publishes or deletes its temporary file
//! by handle. Other paths keep the TER-272 handling.

use super::*;
use crate::protocol::{CHUNK_SIZE, SessionId, StreamTag};
use crate::windows_workspace::{Location, split_file};
use std::io::{Read, Write};
use std::sync::Arc;
use terrarium_winapi::{Dir, Identity, TempFile, Workspace};
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

/// A push into a workspace: the held directory chain and a temporary file
/// that is deleted by handle unless it is published.
struct Confined {
    dir: Dir,
    temp: Option<TempFile>,
    name: String,
    path: PathBuf,
    bytes: u64,
    hasher: Sha256,
}

enum Sink {
    Path(Pending),
    Workspace(Confined),
}

impl Sink {
    fn write(&mut self, data: &[u8]) -> Result<(), TransferError> {
        match self {
            Self::Path(p) => p.write(data),
            Self::Workspace(c) => c.write(data),
        }
    }

    fn finish(&mut self, bytes: u64, sha256: &str) -> Result<(u64, String), TransferError> {
        match self {
            Self::Path(p) => p.finish(bytes, sha256),
            Self::Workspace(c) => c.finish(bytes, sha256),
        }
    }
}

fn not_a_file(path: &Path) -> TransferError {
    TransferError::NotAFile {
        path: path.to_owned(),
    }
}

/// Service authority: provision the workspace before impersonating.
fn provision(
    location: Option<&Location>,
    identity: Option<&Identity>,
    path: &Path,
) -> Result<Option<Workspace>, TransferError> {
    location
        .map(|l| Workspace::open(&l.project, &l.agent, identity, true))
        .transpose()
        .map_err(io("workspace", path))
}

/// As the account: the held chain stays alive until the read ends.
fn open_confined(
    workspace: Workspace,
    location: &Location,
    path: &Path,
) -> Result<(Dir, std::fs::File), TransferError> {
    let (dirs, name) = split_file(location).map_err(|_| not_a_file(path))?;
    let dir = workspace.into_dir(&dirs, false).map_err(io("open", path))?;
    let file = dir.open_read(name).map_err(io("open", path))?;
    Ok((dir, file))
}

fn mismatch(got_bytes: u64, got: String, want_bytes: u64, want: &str) -> TransferError {
    TransferError::Mismatch {
        got_bytes,
        got,
        want_bytes,
        want: want.to_owned(),
    }
}

impl Confined {
    /// Run as the account (impersonating), after the service opened `workspace`.
    fn open(
        workspace: Workspace,
        location: &Location,
        path: &Path,
        mkdirs: bool,
        session: SessionId,
    ) -> Result<Self, TransferError> {
        let (dirs, name) = split_file(location).map_err(|_| not_a_file(path))?;
        let dir = workspace
            .into_dir(&dirs, mkdirs)
            .map_err(io("open", path))?;
        let temp = dir
            .create_temp(u64::from(session))
            .map_err(io("create", path))?;
        Ok(Self {
            dir,
            temp: Some(temp),
            name: name.to_owned(),
            path: path.to_owned(),
            bytes: 0,
            hasher: Sha256::new(),
        })
    }

    fn write(&mut self, data: &[u8]) -> Result<(), TransferError> {
        self.temp
            .as_mut()
            .expect("live transfer")
            .file()
            .write_all(data)
            .map_err(io("write", &self.path))?;
        self.bytes += data.len() as u64;
        self.hasher.update(data);
        Ok(())
    }

    fn finish(&mut self, bytes: u64, sha256: &str) -> Result<(u64, String), TransferError> {
        let got = hex(&self.hasher.clone().finalize());
        if self.bytes != bytes || !got.eq_ignore_ascii_case(sha256) {
            return Err(mismatch(self.bytes, got, bytes, sha256));
        }
        let temp = self.temp.take().expect("live transfer");
        temp.publish(&self.dir, &self.name)
            .map_err(io("rename", &self.path))?;
        Ok((self.bytes, got))
    }
}

impl Pending {
    fn open(path: &Path, mkdirs: bool) -> Result<Self, TransferError> {
        let parent = path
            .parent()
            .filter(|_| path.file_name().is_some())
            .ok_or_else(|| TransferError::NotAFile {
                path: path.to_owned(),
            })?;
        if mkdirs {
            std::fs::create_dir_all(parent).map_err(io("create", parent))?;
        }
        // Random create_new temporary names cannot open an existing symlink.
        let (file, tmp) = tempfile::Builder::new()
            .prefix(".terrarium-")
            .suffix(".part")
            .tempfile_in(parent)
            .map_err(io("create", parent))?
            .into_parts();
        Ok(Self {
            file: Some(file),
            tmp,
            path: path.to_owned(),
            bytes: 0,
            hasher: Sha256::new(),
        })
    }

    fn write(&mut self, data: &[u8]) -> Result<(), TransferError> {
        self.file
            .as_mut()
            .expect("live transfer")
            .write_all(data)
            .map_err(io("write", &self.path))?;
        self.bytes += data.len() as u64;
        self.hasher.update(data);
        Ok(())
    }

    fn finish(&mut self, bytes: u64, sha256: &str) -> Result<(u64, String), TransferError> {
        let got = hex(&self.hasher.clone().finalize());
        if self.bytes != bytes || !got.eq_ignore_ascii_case(sha256) {
            return Err(mismatch(self.bytes, got, bytes, sha256));
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

fn as_user<T>(
    identity: Option<&Identity>,
    f: impl FnOnce() -> std::io::Result<T>,
) -> std::io::Result<T> {
    match identity {
        Some(identity) => identity.with_impersonation(f),
        None => f(),
    }
}

impl PushFile {
    pub async fn open(
        path: &Path,
        mode: Option<u32>,
        mkdirs: bool,
        session: SessionId,
    ) -> Result<Self, TransferError> {
        Self::open_as(path, mode, mkdirs, session, None, None).await
    }

    /// `location`: the path is in an account workspace ([`crate::windows_workspace::locate`]).
    pub async fn open_as(
        path: &Path,
        _mode: Option<u32>,
        mkdirs: bool,
        session: SessionId,
        identity: Option<Arc<Identity>>,
        location: Option<Location>,
    ) -> Result<Self, TransferError> {
        let (tx, mut rx) = mpsc::channel(8);
        let (ready, opened) = oneshot::channel();
        let worker_path = path.to_owned();
        std::thread::Builder::new()
            .name("terrarium-push".into())
            .spawn(move || {
                let mut ready = Some(ready);
                let workspace =
                    match provision(location.as_ref(), identity.as_deref(), &worker_path) {
                        Ok(w) => w,
                        Err(error) => {
                            let _ = ready.take().expect("ready once").send(Err(error));
                            return;
                        }
                    };
                let result = as_user(identity.as_deref(), || {
                    let opened = match (workspace, &location) {
                        (Some(w), Some(l)) => {
                            Confined::open(w, l, &worker_path, mkdirs, session).map(Sink::Workspace)
                        }
                        _ => Pending::open(&worker_path, mkdirs).map(Sink::Path),
                    };
                    let mut file = match opened {
                        Ok(file) => file,
                        Err(error) => {
                            let _ = ready.take().expect("ready once").send(Err(error));
                            return Ok(());
                        }
                    };
                    if ready.take().expect("ready once").send(Ok(())).is_err() {
                        return Ok(());
                    }
                    while let Some(message) = rx.blocking_recv() {
                        match message {
                            Message::Write(data, reply) => {
                                if reply.send(file.write(&data)).is_err() {
                                    break;
                                }
                            }
                            Message::Finish(bytes, sha, reply) => {
                                let _ = reply.send(file.finish(bytes, &sha));
                                break;
                            }
                            Message::Abort(reply) => {
                                drop(file);
                                let _ = reply.send(());
                                return Ok(());
                            }
                        }
                    }
                    // Pending drops here, on this thread, still impersonating.
                    Ok(())
                });
                if let Err(error) = result
                    && let Some(ready) = ready
                {
                    let _ = ready.send(Err(io("impersonate", &worker_path)(error)));
                }
            })
            .map_err(io("worker", path))?;
        opened.await.map_err(|_| stopped(path))??;
        Ok(Self {
            tx,
            path: path.to_owned(),
        })
    }

    pub async fn write(&mut self, data: &[u8]) -> Result<(), TransferError> {
        let (tx, rx) = oneshot::channel();
        self.tx
            .send(Message::Write(data.to_vec(), tx))
            .await
            .map_err(|_| stopped(&self.path))?;
        rx.await.map_err(|_| stopped(&self.path))?
    }

    pub async fn finish(self, bytes: u64, sha256: &str) -> Result<(u64, String), TransferError> {
        let (tx, rx) = oneshot::channel();
        self.tx
            .send(Message::Finish(bytes, sha256.to_owned(), tx))
            .await
            .map_err(|_| stopped(&self.path))?;
        rx.await.map_err(|_| stopped(&self.path))?
    }

    pub async fn abort(self) {
        let (tx, rx) = oneshot::channel();
        if self.tx.send(Message::Abort(tx)).await.is_ok() {
            let _ = rx.await;
        }
    }
}

pub async fn pull(path: &Path, outbox: &Outbox) -> Result<(u64, String), TransferError> {
    pull_as(path, outbox, None, None).await
}

/// `location`: the path is in an account workspace ([`crate::windows_workspace::locate`]).
pub async fn pull_as(
    path: &Path,
    outbox: &Outbox,
    identity: Option<Arc<Identity>>,
    location: Option<Location>,
) -> Result<(u64, String), TransferError> {
    let (tx, mut rx) = mpsc::channel(8);
    let worker_path = path.to_owned();
    let (done, result) = oneshot::channel();
    std::thread::Builder::new()
        .name("terrarium-pull".into())
        .spawn(move || {
            let workspace = match provision(location.as_ref(), identity.as_deref(), &worker_path) {
                Ok(w) => w,
                Err(error) => {
                    let _ = done.send(Err(error));
                    return;
                }
            };
            let transfer = as_user(identity.as_deref(), || {
                let result = (|| {
                    let (_dir, mut file) = match (workspace, &location) {
                        (Some(w), Some(l)) => {
                            let (dir, file) = open_confined(w, l, &worker_path)?;
                            (Some(dir), file)
                        }
                        _ => {
                            let file = std::fs::File::open(&worker_path);
                            (None, file.map_err(io("open", &worker_path))?)
                        }
                    };
                    if !file.metadata().map_err(io("stat", &worker_path))?.is_file() {
                        return Err(TransferError::NotAFile {
                            path: worker_path.clone(),
                        });
                    }
                    let mut hasher = Sha256::new();
                    let mut bytes = 0;
                    loop {
                        let mut data = vec![0; CHUNK_SIZE];
                        let n = file.read(&mut data).map_err(io("read", &worker_path))?;
                        if n == 0 {
                            break;
                        }
                        data.truncate(n);
                        bytes += n as u64;
                        hasher.update(&data);
                        tx.blocking_send(data).map_err(|_| stopped(&worker_path))?;
                    }
                    Ok((bytes, hex(&hasher.finalize())))
                })();
                Ok(result)
            });
            let _ = done.send(
                transfer
                    .map_err(io("impersonate", &worker_path))
                    .and_then(|r| r),
            );
            // Close channel only after file operations and impersonation have ended.
            drop(tx);
        })
        .map_err(io("worker", path))?;
    while let Some(data) = rx.recv().await {
        outbox.push(StreamTag::File, data).await?;
    }
    result.await.map_err(|_| stopped(path))?
}
