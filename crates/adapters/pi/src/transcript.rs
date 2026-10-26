//! The unified transcript of a pi session file
//! (`terrarium_core::transcript`, `docs/api.md` §6.6).
//!
//! The file is a tree (`id`, `parentId`); the transcript is in file order.
//! Mapping, by record type:
//!
//! - `message` with role `user`: a user `text` block per text part; an
//!   `image` part is a `system` block "[image omitted]".
//! - `message` with role `assistant`: a block per content part, in order:
//!   `thinking` (text, or `redacted` when pi stores only the signature),
//!   `text`, and `toolCall` (`tool_call`). Each block carries `model`; the
//!   last one carries `usage`. A call that failed or was aborted (`errorMessage`) adds a
//!   `system` block with the message.
//! - `message` with role `toolResult`: a `tool_result` block for
//!   `toolCallId`, with `isError`.
//! - `message` with role `bashExecution` (a `!` command of the user): a
//!   `system` block with the command and its output.
//! - `message` with another role (`custom`, `branchSummary`,
//!   `compactionSummary`), `compaction`, `branch_summary`,
//!   `custom_message`: `system` blocks.
//!
//! Left out on purpose: `session` (the header), `model_change` (its model
//! goes on the next blocks), `thinking_level_change`, `label`,
//! `session_info`. Any other record is a `system` block with the raw
//! type. Ids: `<record id>:<n>`.

use serde_json::Value;
use terrarium_core::HarnessId;
use terrarium_core::tail::Lines;
use terrarium_core::transcript::{
    Block, BlockKind, BlockRole, BlockUsage, IMAGE_OMITTED, Origin, Transcript, unknown,
};

const H: HarnessId = HarnessId::Pi;

/// Record types with no conversation in them.
const SKIPPED_TYPES: &[&str] = &[
    "session",
    "model_change",
    "thinking_level_change",
    "label",
    "session_info",
];

/// The transcript parser for pi session files.
#[derive(Debug, Default)]
pub struct PiTranscript {
    lines: Lines,
}

impl PiTranscript {
    pub fn new() -> Self {
        Self::default()
    }

    fn line(&mut self, no: u64, line: &str) -> Vec<Block> {
        if line.trim().is_empty() {
            return Vec::new();
        }
        let Ok(v) = serde_json::from_str::<Value>(line) else {
            return vec![unknown(format!("L{no}"), H, no, None, "invalid_json")];
        };
        let at = s(&v, "timestamp");
        let id = s(&v, "id").unwrap_or_else(|| format!("L{no}"));
        let kind = v.get("type").and_then(Value::as_str).unwrap_or("");
        match kind {
            "message" => message(v.get("message").unwrap_or(&Value::Null), &id, no, at),
            "compaction" => {
                let summary = s(&v, "summary").unwrap_or_default();
                vec![system(
                    format!("{id}:0"),
                    no,
                    at,
                    &format!("[context compacted]\n{summary}"),
                    kind,
                )]
            }
            "branch_summary" => {
                let summary = s(&v, "summary").unwrap_or_default();
                vec![system(format!("{id}:0"), no, at, &summary, kind)]
            }
            "custom_message" => {
                let text = content_text(v.get("content"));
                vec![system(format!("{id}:0"), no, at, &text, kind)]
            }
            k if SKIPPED_TYPES.contains(&k) => Vec::new(),
            "" => vec![unknown(id, H, no, at, "untyped")],
            k => vec![unknown(id, H, no, at, k)],
        }
    }
}

impl Transcript for PiTranscript {
    fn harness(&self) -> HarnessId {
        H
    }

    fn parse_lines(&mut self, bytes: &[u8]) -> Vec<Block> {
        let lines = self.lines.feed(bytes);
        lines
            .into_iter()
            .flat_map(|(no, l)| self.line(no, &l))
            .collect()
    }
}

