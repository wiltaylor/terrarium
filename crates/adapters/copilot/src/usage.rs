//! Usage accounting (PLAN §16.2): the Copilot session file and the
//! `/usage` screen.
//!
//! Rules from `docs/compat.md`, "Session file and usage accounting":
//!
//! - `session.usage_record`: one per model call on the main thread, with
//!   tokens and `cost`, the premium-request multiplier of the call (0.33
//!   for `claude-haiku-4.5`). Count each `accounting.usageId` (else
//!   `apiCallId`) once.
//! - `session.usage_checkpoint.totalPremiumRequests`: the running total of
//!   the session. It keeps counting over a resume (0.33, then 0.66).
//! - `session.shutdown.accountingSnapshot`: the totals per model, at each
//!   exit. Also cumulative over a resume.
//! - The title side call (`gpt-4o-mini`, multiplier 0) is only in
//!   `model.model_call_success`. It is not counted here.
//!
//! The `/usage` screen shows "Requests 0 Premium" and "Weekly ■■■… 0% used
//! • resets on Oct 26 at 10 AM". [`parse_quota_screen`] reads it, for the
//! subscription probe.

use std::collections::{BTreeMap, HashSet};
use std::fs::File;
use std::io::{Read, Seek, SeekFrom};
use std::path::{Path, PathBuf};

use chrono::{DateTime, Datelike, NaiveDate, NaiveDateTime, NaiveTime, Utc};
use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::error::{CopilotError, Result};

/// The usage of one model call.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct UsageSample {
    /// `accounting.usageId`, else `apiCallId`. Unique per call.
    pub usage_id: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub session_id: Option<String>,
    pub model: String,
    pub input_tokens: u64,
    pub output_tokens: u64,
    pub cache_read_tokens: u64,
    pub cache_write_tokens: u64,
    /// Part of `output_tokens` (not checked).
    pub reasoning_tokens: u64,
    /// The premium-request multiplier of this call.
    pub cost: f64,
    /// `user` for a turn. Other values were not seen.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub initiator: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub at: Option<DateTime<Utc>>,
}

impl UsageSample {
    /// Cache reads plus cache writes.
    pub fn cache_tokens(&self) -> u64 {
        self.cache_read_tokens + self.cache_write_tokens
    }
}

/// The totals of one model in `session.shutdown`.
#[derive(Debug, Clone, PartialEq, Default, Serialize, Deserialize)]
pub struct ModelTotals {
    pub requests: u64,
    /// Premium requests (the sum of the multipliers).
    pub cost: f64,
    pub input_tokens: u64,
    pub output_tokens: u64,
    pub cache_read_tokens: u64,
    pub cache_write_tokens: u64,
    pub reasoning_tokens: u64,
}

/// The totals of a session at an exit (`session.shutdown`).
#[derive(Debug, Clone, PartialEq, Default, Serialize, Deserialize)]
pub struct SessionTotals {
    pub premium_requests: f64,
    pub user_requests: u64,
    pub models: BTreeMap<String, ModelTotals>,
}

/// One usage item from the session file.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum UsageRecord {
    /// A new model call.
    Sample(UsageSample),
    /// The running total moved. `delta` is the change since the last
    /// checkpoint that this parser saw.
    Checkpoint {
        total_premium_requests: f64,
        delta: f64,
    },
    /// The session ended.
    Shutdown(SessionTotals),
}

fn u(v: &Value, key: &str) -> u64 {
    v.get(key).and_then(Value::as_u64).unwrap_or(0)
}

