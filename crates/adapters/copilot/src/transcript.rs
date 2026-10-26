//! The unified transcript of a Copilot CLI session file,
//! `session-state/<id>/events.jsonl` (`terrarium_core::transcript`,
//! `docs/api.md` §6.6).
//!
//! Mapping, by event type:
//!
//! - `user.message`: a user `text` block with `content` (not the
//!   `transformedContent` that Copilot sends). A `<system_notification>`
//!   prompt (Copilot makes it when a background shell ends) has origin
//!   `system`. An image attachment is a `system` block "[image omitted]".
//! - `assistant.message`: in order, a `thinking` block (`reasoningText`,
//!   or `redacted` when only the opaque reasoning is stored), a `text`
//!   block (`content`, when not empty), and a `tool_call` block per
//!   `toolRequests` item. Ids: `<messageId>:<n>`.
//! - `tool.execution_complete`: a `tool_result` block for `toolCallId`;
//!   `success: false` is an error, with `error.message` as the output.
//! - `tool.execution_start` of a call that no `toolRequests` named: a
//!   `tool_call` block.
//! - `session.usage_record`: the usage of the call, set on the last block
//!   of the assistant message with the same `apiCallId` (it usually comes
//!   just before the message). Copilot counts the
//!   cache inside `inputTokens`; `usage.input` is the rest.
//! - `system.message` (the system prompt that was sent), `subagent.*`,
//!   `skill.invoked`, `abort`, `session.compaction_complete`,
//!   `session.info`, `session.warning`, `session.error`: `system` blocks.
//!
//! Left out on purpose: `hook.*`, `model.*` (the title side call),
//! `assistant.turn_start`, `assistant.turn_end`, and the session records
//! that carry settings or counters ([`SKIPPED_EVENTS`]). Any other event
//! is a `system` block with the raw type.

use std::collections::{HashMap, HashSet};

use serde_json::Value;
use terrarium_core::HarnessId;
use terrarium_core::tail::Lines;
use terrarium_core::transcript::{
    Block, BlockKind, BlockRole, BlockUsage, IMAGE_OMITTED, Origin, Transcript, unknown,
};

const H: HarnessId = HarnessId::Copilot;

/// Session events with settings or counters only.
pub const SKIPPED_EVENTS: &[&str] = &[
    "session.start",
    "session.resume",
    "session.model_change",
    "session.permissions_changed",
    "session.usage_checkpoint",
    "session.shutdown",
    "session.plan_changed",
    "session.mode_changed",
    "session.compaction_start",
    "assistant.turn_start",
    "assistant.turn_end",
];

/// The transcript parser for Copilot `events.jsonl` files.
#[derive(Debug, Default)]
pub struct CopilotTranscript {
    lines: Lines,
    /// Tool calls that an `assistant.message` named.
    requested: HashSet<String>,
    /// The first block of each assistant message, by `apiCallId`.
    by_api_call: HashMap<String, Block>,
    /// Usage that came before its message (the usual order), by
    /// `apiCallId`.
    pending_usage: HashMap<String, BlockUsage>,
}

