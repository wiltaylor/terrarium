//! The output of a monitor script, read by the output kind of the monitor
//! (docs/api.md §30.2). Pure: the engine gives the stdout and the exit
//! code, and gets the status, the value, the text, and the JSON to keep.
//!
//! - `number`: the first number on stdout (`1,234.5`, `-3`, `42%`). The
//!   thresholds give the status.
//! - `status`: the exit code, as Nagios does: 0 ok, 1 warn, 2 crit, any
//!   other code unknown. The first line of stdout is the message.
//! - `text`: stdout, cut to [`TEXT_MAX`] bytes. The exit code gives the
//!   status, as for `status`.
//! - `json`: one JSON object, `{value?, unit?, status?, message?,
//!   rows?}`. `rows` keeps at most [`MAX_ROWS`] objects of at most
//!   [`MAX_COLS`] keys each.

use serde::{Deserialize, Serialize};
use serde_json::{Map, Value, json};

/// The most bytes of text that a `text` result keeps.
pub const TEXT_MAX: usize = 8 * 1024;
/// The most characters of a message (`status`, `json`).
pub const MESSAGE_MAX: usize = 500;
/// The most rows of a `json` result.
pub const MAX_ROWS: usize = 200;
/// The most keys of one row of a `json` result.
pub const MAX_COLS: usize = 20;
/// The most characters of one cell of a row, and of a `json` unit.
pub const CELL_MAX: usize = 500;

/// The output kind of a monitor.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Kind {
    Number,
    Status,
    Text,
    Json,
}

impl Kind {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Number => "number",
            Self::Status => "status",
            Self::Text => "text",
            Self::Json => "json",
        }
    }

    pub fn parse(s: &str) -> Option<Self> {
        match s {
            "number" => Some(Self::Number),
            "status" => Some(Self::Status),
            "text" => Some(Self::Text),
            "json" => Some(Self::Json),
            _ => None,
        }
    }

    /// The kind gives numbers: a stat and a chart block can show it.
    pub fn has_value(self) -> bool {
        matches!(self, Self::Number | Self::Json)
    }
}

/// The status of one result.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Status {
    Ok,
    Warn,
    Crit,
    Unknown,
}

impl Status {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Ok => "ok",
            Self::Warn => "warn",
            Self::Crit => "crit",
            Self::Unknown => "unknown",
        }
    }

    pub fn parse(s: &str) -> Option<Self> {
        match s {
            "ok" => Some(Self::Ok),
            "warn" => Some(Self::Warn),
            "crit" => Some(Self::Crit),
            "unknown" => Some(Self::Unknown),
            _ => None,
        }
    }

    /// The Nagios meaning of an exit code: 0 ok, 1 warn, 2 crit, any
    /// other code (or none) unknown.
    pub fn of_exit(code: Option<i32>) -> Self {
        match code {
            Some(0) => Self::Ok,
            Some(1) => Self::Warn,
            Some(2) => Self::Crit,
            _ => Self::Unknown,
        }
    }

    /// The worse of two: crit, then warn, then unknown, then ok.
    pub fn worse(self, other: Self) -> Self {
        if self.rank() >= other.rank() {
            self
        } else {
            other
        }
    }

    fn rank(self) -> u8 {
        match self {
            Self::Ok => 0,
            Self::Unknown => 1,
            Self::Warn => 2,
            Self::Crit => 3,
        }
    }
}

/// Which side of a threshold is bad.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Direction {
    /// A value at or above the threshold is bad (disk used, load).
    #[default]
    Above,
    /// A value at or below the threshold is bad (free space, uptime).
    Below,
}

impl Direction {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Above => "above",
            Self::Below => "below",
        }
    }
}

/// The thresholds of a monitor.
#[derive(Debug, Clone, Copy, Default, PartialEq)]
pub struct Thresholds {
    pub warn: Option<f64>,
    pub crit: Option<f64>,
    pub direction: Direction,
}

