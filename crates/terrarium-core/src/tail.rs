//! Follow a JSON-lines file as a harness appends to it.
//!
//! Every session-file reader (the usage tails and the transcript parsers of
//! the adapters, the daemon's transcript cache) uses [`LineTail`]: it keeps
//! the read offset, gives only complete lines, and starts again from the
//! top when the file gets shorter.

use std::fs::File;
use std::io::{Read, Seek, SeekFrom};
use std::path::{Path, PathBuf};

/// The new complete lines of one [`LineTail::poll`].
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct Chunk {
    /// Complete lines, each with its `\n`. Empty when nothing new came.
    pub lines: Vec<u8>,
    /// The file got shorter than the offset: the tail started again from
    /// the top, and `lines` are from the start of the file. A reader with
    /// state from the old content drops it.
    pub reset: bool,
}

/// The read state of one file. A partial last line waits for the next poll.
#[derive(Debug, Clone)]
pub struct LineTail {
    path: PathBuf,
    offset: u64,
    partial: Vec<u8>,
}

impl LineTail {
    pub fn new(path: impl Into<PathBuf>) -> Self {
        Self {
            path: path.into(),
            offset: 0,
            partial: Vec::new(),
        }
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    /// The number of bytes read so far (complete lines and the partial
    /// last line).
    pub fn offset(&self) -> u64 {
        self.offset
    }

    /// Read the bytes after the offset. A missing file gives an empty
    /// chunk: some harnesses write the file at the first message. With no
    /// change in the file length, nothing is read.
    pub fn poll(&mut self) -> std::io::Result<Chunk> {
        let mut file = match File::open(&self.path) {
            Ok(f) => f,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(Chunk::default()),
            Err(e) => return Err(e),
        };
        let len = file.metadata()?.len();
        let mut reset = false;
        if len < self.offset {
            self.offset = 0;
            self.partial.clear();
            reset = true;
        }
        if len == self.offset {
            return Ok(Chunk {
                lines: Vec::new(),
                reset,
            });
        }
        file.seek(SeekFrom::Start(self.offset))?;
        let mut buf = Vec::new();
        file.take(len - self.offset).read_to_end(&mut buf)?;
        self.offset += buf.len() as u64;
        self.partial.extend_from_slice(&buf);
        let Some(end) = self.partial.iter().rposition(|&b| b == b'\n') else {
            return Ok(Chunk {
                lines: Vec::new(),
                reset,
            });
        };
        let rest = self.partial.split_off(end + 1);
        let lines = std::mem::replace(&mut self.partial, rest);
        Ok(Chunk { lines, reset })
    }
}

/// Splits bytes that come in any chunks into numbered lines. A partial
/// last line waits for the next [`Lines::feed`]. Line numbers count from 1.
#[derive(Debug, Default, Clone)]
pub struct Lines {
    partial: Vec<u8>,
    line: u64,
}

impl Lines {
    pub fn new() -> Self {
        Self::default()
    }

    /// The number of complete lines so far.
    pub fn count(&self) -> u64 {
        self.line
    }

    /// The complete lines in `partial + bytes`, with their numbers. Bytes
    /// that are not UTF-8 are replaced.
    pub fn feed(&mut self, bytes: &[u8]) -> Vec<(u64, String)> {
        self.partial.extend_from_slice(bytes);
        let Some(end) = self.partial.iter().rposition(|&b| b == b'\n') else {
            return Vec::new();
        };
        let rest = self.partial.split_off(end + 1);
        let complete = std::mem::replace(&mut self.partial, rest);
        // `complete` ends with a newline: drop it, so `split` gives no
        // empty piece at the end.
        complete[..complete.len() - 1]
            .split(|&b| b == b'\n')
            .map(|l| {
                self.line += 1;
                let l = l.strip_suffix(b"\r").unwrap_or(l);
                (self.line, String::from_utf8_lossy(l).into_owned())
            })
            .collect()
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::*;
    use std::io::Write;

    #[test]
    fn lines_wait_for_the_newline_and_count_from_one() {
        let mut l = Lines::new();
        assert!(l.feed(b"{\"a\":1}\n{\"b\"").len() == 1);
        let got = l.feed(b":2}\r\n\n{\"c\":3}\n");
        assert_eq!(
            got,
            vec![
                (2, "{\"b\":2}".to_owned()),
                (3, String::new()),
                (4, "{\"c\":3}".to_owned())
            ]
        );
        assert_eq!(l.count(), 4);
    }

    #[test]
    fn tail_reads_complete_lines_once_and_resets_on_shrink() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("s.jsonl");
        let mut t = LineTail::new(&path);
        assert_eq!(t.poll().unwrap(), Chunk::default());
        let mut f = File::create(&path).unwrap();
        f.write_all(b"one\ntw").unwrap();
        assert_eq!(t.poll().unwrap().lines, b"one\n");
        assert!(t.poll().unwrap().lines.is_empty());
        f.write_all(b"o\n").unwrap();
        assert_eq!(t.poll().unwrap().lines, b"two\n");
        assert_eq!(t.offset(), 8);
        std::fs::write(&path, b"x\n").unwrap();
        let c = t.poll().unwrap();
        assert!(c.reset);
        assert_eq!(c.lines, b"x\n");
    }
}