impl CopilotTranscript {
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
        let d = v.get("data").cloned().unwrap_or(Value::Null);
        let id = s(&v, "id").unwrap_or_else(|| format!("L{no}"));
        let kind = v.get("type").and_then(Value::as_str).unwrap_or("");
        match kind {
            "user.message" => user(&d, &id, no, at),
            "assistant.message" => self.assistant(&d, &id, no, at),
            "tool.execution_start" => {
                let call = s(&d, "toolCallId").unwrap_or_default();
                if self.requested.contains(&call) {
                    return Vec::new();
                }
                self.requested.insert(call.clone());
                vec![
                    Block::new(
                        format!("{id}:0"),
                        BlockRole::Assistant,
                        BlockKind::ToolCall,
                        H,
                        no,
                        at,
                    )
                    .model(s(&d, "model"))
                    .tool(
                        call,
                        s(&d, "toolName").unwrap_or_default(),
                        d.get("arguments").cloned().unwrap_or(Value::Null),
                    ),
                ]
            }
            "tool.execution_complete" => {
                let ok = d.get("success").and_then(Value::as_bool).unwrap_or(true);
                let output = if ok {
                    result_text(d.get("result"))
                } else {
                    d.get("error")
                        .and_then(|e| s(e, "message"))
                        .unwrap_or_else(|| result_text(d.get("result")))
                };
                vec![
                    Block::new(
                        format!("{id}:0"),
                        BlockRole::User,
                        BlockKind::ToolResult,
                        H,
                        no,
                        at,
                    )
                    .result(
                        s(&d, "toolCallId").unwrap_or_default(),
                        output,
                        !ok,
                    ),
                ]
            }
            "session.usage_record" => self.usage(&d),
            "system.message" => vec![system(
                id,
                no,
                at,
                &s(&d, "content").unwrap_or_default(),
                kind,
            )],
            "subagent.started" | "subagent.completed" | "subagent.failed" => {
                let name = s(&d, "agentDisplayName")
                    .or_else(|| s(&d, "agentName"))
                    .unwrap_or_default();
                let what = kind.trim_start_matches("subagent.");
                let mut text = format!("Sub-agent {name} {what}");
                if let Some(e) = s(&d, "error") {
                    text.push_str(&format!(": {e}"));
                }
                vec![system(id, no, at, &text, kind)]
            }
            "skill.invoked" => {
                let name = s(&d, "name").unwrap_or_default();
                vec![system(id, no, at, &format!("Skill {name} invoked"), kind)]
            }
            "abort" => {
                let reason = s(&d, "reason").unwrap_or_default();
                vec![system(id, no, at, &format!("Aborted: {reason}"), kind)]
            }
            "session.compaction_complete" => {
                vec![system(id, no, at, "[context compacted]", kind)]
            }
            "session.info" | "session.warning" | "session.error" => {
                let msg = s(&d, "message").unwrap_or_default();
                vec![system(id, no, at, &msg, kind)]
            }
            k if SKIPPED_EVENTS.contains(&k) => Vec::new(),
            k if k.starts_with("hook.") || k.starts_with("model.") => Vec::new(),
            "" => vec![unknown(id, H, no, at, "untyped")],
            k => vec![unknown(id, H, no, at, k)],
        }
    }

    fn assistant(&mut self, d: &Value, id: &str, no: u64, at: Option<String>) -> Vec<Block> {
        let mid = s(d, "messageId").unwrap_or_else(|| id.to_owned());
        let model = s(d, "model");
        let mut out = Vec::new();
        let new = |n: usize, kind| {
            Block::new(
                format!("{mid}:{n}"),
                BlockRole::Assistant,
                kind,
                H,
                no,
                at.clone(),
            )
            .model(model.clone())
        };
        let reasoning = s(d, "reasoningText").filter(|t| !t.trim().is_empty());
        let opaque = d.get("reasoningOpaque").is_some() || d.get("encryptedContent").is_some();
        if let Some(t) = reasoning {
            out.push(new(out.len(), BlockKind::Thinking).text(t));
        } else if opaque {
            let mut b = new(out.len(), BlockKind::Thinking);
            b.redacted = true;
            out.push(b);
        }
        if let Some(t) = s(d, "content").filter(|t| !t.trim().is_empty()) {
            out.push(new(out.len(), BlockKind::Text).text(t));
        }
        for r in d
            .get("toolRequests")
            .and_then(Value::as_array)
            .into_iter()
            .flatten()
        {
            let call = s(r, "toolCallId").unwrap_or_default();
            self.requested.insert(call.clone());
            out.push(new(out.len(), BlockKind::ToolCall).tool(
                call,
                s(r, "name").unwrap_or_default(),
                r.get("arguments").cloned().unwrap_or(Value::Null),
            ));
        }
        if let Some(api) = s(d, "apiCallId")
            && let Some(last) = out.last_mut()
        {
            match self.pending_usage.remove(&api) {
                Some(u) => last.usage = Some(u),
                None => {
                    self.by_api_call.insert(api, last.clone());
                }
            }
        }
        out
    }

    fn usage(&mut self, d: &Value) -> Vec<Block> {
        let Some(u) = d.get("usage") else {
            return Vec::new();
        };
        let Some(api) = s(u, "apiCallId") else {
            return Vec::new();
        };
        let n = |k: &str| u.get(k).and_then(Value::as_u64).unwrap_or(0);
        let (read, write) = (n("cacheReadTokens"), n("cacheWriteTokens"));
        let usage = BlockUsage {
            input: n("inputTokens").saturating_sub(read + write),
            output: n("outputTokens"),
            cache_read: read,
            cache_write: write,
        };
        let Some(mut b) = self.by_api_call.remove(&api) else {
            self.pending_usage.insert(api, usage);
            return Vec::new();
        };
        b.usage = Some(usage);
        vec![b]
    }
}

