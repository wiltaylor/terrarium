//! `cp` in both directions, one regular file per session.
//!
//! A push writes to a temporary file next to the destination, hashes the
//! bytes as they arrive, and renames the file into place only when the byte
//! count and the SHA-256 match the daemon's `cp_end`. A pull streams the
//! file through the session's outbox (so it is replayed after a reconnect)
//! and reports the byte count and the SHA-256 in `cp_done`.

use std::path::{Path, PathBuf};

use sha2::{Digest, Sha256};
use tokio::io::{AsyncReadExt, AsyncWriteExt};

use crate::outbox::{Outbox, Overflow};
use crate::protocol::{CHUNK_SIZE, ErrorCode, SessionId, StreamTag, hex};

#[derive(Debug, thiserror::Error)]
pub enum TransferError {
    #[error("{op} {path}: {source}")]
    Io {
        op: &'static str,
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },
    #[error("{path} is not a regular file")]
    NotAFile { path: PathBuf },
    #[error(
        "checksum mismatch: got {got_bytes} bytes sha256 {got}, expected {want_bytes} bytes sha256 {want}"
    )]
    Mismatch {
        got_bytes: u64,
        got: String,
        want_bytes: u64,
        want: String,
    },
    #[error(transparent)]
    Overflow(#[from] Overflow),
}

impl TransferError {
    pub fn code(&self) -> ErrorCode {
        match self {
            TransferError::Mismatch { .. } => ErrorCode::ChecksumMismatch,
            TransferError::NotAFile { .. } => ErrorCode::BadRequest,
            TransferError::Io { .. } | TransferError::Overflow(_) => ErrorCode::Io,
        }
    }
}

fn io<'a>(op: &'static str, path: &'a Path) -> impl FnOnce(std::io::Error) -> TransferError + 'a {
    move |source| TransferError::Io {
        op,
        path: path.to_owned(),
        source,
    }
}

/// A push in progress.
#[derive(Debug)]
pub struct PushFile {
    path: PathBuf,
    tmp: PathBuf,
    file: tokio::fs::File,
    hasher: Sha256,
    bytes: u64,
    mode: Option<u32>,
}

impl PushFile {
    pub async fn open(
        path: &Path,
        mode: Option<u32>,
        mkdirs: bool,
        session: SessionId,
    ) -> Result<Self, TransferError> {
        let parent = path.parent().ok_or_else(|| TransferError::NotAFile {
            path: path.to_owned(),
        })?;
        let name = path.file_name().ok_or_else(|| TransferError::NotAFile {
            path: path.to_owned(),
        })?;
        if mkdirs {
            tokio::fs::create_dir_all(parent)
                .await
                .map_err(io("create", parent))?;
        }
        let tmp = parent.join(format!(
            ".{}.terrarium-{session}.part",
            name.to_string_lossy()
        ));
        let file = tokio::fs::File::create(&tmp)
            .await
            .map_err(io("create", &tmp))?;
        Ok(Self {
            path: path.to_owned(),
            tmp,
            file,
            hasher: Sha256::new(),
            bytes: 0,
            mode,
        })
    }

    pub async fn write(&mut self, data: &[u8]) -> Result<(), TransferError> {
        self.hasher.update(data);
        self.bytes += data.len() as u64;
        self.file
            .write_all(data)
            .await
            .map_err(io("write", &self.tmp))
    }

    /// Check against `cp_end` and rename into place.
    pub async fn finish(
        mut self,
        bytes: u64,
        sha256: &str,
    ) -> Result<(u64, String), TransferError> {
        let got = hex(&self.hasher.clone().finalize());
        if got != sha256.to_ascii_lowercase() || self.bytes != bytes {
            let got_bytes = self.bytes;
            self.abort().await;
            return Err(TransferError::Mismatch {
                got_bytes,
                got,
                want_bytes: bytes,
                want: sha256.to_owned(),
            });
        }
        let r = async {
            self.file.flush().await.map_err(io("flush", &self.tmp))?;
            self.file.sync_all().await.map_err(io("sync", &self.tmp))?;
            #[cfg(unix)]
            if let Some(mode) = self.mode {
                use std::os::unix::fs::PermissionsExt;
                tokio::fs::set_permissions(&self.tmp, std::fs::Permissions::from_mode(mode))
                    .await
                    .map_err(io("chmod", &self.tmp))?;
            }
            tokio::fs::rename(&self.tmp, &self.path)
                .await
                .map_err(io("rename", &self.path))
        }
        .await;
        if let Err(e) = r {
            let _ = tokio::fs::remove_file(&self.tmp).await;
            return Err(e);
        }
        Ok((self.bytes, got))
    }

