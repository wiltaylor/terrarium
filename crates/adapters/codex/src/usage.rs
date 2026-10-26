//! Usage accounting from a Codex session file (PLAN §16.2), and the rate
//! limits that come with it.
//!
//! Rules from `docs/compat.md` ("Codex 0.160.0", "Session file and usage
//! accounting"):
//!
//! - `token_usage_record.payload.usage` is the usage of one model response.
//!   Count each `response_id` once. `thread_token_usage` is the running
//!   total of the session.
//! - The model is in `turn_context.payload.model` (one per turn), not in
//!   the usage record.
//! - `event_msg` with `payload.type` = `token_count` carries `rate_limits`
//!   with every turn: a free quota probe. On the Pro plan only the 7-day
//!   primary window was there.
//! - `input_tokens` includes `cached_input_tokens`: `total_tokens` is
//!   `input_tokens + output_tokens` in every captured record.

use std::collections::HashSet;
use std::path::{Path, PathBuf};

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use terrarium_core::tail::LineTail;

use crate::error::{CodexError, Result};

/// Token counts in the Codex shape.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
pub struct TokenUsage {
    /// All input tokens, the cached ones included.
    #[serde(default)]
    pub input_tokens: u64,
    /// The part of `input_tokens` that came from the cache.
    #[serde(default)]
    pub cached_input_tokens: u64,
    #[serde(default)]
    pub cache_write_input_tokens: u64,
    #[serde(default)]
    pub output_tokens: u64,
    /// Part of `output_tokens`.
    #[serde(default)]
    pub reasoning_output_tokens: u64,
    #[serde(default)]
    pub total_tokens: u64,
}

impl TokenUsage {
    /// The input tokens that did not come from the cache.
    pub fn uncached_input_tokens(&self) -> u64 {
        self.input_tokens.saturating_sub(self.cached_input_tokens)
    }

    /// Cache reads plus cache writes, for `usage_samples.cache_tokens`.
    pub fn cache_tokens(&self) -> u64 {
        self.cached_input_tokens + self.cache_write_input_tokens
    }
}

/// The usage of one model response.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct UsageSample {
    /// The dedup key.
    pub response_id: String,
    /// The session ID (`thread_id`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub session_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub turn_id: Option<String>,
    /// From the last `turn_context`. Empty when none came first.
    pub model: String,
    pub usage: TokenUsage,
    /// The session totals after this response.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub thread_total: Option<TokenUsage>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub at: Option<DateTime<Utc>>,
}

/// One rate-limit window.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct RateWindow {
    /// 0 to 100.
    pub used_percent: f64,
    /// The window length. 10080 is 7 days.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub window_minutes: Option<u64>,
    /// When the window resets, in Unix seconds.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub resets_at: Option<i64>,
}

impl RateWindow {
    /// `resets_at` as a time.
    pub fn resets(&self) -> Option<DateTime<Utc>> {
        DateTime::from_timestamp(self.resets_at?, 0)
    }

    /// 100 minus `used_percent`, as `/status` shows it ("100% left").
    pub fn left_percent(&self) -> f64 {
        (100.0 - self.used_percent).clamp(0.0, 100.0)
    }
}

/// The credits block of `rate_limits`.
#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
pub struct Credits {
    #[serde(default)]
    pub has_credits: bool,
    #[serde(default)]
    pub unlimited: bool,
    /// A decimal string, for example `"0"`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub balance: Option<String>,
}

/// `token_count.rate_limits`. The subscriptions probe reads the windows.
#[derive(Debug, Clone, PartialEq, Default, Serialize, Deserialize)]
pub struct RateLimits {
    /// For example `codex`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub limit_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub limit_name: Option<String>,
    /// The 7-day window on the Pro plan.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub primary: Option<RateWindow>,
    /// `null` on the Pro plan. Other plans are not tested.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub secondary: Option<RateWindow>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub credits: Option<Credits>,
    /// For example `pro`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub plan_type: Option<String>,
    /// `null` until a limit is hit. The values are not tested.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub rate_limit_reached_type: Option<String>,
    /// The time of the record.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub at: Option<DateTime<Utc>>,
}

