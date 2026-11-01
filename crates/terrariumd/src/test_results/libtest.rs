//! libtest JSON: `cargo test -- -Z unstable-options --format json` (one
//! JSON object on each line) and `cargo nextest run --message-format
//! libtest-json`.
//!
//! - `{"type":"suite","event":"started"}` starts a test binary: a suite.
//!   With nextest, the object has `nextest.crate`, and a test name is
//!   `<binary>$<test>`: the suite is the binary.
//! - `{"type":"test","event":"ok"|"failed"|"ignored", "name",
//!   "exec_time" (seconds), "stdout", "message"}` ends a test. A test that
//!   starts and has no end (the binary crashed) is errored.
//! - A failed test's message is the panic message from its output, and
//!   the file and line come from `panicked at <file>:<line>:<col>`.
//! - A line that is not JSON is skipped, with a note.

use std::collections::BTreeMap;

use serde_json::Value;

use super::{Case, Format, ParseError, ParsedRun, Sink, Status, invalid, secs_to_ms};

/// The panic message, file, and line of a test output.
pub fn panic_of(out: &str) -> (Option<String>, Option<String>, Option<i64>) {
    let Some(at) = out.find("panicked at ") else {
        return (None, None, None);
    };
    let rest = &out[at + "panicked at ".len()..];
    let (head, body) = rest.split_once('\n').unwrap_or((rest, ""));
    // Old form: panicked at 'message', src/lib.rs:10:5
    if let Some(q) = head.strip_prefix('\'')
        && let Some((msg, loc)) = q.rsplit_once("', ")
    {
        let (file, line) = location(loc);
        return (Some(msg.to_owned()), file, line);
    }
    let (file, line) = location(head.trim_end_matches(':'));
    let msg: Vec<&str> = body
        .lines()
        .take_while(|l| !l.starts_with("note: ") && !l.starts_with("stack backtrace:"))
        .collect();
    let msg = msg.join("\n").trim().to_owned();
    ((!msg.is_empty()).then_some(msg), file, line)
}

/// `src/lib.rs:10:64` -> the file and the line.
fn location(loc: &str) -> (Option<String>, Option<i64>) {
    let mut parts = loc.trim().rsplitn(3, ':');
    let _col = parts.next();
    let line = parts.next().and_then(|l| l.parse().ok());
    let file = parts.next().map(str::to_owned);
    match (file, line) {
        (Some(f), Some(l)) => (Some(f), Some(l)),
        _ => (None, None),
    }
}

struct Pending {
    suite: usize,
}

pub fn parse(text: &str, mut sink: Sink) -> Result<ParsedRun, ParseError> {
    let mut bad_lines = 0usize;
    let mut objects = 0usize;
    let mut binaries = 0usize;
    // The suite of the current binary (plain cargo test).
    let mut current: Option<usize> = None;
    let mut started: BTreeMap<String, Pending> = BTreeMap::new();
    let mut total_ms: Option<i64> = None;
    let mut nextest = false;
    for line in text.lines() {
        let line = line.trim();
        if line.is_empty() {
            continue;
        }
        let Ok(v) = serde_json::from_str::<Value>(line) else {
            bad_lines += 1;
            continue;
        };
        objects += 1;
        let ty = v.get("type").and_then(Value::as_str).unwrap_or_default();
        let event = v.get("event").and_then(Value::as_str).unwrap_or_default();
        match (ty, event) {
            ("suite", "started") => {
                binaries += 1;
                if v.get("nextest").is_some() {
                    nextest = true;
                    current = None;
                } else {
                    current = Some(sink.suite(&format!("test binary {binaries}")));
                }
            }
            ("suite", "ok" | "failed") => {
                let ms = v
                    .get("exec_time")
                    .and_then(Value::as_f64)
                    .and_then(secs_to_ms);
                if let Some(ms) = ms {
                    total_ms = Some(total_ms.unwrap_or(0) + ms);
                    if let Some(s) = current {
                        sink.suite_duration(s, Some(ms));
                    }
                }
            }
            ("test", ev) => {
                let full = v.get("name").and_then(Value::as_str).unwrap_or_default();
                let (suite_name, name) = match full.split_once('$') {
                    Some((b, n)) => (Some(b), n),
                    None => (None, full),
                };
                let suite = match (suite_name, current) {
                    (Some(b), _) => sink.suite(b),
                    (None, Some(s)) => s,
                    (None, None) => {
                        let s = sink.suite("tests");
                        current = Some(s);
                        s
                    }
                };
                let key = format!("{suite}\u{0}{name}");
                let status = match ev {
                    "started" => {
                        started.insert(key, Pending { suite });
                        continue;
                    }
                    "ok" => Status::Passed,
                    "failed" => Status::Failed,
                    "ignored" => Status::Skipped,
                    // `timeout` is a warning while the test runs.
                    _ => continue,
                };
                started.remove(&key);
                let mut k = Case::new(name, status);
                k.classname = name.rsplit_once("::").map(|(m, _)| m.to_owned());
                k.duration_ms = v
                    .get("exec_time")
                    .and_then(Value::as_f64)
                    .and_then(secs_to_ms);
                let out = v.get("stdout").and_then(Value::as_str).map(str::to_owned);
                let msg = v.get("message").and_then(Value::as_str).map(str::to_owned);
                if status == Status::Failed {
                    let (pm, file, line) = out.as_deref().map(panic_of).unwrap_or_default();
                    k.message = msg.or(pm);
                    k.file = file;
                    k.line = line;
                    k.trace = out.clone();
                } else {
                    k.message = msg;
                }
                k.stdout = out;
                sink.case(suite, k)?;
            }
            _ => {}
        }
    }
    if objects == 0 {
        return Err(invalid(
            "no libtest JSON object in the file; run `cargo test -- -Z unstable-options --format json`",
        ));
    }
    for (key, p) in std::mem::take(&mut started) {
        let name = key.split_once('\u{0}').map_or(key.as_str(), |(_, n)| n);
        let mut k = Case::new(name, Status::Errored);
        k.classname = name.rsplit_once("::").map(|(m, _)| m.to_owned());
        k.message = Some("the test started but did not finish (the test binary stopped)".into());
        sink.case(p.suite, k)?;
    }
    if bad_lines > 0 {
        sink.note(format!("{bad_lines} lines are not JSON; they were skipped"));
    }
    let tool = if nextest {
        "cargo-nextest"
    } else {
        "cargo test"
    };
    Ok(sink.finish(Format::LibtestJson, Some(tool.into()), None, None, total_ms))
}
