//! The unified transcript of a Claude Code session file
//! (`terrarium_core::transcript`, `docs/api.md` §6.6).
//!
//! Mapping, by record type:
//!
//! - `user` with string content: a user `text` block. `isMeta` and
//!   command wrappers (`<command-name>`, `<local-command-…>`) have origin
//!   `system`; a typed prompt has origin `user` (the daemon tells operator
//!   and pointer turns apart).
//! - `user` with content parts: `text` is a user `text` block; `tool_result`
//!   is a `tool_result` block (role `user`) that names its `tool_use_id`;
//!   `image` is a `system` block "[image omitted]". An image in a tool
//!   result becomes "[image omitted]" in the output.
//! - `assistant`: one API response can make several records (one per
//!   content block) that repeat `message.id` and `usage`. Each content
//!   block is one block, `<message.id>:<n>`; a repeated identical part is
//!   dropped, and a repeated `tool_use` id replaces the earlier block
//!   (the dedupe rule of [`crate::usage`]). `text`, `thinking` (text, or
//!   `redacted` when the harness stores only the signature),
//!   `redacted_thinking` (`redacted`), and `tool_use` (`tool_call`). Each
//!   block carries `model`; only the last block of a message carries
//!   `usage`, so a new part of the message moves it (an update of the
//!   block before).
//! - `attachment` `queued_command`: a user `text` block (a prompt typed
//!   while busy; a `<task-notification>` has origin `system`).
//! - `system` `compact_boundary`, `away_summary`, `informational`,
//!   `local_command`, `api_error`: a `system` block with the content.
//!
//! Left out on purpose: `cost-state`, `api-request*`, `last-prompt`,
//! `ai-title`, `custom-title`, `agent-name`, `mode`, `permission-mode`,
//! `atis-latch`, `file-history-*`, `queue-operation` (the queued prompt
//! comes as the `queued_command` attachment), `system` `turn_duration` and
//! `stop_hook_summary`, and the attachments that inject context
//! ([`SKIPPED_ATTACHMENTS`]). Any other record is a `system` block with the
//! raw type.

use std::collections::HashMap;

use serde_json::Value;
use terrarium_core::HarnessId;
use terrarium_core::tail::Lines;
use terrarium_core::transcript::{
    Block, BlockKind, BlockRole, BlockUsage, IMAGE_OMITTED, Origin, Transcript, unknown,
};

const H: HarnessId = HarnessId::Claude;

/// Top-level record types with no conversation in them.
const SKIPPED_TYPES: &[&str] = &[
    "cost-state",
    "api-request",
    "api-request-shape",
    "api-request-blob",
    "last-prompt",
    "ai-title",
    "custom-title",
    "agent-name",
    "mode",
    "permission-mode",
    "atis-latch",
    "file-history-snapshot",
    "file-history-delta",
    "queue-operation",
];

/// `system` subtypes with no conversation in them.
const SKIPPED_SYSTEM: &[&str] = &["turn_duration", "stop_hook_summary"];

/// Attachments that inject context or record a hook run.
pub const SKIPPED_ATTACHMENTS: &[&str] = &[
    "hook_success",
    "hook_non_blocking_error",
    "hook_additional_context",
    "total_tokens_reminder",
    "batching_reminder_sent",
    "bash_output_audience_note",
    "silent_turn_reminder",
    "skill_listing",
    "date",
    "deferred_tools_record",
    "deferred_tools_delta",
    "mcp_instructions_delta",
    "model",
    "environment",
    "agent_listing_delta",
    "session_context",
    "credential_org",
    "instructions",
    "auto_mode",
    "command_permissions",
    "thinking_drop",
    "remote_session_change",
    "prompt_snapshot",
    "invoked_skills",
    "compact_file_reference",
    "read_truncation_notice",
    "file",
    "edited_text_file",
    "plan_mode",
    "todo_reminder",
    "nested_memory",
];

/// The parts of one assistant message seen so far.
#[derive(Debug, Default)]
struct Message {
    /// The JSON of each part, for the dedupe of identical repeats.
    parts: Vec<String>,
    /// `tool_use` id to part index.
    tools: HashMap<String, usize>,
    /// The blocks of the message, by part index. The last one carries the
    /// usage.
    blocks: Vec<Block>,
    usage: Option<BlockUsage>,
}