fn s(v: &Value, key: &str) -> Option<String> {
    v.get(key).and_then(Value::as_str).map(str::to_owned)
}

fn system(id: String, no: u64, at: Option<String>, text: &str, raw: &str) -> Block {
    Block::new(id, BlockRole::System, BlockKind::System, H, no, at)
        .text(text)
        .raw_type(raw)
}

/// A string, or the text parts joined ("[image omitted]" for an image).
fn content_text(c: Option<&Value>) -> String {
    match c {
        Some(Value::String(t)) => t.clone(),
        Some(Value::Array(parts)) => parts
            .iter()
            .map(|p| match p.get("type").and_then(Value::as_str) {
                Some("text") => s(p, "text").unwrap_or_default(),
                Some("image") => IMAGE_OMITTED.to_owned(),
                _ => p.to_string(),
            })
            .collect::<Vec<_>>()
            .join("\n"),
        _ => String::new(),
    }
}

fn parts(m: &Value) -> Vec<Value> {
    match m.get("content") {
        Some(Value::Array(a)) => a.clone(),
        Some(Value::String(t)) => vec![serde_json::json!({"type": "text", "text": t})],
        _ => Vec::new(),
    }
}

fn message(m: &Value, id: &str, no: u64, at: Option<String>) -> Vec<Block> {
    let role = m.get("role").and_then(Value::as_str).unwrap_or("");
    match role {
        "user" => parts(m)
            .iter()
            .enumerate()
            .map(|(i, p)| {
                let bid = format!("{id}:{i}");
                match p.get("type").and_then(Value::as_str) {
                    Some("text") => {
                        Block::new(bid, BlockRole::User, BlockKind::Text, H, no, at.clone())
                            .text(s(p, "text").unwrap_or_default())
                            .origin(Origin::User)
                    }
                    Some("image") => system(bid, no, at.clone(), IMAGE_OMITTED, "user:image"),
                    other => unknown(
                        bid,
                        H,
                        no,
                        at.clone(),
                        &format!("user:{}", other.unwrap_or("")),
                    ),
                }
            })
            .collect(),
        "assistant" => assistant(m, id, no, at),
        "toolResult" => vec![
            Block::new(
                format!("{id}:0"),
                BlockRole::User,
                BlockKind::ToolResult,
                H,
                no,
                at,
            )
            .result(
                s(m, "toolCallId").unwrap_or_default(),
                content_text(m.get("content")),
                m.get("isError").and_then(Value::as_bool).unwrap_or(false),
            ),
        ],
        "bashExecution" => {
            let cmd = s(m, "command").unwrap_or_default();
            let out = s(m, "output").unwrap_or_default();
            vec![system(
                format!("{id}:0"),
                no,
                at,
                &format!("$ {cmd}\n{out}"),
                "message:bashExecution",
            )]
        }
        other => {
            let text = content_text(m.get("content"))
                + &s(m, "summary")
                    .map(|t| format!("\n{t}"))
                    .unwrap_or_default();
            vec![system(
                format!("{id}:0"),
                no,
                at,
                text.trim(),
                &format!("message:{other}"),
            )]
        }
    }
}

