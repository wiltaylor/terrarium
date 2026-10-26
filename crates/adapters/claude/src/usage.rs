//! Usage accounting from a Claude Code session file (PLAN §16.2).
//!
//! Rules from `docs/compat.md`, "Session file and usage accounting":
//!
//! - One API response can make several `assistant` records (one per
//!   content block). They repeat the same `message.id` and `usage`. Count
//!   each `message.id` once.
//! - `assistant` records cover the main thread only. Side calls (for
//!   example the title call) are only in `cost-state`. Use the `cost-state`
//!   deltas for totals, and `assistant` records for per-turn detail.
//! - `cost-state` repeats with the same values. Use the delta from the last
//!   record that was seen.
//! - `costUSD` is the API-price equivalent, not the subscription charge.

use std::collections::{BTreeMap, HashSet};
use std::path::{Path, PathBuf};

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use terrarium_core::tail::LineTail;

use crate::error::{ClaudeError, Result};

/// The usage of one API response on the main thread.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct UsageSample {
    pub message_id: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub request_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub session_id: Option<String>,
    pub model: String,
    pub input_tokens: u64,
    pub output_tokens: u64,
    pub cache_creation_input_tokens: u64,
    pub cache_read_input_tokens: u64,
    /// Part of `output_tokens`.
    pub thinking_tokens: u64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub at: Option<DateTime<Utc>>,
}

impl UsageSample {
    /// Cache reads plus cache writes, for `usage_samples.cache_tokens`.
    pub fn cache_tokens(&self) -> u64 {
        self.cache_creation_input_tokens + self.cache_read_input_tokens
    }
}

/// Usage of one model in a `cost-state` record.
#[derive(Debug, Clone, PartialEq, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ModelUsage {
    #[serde(default)]
    pub input_tokens: u64,
    #[serde(default)]
    pub output_tokens: u64,
    #[serde(default)]
    pub thinking_tokens: u64,
    #[serde(default)]
    pub cache_read_input_tokens: u64,
    #[serde(default)]
    pub cache_creation_input_tokens: u64,
    #[serde(default)]
    pub web_search_requests: u64,
    #[serde(default, rename = "costUSD")]
    pub cost_usd: f64,
}

impl ModelUsage {
    fn minus(&self, before: &ModelUsage) -> ModelUsage {
        ModelUsage {
            input_tokens: self.input_tokens.saturating_sub(before.input_tokens),
            output_tokens: self.output_tokens.saturating_sub(before.output_tokens),
            thinking_tokens: self.thinking_tokens.saturating_sub(before.thinking_tokens),
            cache_read_input_tokens: self
                .cache_read_input_tokens
                .saturating_sub(before.cache_read_input_tokens),
            cache_creation_input_tokens: self
                .cache_creation_input_tokens
                .saturating_sub(before.cache_creation_input_tokens),
            web_search_requests: self
                .web_search_requests
                .saturating_sub(before.web_search_requests),
            cost_usd: (self.cost_usd - before.cost_usd).max(0.0),
        }
    }

    fn is_zero(&self) -> bool {
        self.input_tokens == 0
            && self.output_tokens == 0
            && self.thinking_tokens == 0
            && self.cache_read_input_tokens == 0
            && self.cache_creation_input_tokens == 0
            && self.web_search_requests == 0
            && self.cost_usd == 0.0
    }
}

/// The running totals of a session (a `cost-state` record), or a delta
/// between two of them.
#[derive(Debug, Clone, PartialEq, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct CostState {
    #[serde(default, rename = "totalCostUSD")]
    pub total_cost_usd: f64,
    #[serde(default)]
    pub model_usage: BTreeMap<String, ModelUsage>,
}

impl CostState {
    /// `self - before`, per model. Models with no change are left out.
    pub fn delta(&self, before: &CostState) -> CostState {
        let zero = ModelUsage::default();
        let model_usage = self
            .model_usage
            .iter()
            .map(|(m, u)| {
                (
                    m.clone(),
                    u.minus(before.model_usage.get(m).unwrap_or(&zero)),
                )
            })
            .filter(|(_, u)| !u.is_zero())
            .collect();
        CostState {
            total_cost_usd: (self.total_cost_usd - before.total_cost_usd).max(0.0),
            model_usage,
        }
    }

