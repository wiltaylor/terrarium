//! JUnit XML: cargo-nextest (`--profile ci`), Vitest and Jest (jest-junit),
//! pytest (`--junitxml`), go-junit-report, Maven Surefire, and
//! `dotnet test --logger junit`.
//!
//! - The root is `<testsuites>` or one `<testsuite>`. A `testsuite` can
//!   hold other `testsuite` elements; each one is a suite of its own.
//! - A `testcase` has `failure` (failed), `error` (errored), or `skipped`.
//!   `flakyFailure` and `flakyError` (nextest, Surefire) are retries that
//!   passed in the end: the case passed. `rerunFailure` and `rerunError`
//!   go with a `failure` or an `error`.
//! - `time` is in seconds. `system-out` and `system-err` are the output.

use roxmltree::Node;

use super::xml::{self, attr, children, name};
use super::{
    Case, Format, ParseError, ParsedRun, Sink, Status, first_line, invalid, norm_time, secs_to_ms,
};

fn time_ms(n: &Node<'_, '_>) -> Option<i64> {
    attr(n, "time")
        .and_then(|t| t.trim().replace(',', "").parse::<f64>().ok())
        .and_then(secs_to_ms)
}

/// The tool from the names that the writers use.
fn tool_of(root: &Node<'_, '_>) -> Option<String> {
    let n = attr(root, "name").unwrap_or_default().to_ascii_lowercase();
    if n == "nextest-run" {
        return Some("cargo-nextest".into());
    }
    for t in ["vitest", "jest", "pytest", "playwright", "mocha", "cypress"] {
        if n.contains(t) {
            return Some(t.into());
        }
    }
    let has_prop = |key: &str| {
        root.descendants()
            .filter(|d| d.is_element() && name(d) == "property")
            .any(|p| attr(&p, "name").is_some_and(|k| k.starts_with(key)))
    };
    if has_prop("java.") || has_prop("maven.") {
        return Some("junit".into());
    }
    None
}

pub fn parse(text: &str, mut sink: Sink) -> Result<ParsedRun, ParseError> {
    let doc = xml::parse(text)?;
    let root = doc.root_element();
    match name(&root) {
        "testsuites" | "testsuite" => {}
        other => {
            return Err(invalid(format!(
                "a JUnit file has <testsuites> or <testsuite> as its root, not <{other}>"
            )));
        }
    }
    let tool = tool_of(&root);
    let started_at = attr(&root, "timestamp").and_then(norm_time).or_else(|| {
        root.descendants()
            .filter(|d| d.is_element() && name(d) == "testsuite")
            .find_map(|s| attr(&s, "timestamp").and_then(norm_time))
    });
    let duration = time_ms(&root);
    if name(&root) == "testsuite" {
        suite(&mut sink, root)?;
    } else {
        for c in root.children().filter(Node::is_element) {
            match name(&c) {
                "testsuite" => suite(&mut sink, c)?,
                "testcase" => {
                    let s = sink.suite(attr(&root, "name").unwrap_or("tests"));
                    case(&mut sink, s, c)?;
                }
                _ => {}
            }
        }
    }
    let suggested = attr(&root, "name")
        .filter(|n| {
            ![
                "nextest-run",
                "vitest tests",
                "jest tests",
                "pytest tests",
                "pytest",
            ]
            .contains(n)
        })
        .map(str::to_owned);
    Ok(sink.finish(Format::Junit, tool, suggested, started_at, duration))
}

fn suite(sink: &mut Sink, s: Node<'_, '_>) -> Result<(), ParseError> {
    let suite_name = attr(&s, "name")
        .filter(|n| !n.trim().is_empty())
        .or_else(|| attr(&s, "package"))
        .unwrap_or("tests");
    let mut index = None;
    for c in s.children().filter(Node::is_element) {
        match name(&c) {
            "testcase" => {
                let i = *index.get_or_insert_with(|| sink.suite(suite_name));
                case(sink, i, c)?;
            }
            "testsuite" => suite(sink, c)?,
            _ => {}
        }
    }
    if let Some(i) = index {
        sink.suite_duration(i, time_ms(&s));
    }
    Ok(())
}

/// The message of a `failure`, `error`, or `skipped`: its `message`, or the
/// first line of its text.
fn message_of(n: &Node<'_, '_>) -> Option<String> {
    attr(n, "message")
        .filter(|m| !m.trim().is_empty())
        .map(str::to_owned)
        .or_else(|| first_line(&xml::text(*n)))
}

fn case(sink: &mut Sink, suite: usize, c: Node<'_, '_>) -> Result<(), ParseError> {
    let mut k = Case::new(attr(&c, "name").unwrap_or_default(), Status::Passed);
    k.classname = attr(&c, "classname").map(str::to_owned);
    k.file = attr(&c, "file").map(str::to_owned);
    k.line = attr(&c, "line").and_then(|l| l.trim().parse().ok());
    k.duration_ms = time_ms(&c);
    let failures: Vec<Node> = children(c, "failure").collect();
    let errors: Vec<Node> = children(c, "error").collect();
    let main = if !errors.is_empty() {
        k.status = Status::Errored;
        errors
    } else if !failures.is_empty() {
        k.status = Status::Failed;
        failures
    } else {
        Vec::new()
    };
    if let Some(first) = main.first() {
        k.message = message_of(first);
        let traces: Vec<String> = main
            .iter()
            .map(|n| xml::text(*n))
            .filter(|t| !t.trim().is_empty())
            .collect();
        if !traces.is_empty() {
            k.trace = Some(traces.join("\n\n"));
        }
    } else if let Some(s) = xml::child(c, "skipped") {
        k.status = Status::Skipped;
        k.message = message_of(&s);
    }
    // The retries (nextest, Surefire): in the trace, after the result.
    let retries: Vec<String> = c
        .children()
        .filter(|n| {
            n.is_element()
                && matches!(
                    name(n),
                    "rerunFailure" | "rerunError" | "flakyFailure" | "flakyError"
                )
        })
        .map(|n| {
            let what = name(&n);
            let body = xml::text(n);
            let msg = attr(&n, "message").unwrap_or_default();
            format!("[{what}] {msg}\n{body}").trim().to_owned()
        })
        .collect();
    if !retries.is_empty() {
        let mut t = k.trace.take().unwrap_or_default();
        if !t.is_empty() {
            t.push_str("\n\n");
        }
        t.push_str(&retries.join("\n\n"));
        k.trace = Some(t);
    }
    k.stdout = xml::text_opt(xml::child(c, "system-out"));
    k.stderr = xml::text_opt(xml::child(c, "system-err"));
    sink.case(suite, k)
}