/// The transcript parser for Claude Code session files.
#[derive(Debug, Default)]
pub struct ClaudeTranscript {
    lines: Lines,
    messages: HashMap<String, Message>,
}

impl ClaudeTranscript {
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
        let id = s(&v, "uuid").unwrap_or_else(|| format!("L{no}"));
        let kind = v.get("type").and_then(Value::as_str).unwrap_or("");
        match kind {
            "user" => user(&v, &id, no, at),
            "assistant" => self.assistant(&v, &id, no, at),
            "attachment" => attachment(&v, &id, no, at),
            "system" => system(&v, &id, no, at),
            k if SKIPPED_TYPES.contains(&k) => Vec::new(),
            "" => vec![unknown(id, H, no, at, "untyped")],
            k => vec![unknown(id, H, no, at, k)],
        }
    }

    fn assistant(&mut self, v: &Value, id: &str, no: u64, at: Option<String>) -> Vec<Block> {
        let Some(msg) = v.get("message") else {
            return vec![unknown(id, H, no, at, "assistant")];
        };
        let mid = s(msg, "id").unwrap_or_else(|| id.to_owned());
        let model = s(msg, "model").filter(|m| m != "<synthetic>");
        let usage = msg.get("usage").map(usage);
        let parts: Vec<Value> = match msg.get("content") {
            Some(Value::Array(a)) => a.clone(),
            Some(Value::String(t)) => vec![serde_json::json!({"type": "text", "text": t})],
            _ => Vec::new(),
        };
        let m = self.messages.entry(mid.clone()).or_default();
        if usage.is_some() {
            m.usage = usage;
        }
        // The part indexes that this record added or changed.
        let mut touched = Vec::new();
        for part in parts {
            let key = part.to_string();
            if m.parts.contains(&key) {
                continue;
            }
            let tool_id = (part.get("type").and_then(Value::as_str) == Some("tool_use"))
                .then(|| s(&part, "id"))
                .flatten();
            let index = match tool_id.as_ref().and_then(|t| m.tools.get(t)) {
                Some(&i) => i,
                None => {
                    m.parts.push(key.clone());
                    m.parts.len() - 1
                }
            };
            if let Some(t) = tool_id {
                m.tools.insert(t, index);
            }
            m.parts[index] = key;
            let block = assistant_part(&part, &format!("{mid}:{index}"), no, at.clone())
                .model(model.clone());
            if index < m.blocks.len() {
                m.blocks[index] = block;
            } else {
                m.blocks.push(block);
            }
            touched.push(index);
        }
        // The usage goes on the last block of the message only: a block
        // that stops being the last one loses it (an update).
        let last = m.blocks.len().saturating_sub(1);
        for (i, b) in m.blocks.iter_mut().enumerate() {
            let want = if i == last { m.usage } else { None };
            if b.usage != want {
                b.usage = want;
                if !touched.contains(&i) {
                    touched.push(i);
                }
            }
        }
        touched.sort_unstable();
        touched.dedup();
        let out: Vec<Block> = touched.iter().map(|&i| m.blocks[i].clone()).collect();
        out
    }
}