impl RateLimits {
    /// Every window that is there, primary first.
    pub fn windows(&self) -> impl Iterator<Item = &RateWindow> {
        self.primary.iter().chain(self.secondary.iter())
    }
}

/// One usage item from the session file.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum UsageRecord {
    /// A new model response.
    Sample(UsageSample),
    /// The rate limits after a response.
    RateLimits(RateLimits),
}

/// Reads session-file lines one at a time and keeps the state that the
/// dedup rule and the model lookup need.
#[derive(Debug, Default)]
pub struct UsageParser {
    seen: HashSet<String>,
    model: Option<String>,
    last_limits: Option<RateLimits>,
    last_total: Option<TokenUsage>,
}

fn s(v: &Value, key: &str) -> Option<String> {
    v.get(key).and_then(Value::as_str).map(str::to_owned)
}

fn time(v: &Value) -> Option<DateTime<Utc>> {
    v.get("timestamp")
        .and_then(Value::as_str)
        .and_then(|t| DateTime::parse_from_rfc3339(t).ok())
        .map(|t| t.with_timezone(&Utc))
}

fn tokens(v: Option<&Value>) -> Option<TokenUsage> {
    v.and_then(|u| serde_json::from_value(u.clone()).ok())
}

impl UsageParser {
    pub fn new() -> Self {
        Self::default()
    }

    /// Parse one line. Lines that do not parse, or that carry no new usage,
    /// give `None`.
    pub fn line(&mut self, line: &str) -> Option<UsageRecord> {
        let v: Value = serde_json::from_str(line.trim()).ok()?;
        let payload = v.get("payload")?;
        match v.get("type").and_then(Value::as_str)? {
            "turn_context" => {
                if let Some(m) = s(payload, "model") {
                    self.model = Some(m);
                }
                None
            }
            "token_usage_record" => self.sample(&v, payload).map(UsageRecord::Sample),
            "event_msg" if payload.get("type").and_then(Value::as_str) == Some("token_count") => {
                let limits = payload.get("rate_limits").filter(|l| !l.is_null())?;
                let mut limits: RateLimits = serde_json::from_value(limits.clone()).ok()?;
                limits.at = time(&v);
                self.last_limits = Some(limits.clone());
                Some(UsageRecord::RateLimits(limits))
            }
            _ => None,
        }
    }

    fn sample(&mut self, v: &Value, p: &Value) -> Option<UsageSample> {
        let id = s(p, "response_id")?;
        let usage = tokens(p.get("usage"))?;
        if !self.seen.insert(id.clone()) {
            return None;
        }
        let thread_total = tokens(p.get("thread_token_usage"));
        if thread_total.is_some() {
            self.last_total = thread_total;
        }
        Some(UsageSample {
            response_id: id,
            session_id: s(p, "thread_id").or_else(|| s(p, "session_id")),
            turn_id: s(p, "turn_id"),
            model: self.model.clone().unwrap_or_default(),
            usage,
            thread_total,
            at: time(v),
        })
    }

    /// The last rate limits seen.
    pub fn rate_limits(&self) -> Option<&RateLimits> {
        self.last_limits.as_ref()
    }

    /// The last session totals seen.
    pub fn totals(&self) -> Option<&TokenUsage> {
        self.last_total.as_ref()
    }
}

/// The usage in one whole session file.
#[derive(Debug, Clone, PartialEq, Default, Serialize, Deserialize)]
pub struct TranscriptUsage {
    /// One per `response_id`, in file order.
    pub samples: Vec<UsageSample>,
    /// The last `thread_token_usage`.
    pub totals: Option<TokenUsage>,
    /// The last `rate_limits`.
    pub rate_limits: Option<RateLimits>,
}