impl Thresholds {
    /// The status of a value: crit past `crit`, warn past `warn`, else ok.
    /// A value on a threshold is past it.
    pub fn status(&self, v: f64) -> Status {
        let past = |t: f64| match self.direction {
            Direction::Above => v >= t,
            Direction::Below => v <= t,
        };
        if self.crit.is_some_and(past) {
            Status::Crit
        } else if self.warn.is_some_and(past) {
            Status::Warn
        } else {
            Status::Ok
        }
    }

    /// `warn` before `crit` in the direction: for `above`, warn <= crit.
    pub fn check(&self) -> Result<(), String> {
        for (name, v) in [("warn", self.warn), ("crit", self.crit)] {
            if v.is_some_and(|v| !v.is_finite()) {
                return Err(format!("{name} is not a finite number"));
            }
        }
        if let (Some(w), Some(c)) = (self.warn, self.crit) {
            let ok = match self.direction {
                Direction::Above => w <= c,
                Direction::Below => w >= c,
            };
            if !ok {
                return Err(format!(
                    "with direction {}, warn ({w}) comes before crit ({c}): use warn {} crit",
                    self.direction.as_str(),
                    if self.direction == Direction::Above {
                        "<="
                    } else {
                        ">="
                    }
                ));
            }
        }
        Ok(())
    }
}

/// What one run gave.
#[derive(Debug, Clone, PartialEq)]
pub struct Parsed {
    pub status: Status,
    pub value: Option<f64>,
    /// The message (`status`, `json`) or the text (`text`).
    pub text: Option<String>,
    /// The checked object of a `json` result.
    pub json: Option<Value>,
    pub error: Option<String>,
}

impl Parsed {
    /// An unknown result with an error and nothing else: a run that timed
    /// out, a target that is down, a refused right.
    pub fn unknown(error: impl Into<String>) -> Self {
        Self {
            status: Status::Unknown,
            value: None,
            text: None,
            json: None,
            error: Some(error.into()),
        }
    }
}

/// Cut `s` to at most `max` bytes, on a character boundary.
pub fn cut_bytes(s: &str, max: usize) -> (String, bool) {
    if s.len() <= max {
        return (s.to_owned(), false);
    }
    let mut end = max;
    while !s.is_char_boundary(end) {
        end -= 1;
    }
    (s[..end].to_owned(), true)
}

/// Cut `s` to at most `max` characters, with `…` when cut.
pub fn cut_chars(s: &str, max: usize) -> String {
    if s.chars().count() <= max {
        return s.to_owned();
    }
    let mut t: String = s.chars().take(max.saturating_sub(1)).collect();
    t.push('…');
    t
}

/// The first number in `s`: an optional `-` or `+`, digits with optional
/// `,` thousands separators, an optional fraction, and an optional
/// exponent. `42%`, `3.5GB`, `load: 0.42`, `1,234,567` and `-12` all
/// give their number. `None` when there is no digit.
pub fn first_number(s: &str) -> Option<f64> {
    let b = s.as_bytes();
    let mut i = 0;
    while i < b.len() {
        let c = b[i];
        let starts = c.is_ascii_digit()
            || (c == b'.' && b.get(i + 1).is_some_and(u8::is_ascii_digit))
            || ((c == b'-' || c == b'+')
                && (b.get(i + 1).is_some_and(u8::is_ascii_digit)
                    || (b.get(i + 1) == Some(&b'.')
                        && b.get(i + 2).is_some_and(u8::is_ascii_digit))));
        // A sign counts only at the start of a word: `a-5` is not -5.
        let sign_ok = !(c == b'-' || c == b'+')
            || i == 0
            || !(b[i - 1].is_ascii_alphanumeric() || b[i - 1] == b'_');
        if starts && sign_ok {
            return read_number(&s[i..]);
        }
        i += 1;
    }
    None
}

