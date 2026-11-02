//! The dashboard of a project (docs/api.md §30.4): an ordered list of
//! blocks, each a view of one monitor or a Markdown note. And the
//! downsample of results for a chart (`?bucket=`).

use std::collections::{HashMap, HashSet};

use serde::{Deserialize, Serialize};

use super::parse::{Kind, Status};

/// The most blocks of one dashboard.
pub const MAX_BLOCKS: usize = 100;
/// The most characters of a note.
pub const NOTE_MAX: usize = 4000;
/// The most characters of a block title.
pub const TITLE_MAX: usize = 120;
/// The most characters of a block ID.
pub const ID_MAX: usize = 40;
/// The most decimals of a value.
pub const MAX_DECIMALS: u8 = 6;

/// What a block shows.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum BlockKind {
    /// The last value, big, with a 24-hour sparkline.
    Stat,
    /// The status badge and the message.
    Status,
    /// A line chart of the values over a range.
    Chart,
    /// The last text, monospace.
    Text,
    /// The rows of the last `json` result.
    Table,
    /// A Markdown note: a section header. No monitor.
    Note,
}

impl BlockKind {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Stat => "stat",
            Self::Status => "status",
            Self::Chart => "chart",
            Self::Text => "text",
            Self::Table => "table",
            Self::Note => "note",
        }
    }
}

/// The width of a block: 1, 2, or 4 columns of the desktop grid.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Size {
    #[default]
    S,
    M,
    L,
}

/// The time range of a chart block.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
pub enum Range {
    #[serde(rename = "1h")]
    H1,
    #[serde(rename = "6h")]
    H6,
    #[default]
    #[serde(rename = "24h")]
    H24,
    #[serde(rename = "7d")]
    D7,
}

/// The options of a block. Each is optional; the dashboard has defaults.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct BlockOptions {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub range: Option<Range>,
    /// A stat block: show the 24-hour sparkline (default true).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub sparkline: Option<bool>,
    /// The decimals of a value (0-6). Default: as the value needs.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub decimals: Option<u8>,
}

/// One block of a dashboard.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Block {
    pub id: String,
    pub kind: BlockKind,
    /// The monitor ID. None for a note.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub monitor: Option<String>,
    #[serde(default)]
    pub size: Size,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub title: Option<String>,
    /// The Markdown of a note.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub text: Option<String>,
    #[serde(default)]
    pub options: BlockOptions,
}

/// The layout of a dashboard: the blocks in order.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Layout {
    #[serde(default)]
    pub blocks: Vec<Block>,
}

/// Check a layout against the monitors of the project (ID to kind). The
/// error names the block: `blocks[2].monitor: ...`.
pub fn check(layout: &Layout, monitors: &HashMap<String, Kind>) -> Result<(), (String, String)> {
    if layout.blocks.len() > MAX_BLOCKS {
        return Err((
            "blocks".into(),
            format!(
                "{} blocks; a dashboard has at most {MAX_BLOCKS}",
                layout.blocks.len()
            ),
        ));
    }
    let mut ids = HashSet::new();
    for (i, b) in layout.blocks.iter().enumerate() {
        let at = |f: &str| format!("blocks[{i}].{f}");
        let ok_id = !b.id.is_empty()
            && b.id.len() <= ID_MAX
            && b.id
                .chars()
                .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_');
        if !ok_id {
            return Err((
                at("id"),
                format!("`{}` is not 1-{ID_MAX} of a-z, A-Z, 0-9, `-`, `_`", b.id),
            ));
        }
        if !ids.insert(b.id.as_str()) {
            return Err((at("id"), format!("`{}` is used twice", b.id)));
        }
        if b.title
            .as_ref()
            .is_some_and(|t| t.chars().count() > TITLE_MAX)
        {
            return Err((at("title"), format!("longer than {TITLE_MAX} characters")));
        }
        if b.options.decimals.is_some_and(|d| d > MAX_DECIMALS) {
            return Err((at("options.decimals"), format!("use 0-{MAX_DECIMALS}")));
        }
        if b.kind == BlockKind::Note {
            if b.monitor.is_some() {
                return Err((at("monitor"), "a note shows no monitor".into()));
            }
            let text = b.text.as_deref().unwrap_or_default();
            if text.trim().is_empty() {
                return Err((at("text"), "a note needs Markdown text".into()));
            }
            if text.chars().count() > NOTE_MAX {
                return Err((at("text"), format!("longer than {NOTE_MAX} characters")));
            }
            continue;
        }
        if b.text.is_some() {
            return Err((at("text"), "only a note has text".into()));
        }
        let Some(m) = &b.monitor else {
            return Err((
                at("monitor"),
                format!("a {} block needs a monitor", b.kind.as_str()),
            ));
        };
        let Some(kind) = monitors.get(m) else {
            return Err((at("monitor"), format!("no monitor `{m}` in this project")));
        };
        match b.kind {
            BlockKind::Stat | BlockKind::Chart if !kind.has_value() => {
                return Err((
                    at("kind"),
                    format!(
                        "a {} block needs numbers; monitor `{m}` is a {} monitor (use status or text)",
                        b.kind.as_str(),
                        kind.as_str()
                    ),
                ));
            }
            BlockKind::Table if *kind != Kind::Json => {
                return Err((
                    at("kind"),
                    format!(
                        "a table block needs a json monitor; monitor `{m}` is a {} monitor",
                        kind.as_str()
                    ),
                ));
            }
            _ => {}
        }
    }
    Ok(())
}