fn f(v: &Value, key: &str) -> f64 {
    v.get(key).and_then(Value::as_f64).unwrap_or(0.0)
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

/// Reads session-file lines one at a time and keeps the dedup state.
#[derive(Debug, Default)]
pub struct UsageParser {
    seen: HashSet<String>,
    last_total: Option<f64>,
    last_shutdown: Option<SessionTotals>,
}

impl UsageParser {
    pub fn new() -> Self {
        Self::default()
    }

    /// Parse one line. Lines that do not parse, or carry no new usage, give
    /// `None`.
    pub fn line(&mut self, line: &str) -> Option<UsageRecord> {
        let v: Value = serde_json::from_str(line.trim()).ok()?;
        let data = v.get("data")?;
        match v.get("type").and_then(Value::as_str)? {
            "session.usage_record" => self.sample(&v, data).map(UsageRecord::Sample),
            "session.usage_checkpoint" => {
                let total = data.get("totalPremiumRequests")?.as_f64()?;
                let delta = total - self.last_total.unwrap_or(0.0);
                self.last_total = Some(total);
                Some(UsageRecord::Checkpoint {
                    total_premium_requests: total,
                    delta: delta.max(0.0),
                })
            }
            "session.shutdown" => {
                let totals = shutdown_totals(data.get("accountingSnapshot")?);
                self.last_shutdown = Some(totals.clone());
                Some(UsageRecord::Shutdown(totals))
            }
            _ => None,
        }
    }

    fn sample(&mut self, rec: &Value, data: &Value) -> Option<UsageSample> {
        let usage = data.get("usage")?;
        let id = usage
            .get("accounting")
            .and_then(|a| s(a, "usageId"))
            .or_else(|| s(usage, "apiCallId"))?;
        if !self.seen.insert(id.clone()) {
            return None;
        }
        Some(UsageSample {
            usage_id: id,
            session_id: usage
                .get("accounting")
                .and_then(|a| s(a, "sourceSessionId")),
            model: s(usage, "model").unwrap_or_default(),
            input_tokens: u(usage, "inputTokens"),
            output_tokens: u(usage, "outputTokens"),
            cache_read_tokens: u(usage, "cacheReadTokens"),
            cache_write_tokens: u(usage, "cacheWriteTokens"),
            reasoning_tokens: u(usage, "reasoningTokens"),
            cost: f(usage, "cost"),
            initiator: s(usage, "initiator"),
            at: time(rec),
        })
    }

    /// The last running total of premium requests.
    pub fn total_premium_requests(&self) -> Option<f64> {
        self.last_total
    }

    /// The totals of the last `session.shutdown`.
    pub fn last_shutdown(&self) -> Option<&SessionTotals> {
        self.last_shutdown.as_ref()
    }
}

fn shutdown_totals(snap: &Value) -> SessionTotals {
    let models = snap
        .get("modelMetrics")
        .and_then(Value::as_object)
        .map(|m| {
            m.iter()
                .map(|(model, v)| {
                    let req = v.get("requests").cloned().unwrap_or(Value::Null);
                    let usage = v.get("usage").cloned().unwrap_or(Value::Null);
                    (
                        model.clone(),
                        ModelTotals {
                            requests: u(&req, "count"),
                            cost: f(&req, "cost"),
                            input_tokens: u(&usage, "inputTokens"),
                            output_tokens: u(&usage, "outputTokens"),
                            cache_read_tokens: u(&usage, "cacheReadTokens"),
                            cache_write_tokens: u(&usage, "cacheWriteTokens"),
                            reasoning_tokens: u(&usage, "reasoningTokens"),
                        },
                    )
                })
                .collect()
        })
        .unwrap_or_default();
    SessionTotals {
        premium_requests: f(snap, "totalPremiumRequestCost"),
        user_requests: u(snap, "totalUserRequests"),
        models,
    }
}

/// The usage in one whole session file.
#[derive(Debug, Clone, PartialEq, Default, Serialize, Deserialize)]
pub struct TranscriptUsage {
    /// One per model call, in file order.
    pub samples: Vec<UsageSample>,
    /// The last running total of premium requests.
    pub total_premium_requests: Option<f64>,
    /// The totals of the last exit.
    pub totals: Option<SessionTotals>,
}

/// Read a whole session file (`session-state/<id>/events.jsonl`).
pub fn read_transcript(path: &Path) -> Result<TranscriptUsage> {
    let text = std::fs::read_to_string(path).map_err(|e| CopilotError::io(path, e))?;
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
        total_premium_requests: parser.total_premium_requests(),
        totals: parser.last_shutdown().cloned(),
    })
}

