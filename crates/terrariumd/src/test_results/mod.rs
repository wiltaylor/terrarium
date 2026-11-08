//! Test result files (docs/api.md §29, PLAN §15.5): parse the standard formats into
//! one model.
//!
//! - Formats: JUnit XML, TRX (.NET VSTest), CTRF JSON, libtest JSON
//!   (`cargo test` and cargo-nextest), the Vitest and Jest JSON reports,
//!   and TAP 13/14. [`detect`] finds the format from the content.
//! - The parsers are pure and tolerant: an unknown element or key is
//!   skipped, a missing value stays empty. A file that is not of the
//!   format, or that is not text, is an [`ParseError::Invalid`].
//! - XML is safe: no DTD, no entity other than the five predefined ones,
//!   no external reference. A `DOCTYPE` is an error ([`xml`]).
//! - Limits: [`MAX_BYTES`] for a file, [`MAX_CASES`] cases, and
//!   [`MAX_TEXT`] for each message, trace, and output (longer text is cut,
//!   with a note).

mod ctrf;
mod jest;
mod junit;
mod libtest;
mod tap;
mod trx;
mod xml;

#[cfg(test)]
mod tests;

use std::collections::HashMap;
use std::fmt;

use serde::{Deserialize, Serialize};

/// The largest file, in bytes.
pub const MAX_BYTES: usize = 20 * 1024 * 1024;
/// The most test cases in one run.
pub const MAX_CASES: usize = 100_000;
/// The longest message, trace, stdout, or stderr of a case, in bytes.
pub const MAX_TEXT: usize = 64 * 1024;

/// A file format.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum Format {
    Junit,
    Trx,
    Ctrf,
    LibtestJson,
    VitestJson,
    JestJson,
    Tap,
}

impl Format {
    pub const ALL: [Format; 7] = [
        Format::Junit,
        Format::Trx,
        Format::Ctrf,
        Format::LibtestJson,
        Format::VitestJson,
        Format::JestJson,
        Format::Tap,
    ];

    pub fn as_str(self) -> &'static str {
        match self {
            Format::Junit => "junit",
            Format::Trx => "trx",
            Format::Ctrf => "ctrf",
            Format::LibtestJson => "libtest-json",
            Format::VitestJson => "vitest-json",
            Format::JestJson => "jest-json",
            Format::Tap => "tap",
        }
    }

    /// The format of a name, or `None` for `auto` (and for no name).
    pub fn parse_name(name: &str) -> Result<Option<Format>, ParseError> {
        let n = name.trim().to_ascii_lowercase();
        if n.is_empty() || n == "auto" {
            return Ok(None);
        }
        Format::ALL
            .into_iter()
            .find(|f| f.as_str() == n)
            .map(Some)
            .ok_or_else(|| {
                let names: Vec<&str> = Format::ALL.iter().map(|f| f.as_str()).collect();
                ParseError::Invalid(format!(
                    "unknown format `{name}`: use auto, {}",
                    names.join(", ")
                ))
            })
    }

    /// The media type of the original file.
    pub fn content_type(self) -> &'static str {
        match self {
            Format::Junit | Format::Trx => "application/xml",
            Format::Ctrf | Format::VitestJson | Format::JestJson => "application/json",
            Format::LibtestJson => "application/x-ndjson",
            Format::Tap => "text/plain; charset=utf-8",
        }
    }
}

impl fmt::Display for Format {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

/// The result of one test case.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Status {
    Passed,
    Failed,
    Errored,
    Skipped,
}

impl Status {
    pub fn as_str(self) -> &'static str {
        match self {
            Status::Passed => "passed",
            Status::Failed => "failed",
            Status::Errored => "errored",
            Status::Skipped => "skipped",
        }
    }

    pub fn from_name(s: &str) -> Option<Status> {
        match s {
            "passed" => Some(Status::Passed),
            "failed" => Some(Status::Failed),
            "errored" => Some(Status::Errored),
            "skipped" => Some(Status::Skipped),
            _ => None,
        }
    }

    /// Failed or errored.
    pub fn failing(self) -> bool {
        matches!(self, Status::Failed | Status::Errored)
    }
}

