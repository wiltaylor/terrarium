//! The Vitest JSON report (`vitest --reporter=json`) and the Jest JSON
//! report (`jest --json`). They have the same shape:
//!
//! - `testResults[]` is one test file: `name` (the path), `status`,
//!   `message`, `startTime`, `endTime`, and `assertionResults[]`.
//! - An assertion result has `title`, `ancestorTitles` (the `describe`
//!   blocks), `status` (`passed`, `failed`, `pending`, `skipped`, `todo`,
//!   `disabled`), `duration` in milliseconds, `failureMessages[]`, and
//!   `location.line`.
//! - The suite is the file path, relative to the common folder of the
//!   files. The case name is the `describe` titles and the title, joined
//!   with ` > `.
//! - A file that failed with no assertion result (a syntax error, a
//!   failed import) gives one errored case with the file's message.

use serde_json::Value;

use super::{
    Case, Format, ParseError, ParsedRun, Sink, Status, epoch_ms, first_line, float_ms, invalid,
};

fn status_of(s: &str) -> Status {
    match s {
        "passed" | "focused" => Status::Passed,
        "failed" => Status::Failed,
        "pending" | "skipped" | "todo" | "disabled" => Status::Skipped,
        _ => Status::Errored,
    }
}

/// The folder names that stay in a suite name.
const SOURCE_DIRS: &[&str] = &[
    "src",
    "test",
    "tests",
    "spec",
    "__tests__",
    "e2e",
    "lib",
    "app",
];

/// The folder that all the paths share, with its `/`. A source folder
/// ([`SOURCE_DIRS`]) at its end stays in the names.
fn common_dir(paths: &[&str]) -> String {
    let abs: Vec<&str> = paths
        .iter()
        .copied()
        .filter(|p| p.starts_with('/'))
        .collect();
    if abs.len() != paths.len() || abs.is_empty() {
        return String::new();
    }
    let mut prefix: &str = abs[0].rsplit_once('/').map_or("", |(d, _)| d);
    for p in &abs[1..] {
        while !prefix.is_empty() && !p.starts_with(&format!("{prefix}/")) {
            prefix = prefix.rsplit_once('/').map_or("", |(d, _)| d);
        }
    }
    // Keep a source folder name: `src/a.test.ts`, not `a.test.ts`.
    if let Some((parent, last)) = prefix.rsplit_once('/')
        && SOURCE_DIRS.contains(&last)
    {
        prefix = parent;
    }
    if prefix.is_empty() {
        String::new()
    } else {
        format!("{prefix}/")
    }
}

pub fn parse(text: &str, mut sink: Sink, format: Format) -> Result<ParsedRun, ParseError> {
    let v: Value = serde_json::from_str(text).map_err(|e| invalid(format!("bad JSON: {e}")))?;
    let files = v
        .get("testResults")
        .and_then(Value::as_array)
        .ok_or_else(|| invalid("a Vitest or Jest JSON report has a `testResults` list"))?;
    let names: Vec<&str> = files
        .iter()
        .filter_map(|f| f.get("name").and_then(Value::as_str))
        .collect();
    let dir = common_dir(&names);
    let start = v.get("startTime").and_then(Value::as_f64);
    let mut end: Option<f64> = None;
    for f in files {
        let path = f.get("name").and_then(Value::as_str).unwrap_or("tests");
        let rel = path.strip_prefix(dir.as_str()).unwrap_or(path);
        let suite = sink.suite(rel);
        let (fs, fe) = (
            f.get("startTime").and_then(Value::as_f64),
            f.get("endTime").and_then(Value::as_f64),
        );
        if let (Some(a), Some(b)) = (fs, fe)
            && b >= a
        {
            sink.suite_duration(suite, float_ms(b - a));
        }
        if let Some(b) = fe {
            end = Some(end.map_or(b, |e: f64| e.max(b)));
        }
        let asserts = f
            .get("assertionResults")
            .and_then(Value::as_array)
            .cloned()
            .unwrap_or_default();
        if asserts.is_empty() {
            let failed = f.get("status").and_then(Value::as_str) == Some("failed");
            let msg = f
                .get("message")
                .or_else(|| f.get("failureMessage"))
                .and_then(Value::as_str)
                .unwrap_or_default();
            if failed || !msg.trim().is_empty() {
                let mut k = Case::new("(the file did not run)", Status::Errored);
                k.file = Some(rel.to_owned());
                k.message = first_line(msg).or_else(|| Some("the test file failed".into()));
                k.trace = Some(msg.to_owned());
                sink.case(suite, k)?;
            }
            continue;
        }
        for a in &asserts {
            let title = a.get("title").and_then(Value::as_str).unwrap_or_default();
            let ancestors: Vec<&str> = a
                .get("ancestorTitles")
                .and_then(Value::as_array)
                .map(|x| x.iter().filter_map(Value::as_str).collect())
                .unwrap_or_default();
            let mut path = ancestors.clone();
            path.push(title);
            let mut k = Case::new(
                path.join(" > "),
                status_of(a.get("status").and_then(Value::as_str).unwrap_or_default()),
            );
            k.classname = (!ancestors.is_empty()).then(|| ancestors.join(" > "));
            k.file = Some(rel.to_owned());
            k.line = a.pointer("/location/line").and_then(Value::as_i64);
            k.duration_ms = a.get("duration").and_then(Value::as_f64).and_then(float_ms);
            let fails: Vec<&str> = a
                .get("failureMessages")
                .and_then(Value::as_array)
                .map(|x| x.iter().filter_map(Value::as_str).collect())
                .unwrap_or_default();
            if !fails.is_empty() {
                k.message = fails.first().and_then(|m| first_line(m));
                k.trace = Some(fails.join("\n\n"));
            }
            sink.case(suite, k)?;
        }
    }
    let duration = match (start, end) {
        (Some(a), Some(b)) if b >= a => float_ms(b - a),
        _ => None,
    };
    let tool = match format {
        Format::VitestJson => "vitest",
        _ => "jest",
    };
    Ok(sink.finish(
        format,
        Some(tool.into()),
        None,
        start.and_then(epoch_ms),
        duration,
    ))
}
