//! The unified session transcript: one block model for the session files
//! of every harness (`docs/api.md` §6.6).
//!
//! Each adapter crate has a [`Transcript`] parser for its harness's session
//! file, next to its usage parser. A parser reads bytes in any chunks and
//! keeps its own state, so the daemon feeds it only the new bytes of a
//! growing file. It returns new blocks, and blocks that changed: a block
//! with an `id` that came before replaces the earlier one (for example, a
//! usage record that comes after its message). [`TranscriptLog`] merges
//! the parser output and numbers the blocks.
//!
//! Nothing silently disappears: a record type that the parser does not
//! know becomes a `system` block with the raw type in `raw_type`. Record
//! types that carry no conversation (usage counters, hook runs, side
//! calls, context the harness injects) are known and left out on purpose;
//! each adapter's module documents its list.

use std::collections::HashMap;

use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::harness::HarnessId;

/// The longest tool output and text in a block, in bytes. A longer one is
/// cut at a character boundary, with `truncated: true`.
pub const MAX_OUTPUT: usize = 64 * 1024;

/// The longest tool input, as compact JSON. A longer input becomes a JSON
/// string of its first [`MAX_OUTPUT`] bytes.
pub const MAX_INPUT: usize = 64 * 1024;

/// The longest tool summary, in characters.
pub const SUMMARY_CHARS: usize = 120;

/// The text of a block for content that is not text (an image, a file).
pub const IMAGE_OMITTED: &str = "[image omitted]";

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum BlockRole {
    User,
    Assistant,
    System,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum BlockKind {
    Text,
    Thinking,
    ToolCall,
    ToolResult,
    System,
}

/// Who typed a user text, when it is known.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Origin {
    /// The operator, through `POST /api/agents/{id}/send` (the daemon's
    /// delivery log tells).
    Operator,
    /// A push pointer that the daemon typed: mail, card, lead, or lease.
    AgentMailPointer,
    /// The harness or the daemon made it: a recap turn, a background-task
    /// notification, injected context.
    System,
    /// A prompt that was typed into the harness, with no other source
    /// known.
    User,
}

/// A tool call of the model.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ToolCall {
    /// The harness's call id. The `tool_result` block names it.
    pub id: String,
    pub name: String,
    pub input: Value,
    /// One line for a list view, from the input.
    pub summary: String,
}

/// The result of a tool call.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ToolResult {
    /// The `tool.id` of the call.
    pub tool_id: String,
    pub output: String,
    #[serde(default, skip_serializing_if = "is_false")]
    pub truncated: bool,
    pub is_error: bool,
}

/// The tokens of one model response.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
pub struct BlockUsage {
    /// Input tokens that did not come from the cache.
    pub input: u64,
    pub output: u64,
    pub cache_read: u64,
    pub cache_write: u64,
}

/// Where a block comes from in the session file.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct BlockSource {
    pub harness: HarnessId,
    /// The file name of the session file (no directory). The daemon sets
    /// it; a parser leaves it empty.
    #[serde(default)]
    pub file: String,
    /// The line of the record, counted from 1.
    pub line: u64,
}

/// One item of a transcript.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Block {
    /// Stable within the session: the harness's message or record id, and
    /// the index of the part, for example `msg_01:2`.
    pub id: String,
    /// The Terrarium session. The daemon sets it.
    #[serde(default)]
    pub session: String,
    /// Monotonic within the session, from 1. [`TranscriptLog`] sets it.
    #[serde(default)]
    pub seq: u64,
    /// RFC 3339, when the record has a time.
    pub at: Option<String>,
    pub role: BlockRole,
    pub kind: BlockKind,
    /// Markdown or plain text.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub text: Option<String>,
    /// `text` was cut at [`MAX_OUTPUT`].
    #[serde(default, skip_serializing_if = "is_false")]
    pub truncated: bool,
    /// A thinking block whose text the harness does not store.
    #[serde(default, skip_serializing_if = "is_false")]
    pub redacted: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub origin: Option<Origin>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tool: Option<ToolCall>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub result: Option<ToolResult>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub model: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub usage: Option<BlockUsage>,
    /// The record type of a `system` block that maps a record the parser
    /// does not know, or a harness notice (`event_msg:turn_aborted`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub raw_type: Option<String>,
    pub source: BlockSource,
}

fn is_false(b: &bool) -> bool {
    !*b
}