/// The number of cases of each status.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Counts {
    pub passed: i64,
    pub failed: i64,
    pub errored: i64,
    pub skipped: i64,
    pub total: i64,
}

impl Counts {
    pub fn add(&mut self, s: Status) {
        match s {
            Status::Passed => self.passed += 1,
            Status::Failed => self.failed += 1,
            Status::Errored => self.errored += 1,
            Status::Skipped => self.skipped += 1,
        }
        self.total += 1;
    }

    /// `failed` when a case failed or errored, `passed` when cases ran and
    /// none failed, `empty` when the run has no case.
    pub fn status(&self) -> &'static str {
        if self.failed + self.errored > 0 {
            "failed"
        } else if self.total == 0 {
            "empty"
        } else {
            "passed"
        }
    }
}

/// One test case.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Case {
    pub name: String,
    pub classname: Option<String>,
    pub file: Option<String>,
    pub line: Option<i64>,
    pub status: Status,
    pub duration_ms: Option<i64>,
    pub message: Option<String>,
    pub trace: Option<String>,
    pub stdout: Option<String>,
    pub stderr: Option<String>,
}

impl Case {
    pub fn new(name: impl Into<String>, status: Status) -> Self {
        Self {
            name: name.into(),
            classname: None,
            file: None,
            line: None,
            status,
            duration_ms: None,
            message: None,
            trace: None,
            stdout: None,
            stderr: None,
        }
    }
}

/// A group of cases: a JUnit `testsuite`, a test class, a test file.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Suite {
    pub name: String,
    pub duration_ms: Option<i64>,
    pub cases: Vec<Case>,
}

impl Suite {
    pub fn counts(&self) -> Counts {
        let mut c = Counts::default();
        for k in &self.cases {
            c.add(k.status);
        }
        c
    }
}

/// A parsed file.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ParsedRun {
    pub format: Format,
    /// The tool that wrote the file, when the file says it.
    pub tool: Option<String>,
    /// A name for the run from the file (a project, an assembly), if any.
    pub suggested_name: Option<String>,
    /// RFC 3339, UTC.
    pub started_at: Option<String>,
    pub duration_ms: Option<i64>,
    pub suites: Vec<Suite>,
    /// What the parser changed or skipped: truncated text, skipped lines.
    pub notes: Vec<String>,
}

impl ParsedRun {
    pub fn counts(&self) -> Counts {
        let mut c = Counts::default();
        for s in &self.suites {
            for k in &s.cases {
                c.add(k.status);
            }
        }
        c
    }

    pub fn case_count(&self) -> usize {
        self.suites.iter().map(|s| s.cases.len()).sum()
    }
}

/// Why a file cannot be read.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ParseError {
    /// Over [`MAX_BYTES`].
    TooLarge { size: usize, max: usize },
    /// Not of the format, not text, a DOCTYPE, too many cases.
    Invalid(String),
}

impl fmt::Display for ParseError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            ParseError::TooLarge { size, max } => {
                write!(f, "the file is {size} bytes; the limit is {max} bytes")
            }
            ParseError::Invalid(m) => f.write_str(m),
        }
    }
}

impl std::error::Error for ParseError {}

pub(crate) fn invalid(m: impl Into<String>) -> ParseError {
    ParseError::Invalid(m.into())
}

// ---- the builder ----

/// Collects the suites and cases of a parse, and keeps the limits.
pub(crate) struct Sink {
    suites: Vec<Suite>,
    index: HashMap<String, usize>,
    cases: usize,
    truncated: usize,
    max_cases: usize,
    max_text: usize,
    pub notes: Vec<String>,
}

impl Sink {
    pub fn new() -> Self {
        Self::with_limits(MAX_CASES, MAX_TEXT)
    }

    pub fn with_limits(max_cases: usize, max_text: usize) -> Self {
        Self {
            suites: Vec::new(),
            index: HashMap::new(),
            cases: 0,
            truncated: 0,
            max_cases,
            max_text,
            notes: Vec::new(),
        }
    }