impl Transcript for CopilotTranscript {
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

fn user(d: &Value, id: &str, no: u64, at: Option<String>) -> Vec<Block> {
    let mid = s(d, "messageId").unwrap_or_else(|| id.to_owned());
    let text = s(d, "content").unwrap_or_default();
    let origin = if text.trim_start().starts_with("<system_notification>") {
        Origin::System
    } else {
        Origin::User
    };
    let mut out = vec![
        Block::new(
            format!("{mid}:0"),
            BlockRole::User,
            BlockKind::Text,
            H,
            no,
            at.clone(),
        )
        .text(text)
        .origin(origin),
    ];
    for a in d
        .get("attachments")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
    {
        let kind = a.get("type").and_then(Value::as_str).unwrap_or("");
        if kind == "image" || kind == "blob" {
            out.push(system(
                format!("{mid}:{}", out.len()),
                no,
                at.clone(),
                IMAGE_OMITTED,
                "user.message:image",
            ));
        }
    }
    out
}

/// The text of a tool result: `content`, or `detailedContent`, or the JSON.
fn result_text(r: Option<&Value>) -> String {
    match r {
        Some(Value::Object(o)) => match o.get("content").or_else(|| o.get("detailedContent")) {
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
            Some(other) => other.to_string(),
            None => Value::Object(o.clone()).to_string(),
        },
        Some(Value::String(t)) => t.clone(),
        Some(Value::Null) | None => String::new(),
        Some(other) => other.to_string(),
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
        parse_all(&mut CopilotTranscript::new(), &bytes)
    }

    #[test]
    fn transcript_matches_golden() {
        check_golden(
            &parse("transcript.jsonl"),
            &fixture("transcript.golden.json"),
        );
    }

    /// The usage fixture (the spike capture of a two-turn session with a
    /// resume): every event maps.
    #[test]
    fn events_fixture_matches_golden() {
        let log = parse("events.jsonl");
        assert!(
            log.blocks()
                .iter()
                .all(|b| b.raw_type.is_none() || b.raw_type.as_deref() == Some("system.message"))
        );
        check_golden(&log, &fixture("events.transcript.golden.json"));
    }

    #[test]
    fn tool_requests_results_usage_and_notices() {
        let log = parse("transcript.jsonl");
        let b = log.blocks();
        // Every call has its result, and no call is there twice.
        let calls: Vec<&str> = b
            .iter()
            .filter_map(|b| b.tool.as_ref().map(|t| t.id.as_str()))
            .collect();
        let mut unique = calls.clone();
        unique.sort_unstable();
        unique.dedup();
        assert_eq!(calls.len(), unique.len());
        for id in &calls {
            assert!(
                b.iter()
                    .any(|r| r.result.as_ref().is_some_and(|r| r.tool_id == *id)),
                "no result for {id}"
            );
        }
        let failed = b
            .iter()
            .find(|r| r.result.as_ref().is_some_and(|r| r.tool_id == "tc-view"))
            .unwrap();
        let r = failed.result.as_ref().unwrap();
        assert!(r.is_error);
        assert_eq!(r.output, "Path does not exist");
        let think = b.iter().find(|b| b.id == "am-think:0").unwrap();
        assert_eq!(think.kind, BlockKind::Thinking);
        assert_eq!(think.usage, None);
        let call = b.iter().find(|b| b.id == "am-think:1").unwrap();
        assert_eq!(
            call.usage,
            Some(BlockUsage {
                input: 50,
                output: 5,
                cache_read: 50,
                cache_write: 0
            })
        );
        assert!(b.iter().find(|b| b.id == "am-redacted:0").unwrap().redacted);
        let note = b.iter().find(|b| b.id == "um-sys:0").unwrap();
        assert_eq!(note.origin, Some(Origin::System));
        for raw in [
            "user.message:image",
            "subagent.failed",
            "abort",
            "session.error",
            "session.brand_new",
        ] {
            assert!(
                b.iter().any(|b| b.raw_type.as_deref() == Some(raw)),
                "{raw}"
            );
        }
    }
}