impl Transcript for ClaudeTranscript {
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

fn u(v: &Value, key: &str) -> u64 {
    v.get(key).and_then(Value::as_u64).unwrap_or(0)
}

fn usage(v: &Value) -> BlockUsage {
    BlockUsage {
        input: u(v, "input_tokens"),
        output: u(v, "output_tokens"),
        cache_read: u(v, "cache_read_input_tokens"),
        cache_write: u(v, "cache_creation_input_tokens"),
    }
}

fn system_text(id: String, no: u64, at: Option<String>, text: &str, raw: &str) -> Block {
    Block::new(id, BlockRole::System, BlockKind::System, H, no, at)
        .text(text)
        .raw_type(raw)
}

/// The origin of a user string: harness-made or typed.
fn user_origin(v: &Value, text: &str) -> Origin {
    let t = text.trim_start();
    let made = v.get("isMeta").and_then(Value::as_bool) == Some(true)
        || v.get("isCompactSummary").and_then(Value::as_bool) == Some(true)
        || t.starts_with("<command-name>")
        || t.starts_with("<command-message>")
        || t.starts_with("<local-command-")
        || t.starts_with("<task-notification>")
        || t.starts_with("<system-reminder>");
    if made { Origin::System } else { Origin::User }
}

fn user(v: &Value, id: &str, no: u64, at: Option<String>) -> Vec<Block> {
    let content = v.get("message").and_then(|m| m.get("content"));
    match content {
        Some(Value::String(text)) => vec![
            Block::new(
                format!("{id}:0"),
                BlockRole::User,
                BlockKind::Text,
                H,
                no,
                at,
            )
            .text(text.clone())
            .origin(user_origin(v, text)),
        ],
        Some(Value::Array(parts)) => parts
            .iter()
            .enumerate()
            .map(|(i, p)| {
                let bid = format!("{id}:{i}");
                match p.get("type").and_then(Value::as_str).unwrap_or("") {
                    "text" => {
                        let text = s(p, "text").unwrap_or_default();
                        let origin = user_origin(v, &text);
                        Block::new(bid, BlockRole::User, BlockKind::Text, H, no, at.clone())
                            .text(text)
                            .origin(origin)
                    }
                    "tool_result" => Block::new(
                        bid,
                        BlockRole::User,
                        BlockKind::ToolResult,
                        H,
                        no,
                        at.clone(),
                    )
                    .result(
                        s(p, "tool_use_id").unwrap_or_default(),
                        result_text(p.get("content")),
                        p.get("is_error").and_then(Value::as_bool).unwrap_or(false),
                    ),
                    "image" | "document" => {
                        system_text(bid, no, at.clone(), IMAGE_OMITTED, "user:image")
                    }
                    other => unknown(bid, H, no, at.clone(), &format!("user:{other}")),
                }
            })
            .collect(),
        _ => vec![unknown(id, H, no, at, "user")],
    }
}

/// The text of a `tool_result` content: a string, or text parts joined,
/// with "[image omitted]" for an image.
pub fn result_text(content: Option<&Value>) -> String {
    match content {
        Some(Value::String(t)) => t.clone(),
        Some(Value::Array(parts)) => parts
            .iter()
            .map(|p| match p.get("type").and_then(Value::as_str) {
                Some("text") => s(p, "text").unwrap_or_default(),
                Some("image" | "document") => IMAGE_OMITTED.to_owned(),
                Some("tool_reference") => {
                    format!("[tool {}]", s(p, "tool_name").unwrap_or_default())
                }
                _ => p.to_string(),
            })
            .collect::<Vec<_>>()
            .join("\n"),
        Some(Value::Null) | None => String::new(),
        Some(other) => other.to_string(),
    }
}

fn assistant_part(part: &Value, id: &str, no: u64, at: Option<String>) -> Block {
    let new = |kind| Block::new(id, BlockRole::Assistant, kind, H, no, at.clone());
    match part.get("type").and_then(Value::as_str).unwrap_or("") {
        "text" => new(BlockKind::Text).text(s(part, "text").unwrap_or_default()),
        "thinking" => {
            let text = s(part, "thinking").unwrap_or_default();
            if text.trim().is_empty() {
                let mut b = new(BlockKind::Thinking);
                b.redacted = true;
                b
            } else {
                new(BlockKind::Thinking).text(text)
            }
        }
        "redacted_thinking" => {
            let mut b = new(BlockKind::Thinking);
            b.redacted = true;
            b
        }
        "tool_use" | "server_tool_use" => new(BlockKind::ToolCall).tool(
            s(part, "id").unwrap_or_default(),
            s(part, "name").unwrap_or_default(),
            part.get("input").cloned().unwrap_or(Value::Null),
        ),
        "image" => system_text(id.to_owned(), no, at, IMAGE_OMITTED, "assistant:image"),
        other => unknown(id, H, no, at, &format!("assistant:{other}")),
    }
}

fn attachment(v: &Value, id: &str, no: u64, at: Option<String>) -> Vec<Block> {
    let a = v.get("attachment").cloned().unwrap_or(Value::Null);
    let kind = a.get("type").and_then(Value::as_str).unwrap_or("");
    match kind {
        "queued_command" => {
            let text = s(&a, "prompt").unwrap_or_default();
            let origin = user_origin(v, &text);
            vec![
                Block::new(
                    format!("{id}:0"),
                    BlockRole::User,
                    BlockKind::Text,
                    H,
                    no,
                    at,
                )
                .text(text)
                .origin(origin),
            ]
        }
        k if SKIPPED_ATTACHMENTS.contains(&k) => Vec::new(),
        k => vec![unknown(id, H, no, at, &format!("attachment:{k}"))],
    }
}

fn system(v: &Value, id: &str, no: u64, at: Option<String>) -> Vec<Block> {
    let sub = v.get("subtype").and_then(Value::as_str).unwrap_or("");
    if SKIPPED_SYSTEM.contains(&sub) {
        return Vec::new();
    }
    let raw = format!("system:{sub}");
    match s(v, "content") {
        Some(text) => vec![system_text(id.to_owned(), no, at, &text, &raw)],
        None => vec![unknown(id, H, no, at, &raw)],
    }
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
        parse_all(&mut ClaudeTranscript::new(), &bytes)
    }