fn read_number(s: &str) -> Option<f64> {
    let b = s.as_bytes();
    let mut out = String::new();
    let mut i = 0;
    if matches!(b.first(), Some(b'-' | b'+')) {
        if b[0] == b'-' {
            out.push('-');
        }
        i = 1;
    }
    // The integer part: digits, and a `,` with three digits after it.
    while i < b.len() {
        let c = b[i];
        if c.is_ascii_digit() {
            out.push(c as char);
            i += 1;
        } else if c == b','
            && !out.trim_start_matches('-').is_empty()
            && b.len() >= i + 4
            && b[i + 1..i + 4].iter().all(u8::is_ascii_digit)
            && !b.get(i + 4).is_some_and(u8::is_ascii_digit)
        {
            i += 1;
        } else {
            break;
        }
    }
    if b.get(i) == Some(&b'.') && b.get(i + 1).is_some_and(u8::is_ascii_digit) {
        out.push('.');
        i += 1;
        while i < b.len() && b[i].is_ascii_digit() {
            out.push(b[i] as char);
            i += 1;
        }
    }
    if matches!(b.get(i), Some(b'e' | b'E')) {
        let mut j = i + 1;
        let mut exp = String::from("e");
        if matches!(b.get(j), Some(b'-' | b'+')) {
            exp.push(b[j] as char);
            j += 1;
        }
        let digits_at = j;
        while j < b.len() && b[j].is_ascii_digit() {
            exp.push(b[j] as char);
            j += 1;
        }
        if j > digits_at {
            out.push_str(&exp);
        }
    }
    if out.starts_with('.') || out.starts_with("-.") {
        out = out.replacen('.', "0.", 1);
    }
    out.parse::<f64>().ok().filter(|v| v.is_finite())
}

/// The first line of `s` that is not blank, trimmed and cut.
fn first_line(s: &str) -> Option<String> {
    s.lines()
        .map(str::trim)
        .find(|l| !l.is_empty())
        .map(|l| cut_chars(l, MESSAGE_MAX))
}

fn exit_note(code: Option<i32>) -> String {
    match code {
        Some(c) => format!("exit code {c}"),
        None => "no exit code".into(),
    }
}

/// Read the output of one run.
pub fn parse(kind: Kind, th: &Thresholds, stdout: &str, exit_code: Option<i32>) -> Parsed {
    match kind {
        Kind::Number => match first_number(stdout) {
            Some(v) => Parsed {
                status: th.status(v),
                value: Some(v),
                text: None,
                json: None,
                error: None,
            },
            None => {
                let mut p = Parsed::unknown(format!(
                    "no number in the output ({})",
                    exit_note(exit_code)
                ));
                p.text = first_line(stdout);
                p
            }
        },
        Kind::Status => {
            let status = Status::of_exit(exit_code);
            Parsed {
                status,
                value: None,
                text: first_line(stdout),
                json: None,
                error: (status == Status::Unknown)
                    .then(|| format!("{}: use 0 ok, 1 warn, 2 crit", exit_note(exit_code))),
            }
        }
        Kind::Text => {
            let status = Status::of_exit(exit_code);
            let (text, cut) = cut_bytes(stdout.trim_end(), TEXT_MAX);
            Parsed {
                status,
                value: None,
                text: Some(text),
                json: cut.then(|| json!({"truncated": true})),
                error: (status == Status::Unknown).then(|| exit_note(exit_code)),
            }
        }
        Kind::Json => parse_json(th, stdout, exit_code),
    }
}

fn cell(v: Value) -> Value {
    match v {
        Value::String(s) => Value::String(cut_chars(&s, CELL_MAX)),
        Value::Array(_) | Value::Object(_) => Value::String(cut_chars(&v.to_string(), CELL_MAX)),
        other => other,
    }
}

