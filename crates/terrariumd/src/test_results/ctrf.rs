//! CTRF, the Common Test Report Format (<https://ctrf.io>): JSON with
//! `results.tool`, `results.summary`, and `results.tests[]`.
//!
//! - A test has `name`, `status` (`passed`, `failed`, `skipped`,
//!   `pending`, `other`), `duration` in milliseconds, and optional
//!   `message`, `trace`, `stdout`/`stderr` (a list of lines or a string),
//!   `suite` (a string, or a list of names), `filePath`, and `line`.
//! - `pending` is skipped; `other` (an interrupted or unknown result) is
//!   errored.
//! - The suite is `suite`, else the file, else `tests`.

use serde_json::Value;

use super::{
    Case, Format, ParseError, ParsedRun, Sink, Status, epoch_ms, first_line, float_ms, invalid,
};

fn status_of(s: &str) -> Status {
    match s {
        "passed" => Status::Passed,
        "failed" => Status::Failed,
        "skipped" | "pending" => Status::Skipped,
        _ => Status::Errored,
    }
}

/// A string, or a list of strings joined with `sep`.
fn text_of(v: Option<&Value>, sep: &str) -> Option<String> {
    match v? {
        Value::String(s) => Some(s.clone()),
        Value::Array(a) => {
            let parts: Vec<String> = a
                .iter()
                .map(|x| match x {
                    Value::String(s) => s.clone(),
                    other => other.to_string(),
                })
                .collect();
            (!parts.is_empty()).then(|| parts.join(sep))
        }
        Value::Null => None,
        other => Some(other.to_string()),
    }
}

pub fn parse(text: &str, mut sink: Sink) -> Result<ParsedRun, ParseError> {
    let v: Value = serde_json::from_str(text).map_err(|e| invalid(format!("bad JSON: {e}")))?;
    let results = v
        .get("results")
        .ok_or_else(|| invalid("a CTRF report has a `results` object"))?;
    let tests = results
        .get("tests")
        .and_then(Value::as_array)
        .ok_or_else(|| invalid("a CTRF report has a `results.tests` list"))?;
    let tool = results
        .pointer("/tool/name")
        .and_then(Value::as_str)
        .map(str::to_owned);
    let summary = results.get("summary");
    let start = summary.and_then(|s| s.get("start")).and_then(Value::as_f64);
    let stop = summary.and_then(|s| s.get("stop")).and_then(Value::as_f64);
    let duration = match (start, stop) {
        (Some(a), Some(b)) if b >= a && a > 0.0 => float_ms(b - a),
        _ => None,
    };
    for t in tests {
        let Some(obj) = t.as_object() else { continue };
        let name = obj.get("name").and_then(Value::as_str).unwrap_or_default();
        let status = status_of(obj.get("status").and_then(Value::as_str).unwrap_or("other"));
        let mut k = Case::new(name, status);
        k.duration_ms = obj
            .get("duration")
            .and_then(Value::as_f64)
            .and_then(float_ms);
        k.file = obj
            .get("filePath")
            .and_then(Value::as_str)
            .map(str::to_owned);
        k.line = obj.get("line").and_then(Value::as_i64);
        k.message = text_of(obj.get("message"), "\n");
        k.trace = text_of(obj.get("trace"), "\n");
        if k.message.is_none()
            && let Some(t) = &k.trace
        {
            k.message = first_line(t);
        }
        k.stdout = text_of(obj.get("stdout"), "\n");
        k.stderr = text_of(obj.get("stderr"), "\n");
        let suite_name = text_of(obj.get("suite"), " > ")
            .or_else(|| k.file.clone())
            .unwrap_or_else(|| "tests".into());
        let s = sink.suite(&suite_name);
        sink.case(s, k)?;
    }
    let suggested = results
        .pointer("/environment/appName")
        .and_then(Value::as_str)
        .map(str::to_owned);
    Ok(sink.finish(
        Format::Ctrf,
        tool,
        suggested,
        start.and_then(epoch_ms),
        duration,
    ))
}