    pub fn is_empty(&self) -> bool {
        self.model_usage.is_empty()
    }
}

/// One usage item from the session file.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum UsageRecord {
    /// A new API response on the main thread.
    Sample(UsageSample),
    /// The totals moved. `delta` is the change since the last `cost-state`.
    Cost { totals: CostState, delta: CostState },
}

/// Reads session-file lines one at a time and keeps the state that the
/// dedup rules need.
#[derive(Debug, Default)]
pub struct UsageParser {
    seen: HashSet<String>,
    last_cost: Option<CostState>,
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

    /// Parse one line. Lines that do not parse, or that carry no new usage,
    /// give `None`.
    pub fn line(&mut self, line: &str) -> Option<UsageRecord> {
        let v: Value = serde_json::from_str(line.trim()).ok()?;
        match v.get("type").and_then(Value::as_str)? {
            "assistant" => self.assistant(&v).map(UsageRecord::Sample),
            "cost-state" => self.cost(v),
            _ => None,
        }
    }

    fn assistant(&mut self, v: &Value) -> Option<UsageSample> {
        let msg = v.get("message")?;
        let usage = msg.get("usage")?;
        let id = s(msg, "id")?;
        if !self.seen.insert(id.clone()) {
            return None;
        }
        Some(UsageSample {
            message_id: id,
            request_id: s(v, "requestId"),
            session_id: s(v, "sessionId"),
            model: s(msg, "model").unwrap_or_default(),
            input_tokens: u(usage, "input_tokens"),
            output_tokens: u(usage, "output_tokens"),
            cache_creation_input_tokens: u(usage, "cache_creation_input_tokens"),
            cache_read_input_tokens: u(usage, "cache_read_input_tokens"),
            thinking_tokens: usage
                .get("output_tokens_details")
                .map(|d| u(d, "thinking_tokens"))
                .unwrap_or(0),
            at: v
                .get("timestamp")
                .and_then(Value::as_str)
                .and_then(|t| DateTime::parse_from_rfc3339(t).ok())
                .map(|t| t.with_timezone(&Utc)),
        })
    }

    fn cost(&mut self, v: Value) -> Option<UsageRecord> {
        let totals: CostState = serde_json::from_value(v).ok()?;
        let delta = totals.delta(self.last_cost.as_ref().unwrap_or(&CostState::default()));
        self.last_cost = Some(totals.clone());
        (!delta.is_empty()).then_some(UsageRecord::Cost { totals, delta })
    }

    /// The last `cost-state` totals seen.
    pub fn totals(&self) -> Option<&CostState> {
        self.last_cost.as_ref()
    }
}

/// The usage in one whole session file.
#[derive(Debug, Clone, PartialEq, Default, Serialize, Deserialize)]
pub struct TranscriptUsage {
    /// One per `message.id`, in file order.
    pub samples: Vec<UsageSample>,
    /// The last `cost-state` record: the session totals, side calls
    /// included.
    pub totals: Option<CostState>,
}

/// Read a whole session file. The daemon calls this with the
/// `transcript_path` of a hook payload.
pub fn read_transcript(path: &Path) -> Result<TranscriptUsage> {
    let text = std::fs::read_to_string(path).map_err(|e| ClaudeError::io(path, e))?;
    let mut parser = UsageParser::new();
    let samples = text
        .lines()
        .filter_map(|l| match parser.line(l) {
            Some(UsageRecord::Sample(s)) => Some(s),
            _ => None,
        })
        .collect();
    Ok(TranscriptUsage {
        samples,
        totals: parser.totals().cloned(),
    })
}

/// Follows a session file as it grows. Call [`TranscriptTail::poll`] on a
/// timer or after each `Stop` hook. A partial last line waits for the next
/// poll. If the file gets shorter, the tail starts again from the top (the
/// dedup state stays, so nothing counts twice).
#[derive(Debug)]
pub struct TranscriptTail {
    tail: LineTail,
    parser: UsageParser,
}

impl TranscriptTail {
    pub fn new(path: impl Into<PathBuf>) -> Self {
        Self {
            tail: LineTail::new(path),
            parser: UsageParser::new(),
        }
    }

    pub fn path(&self) -> &Path {
        self.tail.path()
    }