fn parse_json(th: &Thresholds, stdout: &str, exit_code: Option<i32>) -> Parsed {
    let obj: Map<String, Value> = match serde_json::from_str::<Value>(stdout.trim()) {
        Ok(Value::Object(m)) => m,
        Ok(other) => {
            return Parsed::unknown(format!(
                "the output is JSON but not an object (it is {})",
                match other {
                    Value::Array(_) => "an array",
                    Value::String(_) => "a string",
                    Value::Number(_) => "a number",
                    Value::Bool(_) => "a boolean",
                    _ => "null",
                }
            ));
        }
        Err(e) => {
            let mut p = Parsed::unknown(format!("the output is not JSON: {e}"));
            p.text = first_line(stdout);
            return p;
        }
    };
    let mut out = Map::new();
    let mut errors: Vec<String> = Vec::new();
    let value = match obj.get("value") {
        None | Some(Value::Null) => None,
        Some(Value::Number(n)) => n.as_f64().filter(|v| v.is_finite()),
        Some(Value::String(s)) => first_number(s),
        Some(_) => {
            errors.push("`value` is not a number".into());
            None
        }
    };
    if let Some(v) = value {
        out.insert("value".into(), json!(v));
    }
    if let Some(u) = obj.get("unit").and_then(Value::as_str) {
        out.insert("unit".into(), json!(cut_chars(u, 32)));
    }
    let message = obj
        .get("message")
        .and_then(Value::as_str)
        .map(|m| cut_chars(m.trim(), MESSAGE_MAX));
    if let Some(m) = &message {
        out.insert("message".into(), json!(m));
    }
    let given = match obj.get("status") {
        None | Some(Value::Null) => None,
        Some(Value::String(s)) => match Status::parse(&s.to_ascii_lowercase()) {
            Some(st) => Some(st),
            None => {
                errors.push(format!("`status` `{s}` is not ok, warn, crit, or unknown"));
                Some(Status::Unknown)
            }
        },
        Some(_) => {
            errors.push("`status` is not a string".into());
            Some(Status::Unknown)
        }
    };
    if let Some(rows) = obj.get("rows") {
        match rows {
            Value::Array(items) => {
                let mut kept = Vec::new();
                let mut cut = items.len() > MAX_ROWS;
                for r in items.iter().take(MAX_ROWS) {
                    let Value::Object(m) = r else {
                        errors.push("each item of `rows` is an object".into());
                        kept.clear();
                        break;
                    };
                    if m.len() > MAX_COLS {
                        cut = true;
                    }
                    let row: Map<String, Value> = m
                        .iter()
                        .take(MAX_COLS)
                        .map(|(k, v)| (cut_chars(k, 64), cell(v.clone())))
                        .collect();
                    kept.push(Value::Object(row));
                }
                if errors.is_empty() {
                    out.insert("rows".into(), Value::Array(kept));
                    if cut {
                        out.insert("truncated".into(), json!(true));
                    }
                }
            }
            Value::Null => {}
            _ => errors.push("`rows` is not an array".into()),
        }
    }
    let status = match (given, value) {
        _ if !errors.is_empty() => Status::Unknown,
        (Some(s), _) => s,
        (None, Some(v)) if th.warn.is_some() || th.crit.is_some() => th.status(v),
        (None, _) => Status::of_exit(exit_code),
    };
    out.insert("status".into(), json!(status.as_str()));
    let error = if !errors.is_empty() {
        Some(errors.join("; "))
    } else if given.is_none() && value.is_none() && status == Status::Unknown {
        Some(exit_note(exit_code))
    } else {
        None
    };
    Parsed {
        status,
        value,
        text: message,
        json: Some(Value::Object(out)),
        error,
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;

    const NONE: Thresholds = Thresholds {
        warn: None,
        crit: None,
        direction: Direction::Above,
    };

    #[test]
    fn numbers_with_units_commas_and_signs() {
        assert_eq!(first_number("42"), Some(42.0));
        assert_eq!(first_number("  42%\n"), Some(42.0));
        assert_eq!(first_number("3.5GB"), Some(3.5));
        assert_eq!(first_number("load: 0.42 0.50 0.61"), Some(0.42));
        assert_eq!(first_number("1,234,567 rows"), Some(1_234_567.0));
        assert_eq!(first_number("1,234.5"), Some(1234.5));
        assert_eq!(first_number("-12 C"), Some(-12.0));
        assert_eq!(first_number("temp=-3.5"), Some(-3.5));
        assert_eq!(first_number("a-5"), Some(5.0));
        assert_eq!(first_number(".5"), Some(0.5));
        assert_eq!(first_number("-.25"), Some(-0.25));
        assert_eq!(first_number("1e3"), Some(1000.0));
        assert_eq!(first_number("2.5E-2"), Some(0.025));
        // A comma that is not a thousands separator ends the number.
        assert_eq!(first_number("1,2,3"), Some(1.0));
        assert_eq!(first_number("12,3456"), Some(12.0));
        assert_eq!(first_number("no digits"), None);
        assert_eq!(first_number(""), None);
        assert_eq!(first_number("- 5"), Some(5.0));
    }

    #[test]
    fn thresholds_above_and_below() {
        let above = Thresholds {
            warn: Some(80.0),
            crit: Some(90.0),
            direction: Direction::Above,
        };
        assert_eq!(above.status(10.0), Status::Ok);
        assert_eq!(above.status(80.0), Status::Warn);
        assert_eq!(above.status(89.9), Status::Warn);
        assert_eq!(above.status(90.0), Status::Crit);
        let below = Thresholds {
            warn: Some(20.0),
            crit: Some(5.0),
            direction: Direction::Below,
        };
        assert_eq!(below.status(50.0), Status::Ok);
        assert_eq!(below.status(20.0), Status::Warn);
        assert_eq!(below.status(4.0), Status::Crit);
        // Only a crit threshold.
        let crit_only = Thresholds {
            warn: None,
            crit: Some(1.0),
            direction: Direction::Above,
        };
        assert_eq!(crit_only.status(0.5), Status::Ok);
        assert_eq!(crit_only.status(1.0), Status::Crit);
        assert_eq!(NONE.status(1e9), Status::Ok);
        assert!(above.check().is_ok());
        assert!(below.check().is_ok());
        let wrong = Thresholds {
            warn: Some(90.0),
            crit: Some(80.0),
            direction: Direction::Above,
        };
        assert!(wrong.check().unwrap_err().contains("warn <= crit"));
        let nan = Thresholds {
            warn: Some(f64::NAN),
            ..NONE
        };
        assert!(nan.check().is_err());
    }

    #[test]
    fn number_kind() {
        let th = Thresholds {
            warn: Some(4.0),
            crit: Some(8.0),
            direction: Direction::Above,
        };
        let p = parse(Kind::Number, &th, "5.25\n", Some(0));
        assert_eq!((p.status, p.value), (Status::Warn, Some(5.25)));
        assert!(p.error.is_none());
        // A number wins over a non-zero exit (`grep -c` exits 1 on 0).
        let p = parse(Kind::Number, &th, "0\n", Some(1));
        assert_eq!((p.status, p.value), (Status::Ok, Some(0.0)));
        let p = parse(Kind::Number, &th, "command not found\n", Some(127));
        assert_eq!(p.status, Status::Unknown);
        assert_eq!(p.value, None);
        assert!(p.error.unwrap().contains("exit code 127"));
        assert_eq!(p.text.as_deref(), Some("command not found"));
    }

    #[test]
    fn exit_codes_map_as_nagios() {
        for (code, want) in [
            (Some(0), Status::Ok),
            (Some(1), Status::Warn),
            (Some(2), Status::Crit),
            (Some(3), Status::Unknown),
            (Some(127), Status::Unknown),
            (Some(-1), Status::Unknown),
            (None, Status::Unknown),
        ] {
            assert_eq!(Status::of_exit(code), want, "{code:?}");
        }
        let p = parse(
            Kind::Status,
            &NONE,
            "\n  docker is active  \nmore\n",
            Some(0),
        );
        assert_eq!(p.status, Status::Ok);
        assert_eq!(p.text.as_deref(), Some("docker is active"));
        assert!(p.error.is_none());
        let p = parse(Kind::Status, &NONE, "HTTP 503\n", Some(2));
        assert_eq!(p.status, Status::Crit);
        let p = parse(Kind::Status, &NONE, "", Some(3));
        assert_eq!(p.status, Status::Unknown);
        assert!(p.error.unwrap().contains("exit code 3"));
    }

    #[test]
    fn text_is_cut_on_a_char_boundary() {
        let p = parse(Kind::Text, &NONE, "up 3 days\n\n", Some(0));
        assert_eq!(p.text.as_deref(), Some("up 3 days"));
        assert_eq!(p.status, Status::Ok);
        assert!(p.json.is_none());
        let long = "é".repeat(TEXT_MAX);
        let p = parse(Kind::Text, &NONE, &long, Some(0));
        let t = p.text.unwrap();
        assert!(t.len() <= TEXT_MAX && t.len() > TEXT_MAX - 2);
        assert_eq!(p.json, Some(json!({"truncated": true})));
        let p = parse(Kind::Text, &NONE, "boom", Some(9));
        assert_eq!(p.status, Status::Unknown);
    }

    #[test]
    fn json_fields_and_status_order() {
        let th = Thresholds {
            warn: Some(1.0),
            crit: Some(2.0),
            direction: Direction::Above,
        };
        // An explicit status wins over the thresholds.
        let p = parse(
            Kind::Json,
            &th,
            r#"{"value": 5, "unit": "ms", "status": "OK", "message": " fine "}"#,
            Some(0),
        );
        assert_eq!(p.status, Status::Ok);
        assert_eq!(p.value, Some(5.0));
        assert_eq!(p.text.as_deref(), Some("fine"));
        assert_eq!(p.json.as_ref().unwrap()["unit"], "ms");
        // No status: the thresholds.
        let p = parse(Kind::Json, &th, r#"{"value": 1.5}"#, Some(0));
        assert_eq!(p.status, Status::Warn);
        // No status, no thresholds: the exit code.
        let p = parse(Kind::Json, &NONE, r#"{"value": 1.5}"#, Some(2));
        assert_eq!(p.status, Status::Crit);
        // A string value with a number.
        let p = parse(Kind::Json, &NONE, r#"{"value": "12.5%"}"#, Some(0));
        assert_eq!(p.value, Some(12.5));
    }

    #[test]
    fn json_validation_and_caps() {
        let p = parse(Kind::Json, &NONE, "not json", Some(0));
        assert_eq!(p.status, Status::Unknown);
        assert!(p.error.unwrap().starts_with("the output is not JSON"));
        let p = parse(Kind::Json, &NONE, "[1,2]", Some(0));
        assert!(p.error.unwrap().contains("an array"));
        let p = parse(Kind::Json, &NONE, r#"{"status": "bad"}"#, Some(0));
        assert_eq!(p.status, Status::Unknown);
        assert!(p.error.unwrap().contains("`bad`"));
        let p = parse(Kind::Json, &NONE, r#"{"value": [1]}"#, Some(0));
        assert_eq!(p.status, Status::Unknown);
        let p = parse(Kind::Json, &NONE, r#"{"rows": [1, 2]}"#, Some(0));
        assert_eq!(p.status, Status::Unknown);
        assert!(p.json.unwrap().get("rows").is_none());
        // 250 rows of 25 columns: 200 rows of 20 columns, and `truncated`.
        let row: Map<String, Value> = (0..25).map(|i| (format!("c{i:02}"), json!(i))).collect();
        let rows: Vec<Value> = (0..250).map(|_| Value::Object(row.clone())).collect();
        let text = json!({"rows": rows, "status": "ok"}).to_string();
        let p = parse(Kind::Json, &NONE, &text, Some(0));
        let j = p.json.unwrap();
        assert_eq!(j["rows"].as_array().unwrap().len(), MAX_ROWS);
        assert_eq!(j["rows"][0].as_object().unwrap().len(), MAX_COLS);
        assert_eq!(j["truncated"], true);
        assert_eq!(p.status, Status::Ok);
        // A nested cell is its JSON text; a long cell is cut.
        let long = "x".repeat(CELL_MAX + 10);
        let text = json!({"rows": [{"a": {"b": 1}, "c": long}]}).to_string();
        let j = parse(Kind::Json, &NONE, &text, Some(0)).json.unwrap();
        assert_eq!(j["rows"][0]["a"], r#"{"b":1}"#);
        assert_eq!(
            j["rows"][0]["c"].as_str().unwrap().chars().count(),
            CELL_MAX
        );
    }

    #[test]
    fn worse_status() {
        assert_eq!(Status::Ok.worse(Status::Warn), Status::Warn);
        assert_eq!(Status::Crit.worse(Status::Unknown), Status::Crit);
        assert_eq!(Status::Unknown.worse(Status::Ok), Status::Unknown);
    }
}