impl Block {
    /// A block with no content yet.
    pub fn new(
        id: impl Into<String>,
        role: BlockRole,
        kind: BlockKind,
        harness: HarnessId,
        line: u64,
        at: Option<String>,
    ) -> Self {
        Self {
            id: id.into(),
            session: String::new(),
            seq: 0,
            at,
            role,
            kind,
            text: None,
            truncated: false,
            redacted: false,
            origin: None,
            tool: None,
            result: None,
            model: None,
            usage: None,
            raw_type: None,
            source: BlockSource {
                harness,
                file: String::new(),
                line,
            },
        }
    }

    /// Set the text, cut at [`MAX_OUTPUT`].
    pub fn text(mut self, text: impl Into<String>) -> Self {
        let (t, cut) = truncate(text.into(), MAX_OUTPUT);
        self.text = Some(t);
        self.truncated = cut;
        self
    }

    pub fn origin(mut self, origin: Origin) -> Self {
        self.origin = Some(origin);
        self
    }

    pub fn model(mut self, model: Option<String>) -> Self {
        self.model = model.filter(|m| !m.is_empty());
        self
    }

    pub fn raw_type(mut self, raw: impl Into<String>) -> Self {
        self.raw_type = Some(raw.into());
        self
    }

    /// A `tool_call` block's call. The input is cut at [`MAX_INPUT`].
    pub fn tool(mut self, id: impl Into<String>, name: impl Into<String>, input: Value) -> Self {
        let name = name.into();
        let summary = tool_summary(&name, &input);
        let input = cap_input(input);
        self.tool = Some(ToolCall {
            id: id.into(),
            name,
            input,
            summary,
        });
        self
    }

    /// A `tool_result` block's result. The output is cut at
    /// [`MAX_OUTPUT`].
    pub fn result(mut self, tool_id: impl Into<String>, output: String, is_error: bool) -> Self {
        let (output, truncated) = truncate(output, MAX_OUTPUT);
        self.result = Some(ToolResult {
            tool_id: tool_id.into(),
            output,
            truncated,
            is_error,
        });
        self
    }

    /// A user or assistant text: the role, `text`, and the text.
    pub fn is_user_text(&self) -> bool {
        self.role == BlockRole::User && self.kind == BlockKind::Text
    }
}

/// A `system` block for a record that the parser does not map: the text is
/// `[<raw type>]`.
pub fn unknown(
    id: impl Into<String>,
    harness: HarnessId,
    line: u64,
    at: Option<String>,
    raw: &str,
) -> Block {
    Block::new(id, BlockRole::System, BlockKind::System, harness, line, at)
        .text(format!("[{raw}]"))
        .raw_type(raw)
}

/// `s` cut to at most `max` bytes at a character boundary, and whether it
/// was cut.
pub fn truncate(mut s: String, max: usize) -> (String, bool) {
    if s.len() <= max {
        return (s, false);
    }
    let mut end = max;
    while !s.is_char_boundary(end) {
        end -= 1;
    }
    s.truncate(end);
    (s, true)
}

fn cap_input(input: Value) -> Value {
    let text = input.to_string();
    if text.len() <= MAX_INPUT {
        return input;
    }
    let s = match input {
        Value::String(s) => s,
        _ => text,
    };
    Value::String(truncate(s, MAX_INPUT).0)
}

/// One line for a tool call: the first of the usual fields of the input
/// (`command`, `file_path`, `path`, `pattern`, `url`, `query`,
/// `description`, `prompt`), else the input itself, cut at
/// [`SUMMARY_CHARS`] characters.
pub fn tool_summary(name: &str, input: &Value) -> String {
    const KEYS: [&str; 9] = [
        "command",
        "cmd",
        "file_path",
        "path",
        "pattern",
        "url",
        "query",
        "description",
        "prompt",
    ];
    let detail = match input {
        Value::String(s) => s.clone(),
        Value::Object(m) => KEYS
            .iter()
            .find_map(|k| match m.get(*k) {
                Some(Value::String(s)) => Some(s.clone()),
                Some(Value::Array(a)) if !a.is_empty() => Some(
                    a.iter()
                        .map(|v| v.as_str().map_or_else(|| v.to_string(), str::to_owned))
                        .collect::<Vec<_>>()
                        .join(" "),
                ),
                _ => None,
            })
            .unwrap_or_else(|| input.to_string()),
        Value::Null => String::new(),
        other => other.to_string(),
    };
    let line = detail.split_whitespace().collect::<Vec<_>>().join(" ");
    let line = if line.is_empty() {
        name.to_owned()
    } else {
        format!("{name}: {line}")
    };
    if line.chars().count() <= SUMMARY_CHARS {
        return line;
    }
    let mut out: String = line.chars().take(SUMMARY_CHARS - 1).collect();
    out.push('…');
    out
}

