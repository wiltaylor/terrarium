//! TRX: the VSTest result file of `dotnet test --logger trx` (xUnit,
//! NUnit, MSTest).
//!
//! - `Results/UnitTestResult` holds each result: `testName`, `outcome`,
//!   `duration` (`hh:mm:ss.fffffff`), and `Output` (`StdOut`, `StdErr`,
//!   `ErrorInfo/Message`, `ErrorInfo/StackTrace`). A data-driven MSTest
//!   result has `InnerResults`: each inner result is a case.
//! - `TestDefinitions/UnitTest/TestMethod` gives the class of a test
//!   (`testId` -> `className`). The class is the suite.
//! - Outcomes: Passed, PassedButRunAborted, Warning, Completed: passed.
//!   Failed: failed. Error, Timeout, Aborted, Disconnected: errored.
//!   NotExecuted, NotRunnable, Inconclusive, Pending, InProgress: skipped.

use std::collections::HashMap;

use roxmltree::Node;

use super::xml::{self, attr, child, name};
use super::{Case, Format, ParseError, ParsedRun, Sink, Status, invalid, norm_time};

/// `hh:mm:ss.fffffff` (and `d.hh:mm:ss`) as milliseconds.
pub fn duration_ms(s: &str) -> Option<i64> {
    let s = s.trim();
    let (days, rest) = match s.split_once('.') {
        Some((d, r)) if !d.contains(':') => (d.parse::<i64>().ok()?, r),
        _ => (0, s),
    };
    let parts: Vec<&str> = rest.split(':').collect();
    let [h, m, sec] = parts.as_slice() else {
        return None;
    };
    let h: i64 = h.parse().ok()?;
    let m: i64 = m.parse().ok()?;
    let sec: f64 = sec.parse().ok()?;
    if !sec.is_finite() || sec < 0.0 || h < 0 || m < 0 || days < 0 {
        return None;
    }
    Some(((days * 24 + h) * 60 + m) * 60_000 + (sec * 1000.0).round() as i64)
}

fn status_of(outcome: &str) -> Status {
    match outcome {
        "Passed" | "PassedButRunAborted" | "Warning" | "Completed" => Status::Passed,
        "Failed" => Status::Failed,
        "NotExecuted" | "NotRunnable" | "Inconclusive" | "Pending" | "InProgress" => {
            Status::Skipped
        }
        _ => Status::Errored,
    }
}

struct Def {
    class: String,
}

fn tool_of(adapter: &str) -> &'static str {
    let a = adapter.to_ascii_lowercase();
    if a.contains("xunit") {
        "xunit"
    } else if a.contains("nunit") {
        "nunit"
    } else if a.contains("mstest") {
        "mstest"
    } else {
        "vstest"
    }
}

pub fn parse(text: &str, mut sink: Sink) -> Result<ParsedRun, ParseError> {
    let doc = xml::parse(text)?;
    let root = doc.root_element();
    if name(&root) != "TestRun" {
        return Err(invalid(format!(
            "a TRX file has <TestRun> as its root, not <{}>",
            name(&root)
        )));
    }
    let mut defs: HashMap<String, Def> = HashMap::new();
    let mut tool = None;
    let mut assembly = None;
    if let Some(d) = child(root, "TestDefinitions") {
        for t in d.children().filter(Node::is_element) {
            let Some(id) = attr(&t, "id") else { continue };
            let Some(m) = child(t, "TestMethod") else {
                continue;
            };
            if tool.is_none() {
                tool = attr(&m, "adapterTypeName").map(|a| tool_of(a).to_owned());
            }
            if assembly.is_none() {
                assembly = attr(&m, "codeBase").and_then(|c| {
                    let file = c.rsplit(['/', '\\']).next()?;
                    let stem = file.strip_suffix(".dll").unwrap_or(file);
                    Some(stem.to_owned())
                });
            }
            defs.insert(
                id.to_owned(),
                Def {
                    class: attr(&m, "className").unwrap_or_default().to_owned(),
                },
            );
        }
    }
    let times = child(root, "Times");
    let start = times.and_then(|t| attr(&t, "start"));
    let finish = times.and_then(|t| attr(&t, "finish"));
    let duration = match (start, finish) {
        (Some(s), Some(f)) => {
            let s = chrono::DateTime::parse_from_rfc3339(s.trim()).ok();
            let f = chrono::DateTime::parse_from_rfc3339(f.trim()).ok();
            match (s, f) {
                (Some(s), Some(f)) if f >= s => Some((f - s).num_milliseconds()),
                _ => None,
            }
        }
        _ => None,
    };
    if let Some(results) = child(root, "Results") {
        for r in results.children().filter(Node::is_element) {
            result(&mut sink, &defs, r, None)?;
        }
    }
    Ok(sink.finish(
        Format::Trx,
        tool,
        assembly,
        start.and_then(norm_time),
        duration,
    ))
}

fn result(
    sink: &mut Sink,
    defs: &HashMap<String, Def>,
    r: Node<'_, '_>,
    parent_class: Option<&str>,
) -> Result<(), ParseError> {
    if !name(&r).ends_with("TestResult") {
        return Ok(());
    }
    let class = attr(&r, "testId")
        .and_then(|id| defs.get(id))
        .map(|d| d.class.as_str())
        .filter(|c| !c.is_empty())
        .or(parent_class);
    if let Some(inner) = child(r, "InnerResults") {
        let kids: Vec<Node> = inner.children().filter(Node::is_element).collect();
        if !kids.is_empty() {
            for k in kids {
                result(sink, defs, k, class)?;
            }
            return Ok(());
        }
    }
    let test_name = attr(&r, "testName").unwrap_or_default();
    let short = class
        .and_then(|c| test_name.strip_prefix(c))
        .and_then(|n| n.strip_prefix('.'))
        .unwrap_or(test_name);
    let mut k = Case::new(short, status_of(attr(&r, "outcome").unwrap_or_default()));
    k.classname = class.map(str::to_owned);
    k.duration_ms = attr(&r, "duration").and_then(duration_ms);
    if let Some(out) = child(r, "Output") {
        k.stdout = xml::text_opt(child(out, "StdOut"));
        k.stderr = xml::text_opt(child(out, "StdErr"));
        if let Some(e) = child(out, "ErrorInfo") {
            k.message = xml::text_opt(child(e, "Message"));
            k.trace = xml::text_opt(child(e, "StackTrace"));
        }
    }
    let suite = sink.suite(class.unwrap_or("tests"));
    sink.case(suite, k)
}
