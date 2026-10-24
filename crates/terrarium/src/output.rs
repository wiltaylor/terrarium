//! Text output: tables that fit the terminal, fields from JSON, and the
//! output sink that tests capture.

use std::io::{IsTerminal, Write};
use std::sync::{Arc, Mutex};

use serde_json::Value;

/// Where command output goes.
#[derive(Clone)]
pub enum Out {
    Stdout,
    /// Tests capture the output here.
    #[cfg_attr(not(test), allow(dead_code))]
    Buffer(Arc<Mutex<Vec<u8>>>),
}

impl Out {
    pub fn write_str(&self, s: &str) {
        match self {
            Self::Stdout => {
                let mut o = std::io::stdout().lock();
                // A closed pipe (`| head`) is not an error worth a message.
                let _ = o.write_all(s.as_bytes());
                let _ = o.flush();
            }
            Self::Buffer(b) => {
                if let Ok(mut b) = b.lock() {
                    b.extend_from_slice(s.as_bytes());
                }
            }
        }
    }

    /// Raw bytes, to stdout or (with `err`) stderr. Tests capture both.
    pub fn write_bytes(&self, err: bool, bytes: &[u8]) {
        match self {
            Self::Stdout if err => {
                let mut o = std::io::stderr().lock();
                let _ = o.write_all(bytes);
                let _ = o.flush();
            }
            Self::Stdout => {
                let mut o = std::io::stdout().lock();
                let _ = o.write_all(bytes);
                let _ = o.flush();
            }
            Self::Buffer(b) => {
                if let Ok(mut b) = b.lock() {
                    b.extend_from_slice(bytes);
                }
            }
        }
    }

    pub fn line(&self, s: impl AsRef<str>) {
        let mut s = s.as_ref().to_owned();
        s.push('\n');
        self.write_str(&s);
    }

    /// The terminal width, or `None` when the output is not a terminal.
    pub fn width(&self) -> Option<usize> {
        match self {
            Self::Buffer(_) => None,
            Self::Stdout => terminal_width(),
        }
    }

    #[cfg(test)]
    pub fn captured(&self) -> String {
        match self {
            Self::Stdout => String::new(),
            Self::Buffer(b) => b
                .lock()
                .map(|b| String::from_utf8_lossy(&b).into_owned())
                .unwrap_or_default(),
        }
    }
}

fn terminal_width() -> Option<usize> {
    if let Some(c) = std::env::var("COLUMNS")
        .ok()
        .and_then(|c| c.parse::<usize>().ok())
        .filter(|&c| c > 0)
    {
        return Some(c);
    }
    if !std::io::stdout().is_terminal() {
        return None;
    }
    crate::attach::terminal_size().map(|(_, cols)| cols as usize)
}

/// A string field of a JSON object, or `-`.
pub fn field(v: &Value, key: &str) -> String {
    match v.get(key) {
        None | Some(Value::Null) => "-".into(),
        Some(Value::String(s)) if s.is_empty() => "-".into(),
        Some(Value::String(s)) => s.clone(),
        Some(Value::Array(a)) if a.is_empty() => "-".into(),
        Some(Value::Array(a)) => a
            .iter()
            .map(|x| match x {
                Value::String(s) => s.clone(),
                // A list of objects shows their IDs or names.
                Value::Object(o) => o
                    .get("id")
                    .or_else(|| o.get("name"))
                    .and_then(Value::as_str)
                    .map_or_else(|| x.to_string(), str::to_owned),
                other => other.to_string(),
            })
            .collect::<Vec<_>>()
            .join(","),
        Some(other) => other.to_string(),
    }
}

/// The first of these fields that is set.
pub fn first_field(v: &Value, keys: &[&str]) -> String {
    keys.iter()
        .map(|k| field(v, k))
        .find(|s| s != "-")
        .unwrap_or_else(|| "-".into())
}

/// Shorten an RFC 3339 time to `YYYY-MM-DD HH:MM`.
pub fn short_time(s: &str) -> String {
    if s.len() >= 16 && s.as_bytes().get(10) == Some(&b'T') {
        format!("{} {}", &s[..10], &s[11..16])
    } else {
        s.to_owned()
    }
}

/// A table with a header row. Columns shrink to fit `width`; long cells wrap
/// inside their column.
pub struct Table {
    headers: Vec<String>,
    rows: Vec<Vec<String>>,
}

impl Table {
    pub fn new(headers: &[&str]) -> Self {
        Self {
            headers: headers.iter().map(|h| (*h).to_owned()).collect(),
            rows: Vec::new(),
        }
    }

    pub fn row(&mut self, cells: Vec<String>) {
        self.rows.push(cells);
    }