/// Remove the blocks of monitor `id`. True when one went.
pub fn drop_monitor(layout: &mut Layout, id: &str) -> bool {
    let n = layout.blocks.len();
    layout.blocks.retain(|b| b.monitor.as_deref() != Some(id));
    layout.blocks.len() != n
}

/// One point of a series: a result, or the summary of a bucket.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct Point {
    pub at: String,
    pub status: Status,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub value: Option<f64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub min: Option<f64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub max: Option<f64>,
    /// The results in the bucket.
    pub count: usize,
}

/// Downsample `(at in ms, status, value)` rows, in time order, into
/// buckets of `bucket_ms`. Each bucket gives the mean, the min, and the
/// max of its values, and the worst status. `at` is the start of the
/// bucket, as RFC 3339.
pub fn downsample(rows: &[(i64, Status, Option<f64>)], bucket_ms: i64) -> Vec<Point> {
    let bucket_ms = bucket_ms.max(1);
    let mut out: Vec<Point> = Vec::new();
    let mut cur: Option<(i64, Status, f64, usize, f64, f64, usize)> = None;
    let flush = |c: (i64, Status, f64, usize, f64, f64, usize), out: &mut Vec<Point>| {
        let (start, status, sum, n, min, max, count) = c;
        out.push(Point {
            at: stamp_ms(start),
            status,
            value: (n > 0).then(|| sum / n as f64),
            min: (n > 0).then_some(min),
            max: (n > 0).then_some(max),
            count,
        });
    };
    for &(at, status, value) in rows {
        let start = at.div_euclid(bucket_ms) * bucket_ms;
        match &mut cur {
            Some(c) if c.0 == start => {
                c.1 = c.1.worse(status);
                if let Some(v) = value {
                    c.2 += v;
                    c.3 += 1;
                    c.4 = c.4.min(v);
                    c.5 = c.5.max(v);
                }
                c.6 += 1;
            }
            _ => {
                if let Some(c) = cur.take() {
                    flush(c, &mut out);
                }
                cur = Some(match value {
                    Some(v) => (start, status, v, 1, v, v, 1),
                    None => (start, status, 0.0, 0, f64::INFINITY, f64::NEG_INFINITY, 1),
                });
            }
        }
    }
    if let Some(c) = cur {
        flush(c, &mut out);
    }
    out
}

