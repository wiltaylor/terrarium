//! `pty.log`: the raw PTY bytes. The file keeps the newest bytes, up to a
//! limit. When it grows past the limit, it moves to `pty.log.1` (the
//! archive) and a new `pty.log` starts. One archive is kept. A limit of 0
//! turns the log off: no file is made.

use std::fs::File;
use std::io::Write;
use std::path::{Path, PathBuf};

use crate::error::{Result, RunnerError};

pub(crate) struct PtyLog {
    path: PathBuf,
    archive: PathBuf,
    /// `None`: the log is off.
    file: Option<File>,
    size: u64,
    max: u64,
}

impl PtyLog {
    pub(crate) fn open(dir: &Path, max: u64) -> Result<Self> {
        let path = dir.join("pty.log");
        let archive = dir.join("pty.log.1");
        if max == 0 {
            return Ok(Self {
                path,
                archive,
                file: None,
                size: 0,
                max,
            });
        }
        let file = open_append(&path)?;
        let size = file
            .metadata()
            .map_err(|e| RunnerError::io("stat", &path, e))?
            .len();
        Ok(Self {
            path,
            archive,
            file: Some(file),
            size,
            max,
        })
    }

    pub(crate) fn write(&mut self, bytes: &[u8]) -> Result<()> {
        if self.file.is_none() {
            return Ok(());
        }
        if self.size > 0 && self.size + bytes.len() as u64 > self.max {
            self.rotate()?;
        }
        let Some(file) = self.file.as_mut() else {
            return Ok(());
        };
        file.write_all(bytes)
            .map_err(|e| RunnerError::io("write", &self.path, e))?;
        self.size += bytes.len() as u64;
        Ok(())
    }

    fn rotate(&mut self) -> Result<()> {
        std::fs::rename(&self.path, &self.archive)
            .map_err(|e| RunnerError::io("rotate", &self.path, e))?;
        self.file = Some(open_append(&self.path)?);
        self.size = 0;
        Ok(())
    }
}

/// No symlink follow (`docs/security-review.md`, F11). `rotate` uses
/// `rename`, which moves a symlink itself, never its target.
fn open_append(path: &Path) -> Result<File> {
    crate::safefs::open_append(path)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rotates_into_one_archive() {
        let dir = tempfile::tempdir().expect("tempdir");
        let mut log = PtyLog::open(dir.path(), 10).expect("open");
        log.write(b"aaaaaaaa").expect("write");
        log.write(b"bbbbbbbb").expect("write");
        log.write(b"cccccccc").expect("write");
        let cur = std::fs::read(dir.path().join("pty.log")).expect("read");
        let old = std::fs::read(dir.path().join("pty.log.1")).expect("read");
        assert_eq!(cur, b"cccccccc");
        assert_eq!(old, b"bbbbbbbb");
    }

    #[test]
    fn a_zero_limit_writes_nothing() {
        let dir = tempfile::tempdir().expect("tempdir");
        let mut log = PtyLog::open(dir.path(), 0).expect("open");
        log.write(b"device code ABCD-1234").expect("write");
        assert!(!dir.path().join("pty.log").exists());
        assert!(!dir.path().join("pty.log.1").exists());
    }
}
