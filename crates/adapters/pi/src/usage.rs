//! Usage accounting from a pi session file (PLAN §16.2).
//!
//! The session file is JSON lines (`docs/compat.md`, "Session file"):
//!
//! - The first line is the header: `{"type":"session","version":3,"id":…,
//!   "timestamp":…,"cwd":…}`.
//! - `model_change` and `thinking_level_change` records.
//! - One `message` record per message. Each has an `id` (8 hex digits,
//!   unique in the file) and a `message`. An assistant message has
//!   `provider`, `model`, `stopReason`, and `usage`: `input`, `output`,
//!   `cacheRead`, `cacheWrite`, `reasoning`, `totalTokens`, and `cost`
//!   (API-price dollars, also for a subscription).
//!
//! Each assistant `message` record with usage is one sample. Count each
//! record `id` once. A failed call (`stopReason` = `error`) has all-zero
//! usage; it gives no sample. Whether `reasoning` is part of `output` is
//! not verified (all captured calls had `reasoning` = 0).

use std::collections::HashSet;
use std::fs::File;
use std::io::{Read, Seek, SeekFrom};
use std::path::{Path, PathBuf};

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::error::{PiError, Result};

/// The usage of one model call.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct UsageSample {
    /// The record `id`. Unique in one session file only: key it with the
    /// session ID.
    pub message_id: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub session_id: Option<String>,
    /// The pi provider id, for example `openai-codex`.
    pub provider: String,
    pub model: String,
    pub input_tokens: u64,
    pub output_tokens: u64,
    pub cache_read_tokens: u64,
    pub cache_write_tokens: u64,
    pub reasoning_tokens: u64,
    pub total_tokens: u64,
    /// pi's API-price estimate in dollars. Not the subscription charge.
    pub cost_usd: f64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub stop_reason: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub at: Option<DateTime<Utc>>,
}

impl UsageSample {
    /// A key that is unique across sessions: `<session id>:<record id>`.
    pub fn key(&self) -> String {
        match &self.session_id {
            Some(s) => format!("{s}:{}", self.message_id),
            None => self.message_id.clone(),
        }
    }

    /// Cache reads plus cache writes, for `usage_samples.cache_tokens`.
    pub fn cache_tokens(&self) -> u64 {
        self.cache_read_tokens + self.cache_write_tokens
    }
}

/// Reads session-file lines one at a time.
#[derive(Debug, Default)]
pub struct UsageParser {
    seen: HashSet<String>,
    session_id: Option<String>,
}

fn u(v: &Value, key: &str) -> u64 {
    v.get(key).and_then(Value::as_u64).unwrap_or(0)
}

fn s(v: &Value, key: &str) -> Option<String> {
    v.get(key).and_then(Value::as_str).map(str::to_owned)
}

impl UsageParser {
    pub fn new() -> Self {
        Self::default()
    }

    /// The session ID from the header, once it was read.
    pub fn session_id(&self) -> Option<&str> {
        self.session_id.as_deref()
    }

    /// Parse one line. Lines that do not parse, or that carry no new usage,
    /// give `None`.
    pub fn line(&mut self, line: &str) -> Option<UsageSample> {
        let v: Value = serde_json::from_str(line.trim()).ok()?;
        match v.get("type").and_then(Value::as_str)? {
            "session" => {
                self.session_id = s(&v, "id");
                None
            }
            "message" => self.message(&v),
            _ => None,
        }
    }

    fn message(&mut self, v: &Value) -> Option<UsageSample> {
        let msg = v.get("message")?;
        if msg.get("role").and_then(Value::as_str) != Some("assistant") {
            return None;
        }
        let usage = msg.get("usage")?;
        let id = s(v, "id")?;
        let sample = UsageSample {
            message_id: id.clone(),
            session_id: self.session_id.clone(),
            provider: s(msg, "provider").unwrap_or_default(),
            model: s(msg, "model").unwrap_or_default(),
            input_tokens: u(usage, "input"),
            output_tokens: u(usage, "output"),
            cache_read_tokens: u(usage, "cacheRead"),
            cache_write_tokens: u(usage, "cacheWrite"),
            reasoning_tokens: u(usage, "reasoning"),
            total_tokens: u(usage, "totalTokens"),
            cost_usd: usage
                .get("cost")
                .and_then(|c| c.get("total"))
                .and_then(Value::as_f64)
                .unwrap_or(0.0),
            stop_reason: s(msg, "stopReason"),
            at: s(v, "timestamp").and_then(|t| t.parse().ok()),
        };
        let empty = sample.input_tokens == 0
            && sample.output_tokens == 0
            && sample.cache_read_tokens == 0
            && sample.cache_write_tokens == 0
            && sample.total_tokens == 0;
        if empty || !self.seen.insert(id) {
            return None;
        }
        Some(sample)
    }
}