    pub fn render(&self, width: Option<usize>) -> String {
        const GAP: usize = 2;
        const MIN: usize = 6;
        let n = self.headers.len();
        let mut widths: Vec<usize> = self.headers.iter().map(|h| h.chars().count()).collect();
        for r in &self.rows {
            for (i, c) in r.iter().enumerate().take(n) {
                let w = c.lines().map(|l| l.chars().count()).max().unwrap_or(0);
                widths[i] = widths[i].max(w);
            }
        }
        if let Some(total) = width {
            let avail = total.saturating_sub(GAP * n.saturating_sub(1));
            // Shrink the widest column first. First keep each column as
            // wide as its longest word (at most 12), so that text wraps at
            // spaces. Then go down to `MIN`.
            let longest_word = |t: &str| t.split_whitespace().map(|w| w.chars().count()).max();
            let word_floor: Vec<usize> = (0..n)
                .map(|i| {
                    let cells = self.rows.iter().filter_map(|r| r.get(i));
                    std::iter::once(&self.headers[i])
                        .chain(cells)
                        .filter_map(|c| longest_word(c))
                        .max()
                        .unwrap_or(0)
                        .clamp(MIN, 12)
                })
                .collect();
            for floors in [word_floor, vec![MIN; n]] {
                while widths.iter().sum::<usize>() > avail {
                    let Some(i) = (0..n)
                        .filter(|&i| widths[i] > floors[i])
                        .max_by_key(|&i| widths[i])
                    else {
                        break;
                    };
                    widths[i] -= 1;
                }
            }
        }
        let mut out = String::new();
        let mut emit = |cells: &[String]| {
            let wrapped: Vec<Vec<String>> = (0..n)
                .map(|i| wrap(cells.get(i).map_or("", String::as_str), widths[i]))
                .collect();
            let height = wrapped.iter().map(Vec::len).max().unwrap_or(1);
            for line in 0..height {
                let mut s = String::new();
                for (i, col) in wrapped.iter().enumerate() {
                    let cell = col.get(line).map_or("", String::as_str);
                    if i + 1 == n {
                        s.push_str(cell);
                    } else {
                        s.push_str(cell);
                        let pad = widths[i] - cell.chars().count() + GAP;
                        s.extend(std::iter::repeat_n(' ', pad));
                    }
                }
                out.push_str(s.trim_end());
                out.push('\n');
            }
        };
        emit(&self.headers);
        for r in &self.rows {
            emit(r);
        }
        out
    }
}

/// Wrap text to `width` characters: at spaces when it can, else hard.
pub fn wrap(text: &str, width: usize) -> Vec<String> {
    let width = width.max(1);
    let mut out = Vec::new();
    for para in text.split('\n') {
        let mut line = String::new();
        for word in para.split(' ') {
            let mut word = word.to_owned();
            loop {
                let len = line.chars().count();
                let wlen = word.chars().count();
                let need = if len == 0 { wlen } else { len + 1 + wlen };
                if need <= width {
                    if len > 0 {
                        line.push(' ');
                    }
                    line.push_str(&word);
                    break;
                }
                if len > 0 {
                    out.push(std::mem::take(&mut line));
                    continue;
                }
                // The word alone is too long: split it.
                let head: String = word.chars().take(width).collect();
                word = word.chars().skip(width).collect();
                out.push(head);
                if word.is_empty() {
                    break;
                }
            }
        }
        out.push(line);
    }
    if out.is_empty() {
        out.push(String::new());
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn fields_read() {
        let v = json!({"a": "x", "b": null, "c": ["p", "q"], "d": 3, "e": "",
                       "f": [{"id": "TER-1", "title": "t"}, {"name": "n"}]});
        assert_eq!(field(&v, "a"), "x");
        assert_eq!(field(&v, "b"), "-");
        assert_eq!(field(&v, "c"), "p,q");
        assert_eq!(field(&v, "d"), "3");
        assert_eq!(field(&v, "e"), "-");
        assert_eq!(field(&v, "f"), "TER-1,n");
        assert_eq!(field(&v, "zz"), "-");
        assert_eq!(first_field(&v, &["b", "a"]), "x");
        assert_eq!(short_time("2026-10-24T06:36:54.149Z"), "2026-10-24 06:36");
    }

    #[test]
    fn tables_align() {
        let mut t = Table::new(&["ID", "NAME"]);
        t.row(vec!["1".into(), "alpha".into()]);
        t.row(vec!["22".into(), "b".into()]);
        assert_eq!(t.render(None), "ID  NAME\n1   alpha\n22  b\n");
    }

    #[test]
    fn narrow_tables_wrap() {
        let mut t = Table::new(&["ID", "SUBJECT"]);
        t.row(vec!["01".into(), "a long subject that will not fit".into()]);
        let s = t.render(Some(20));
        for line in s.lines() {
            assert!(line.chars().count() <= 20, "{line:?}");
        }
        assert!(s.contains("a long subject"));
        assert!(s.lines().count() > 2);
        // Headers stay whole while the body wraps.
        let mut t = Table::new(&["ID", "ASSIGNEE", "TITLE"]);
        t.row(vec![
            "TER-1".into(),
            "terra/worker".into(),
            "Write the parser now".into(),
        ]);
        let s = t.render(Some(30));
        assert!(s.lines().next().unwrap().contains("ASSIGNEE"), "{s}");
    }

    #[test]
    fn wraps_words_and_long_tokens() {
        assert_eq!(wrap("aa bb cc", 5), vec!["aa bb", "cc"]);
        assert_eq!(wrap("abcdefgh", 3), vec!["abc", "def", "gh"]);
        assert_eq!(wrap("", 3), vec![""]);
        assert_eq!(wrap("a\nb", 3), vec!["a", "b"]);
    }
}