/// Parse a JSON string that holds an object (Codex `function_call`
/// arguments); else keep the string.
pub fn json_or_string(s: &str) -> Value {
    match serde_json::from_str::<Value>(s) {
        Ok(v @ (Value::Object(_) | Value::Array(_))) => v,
        _ => Value::String(s.to_owned()),
    }
}

/// A session-file parser of one harness. Object safe: the daemon holds a
/// `Box<dyn Transcript>` per file.
pub trait Transcript: Send {
    fn harness(&self) -> HarnessId;

    /// Read the next bytes of the file, in any chunks: a partial last line
    /// waits for the next call. Returns the new blocks and the blocks that
    /// changed (same `id`), in file order. `seq`, `session`, and
    /// `source.file` are left for the caller.
    fn parse_lines(&mut self, bytes: &[u8]) -> Vec<Block>;
}

/// The merged output of a parser: each `id` once, numbered in the order it
/// first came. Each new or changed block gets a new revision, so a stream
/// can send what changed since its last revision.
#[derive(Debug, Default, Clone)]
pub struct TranscriptLog {
    blocks: Vec<Block>,
    revs: Vec<u64>,
    index: HashMap<String, usize>,
    rev: u64,
}

impl TranscriptLog {
    pub fn new() -> Self {
        Self::default()
    }

    /// Add new blocks and replace changed ones. A replaced block keeps its
    /// `seq`. Returns the number of blocks that were added or changed.
    pub fn apply(&mut self, blocks: Vec<Block>) -> usize {
        let mut changed = 0;
        for mut b in blocks {
            match self.index.get(&b.id) {
                Some(&i) => {
                    b.seq = self.blocks[i].seq;
                    if self.blocks[i] != b {
                        self.rev += 1;
                        self.blocks[i] = b;
                        self.revs[i] = self.rev;
                        changed += 1;
                    }
                }
                None => {
                    self.rev += 1;
                    b.seq = self.blocks.len() as u64 + 1;
                    self.index.insert(b.id.clone(), self.blocks.len());
                    self.blocks.push(b);
                    self.revs.push(self.rev);
                    changed += 1;
                }
            }
        }
        changed
    }

    pub fn blocks(&self) -> &[Block] {
        &self.blocks
    }

    pub fn len(&self) -> usize {
        self.blocks.len()
    }

    pub fn is_empty(&self) -> bool {
        self.blocks.is_empty()
    }

    /// The revision of the last change, 0 when empty.
    pub fn rev(&self) -> u64 {
        self.rev
    }

    /// The blocks with a `seq` after `after`.
    pub fn after(&self, after: u64) -> &[Block] {
        let start = (after as usize).min(self.blocks.len());
        &self.blocks[start..]
    }

    /// The blocks that were added or changed after revision `rev`, in
    /// `seq` order.
    pub fn changed_since(&self, rev: u64) -> Vec<&Block> {
        self.blocks
            .iter()
            .zip(&self.revs)
            .filter(|(_, r)| **r > rev)
            .map(|(b, _)| b)
            .collect()
    }

    /// The first user text that the harness did not make, cut at `chars`
    /// characters on one line.
    pub fn title(&self, chars: usize) -> Option<String> {
        let b = self
            .blocks
            .iter()
            .find(|b| b.is_user_text() && b.origin != Some(Origin::System))?;
        let line = b
            .text
            .as_deref()?
            .split_whitespace()
            .collect::<Vec<_>>()
            .join(" ");
        Some(if line.chars().count() <= chars {
            line
        } else {
            line.chars().take(chars).collect()
        })
    }
}

/// Parse a whole file with `parser` and merge the blocks: for tests and
/// one-off reads.
pub fn parse_all(parser: &mut dyn Transcript, bytes: &[u8]) -> TranscriptLog {
    let mut log = TranscriptLog::new();
    log.apply(parser.parse_lines(bytes));
    log
}