/// Follows a session file as it grows. Call [`TranscriptTail::poll`] on a
/// timer or after each `Stop` hook. A partial last line waits for the next
/// poll. If the file gets shorter, the tail starts again from the top (the
/// dedup state stays, so no call counts twice).
#[derive(Debug)]
pub struct TranscriptTail {
    path: PathBuf,
    offset: u64,
    partial: Vec<u8>,
    parser: UsageParser,
}

impl TranscriptTail {
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

    /// Read the new lines. A missing file gives no records.
    pub fn poll(&mut self) -> Result<Vec<UsageRecord>> {
        let mut file = match File::open(&self.path) {
            Ok(f) => f,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
            Err(e) => return Err(CopilotError::io(&self.path, e)),
        };
        let len = file
            .metadata()
            .map_err(|e| CopilotError::io(&self.path, e))?
            .len();
        if len < self.offset {
            self.offset = 0;
            self.partial.clear();
        }
        file.seek(SeekFrom::Start(self.offset))
            .map_err(|e| CopilotError::io(&self.path, e))?;
        let mut buf = Vec::new();
        file.read_to_end(&mut buf)
            .map_err(|e| CopilotError::io(&self.path, e))?;
        self.offset += buf.len() as u64;
        self.partial.extend_from_slice(&buf);

        let mut out = Vec::new();
        let Some(end) = self.partial.iter().rposition(|&b| b == b'\n') else {
            return Ok(out);
        };
        let rest = self.partial.split_off(end + 1);
        let complete = std::mem::replace(&mut self.partial, rest);
        for line in String::from_utf8_lossy(&complete).lines() {
            out.extend(self.parser.line(line));
        }
        Ok(out)
    }

    pub fn total_premium_requests(&self) -> Option<f64> {
        self.parser.total_premium_requests()
    }
}

/// What the `/usage` screen says about the quota.
#[derive(Debug, Clone, PartialEq, Default, Serialize, Deserialize)]
pub struct QuotaScreen {
    /// "Requests 0 Premium": premium requests of this session.
    pub session_premium_requests: Option<f64>,
    /// "Weekly … 0% used": the weekly quota used, in percent.
    pub weekly_used_percent: Option<f64>,
    /// The reset text as shown, for example "Oct 26 at 10 AM".
    pub weekly_resets: Option<String>,
}

fn number_before(text: &str, word: &str) -> Option<f64> {
    let i = text.find(word)?;
    text[..i]
        .split_whitespace()
        .last()?
        .trim_start_matches(|c: char| !c.is_ascii_digit())
        .parse()
        .ok()
}

/// Read the `/usage` screen text (lines top to bottom). `None` when it has
/// neither the "Requests" nor the "Weekly" line. Captured once, on a plan
/// with legacy premium requests; the AI-credit layout was not seen.
pub fn parse_quota_screen<S: AsRef<str>>(lines: &[S]) -> Option<QuotaScreen> {
    let mut q = QuotaScreen::default();
    let mut found = false;
    for line in lines.iter().map(AsRef::as_ref) {
        let t = line.trim().trim_end_matches('┃').trim();
        if let Some(rest) = t.strip_prefix("Requests") {
            found = true;
            q.session_premium_requests = number_before(rest, "Premium");
        } else if let Some(rest) = t.strip_prefix("Weekly") {
            found = true;
            q.weekly_used_percent = number_before(rest, "% used");
            if let Some(i) = rest.find("resets on") {
                let r = rest[i + "resets on".len()..].trim();
                q.weekly_resets = (!r.is_empty()).then(|| r.to_owned());
            }
        }
    }
    found.then_some(q)
}