fn stamp_ms(ms: i64) -> String {
    chrono::DateTime::from_timestamp_millis(ms)
        .map(crate::time_param::stored)
        .unwrap_or_default()
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use serde_json::json;

    use super::*;

    fn monitors() -> HashMap<String, Kind> {
        [
            ("n".to_owned(), Kind::Number),
            ("s".to_owned(), Kind::Status),
            ("j".to_owned(), Kind::Json),
            ("t".to_owned(), Kind::Text),
        ]
        .into_iter()
        .collect()
    }

    fn layout(v: serde_json::Value) -> Layout {
        serde_json::from_value(v).unwrap()
    }

    #[test]
    fn a_good_layout_passes() {
        let l = layout(json!({"blocks": [
            {"id": "head", "kind": "note", "text": "## Hosts", "size": "l"},
            {"id": "a", "kind": "stat", "monitor": "n", "options": {"decimals": 1, "sparkline": false}},
            {"id": "b", "kind": "chart", "monitor": "j", "size": "m", "options": {"range": "7d"}},
            {"id": "c", "kind": "status", "monitor": "s", "title": "Docker"},
            {"id": "d", "kind": "text", "monitor": "t"},
            {"id": "e", "kind": "table", "monitor": "j"},
            {"id": "f", "kind": "status", "monitor": "n"},
        ]}));
        check(&l, &monitors()).unwrap();
        assert_eq!(l.blocks[1].size, Size::S);
        assert_eq!(l.blocks[2].options.range, Some(Range::D7));
        // The same monitor in two blocks is fine.
        assert_eq!(l.blocks[2].monitor, l.blocks[5].monitor);
    }

    #[test]
    fn bad_layouts_name_the_field() {
        let cases = [
            (json!([{"id": "a", "kind": "stat"}]), "blocks[0].monitor"),
            (
                json!([{"id": "a", "kind": "stat", "monitor": "zzz"}]),
                "blocks[0].monitor",
            ),
            (
                json!([{"id": "a", "kind": "stat", "monitor": "s"}]),
                "blocks[0].kind",
            ),
            (
                json!([{"id": "a", "kind": "chart", "monitor": "t"}]),
                "blocks[0].kind",
            ),
            (
                json!([{"id": "a", "kind": "table", "monitor": "n"}]),
                "blocks[0].kind",
            ),
            (json!([{"id": "a", "kind": "note"}]), "blocks[0].text"),
            (
                json!([{"id": "a", "kind": "note", "text": "x", "monitor": "n"}]),
                "blocks[0].monitor",
            ),
            (
                json!([{"id": "a", "kind": "text", "monitor": "t", "text": "x"}]),
                "blocks[0].text",
            ),
            (
                json!([{"id": "a b", "kind": "note", "text": "x"}]),
                "blocks[0].id",
            ),
            (
                json!([{"id": "a", "kind": "note", "text": "x"}, {"id": "a", "kind": "note", "text": "y"}]),
                "blocks[1].id",
            ),
            (
                json!([{"id": "a", "kind": "stat", "monitor": "n", "options": {"decimals": 9}}]),
                "blocks[0].options.decimals",
            ),
        ];
        for (blocks, field) in cases {
            let l = layout(json!({ "blocks": blocks }));
            let (f, msg) = check(&l, &monitors()).unwrap_err();
            assert_eq!(f, field, "{msg}");
        }
        let many: Vec<_> = (0..=MAX_BLOCKS)
            .map(|i| json!({"id": format!("b{i}"), "kind": "note", "text": "x"}))
            .collect();
        assert_eq!(
            check(&layout(json!({ "blocks": many })), &monitors())
                .unwrap_err()
                .0,
            "blocks"
        );
        // Unknown fields, kinds, sizes, and ranges do not parse.
        for bad in [
            json!({"blocks": [{"id": "a", "kind": "pie", "monitor": "n"}]}),
            json!({"blocks": [{"id": "a", "kind": "stat", "monitor": "n", "size": "xl"}]}),
            json!({"blocks": [{"id": "a", "kind": "chart", "monitor": "n", "options": {"range": "2h"}}]}),
            json!({"blocks": [{"id": "a", "kind": "stat", "monitor": "n", "color": "red"}]}),
        ] {
            assert!(serde_json::from_value::<Layout>(bad).is_err());
        }
    }

    #[test]
    fn deleting_a_monitor_drops_its_blocks() {
        let mut l = layout(json!({"blocks": [
            {"id": "a", "kind": "stat", "monitor": "n"},
            {"id": "b", "kind": "note", "text": "x"},
            {"id": "c", "kind": "chart", "monitor": "n"},
        ]}));
        assert!(drop_monitor(&mut l, "n"));
        assert_eq!(l.blocks.len(), 1);
        assert!(!drop_monitor(&mut l, "n"));
    }

    #[test]
    fn downsample_buckets() {
        let rows = vec![
            (0, Status::Ok, Some(1.0)),
            (30_000, Status::Warn, Some(3.0)),
            (59_999, Status::Ok, None),
            (60_000, Status::Ok, Some(10.0)),
            (300_000, Status::Crit, None),
        ];
        let pts = downsample(&rows, 60_000);
        assert_eq!(pts.len(), 3);
        assert_eq!(pts[0].at, "1970-01-01T00:00:00.000Z");
        assert_eq!(pts[0].value, Some(2.0));
        assert_eq!((pts[0].min, pts[0].max), (Some(1.0), Some(3.0)));
        assert_eq!(pts[0].status, Status::Warn);
        assert_eq!(pts[0].count, 3);
        assert_eq!(pts[1].value, Some(10.0));
        assert_eq!(pts[1].at, "1970-01-01T00:01:00.000Z");
        assert_eq!(pts[2].value, None);
        assert_eq!(pts[2].status, Status::Crit);
        assert!(downsample(&[], 1000).is_empty());
    }
}
