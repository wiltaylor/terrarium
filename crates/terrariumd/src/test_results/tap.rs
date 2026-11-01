//! TAP 13 and 14: the Test Anything Protocol (`node --test
//! --test-reporter=tap`, prove, tap, bats).
//!
//! - A test point is `ok [n] [- description] [# SKIP|TODO reason]` or
//!   `not ok ...`. `ok` is passed, `not ok` is failed. SKIP and TODO are
//!   skipped (a TODO test is not done work, so it does not fail the run).
//! - A YAML block (`---` to `...`, indented under its test point) gives
//!   `message` or `error`, `stack`, `duration_ms`, `severity`, and
//!   `location` (`file:line:col`). The whole block is the trace when it
//!   has no `stack`.
//! - Subtests (TAP 14, Node) are indented before their parent's test
//!   point. A test point with subtests is a group: its subtests are cases
//!   whose suite is the group's path; a test point with no subtest is a
//!   case.
//! - `Bail out!` is a note, and an errored case.

use std::sync::LazyLock;

use regex::Regex;

use super::{Case, Format, ParseError, ParsedRun, Sink, Status, float_ms, invalid};

static POINT: LazyLock<Regex> = LazyLock::new(|| {
    #[allow(clippy::expect_used)]
    Regex::new(r"^(not ok|ok)\b\s*(\d+)?\s*(?:-\s*)?(.*)$").expect("the TAP point regex")
});
static DIRECTIVE: LazyLock<Regex> = LazyLock::new(|| {
    #[allow(clippy::expect_used)]
    Regex::new(r"(?i)(?:^|\s)#\s*(skip|todo)\S*\s*(.*)$").expect("the TAP directive regex")
});

/// `1..N` (and `1..0 # reason`).
pub fn is_plan(line: &str) -> bool {
    let l = line.trim();
    l.strip_prefix("1..")
        .is_some_and(|r| r.chars().next().is_some_and(|c| c.is_ascii_digit()))
}

#[derive(Debug)]
struct Point {
    indent: usize,
    name: String,
    status: Status,
    reason: Option<String>,
    yaml: Vec<String>,
    children: Vec<Point>,
}

fn indent_of(line: &str) -> usize {
    line.chars()
        .take_while(|c| *c == ' ' || *c == '\t')
        .map(|c| if c == '\t' { 4 } else { 1 })
        .sum()
}

/// Remove one level of quotes from a YAML scalar.
fn unquote(s: &str) -> String {
    let s = s.trim();
    if s.len() >= 2
        && ((s.starts_with('\'') && s.ends_with('\'')) || (s.starts_with('"') && s.ends_with('"')))
    {
        let inner = &s[1..s.len() - 1];
        if s.starts_with('\'') {
            return inner.replace("''", "'");
        }
        return inner.replace("\\n", "\n").replace("\\\"", "\"");
    }
    s.to_owned()
}

/// The top-level keys of a YAML block: a scalar, or a block scalar (`|`,
/// `|-`, `>`). A nested map is kept as its text.
fn yaml_keys(lines: &[String]) -> Vec<(String, String)> {
    let base = lines
        .iter()
        .filter(|l| !l.trim().is_empty())
        .map(|l| indent_of(l))
        .min()
        .unwrap_or(0);
    let mut out: Vec<(String, String)> = Vec::new();
    let mut i = 0;
    while i < lines.len() {
        let l = &lines[i];
        i += 1;
        if l.trim().is_empty() || indent_of(l) != base {
            continue;
        }
        let Some((k, v)) = l.trim().split_once(':') else {
            continue;
        };
        let v = v.trim();
        let mut block = Vec::new();
        while i < lines.len() && (lines[i].trim().is_empty() || indent_of(&lines[i]) > base) {
            block.push(lines[i].clone());
            i += 1;
        }
        while block.last().is_some_and(|b| b.trim().is_empty()) {
            block.pop();
        }
        let value = if v.is_empty() || v.starts_with('|') || v.starts_with('>') {
            let inner = block
                .iter()
                .filter(|b| !b.trim().is_empty())
                .map(|b| indent_of(b))
                .min()
                .unwrap_or(0);
            let text: Vec<String> = block
                .iter()
                .map(|b| b.chars().skip(inner).collect::<String>())
                .collect();
            if v.starts_with('>') {
                text.join(" ")
            } else {
                text.join("\n")
            }
        } else {
            unquote(v)
        };
        out.push((k.trim().to_owned(), value));
    }
    out
}

