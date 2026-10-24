//! `pty.log`: the raw PTY bytes. The file keeps the newest bytes, up to a
//! limit. When it grows past the limit, it moves to `pty.log.1` (the
//! archive) and a new `pty.log` starts. One archive is kept.

use std::fs::{File, OpenOptions};
use std::io::Write;
use std::path::{Path, PathBuf};

use crate::error::{Result, RunnerError};

pub(crate) struct PtyLog {
    path: PathBuf,
    archive: PathBuf,
    file: File,
    size: u64,
    max: u64,
}

impl PtyLog {
    pub(crate) fn open(dir: &Path, max: u64) -> Result<Self> {
        let path = dir.join("pty.log");
        let archive = dir.join("pty.log.1");
        let file = open_append(&path)?;
        let size = file
            .metadata()
            .map_err(|e| RunnerError::io("stat", &path, e))?
            .len();
        Ok(Self {
            path,
            archive,
            file,
            size,
            max: max.max(1),
        })
    }

    pub(crate) fn write(&mut self, bytes: &[u8]) -> Result<()> {
        if self.size > 0 && self.size + bytes.len() as u64 > self.max {
            self.rotate()?;
        }
        self.file
            .write_all(bytes)
            .map_err(|e| RunnerError::io("write", &self.path, e))?;
        self.size += bytes.len() as u64;
        Ok(())
    }

    fn rotate(&mut self) -> Result<()> {
        std::fs::rename(&self.path, &self.archive)
            .map_err(|e| RunnerError::io("rotate", &self.path, e))?;
        self.file = open_append(&self.path)?;
        self.size = 0;
        Ok(())
    }
}

fn open_append(path: &Path) -> Result<File> {
    OpenOptions::new()
        .create(true)
        .append(true)
        .open(path)
        .map_err(|e| RunnerError::io("open", path, e))
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
}