/// Parse a whole session file text.
pub fn parse_session(text: &str) -> Vec<UsageSample> {
    let mut p = UsageParser::new();
    text.lines().filter_map(|l| p.line(l)).collect()
}

/// Follows a session file as pi appends to it. Each [`SessionTail::poll`]
/// reads the new complete lines.
#[derive(Debug)]
pub struct SessionTail {
    path: PathBuf,
    offset: u64,
    partial: Vec<u8>,
    parser: UsageParser,
}

impl SessionTail {
    pub fn new(path: impl Into<PathBuf>) -> Self {
        Self {
            path: path.into(),
            offset: 0,
            partial: Vec::new(),
            parser: UsageParser::new(),
        }
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    /// Read the new lines. A missing file gives no samples: pi creates the
    /// file at the first message.
    pub fn poll(&mut self) -> Result<Vec<UsageSample>> {
        let mut file = match File::open(&self.path) {
            Ok(f) => f,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
            Err(e) => return Err(PiError::io(&self.path, e)),
        };
        let len = file
            .metadata()
            .map_err(|e| PiError::io(&self.path, e))?
            .len();
        if len < self.offset {
            self.offset = 0;
            self.partial.clear();
        }
        file.seek(SeekFrom::Start(self.offset))
            .map_err(|e| PiError::io(&self.path, e))?;
        let mut buf = Vec::new();
        file.read_to_end(&mut buf)
            .map_err(|e| PiError::io(&self.path, e))?;
        self.offset += buf.len() as u64;
        self.partial.extend_from_slice(&buf);

        let Some(end) = self.partial.iter().rposition(|&b| b == b'\n') else {
            return Ok(Vec::new());
        };
        let rest = self.partial.split_off(end + 1);
        let complete = std::mem::replace(&mut self.partial, rest);
        Ok(String::from_utf8_lossy(&complete)
            .lines()
            .filter_map(|l| self.parser.line(l))
            .collect())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn fixture() -> String {
        std::fs::read_to_string(format!(
            "{}/tests/fixtures/session.jsonl",
            env!("CARGO_MANIFEST_DIR")
        ))
        .expect("fixture")
    }

    #[test]
    fn captured_session_gives_two_samples() {
        let samples = parse_session(&fixture());
        assert_eq!(samples.len(), 2);
        let a = &samples[0];
        assert_eq!(a.message_id, "a90f1b60");
        assert_eq!(
            a.session_id.as_deref(),
            Some("0aa4e42a-0404-423c-a41c-332c83e48e25")
        );
        assert_eq!(a.key(), "0aa4e42a-0404-423c-a41c-332c83e48e25:a90f1b60");
        assert_eq!(
            (a.provider.as_str(), a.model.as_str()),
            ("openai-codex", "gpt-5.6-luna")
        );
        assert_eq!(
            (a.input_tokens, a.output_tokens, a.total_tokens),
            (1110, 5, 1115)
        );
        assert_eq!(a.stop_reason.as_deref(), Some("stop"));
        assert!((a.cost_usd - 0.000228).abs() < 1e-9);
        assert_eq!(
            a.at.map(|t| t.to_rfc3339()),
            Some("2026-10-24T07:29:10.440+00:00".into())
        );
        let b = &samples[1];
        assert_eq!(
            (b.input_tokens, b.cache_read_tokens, b.cache_tokens()),
            (115, 1024, 1024)
        );
    }

    #[test]
    fn repeats_errors_and_noise_give_nothing() {
        let mut p = UsageParser::new();
        let line = fixture().lines().nth(4).expect("assistant").to_owned();
        assert!(p.line(&line).is_some());
        assert!(p.line(&line).is_none());
        let error = r#"{"type":"message","id":"e1","message":{"role":"assistant","provider":"x","model":"m","stopReason":"error","usage":{"input":0,"output":0,"cacheRead":0,"cacheWrite":0,"totalTokens":0}}}"#;
        assert!(p.line(error).is_none());
        assert!(p.line("not json").is_none());
        assert!(p.line(r#"{"type":"model_change"}"#).is_none());
    }

    #[test]
    fn tail_reads_appended_lines_once() {
        let tmp = tempfile::tempdir().expect("tmp");
        let path = tmp.path().join("s.jsonl");
        let mut tail = SessionTail::new(&path);
        assert!(tail.poll().expect("missing").is_empty());
        let text = fixture();
        let lines: Vec<&str> = text.lines().collect();
        std::fs::write(
            &path,
            format!("{}\n{}", lines[..5].join("\n"), &lines[5][..10]),
        )
        .expect("write");
        assert_eq!(tail.poll().expect("first").len(), 1);
        std::fs::write(&path, format!("{}\n", lines.join("\n"))).expect("write");
        let more = tail.poll().expect("second");
        assert_eq!(more.len(), 1);
        assert_eq!(more[0].message_id, "442754d0");
        assert!(tail.poll().expect("third").is_empty());
    }
}