    #[test]
    fn transcript_matches_golden() {
        check_golden(
            &parse("transcript.jsonl"),
            &fixture("transcript.golden.json"),
        );
    }

    /// The spike capture: a two-turn session with a thinking block.
    #[test]
    fn spike_capture_matches_golden() {
        let path = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("../../../spikes/claude/session-id-and-resume/captured/session.jsonl");
        let bytes = std::fs::read(path).expect("spike capture");
        let log = parse_all(&mut ClaudeTranscript::new(), &bytes);
        assert!(
            log.blocks().iter().all(|b| b.raw_type.is_none()),
            "every record maps"
        );
        check_golden(&log, &fixture("spike.transcript.golden.json"));
    }

    #[test]
    fn the_block_sequence_of_a_tool_turn() {
        let log = parse("transcript.jsonl");
        let kinds: Vec<(BlockRole, BlockKind)> = log
            .blocks()
            .iter()
            .map(|b| (b.role, b.kind))
            .take(6)
            .collect();
        use BlockKind::*;
        use BlockRole::*;
        assert_eq!(
            kinds,
            [
                (User, Text),
                (Assistant, ToolCall),
                (User, ToolResult),
                (Assistant, Thinking),
                (Assistant, Text),
                (User, Text),
            ]
        );
        let b = log.blocks();
        let call = b[1].tool.as_ref().unwrap();
        assert_eq!(call.name, "Bash");
        assert_eq!(call.summary, "Bash: terrarium targets ls");
        assert_eq!(b[2].result.as_ref().unwrap().tool_id, call.id);
        assert!(b[3].redacted && b[3].text.is_none());
        assert_eq!(b[0].origin, Some(Origin::User));
        assert_eq!(b[1].model.as_deref(), Some("claude-haiku-5-5"));
        assert!(b[1].usage.is_some());
        // One message, two blocks: the usage is on the last one only.
        assert_eq!((b[3].usage, b[4].usage.is_some()), (None, true));
        // The duplicate tool_use record gives no second block.
        let reads = b
            .iter()
            .filter(|b| b.tool.as_ref().is_some_and(|t| t.id == "toolu_synthetic_1"))
            .count();
        assert_eq!(reads, 1);
        // The queued task notification is a user text from the harness.
        assert!(b.iter().any(|b| {
            b.is_user_text()
                && b.origin == Some(Origin::System)
                && b.text
                    .as_deref()
                    .unwrap()
                    .starts_with("<task-notification>")
        }));
        // Nothing silently disappears.
        for raw in [
            "attachment:brand_new_attachment",
            "brand-new-record",
            "invalid_json",
        ] {
            assert!(
                b.iter().any(|b| b.raw_type.as_deref() == Some(raw)),
                "{raw}"
            );
        }
    }

    #[test]
    fn chunked_input_gives_the_same_blocks() {
        let bytes = std::fs::read(fixture("transcript.jsonl")).unwrap();
        let whole = parse_all(&mut ClaudeTranscript::new(), &bytes);
        let mut p = ClaudeTranscript::new();
        let mut log = TranscriptLog::new();
        for chunk in bytes.chunks(97) {
            log.apply(p.parse_lines(chunk));
        }
        assert_eq!(log.blocks(), whole.blocks());
    }
}