/// Read a whole session file. The daemon calls this with the
/// `transcript_path` of a hook payload.
pub fn read_transcript(path: &Path) -> Result<TranscriptUsage> {
    let text = std::fs::read_to_string(path).map_err(|e| CodexError::io(path, e))?;
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
        totals: parser.totals().copied(),
        rate_limits: parser.rate_limits().cloned(),
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
            .map_err(|e| CodexError::io(self.tail.path(), e))?;
        Ok(String::from_utf8_lossy(&chunk.lines)
            .lines()
            .filter_map(|l| self.parser.line(l))
            .collect())
    }

    pub fn rate_limits(&self) -> Option<&RateLimits> {
        self.parser.rate_limits()
    }

    pub fn totals(&self) -> Option<&TokenUsage> {
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
    fn samples_from_the_captured_session() {
        let usage = read_transcript(&fixture()).expect("read");
        assert_eq!(usage.samples.len(), 2);
        let a = &usage.samples[0];
        assert_eq!(
            a.response_id,
            "resp_0cd04408c12ec244016adc568c2bcc81919b7219b42f55639e"
        );
        assert_eq!(
            a.session_id.as_deref(),
            Some("01a16cb2-0d95-7b40-894d-e19fc1b66c24")
        );
        assert_eq!(
            a.turn_id.as_deref(),
            Some("01a16cb2-11a8-7280-8120-a56d347b7d4d")
        );
        assert_eq!(a.model, "gpt-6-luna");
        assert_eq!(
            a.usage,
            TokenUsage {
                input_tokens: 14160,
                cached_input_tokens: 11392,
                cache_write_input_tokens: 0,
                output_tokens: 5,
                reasoning_output_tokens: 0,
                total_tokens: 14165,
            }
        );
        assert_eq!(a.usage.uncached_input_tokens(), 2768);
        assert_eq!(a.usage.cache_tokens(), 11392);
        assert!(a.at.is_some());
        let b = &usage.samples[1];
        assert_eq!((b.usage.input_tokens, b.usage.output_tokens), (16700, 5));
        let total = usage.totals.expect("totals");
        assert_eq!((total.input_tokens, total.total_tokens), (30860, 30870));
        // `codex` prints the same numbers at exit:
        // "total=5,526 input=5,516 (+ 25,344 cached) output=10".
        assert_eq!(total.uncached_input_tokens(), 5516);
        for s in &usage.samples {
            assert_eq!(
                s.usage.total_tokens,
                s.usage.input_tokens + s.usage.output_tokens
            );
        }
    }

    #[test]
    fn rate_limits_from_the_captured_session() {
        let usage = read_transcript(&fixture()).expect("read");
        let rl = usage.rate_limits.expect("rate limits");
        assert_eq!(rl.limit_id.as_deref(), Some("codex"));
        assert_eq!(rl.plan_type.as_deref(), Some("pro"));
        assert_eq!(rl.secondary, None);
        assert_eq!(rl.rate_limit_reached_type, None);
        let p = rl.primary.as_ref().expect("primary");
        assert_eq!(p.used_percent, 0.0);
        assert_eq!(p.window_minutes, Some(10080));
        assert_eq!(p.resets_at, Some(1793396550));
        assert_eq!(p.left_percent(), 100.0);
        assert_eq!(
            p.resets().map(|t| t.to_rfc3339()),
            Some("2026-10-30T21:42:30+00:00".into())
        );
        assert_eq!(rl.windows().count(), 1);
        let c = rl.credits.as_ref().expect("credits");
        assert!(!c.has_credits && !c.unlimited);
        assert_eq!(c.balance.as_deref(), Some("0"));
        assert!(rl.at.is_some());
    }

    #[test]
    fn repeats_and_other_lines_give_nothing() {
        let text = std::fs::read_to_string(fixture()).expect("read");
        let mut p = UsageParser::new();
        let first: Vec<UsageRecord> = text.lines().filter_map(|l| p.line(l)).collect();
        // Two samples and two rate-limit records.
        assert_eq!(first.len(), 4);
        let again: Vec<UsageRecord> = text.lines().filter_map(|l| p.line(l)).collect();
        assert!(
            again
                .iter()
                .all(|r| matches!(r, UsageRecord::RateLimits(_)))
        );
        assert!(p.line("not json").is_none());
        assert!(p.line(r#"{"type":"world_state","payload":{}}"#).is_none());
        assert!(
            p.line(r#"{"type":"event_msg","payload":{"type":"token_count","info":null,"rate_limits":null}}"#)
                .is_none()
        );
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
        assert!(tailer.rate_limits().is_some());
        assert_eq!(tailer.totals().map(|t| t.total_tokens), Some(30870));
    }
}