/// The next time that matches a reset text like "Oct 26 at 10 AM" or
/// "Oct 26 at 10:30 PM", at or after `now`. The screen gives no year and no
/// time zone; the result is in the zone of the Copilot process (not
/// checked).
pub fn parse_reset(text: &str, now: NaiveDateTime) -> Option<NaiveDateTime> {
    let (date, clock) = text.split_once(" at ")?;
    let mut d = date.split_whitespace();
    let month = match d.next()?.get(..3)?.to_ascii_lowercase().as_str() {
        "jan" => 1,
        "feb" => 2,
        "mar" => 3,
        "apr" => 4,
        "may" => 5,
        "jun" => 6,
        "jul" => 7,
        "aug" => 8,
        "sep" => 9,
        "oct" => 10,
        "nov" => 11,
        "dec" => 12,
        _ => return None,
    };
    let day: u32 = d.next()?.trim_end_matches(',').parse().ok()?;
    let mut c = clock.split_whitespace();
    let hm = c.next()?;
    let (h, m) = match hm.split_once(':') {
        Some((h, m)) => (h.parse::<u32>().ok()?, m.parse::<u32>().ok()?),
        None => (hm.parse::<u32>().ok()?, 0),
    };
    let h = match c.next().map(str::to_ascii_uppercase).as_deref() {
        Some("AM") => h % 12,
        Some("PM") => h % 12 + 12,
        _ => h,
    };
    let t = NaiveTime::from_hms_opt(h, m, 0)?;
    let at = |y: i32| NaiveDate::from_ymd_opt(y, month, day).map(|d| d.and_time(t));
    match at(now.year()) {
        Some(x) if x >= now => Some(x),
        _ => at(now.year() + 1),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;

    fn fixture(name: &str) -> PathBuf {
        PathBuf::from(format!(
            "{}/tests/fixtures/{name}",
            env!("CARGO_MANIFEST_DIR")
        ))
    }

    #[test]
    fn captured_session_file() {
        let u = read_transcript(&fixture("events.jsonl")).expect("read");
        assert_eq!(u.samples.len(), 2);
        let a = &u.samples[0];
        assert_eq!(a.usage_id, "msg_011CgLeUYTpX4ce5AGtV7TCt");
        assert_eq!(a.model, "claude-haiku-4.5");
        assert_eq!(
            (
                a.input_tokens,
                a.output_tokens,
                a.cache_read_tokens,
                a.cache_write_tokens
            ),
            (20331, 50, 13637, 6684)
        );
        assert_eq!(a.reasoning_tokens, 42);
        assert_eq!(a.cache_tokens(), 13637 + 6684);
        assert!((a.cost - 0.33).abs() < 1e-9);
        assert_eq!(a.initiator.as_deref(), Some("user"));
        assert_eq!(
            a.session_id.as_deref(),
            Some("62e05754-b66a-4ccc-ba6e-4846cc4fc1aa")
        );
        assert!(a.at.is_some());
        // The total keeps counting over the resume.
        assert!((u.total_premium_requests.expect("total") - 0.66).abs() < 1e-9);
        let t = u.totals.expect("shutdown");
        assert!((t.premium_requests - 0.66).abs() < 1e-9);
        assert_eq!(t.user_requests, 2);
        let m = &t.models["claude-haiku-4.5"];
        assert_eq!(
            (m.requests, m.input_tokens, m.output_tokens),
            (2, 40717, 100)
        );
        // The title call (gpt-4o-mini) is not a sample.
        assert!(u.samples.iter().all(|s| s.model == "claude-haiku-4.5"));
    }

    #[test]
    fn checkpoints_give_deltas_and_dups_count_once() {
        let text = std::fs::read_to_string(fixture("events.jsonl")).expect("read");
        let mut p = UsageParser::new();
        let mut recs: Vec<UsageRecord> = text.lines().filter_map(|l| p.line(l)).collect();
        // The same lines again give no new samples.
        recs.extend(text.lines().filter_map(|l| match p.line(l) {
            Some(UsageRecord::Sample(s)) => Some(UsageRecord::Sample(s)),
            _ => None,
        }));
        let deltas: Vec<f64> = recs
            .iter()
            .filter_map(|r| match r {
                UsageRecord::Checkpoint { delta, .. } => Some(*delta),
                _ => None,
            })
            .collect();
        assert_eq!(deltas.len(), 2);
        assert!(deltas.iter().all(|d| (d - 0.33).abs() < 1e-9));
        let samples = recs
            .iter()
            .filter(|r| matches!(r, UsageRecord::Sample(_)))
            .count();
        assert_eq!(samples, 2);
        assert_eq!(
            recs.iter()
                .filter(|r| matches!(r, UsageRecord::Shutdown(_)))
                .count(),
            2
        );
    }

    #[test]
    fn tail_follows_a_growing_file() {
        let tmp = tempfile::tempdir().expect("tmp");
        let path = tmp.path().join("events.jsonl");
        let text = std::fs::read_to_string(fixture("events.jsonl")).expect("read");
        let (first, second) = text.split_at(text.len() / 2);
        let mut tail = TranscriptTail::new(&path);
        assert!(tail.poll().expect("missing").is_empty());
        let mut f = std::fs::File::create(&path).expect("create");
        f.write_all(first.as_bytes()).expect("w");
        let a = tail.poll().expect("a");
        f.write_all(second.as_bytes()).expect("w");
        let b = tail.poll().expect("b");
        let n = |v: &[UsageRecord]| {
            v.iter()
                .filter(|r| matches!(r, UsageRecord::Sample(_)))
                .count()
        };
        assert_eq!(n(&a) + n(&b), 2);
        assert!((tail.total_premium_requests().expect("t") - 0.66).abs() < 1e-9);
        assert_eq!(tail.path(), path);
    }

    #[test]
    fn captured_usage_screen() {
        let text = std::fs::read_to_string(fixture("screens/usage.txt")).expect("read");
        let lines: Vec<&str> = text.lines().collect();
        let q = parse_quota_screen(&lines).expect("quota");
        assert_eq!(q.session_premium_requests, Some(0.0));
        assert_eq!(q.weekly_used_percent, Some(0.0));
        assert_eq!(q.weekly_resets.as_deref(), Some("Oct 26 at 10 AM"));
        assert_eq!(parse_quota_screen(&["nothing here"]), None);
        let q = parse_quota_screen(&[
            "   Requests   12.5 Premium (3m)   ┃",
            "   Weekly     ■■■□□37% used • resets on Nov 2 at 4:30 PM",
        ])
        .expect("quota");
        assert_eq!(q.session_premium_requests, Some(12.5));
        assert_eq!(q.weekly_used_percent, Some(37.0));
        assert_eq!(q.weekly_resets.as_deref(), Some("Nov 2 at 4:30 PM"));
    }

    #[test]
    fn reset_times() {
        let now = NaiveDate::from_ymd_opt(2026, 10, 24)
            .and_then(|d| d.and_hms_opt(17, 0, 0))
            .expect("now");
        let at = |y, mo, d, h, mi| {
            NaiveDate::from_ymd_opt(y, mo, d)
                .and_then(|x| x.and_hms_opt(h, mi, 0))
                .expect("t")
        };
        assert_eq!(
            parse_reset("Oct 26 at 10 AM", now),
            Some(at(2026, 10, 26, 10, 0))
        );
        assert_eq!(
            parse_reset("Nov 2 at 4:30 PM", now),
            Some(at(2026, 11, 2, 16, 30))
        );
        assert_eq!(
            parse_reset("Jan 1 at 12 AM", now),
            Some(at(2027, 1, 1, 0, 0))
        );
        assert_eq!(
            parse_reset("Oct 24 at 12 PM", now),
            Some(at(2027, 10, 24, 12, 0))
        );
        assert_eq!(parse_reset("soon", now), None);
    }
}