fn assistant(m: &Value, id: &str, no: u64, at: Option<String>) -> Vec<Block> {
    let model = s(m, "model");
    let usage = m.get("usage").map(|u| {
        let n = |k: &str| u.get(k).and_then(Value::as_u64).unwrap_or(0);
        BlockUsage {
            input: n("input"),
            output: n("output"),
            cache_read: n("cacheRead"),
            cache_write: n("cacheWrite"),
        }
    });
    let mut out: Vec<Block> = parts(m)
        .iter()
        .enumerate()
        .map(|(i, p)| {
            let new = |kind| {
                Block::new(
                    format!("{id}:{i}"),
                    BlockRole::Assistant,
                    kind,
                    H,
                    no,
                    at.clone(),
                )
            };
            match p.get("type").and_then(Value::as_str) {
                Some("text") => new(BlockKind::Text).text(s(p, "text").unwrap_or_default()),
                Some("thinking") => {
                    let t = s(p, "thinking").unwrap_or_default();
                    if t.trim().is_empty() {
                        let mut b = new(BlockKind::Thinking);
                        b.redacted = true;
                        b
                    } else {
                        new(BlockKind::Thinking).text(t)
                    }
                }
                Some("toolCall") => new(BlockKind::ToolCall).tool(
                    s(p, "id").unwrap_or_default(),
                    s(p, "name").unwrap_or_default(),
                    p.get("arguments").cloned().unwrap_or(Value::Null),
                ),
                Some("image") => system(
                    format!("{id}:{i}"),
                    no,
                    at.clone(),
                    IMAGE_OMITTED,
                    "assistant:image",
                ),
                other => unknown(
                    format!("{id}:{i}"),
                    H,
                    no,
                    at.clone(),
                    &format!("assistant:{}", other.unwrap_or("")),
                ),
            }
        })
        .collect();
    if let Some(err) = s(m, "errorMessage") {
        let reason = s(m, "stopReason").unwrap_or_else(|| "error".into());
        out.push(system(
            format!("{id}:{}", out.len()),
            no,
            at.clone(),
            &format!("{reason}: {err}"),
            "assistant:error",
        ));
    }
    let model = model.filter(|m| !m.is_empty());
    for b in out.iter_mut().filter(|b| b.role == BlockRole::Assistant) {
        b.model.clone_from(&model);
    }
    if let Some(last) = out
        .iter_mut()
        .rev()
        .find(|b| b.role == BlockRole::Assistant)
    {
        last.usage = usage.filter(|u| *u != BlockUsage::default());
    }
    out
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::*;
    use terrarium_core::transcript::{TranscriptLog, check_golden, parse_all};

    fn fixture(name: &str) -> std::path::PathBuf {
        std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .join("tests/fixtures")
            .join(name)
    }

    fn parse(name: &str) -> TranscriptLog {
        let bytes = std::fs::read(fixture(name)).expect("fixture");
        parse_all(&mut PiTranscript::new(), &bytes)
    }

    #[test]
    fn transcript_matches_golden() {
        check_golden(
            &parse("transcript.jsonl"),
            &fixture("transcript.golden.json"),
        );
    }

    /// The usage fixture (the spike capture of a two-turn session): every
    /// record maps.
    #[test]
    fn session_fixture_matches_golden() {
        let log = parse("session.jsonl");
        assert!(log.blocks().iter().all(|b| b.raw_type.is_none()));
        assert_eq!(log.len(), 4);
        check_golden(&log, &fixture("session.transcript.golden.json"));
    }

    #[test]
    fn a_tool_turn_and_the_odd_records() {
        let log = parse("transcript.jsonl");
        let b = log.blocks();
        use BlockKind::*;
        let kinds: Vec<BlockKind> = b.iter().take(5).map(|b| b.kind).collect();
        assert_eq!(kinds, [Text, Thinking, ToolCall, ToolResult, Thinking]);
        assert_eq!(b[2].tool.as_ref().unwrap().summary, "bash: ls");
        assert_eq!(b[3].result.as_ref().unwrap().tool_id, "call_pi_1");
        assert!(b[4].redacted);
        assert_eq!(b[1].model.as_deref(), Some("gpt-5.6-luna"));
        assert_eq!((b[1].usage, b[2].usage.is_some()), (None, true));
        assert!(
            b.iter()
                .any(|r| r.result.as_ref().is_some_and(|r| r.is_error))
        );
        for raw in [
            "user:image",
            "assistant:error",
            "message:bashExecution",
            "compaction",
            "custom_message",
            "brand_new_entry",
        ] {
            assert!(
                b.iter().any(|b| b.raw_type.as_deref() == Some(raw)),
                "{raw}"
            );
        }
    }
}