/// Compare `log` with the golden JSON file `path`. With
/// `TERRARIUM_UPDATE_GOLDEN=1` the file is written instead. Panics with a
/// diff hint on a mismatch. For the adapters' golden tests.
pub fn check_golden(log: &TranscriptLog, path: &std::path::Path) {
    let got = serde_json::to_string_pretty(log.blocks()).unwrap_or_default() + "\n";
    if std::env::var("TERRARIUM_UPDATE_GOLDEN").is_ok_and(|v| v == "1") {
        if let Err(e) = std::fs::write(path, &got) {
            panic!("cannot write {}: {e}", path.display());
        }
        return;
    }
    let want = std::fs::read_to_string(path).unwrap_or_default();
    if want != got {
        let first = want
            .lines()
            .zip(got.lines())
            .position(|(a, b)| a != b)
            .unwrap_or_else(|| want.lines().count().min(got.lines().count()));
        panic!(
            "{} differs from the parse at line {} (run with TERRARIUM_UPDATE_GOLDEN=1 to accept):\nwant: {}\n got: {}",
            path.display(),
            first + 1,
            want.lines().nth(first).unwrap_or("<end>"),
            got.lines().nth(first).unwrap_or("<end>"),
        );
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::*;
    use serde_json::json;

    fn text(id: &str, t: &str) -> Block {
        Block::new(
            id,
            BlockRole::Assistant,
            BlockKind::Text,
            HarnessId::Claude,
            1,
            None,
        )
        .text(t)
    }

    #[test]
    fn log_numbers_new_blocks_and_replaces_changed_ones() {
        let mut log = TranscriptLog::new();
        assert_eq!(log.apply(vec![text("a", "1"), text("b", "2")]), 2);
        let rev = log.rev();
        assert_eq!(log.apply(vec![text("a", "1")]), 0, "no change, no rev");
        assert_eq!(log.apply(vec![text("a", "1+"), text("c", "3")]), 2);
        assert_eq!(
            log.blocks().iter().map(|b| b.seq).collect::<Vec<_>>(),
            [1, 2, 3]
        );
        assert_eq!(log.blocks()[0].text.as_deref(), Some("1+"));
        let changed: Vec<&str> = log
            .changed_since(rev)
            .iter()
            .map(|b| b.id.as_str())
            .collect();
        assert_eq!(changed, ["a", "c"]);
        assert_eq!(log.after(2).len(), 1);
        assert_eq!(log.after(99).len(), 0);
    }

    #[test]
    fn long_text_and_output_are_cut_at_a_char_boundary() {
        let long = "é".repeat(MAX_OUTPUT);
        let b = text("a", &long);
        assert!(b.truncated);
        assert!(b.text.as_ref().unwrap().len() <= MAX_OUTPUT);
        let r = Block::new(
            "r",
            BlockRole::User,
            BlockKind::ToolResult,
            HarnessId::Codex,
            1,
            None,
        )
        .result("c1", long, false);
        assert!(r.result.as_ref().unwrap().truncated);
        let json = serde_json::to_value(&r).unwrap();
        assert_eq!(json["result"]["truncated"], true);
        assert_eq!(json["at"], Value::Null);
        assert!(json.get("text").is_none());
    }

    #[test]
    fn summaries_pick_the_usual_field_on_one_line() {
        assert_eq!(
            tool_summary("Bash", &json!({"command": "ls\n-la", "description": "x"})),
            "Bash: ls -la"
        );
        assert_eq!(
            tool_summary("exec", &json!("tools.exec_command({cmd: 'ls'})")),
            "exec: tools.exec_command({cmd: 'ls'})"
        );
        assert_eq!(
            tool_summary("shell", &json!({"command": ["bash", "-lc", "ls"]})),
            "shell: bash -lc ls"
        );
        assert_eq!(tool_summary("x", &Value::Null), "x");
        let s = tool_summary("Read", &json!({"file_path": "a".repeat(500)}));
        assert_eq!(s.chars().count(), SUMMARY_CHARS);
        assert!(s.ends_with('…'));
        assert_eq!(json_or_string("{\"a\":1}"), json!({"a": 1}));
        assert_eq!(json_or_string("nope"), json!("nope"));
    }

    #[test]
    fn title_skips_harness_text() {
        let mut log = TranscriptLog::new();
        let user = |id: &str, t: &str| {
            Block::new(id, BlockRole::User, BlockKind::Text, HarnessId::Pi, 1, None).text(t)
        };
        log.apply(vec![
            user("s", "<environment_context>").origin(Origin::System),
            user("u", "Run  `ls`\nand reply done.").origin(Origin::User),
        ]);
        assert_eq!(log.title(80).as_deref(), Some("Run `ls` and reply done."));
        assert_eq!(log.title(3).as_deref(), Some("Run"));
    }
}