    /// The suite of this name: the one made before, or a new one.
    pub fn suite(&mut self, name: &str) -> usize {
        let name = clean_line(name);
        let name = if name.is_empty() {
            "(no suite)".to_owned()
        } else {
            name
        };
        if let Some(&i) = self.index.get(&name) {
            return i;
        }
        self.suites.push(Suite {
            name: name.clone(),
            duration_ms: None,
            cases: Vec::new(),
        });
        let i = self.suites.len() - 1;
        self.index.insert(name, i);
        i
    }

    /// Add the duration of a suite (a suite can come twice).
    pub fn suite_duration(&mut self, suite: usize, ms: Option<i64>) {
        if let (Some(s), Some(ms)) = (self.suites.get_mut(suite), ms) {
            s.duration_ms = Some(s.duration_ms.unwrap_or(0) + ms);
        }
    }

    /// Add a case. Its texts go through [`Sink::text`] first.
    pub fn case(&mut self, suite: usize, mut c: Case) -> Result<(), ParseError> {
        if self.cases >= self.max_cases {
            return Err(invalid(format!(
                "the file has more than {} test cases; split the run",
                self.max_cases
            )));
        }
        c.name = clean_line(&c.name);
        if c.name.is_empty() {
            c.name = "(no name)".into();
        }
        c.classname = c
            .classname
            .map(|s| clean_line(&s))
            .filter(|s| !s.is_empty());
        c.file = c.file.map(|s| clean_line(&s)).filter(|s| !s.is_empty());
        c.message = self.text(c.message);
        c.trace = self.text(c.trace);
        c.stdout = self.text(c.stdout);
        c.stderr = self.text(c.stderr);
        self.cases += 1;
        if let Some(s) = self.suites.get_mut(suite) {
            s.cases.push(c);
        }
        Ok(())
    }

    /// A text of a case: `None` when empty; cut to the limit, with a mark.
    fn text(&mut self, s: Option<String>) -> Option<String> {
        let s = s?;
        let s = s.trim_end();
        let s = s.trim_start_matches(['\n', '\r']);
        if s.trim().is_empty() {
            return None;
        }
        if s.len() <= self.max_text {
            return Some(s.to_owned());
        }
        let mut end = self.max_text;
        while !s.is_char_boundary(end) {
            end -= 1;
        }
        self.truncated += 1;
        Some(format!(
            "{}\n[truncated: {} bytes more]",
            &s[..end],
            s.len() - end
        ))
    }

    pub fn note(&mut self, n: impl Into<String>) {
        self.notes.push(n.into());
    }

    pub fn finish(
        mut self,
        format: Format,
        tool: Option<String>,
        suggested_name: Option<String>,
        started_at: Option<String>,
        duration_ms: Option<i64>,
    ) -> ParsedRun {
        if self.truncated > 0 {
            self.notes.push(format!(
                "{} texts were cut to {} KiB",
                self.truncated,
                self.max_text / 1024
            ));
        }
        // An empty suite (a `testsuite` with no case) stays out.
        self.suites.retain(|s| !s.cases.is_empty());
        let duration_ms = duration_ms.or_else(|| {
            let sum: Option<i64> = self.suites.iter().map(|s| s.duration_ms).sum();
            sum.or_else(|| {
                let cases: Vec<i64> = self
                    .suites
                    .iter()
                    .flat_map(|s| s.cases.iter().filter_map(|c| c.duration_ms))
                    .collect();
                (!cases.is_empty()).then(|| cases.iter().sum())
            })
        });
        ParsedRun {
            format,
            tool: tool.map(|t| clean_line(&t)).filter(|t| !t.is_empty()),
            suggested_name: suggested_name
                .map(|t| clean_line(&t))
                .filter(|t| !t.is_empty()),
            started_at,
            duration_ms,
            suites: self.suites,
            notes: self.notes,
        }
    }
}