    /// Remove the partial file.
    pub async fn abort(self) {
        drop(self.file);
        let _ = tokio::fs::remove_file(&self.tmp).await;
    }
}

/// Stream a file into the outbox as `file` frames. Returns bytes and SHA-256.
pub async fn pull(path: &Path, outbox: &Outbox) -> Result<(u64, String), TransferError> {
    let meta = tokio::fs::metadata(path).await.map_err(io("stat", path))?;
    if !meta.is_file() {
        return Err(TransferError::NotAFile {
            path: path.to_owned(),
        });
    }
    let mut f = tokio::fs::File::open(path)
        .await
        .map_err(io("open", path))?;
    let mut hasher = Sha256::new();
    let mut bytes = 0u64;
    let mut buf = vec![0u8; CHUNK_SIZE];
    loop {
        let n = f.read(&mut buf).await.map_err(io("read", path))?;
        if n == 0 {
            break;
        }
        hasher.update(&buf[..n]);
        bytes += n as u64;
        outbox.push(StreamTag::File, buf[..n].to_vec()).await?;
    }
    Ok((bytes, hex(&hasher.finalize())))
}

/// SHA-256 of a byte slice, as lowercase hex.
pub fn sha256_hex(data: &[u8]) -> String {
    hex(&Sha256::digest(data))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::outbox::Outgoing;
    use tokio::sync::mpsc;

    #[tokio::test]
    async fn push_verifies_and_renames() {
        let dir = tempfile::tempdir().unwrap();
        let dst = dir.path().join("sub/out.bin");
        let mut p = PushFile::open(&dst, Some(0o600), true, 7).await.unwrap();
        p.write(b"hello ").await.unwrap();
        p.write(b"world").await.unwrap();
        let want = sha256_hex(b"hello world");
        let (n, sha) = p.finish(11, &want).await.unwrap();
        assert_eq!((n, sha.as_str()), (11, want.as_str()));
        assert_eq!(std::fs::read(&dst).unwrap(), b"hello world");
        use std::os::unix::fs::PermissionsExt;
        let mode = std::fs::metadata(&dst).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode, 0o600);
        assert_eq!(std::fs::read_dir(dst.parent().unwrap()).unwrap().count(), 1);
    }

    #[tokio::test]
    async fn push_mismatch_leaves_nothing() {
        let dir = tempfile::tempdir().unwrap();
        let dst = dir.path().join("out.bin");
        let mut p = PushFile::open(&dst, None, false, 1).await.unwrap();
        p.write(b"abc").await.unwrap();
        let err = p.finish(3, &sha256_hex(b"abd")).await.unwrap_err();
        assert_eq!(err.code(), ErrorCode::ChecksumMismatch);
        assert!(!dst.exists());
        assert_eq!(std::fs::read_dir(dir.path()).unwrap().count(), 0);
    }

    #[tokio::test]
    async fn pull_streams_file_frames() {
        let dir = tempfile::tempdir().unwrap();
        let src = dir.path().join("in.bin");
        let data: Vec<u8> = (0..100_000u32).map(|i| i as u8).collect();
        std::fs::write(&src, &data).unwrap();
        let ob = Outbox::new(1, 1 << 20);
        let (tx, mut rx) = mpsc::unbounded_channel();
        ob.attach(tx, 0);
        let (n, sha) = pull(&src, &ob).await.unwrap();
        assert_eq!(n, 100_000);
        assert_eq!(sha, sha256_hex(&data));
        let mut got = Vec::new();
        while let Ok(Outgoing::Data(d)) = rx.try_recv() {
            assert_eq!(d.stream, StreamTag::File);
            got.extend(d.payload);
        }
        assert_eq!(got, data);
        assert!(matches!(
            pull(dir.path(), &ob).await,
            Err(TransferError::NotAFile { .. })
        ));
    }
}