pub fn parse(text: &str, mut sink: Sink) -> Result<ParsedRun, ParseError> {
    let mut points: Vec<Point> = Vec::new();
    let mut yaml: Option<(usize, Vec<String>)> = None;
    let mut bail: Option<String> = None;
    let mut plan: Option<usize> = None;
    let mut version = None;
    for raw in text.lines() {
        let line = raw.trim_end();
        let indent = indent_of(line);
        let body = line.trim_start();
        if let Some((yi, buf)) = &mut yaml {
            if body == "..." && indent <= *yi + 2 {
                if let Some(p) = points.last_mut() {
                    p.yaml = std::mem::take(buf);
                }
                yaml = None;
            } else {
                buf.push(line.to_owned());
            }
            continue;
        }
        if body == "---" && points.last().is_some_and(|p| indent > p.indent) {
            yaml = Some((indent, Vec::new()));
            continue;
        }
        if let Some(v) = body.strip_prefix("TAP version") {
            version.get_or_insert(v.trim().to_owned());
            continue;
        }
        if let Some(r) = body.strip_prefix("Bail out!") {
            bail = Some(r.trim().to_owned());
            continue;
        }
        if indent == 0 && is_plan(body) {
            plan = body[3..]
                .split(|c: char| !c.is_ascii_digit())
                .next()
                .and_then(|n| n.parse().ok());
            continue;
        }
        let Some(c) = POINT.captures(body) else {
            continue;
        };
        let ok = &c[1] == "ok";
        let mut desc = c.get(3).map_or("", |m| m.as_str()).to_owned();
        let mut status = if ok { Status::Passed } else { Status::Failed };
        let mut reason = None;
        if let Some(d) = DIRECTIVE.captures(&desc.clone()) {
            status = Status::Skipped;
            let r = d.get(2).map_or("", |m| m.as_str()).trim();
            let kind = d[1].to_ascii_uppercase();
            reason = Some(if r.is_empty() {
                kind
            } else {
                format!("{kind}: {r}")
            });
            if let Some(m) = d.get(0) {
                desc.truncate(m.start());
            }
        }
        let name = if desc.trim().is_empty() {
            format!("test {}", c.get(2).map_or("?", |m| m.as_str()))
        } else {
            desc.trim().to_owned()
        };
        points.push(Point {
            indent,
            name,
            status,
            reason,
            yaml: Vec::new(),
            children: Vec::new(),
        });
    }
    if points.is_empty() && bail.is_none() {
        return Err(invalid("no TAP test point (`ok` or `not ok`) in the file"));
    }
    // Nest: the deeper points before a point are its subtests.
    let mut stack: Vec<Point> = Vec::new();
    let top_level = points.iter().map(|p| p.indent).min().unwrap_or(0);
    for mut p in points {
        let mut kids = Vec::new();
        while stack.last().is_some_and(|q| q.indent > p.indent) {
            if let Some(q) = stack.pop() {
                kids.push(q);
            }
        }
        kids.reverse();
        p.children = kids;
        stack.push(p);
    }
    let top_count = stack.iter().filter(|p| p.indent == top_level).count();
    for p in stack {
        emit(&mut sink, &[], p)?;
    }
    if let Some(n) = plan
        && n != top_count
    {
        sink.note(format!("the plan says {n} tests; the file has {top_count}"));
    }
    if let Some(r) = bail {
        sink.note(format!("the run bailed out: {r}"));
        let s = sink.suite("tests");
        let mut k = Case::new("Bail out!", Status::Errored);
        k.message = Some(if r.is_empty() {
            "the run bailed out".into()
        } else {
            r
        });
        sink.case(s, k)?;
    }
    Ok(sink.finish(Format::Tap, None, None, None, None))
}

fn emit(sink: &mut Sink, path: &[String], p: Point) -> Result<(), ParseError> {
    if !p.children.is_empty() {
        let mut sub = path.to_vec();
        sub.push(p.name.clone());
        for c in p.children {
            emit(sink, &sub, c)?;
        }
        return Ok(());
    }
    let suite = if path.is_empty() {
        sink.suite("tests")
    } else {
        sink.suite(&path.join(" > "))
    };
    let keys = yaml_keys(&p.yaml);
    let get = |k: &str| keys.iter().find(|(x, _)| x == k).map(|(_, v)| v.clone());
    let mut k = Case::new(p.name, p.status);
    k.classname = (!path.is_empty()).then(|| path.join(" > "));
    k.duration_ms = get("duration_ms")
        .and_then(|d| d.parse::<f64>().ok())
        .and_then(float_ms);
    if let Some(loc) = get("location") {
        let loc = loc.trim_start_matches("file://");
        let mut parts = loc.rsplitn(3, ':');
        let _col = parts.next();
        let line = parts.next().and_then(|l| l.parse().ok());
        if let (Some(f), Some(l)) = (parts.next(), line) {
            k.file = Some(f.to_owned());
            k.line = Some(l);
        }
    }
    if p.status == Status::Skipped {
        k.message = p.reason;
    } else if p.status.failing() {
        k.message = get("message")
            .or_else(|| get("error"))
            .map(|m| m.trim().to_owned());
        k.trace = get("stack").or_else(|| {
            (!p.yaml.is_empty()).then(|| {
                let base = p
                    .yaml
                    .iter()
                    .filter(|l| !l.trim().is_empty())
                    .map(|l| indent_of(l))
                    .min()
                    .unwrap_or(0);
                p.yaml
                    .iter()
                    .map(|l| l.chars().skip(base).collect::<String>())
                    .collect::<Vec<_>>()
                    .join("\n")
            })
        });
    }
    sink.case(suite, k)
}