/// One line of at most 1000 characters, with no control characters.
pub(crate) fn clean_line(s: &str) -> String {
    let one: String = s
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ")
        .chars()
        .filter(|c| !c.is_control())
        .collect();
    if one.chars().count() > 1000 {
        one.chars().take(1000).collect()
    } else {
        one
    }
}

// ---- values ----

/// Seconds (a float) as milliseconds.
pub(crate) fn secs_to_ms(secs: f64) -> Option<i64> {
    (secs.is_finite() && secs >= 0.0).then(|| (secs * 1000.0).round() as i64)
}

/// Milliseconds (a float) as whole milliseconds.
pub(crate) fn float_ms(ms: f64) -> Option<i64> {
    (ms.is_finite() && ms >= 0.0).then(|| ms.round() as i64)
}

/// A time as RFC 3339 UTC with milliseconds. A time without a zone is UTC.
pub(crate) fn norm_time(s: &str) -> Option<String> {
    use chrono::{DateTime, NaiveDateTime, SecondsFormat, Utc};
    let s = s.trim();
    if let Ok(t) = DateTime::parse_from_rfc3339(s) {
        return Some(
            t.with_timezone(&Utc)
                .to_rfc3339_opts(SecondsFormat::Millis, true),
        );
    }
    for f in ["%Y-%m-%dT%H:%M:%S%.f", "%Y-%m-%d %H:%M:%S%.f"] {
        if let Ok(t) = NaiveDateTime::parse_from_str(s, f) {
            return Some(t.and_utc().to_rfc3339_opts(SecondsFormat::Millis, true));
        }
    }
    None
}

/// Unix milliseconds as RFC 3339 UTC.
pub(crate) fn epoch_ms(ms: f64) -> Option<String> {
    use chrono::{DateTime, SecondsFormat};
    if !ms.is_finite() || ms <= 0.0 {
        return None;
    }
    DateTime::from_timestamp_millis(ms as i64)
        .map(|t| t.to_rfc3339_opts(SecondsFormat::Millis, true))
}

/// The first line of a text that is not empty.
pub(crate) fn first_line(s: &str) -> Option<String> {
    s.lines()
        .map(str::trim)
        .find(|l| !l.is_empty())
        .map(str::to_owned)
}

// ---- entry points ----

/// The text of a file: UTF-8 (with or without a BOM), or UTF-16 with a BOM.
fn decode(bytes: &[u8]) -> Result<String, ParseError> {
    if let Some(rest) = bytes.strip_prefix(&[0xEF, 0xBB, 0xBF]) {
        return String::from_utf8(rest.to_vec()).map_err(|_| invalid("the file is not UTF-8 text"));
    }
    let utf16 = |rest: &[u8], le: bool| -> Result<String, ParseError> {
        let units: Vec<u16> = rest
            .chunks_exact(2)
            .map(|c| {
                if le {
                    u16::from_le_bytes([c[0], c[1]])
                } else {
                    u16::from_be_bytes([c[0], c[1]])
                }
            })
            .collect();
        String::from_utf16(&units).map_err(|_| invalid("the file is not UTF-16 text"))
    };
    if let Some(rest) = bytes.strip_prefix(&[0xFF, 0xFE]) {
        return utf16(rest, true);
    }
    if let Some(rest) = bytes.strip_prefix(&[0xFE, 0xFF]) {
        return utf16(rest, false);
    }
    String::from_utf8(bytes.to_vec()).map_err(|_| {
        invalid("the file is not UTF-8 text; send a test result file (JUnit, TRX, CTRF, libtest JSON, Vitest, Jest, or TAP)")
    })
}

