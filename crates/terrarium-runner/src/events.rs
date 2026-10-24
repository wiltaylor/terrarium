//! `events.jsonl`: one `EventRecord` on each line. The seq of a record is
//! its line number, from 1. The seq goes on across runner restarts, because
//! the runner appends to the file.

use std::fs::{File, OpenOptions};
use std::io::{BufRead, BufReader, Read, Seek, SeekFrom, Write};
use std::path::Path;

use terrarium_core::EventRecord;

use crate::error::{Result, RunnerError};

/// The open log and the seq of its last line.
pub(crate) struct EventLog {
    file: File,
    seq: u64,
}

impl EventLog {
    /// Open the log for append. A last line with no newline (the runner
    /// died while it wrote) gets one, so that the next record starts on a
    /// new line. That broken line keeps its seq.
    pub(crate) fn open(path: &Path) -> Result<Self> {
        let mut file = OpenOptions::new()
            .create(true)
            .read(true)
            .append(true)
            .open(path)
            .map_err(|e| RunnerError::io("open", path, e))?;
        let (lines, ends_in_newline) =
            count_lines(&mut file).map_err(|e| RunnerError::io("read", path, e))?;
        let mut seq = lines;
        if !ends_in_newline {
            file.write_all(b"\n")
                .map_err(|e| RunnerError::io("write", path, e))?;
            seq += 1;
        }
        Ok(Self { file, seq })
    }

    /// The seq of the last line.
    pub(crate) fn seq(&self) -> u64 {
        self.seq
    }

    /// Give `record` the next seq and append it.
    pub(crate) fn append(&mut self, record: &mut EventRecord) -> std::io::Result<()> {
        self.seq += 1;
        record.seq = Some(self.seq);
        let mut line = serde_json::to_vec(record).map_err(std::io::Error::other)?;
        line.push(b'\n');
        self.file.write_all(&line)
    }
}

/// The number of complete lines, and whether the file is empty or ends in
/// a newline.
fn count_lines(file: &mut File) -> std::io::Result<(u64, bool)> {
    file.seek(SeekFrom::Start(0))?;
    let mut buf = vec![0u8; 64 * 1024];
    let mut lines = 0u64;
    let mut last = b'\n';
    loop {
        let n = file.read(&mut buf)?;
        if n == 0 {
            break;
        }
        lines += buf[..n].iter().filter(|b| **b == b'\n').count() as u64;
        last = buf[n - 1];
    }
    Ok((lines, last == b'\n'))
}

/// The records after `since`, in order, with their seq set from the line
/// number. Lines that do not parse are skipped. A missing file has none.
pub fn read_since(path: &Path, since: u64) -> std::io::Result<Vec<EventRecord>> {
    let file = match File::open(path) {
        Ok(f) => f,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(e) => return Err(e),
    };
    let mut out = Vec::new();
    for (i, line) in BufReader::new(file).split(b'\n').enumerate() {
        let line = line?;
        let seq = i as u64 + 1;
        if seq <= since {
            continue;
        }
        match serde_json::from_slice::<EventRecord>(&line) {
            Ok(mut rec) => {
                rec.seq = Some(seq);
                out.push(rec);
            }
            Err(e) => {
                if !line.iter().all(u8::is_ascii_whitespace) {
                    tracing::debug!("{}: line {seq}: {e}", path.display());
                }
            }
        }
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;
    use terrarium_core::{AgentId, Event};

    #[test]
    fn seq_goes_on_after_a_reopen_and_a_torn_line() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("events.jsonl");
        let agent = AgentId::new();
        let mut log = EventLog::open(&path).expect("open");
        assert_eq!(log.seq(), 0);
        for _ in 0..2 {
            log.append(&mut EventRecord::now(agent, Event::TurnEnd))
                .expect("append");
        }
        drop(log);
        // A torn write: half a line with no newline.
        std::fs::OpenOptions::new()
            .append(true)
            .open(&path)
            .and_then(|mut f| f.write_all(b"{\"id\":"))
            .expect("torn");
        let mut log = EventLog::open(&path).expect("reopen");
        assert_eq!(log.seq(), 3);
        let mut rec = EventRecord::now(agent, Event::TurnEnd);
        log.append(&mut rec).expect("append");
        assert_eq!(rec.seq, Some(4));
        let all = read_since(&path, 0).expect("read");
        let seqs: Vec<_> = all.iter().map(|r| r.seq).collect();
        assert_eq!(seqs, vec![Some(1), Some(2), Some(4)]);
        assert_eq!(read_since(&path, 2).expect("read").len(), 1);
    }
}