    /// Read the new lines. A missing file gives no records.
    pub fn poll(&mut self) -> Result<Vec<UsageRecord>> {
        let chunk = self
            .tail
            .poll()
            .map_err(|e| ClaudeError::io(self.tail.path(), e))?;
        Ok(String::from_utf8_lossy(&chunk.lines)
            .lines()
            .filter_map(|l| self.parser.line(l))
            .collect())
    }

    pub fn totals(&self) -> Option<&CostState> {
        self.parser.totals()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs::File;
    use std::io::Write;

    fn fixture() -> PathBuf {
        PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/session.jsonl")
    }

    #[test]
    fn repeated_assistant_records_count_once() {
        let usage = read_transcript(&fixture()).expect("read");
        assert_eq!(usage.samples.len(), 2);
        let a = &usage.samples[0];
        assert_eq!(a.message_id, "msg_011CgLZ4D8Uzva9rUiX7ME9A");
        assert_eq!(
            a.request_id.as_deref(),
            Some("req_011CgLZ4CvLCehzhpuUEXtfp")
        );
        assert_eq!(a.model, "claude-haiku-5-5");
        assert_eq!(
            (a.input_tokens, a.output_tokens, a.thinking_tokens),
            (2, 45, 41)
        );
        assert_eq!(
            (a.cache_creation_input_tokens, a.cache_read_input_tokens),
            (36712, 0)
        );
        assert_eq!(a.cache_tokens(), 36712);
        assert!(a.at.is_some());
        let b = &usage.samples[1];
        assert_eq!(
            (
                b.input_tokens,
                b.output_tokens,
                b.cache_creation_input_tokens,
                b.cache_read_input_tokens
            ),
            (2, 10, 271, 36712)
        );
        let totals = usage.totals.expect("totals");
        assert!((totals.total_cost_usd - 0.00791902).abs() < 1e-12);
        let m = &totals.model_usage["claude-haiku-5-5"];
        // Side calls are in cost-state: 1188 input tokens against 4 in the
        // assistant records.
        assert_eq!((m.input_tokens, m.output_tokens), (1188, 73));
        assert_eq!(
            (m.cache_read_input_tokens, m.cache_creation_input_tokens),
            (36712, 36983)
        );
    }

    #[test]
    fn cost_deltas_skip_repeats() {
        let text = std::fs::read_to_string(fixture()).expect("read");
        let mut p = UsageParser::new();
        let deltas: Vec<CostState> = text
            .lines()
            .filter_map(|l| match p.line(l) {
                Some(UsageRecord::Cost { delta, .. }) => Some(delta),
                _ => None,
            })
            .collect();
        // Six cost-state records, two distinct values.
        assert_eq!(deltas.len(), 2);
        assert_eq!(deltas[0].model_usage["claude-haiku-5-5"].input_tokens, 1186);
        let d = &deltas[1].model_usage["claude-haiku-5-5"];
        assert_eq!(
            (
                d.input_tokens,
                d.output_tokens,
                d.cache_read_input_tokens,
                d.cache_creation_input_tokens
            ),
            (2, 10, 36712, 271)
        );
        assert!((deltas[1].total_cost_usd - 0.00042652).abs() < 1e-9);
        assert!(p.line("not json").is_none());
        assert!(p.line(r#"{"type":"user"}"#).is_none());
    }

    #[test]
    fn tail_reads_only_complete_new_lines() {
        let tmp = tempfile::tempdir().expect("tmp");
        let path = tmp.path().join("s.jsonl");
        let text = std::fs::read_to_string(fixture()).expect("read");
        let (head, tail) = text.split_at(text.len() / 2);
        let mut tailer = TranscriptTail::new(&path);
        assert!(tailer.poll().expect("missing").is_empty());

        let mut f = File::create(&path).expect("create");
        f.write_all(head.as_bytes()).expect("write");
        let first = tailer.poll().expect("poll 1");
        f.write_all(tail.as_bytes()).expect("write");
        let second = tailer.poll().expect("poll 2");
        assert!(tailer.poll().expect("poll 3").is_empty());

        let samples = first
            .iter()
            .chain(&second)
            .filter(|r| matches!(r, UsageRecord::Sample(_)))
            .count();
        assert_eq!(samples, 2);
        let costs = first
            .iter()
            .chain(&second)
            .filter(|r| matches!(r, UsageRecord::Cost { .. }))
            .count();
        assert_eq!(costs, 2);
        assert!(tailer.totals().is_some());
    }
}