/// Find the format of a text from its content.
///
/// - XML: the root `<testsuites>` or `<testsuite>` is JUnit; `<TestRun>`
///   is TRX.
/// - JSON: `results.tests` is CTRF; `testResults[]` is the Vitest or Jest
///   report (Vitest when an assertion result has `meta`); lines of objects
///   with `type` are libtest JSON.
/// - TAP: a `TAP version` line, a plan (`1..N`), or a test point (`ok`,
///   `not ok`) first.
pub fn detect(text: &str) -> Result<Format, ParseError> {
    let t = text.trim_start_matches('\u{feff}').trim_start();
    if t.starts_with('<') {
        let root = xml::root_name(t)?;
        return match root.as_str() {
            "testsuites" | "testsuite" => Ok(Format::Junit),
            "TestRun" => Ok(Format::Trx),
            other => Err(invalid(format!(
                "unknown XML root element <{other}>: JUnit has <testsuites> or <testsuite>, TRX has <TestRun>"
            ))),
        };
    }
    if t.starts_with('{') || t.starts_with('[') {
        match serde_json::from_str::<serde_json::Value>(t) {
            Ok(v) => return detect_json(&v),
            Err(e) => {
                if libtest_lines(t) {
                    return Ok(Format::LibtestJson);
                }
                return Err(invalid(format!("bad JSON: {e}")));
            }
        }
    }
    // Lines of JSON after other lines (cargo writes `Compiling` first).
    if libtest_lines(t) {
        return Ok(Format::LibtestJson);
    }
    let first = t
        .lines()
        .map(str::trim)
        .find(|l| !l.is_empty() && !l.starts_with('#'));
    if let Some(l) = first
        && (l.starts_with("TAP version")
            || tap::is_plan(l)
            || l.starts_with("ok")
            || l.starts_with("not ok"))
    {
        return Ok(Format::Tap);
    }
    Err(invalid(
        "cannot tell the format of the file; give the format (junit, trx, ctrf, libtest-json, vitest-json, jest-json, tap)",
    ))
}

/// One of the first 20 lines contains a libtest JSON event.
fn libtest_lines(t: &str) -> bool {
    t.lines()
        .map(str::trim)
        .filter(|l| !l.is_empty())
        .take(20)
        .any(|l| libtest::event_on_line(l).is_some())
}

fn detect_json(v: &serde_json::Value) -> Result<Format, ParseError> {
    if v.pointer("/results/tests").is_some_and(|t| t.is_array()) {
        return Ok(Format::Ctrf);
    }
    if let Some(results) = v.get("testResults").and_then(|r| r.as_array()) {
        let vitest = results.iter().any(|r| {
            r.get("assertionResults")
                .and_then(|a| a.as_array())
                .is_some_and(|a| a.iter().any(|x| x.get("meta").is_some()))
        });
        return Ok(if vitest {
            Format::VitestJson
        } else {
            Format::JestJson
        });
    }
    if v.get("type").and_then(|x| x.as_str()).is_some() {
        return Ok(Format::LibtestJson);
    }
    Err(invalid(
        "unknown JSON report: CTRF has `results.tests`, Vitest and Jest have `testResults`, libtest has lines with `type`",
    ))
}

/// Parse a file. `format`: `None` finds it from the content.
pub fn parse(bytes: &[u8], format: Option<Format>) -> Result<ParsedRun, ParseError> {
    parse_with(bytes, format, Sink::new())
}

pub(crate) fn parse_with(
    bytes: &[u8],
    format: Option<Format>,
    sink: Sink,
) -> Result<ParsedRun, ParseError> {
    if bytes.len() > MAX_BYTES {
        return Err(ParseError::TooLarge {
            size: bytes.len(),
            max: MAX_BYTES,
        });
    }
    let text = decode(bytes)?;
    let text = text.trim_start_matches('\u{feff}');
    if text.trim().is_empty() {
        return Err(invalid("the file is empty"));
    }
    let format = match format {
        Some(f) => f,
        None => detect(text)?,
    };
    match format {
        Format::Junit => junit::parse(text, sink),
        Format::Trx => trx::parse(text, sink),
        Format::Ctrf => ctrf::parse(text, sink),
        Format::LibtestJson => libtest::parse(text, sink),
        Format::VitestJson => jest::parse(text, sink, Format::VitestJson),
        Format::JestJson => jest::parse(text, sink, Format::JestJson),
        Format::Tap => tap::parse(text, sink),
    }
}
