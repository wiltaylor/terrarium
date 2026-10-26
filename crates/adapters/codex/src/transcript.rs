//! The unified transcript of a Codex rollout file
//! (`terrarium_core::transcript`, `docs/api.md` §6.6).
//!
//! Mapping, by record type (`payload.type` for `response_item` and
//! `event_msg`):
//!
//! - `response_item` `message`: role `user` gives user `text` blocks (the
//!   context that Codex injects, such as `<environment_context>`, has
//!   origin `system`); role `developer` or `system` gives `system` blocks
//!   (the instructions Codex sends); role `assistant` gives assistant
//!   `text` blocks. `input_image` is a `system` block "[image omitted]".
//!   One block per content part: `<payload.id>:<n>`.
//! - `reasoning`: a `thinking` block with the summary text, or `redacted`
//!   when there is only `encrypted_content`.
//! - `function_call`, `custom_tool_call`, `local_shell_call`,
//!   `web_search_call`: a `tool_call` block. `arguments` that parse as JSON
//!   become the input object.
//! - `function_call_output`, `custom_tool_call_output`: a `tool_result`
//!   block for `call_id`. An output object with `success: false` is an
//!   error.
//! - `agent_message` (a message from a sub-agent): a `system` block.
//! - `token_usage_record`: the usage of the response, set on the last
//!   assistant block before it (an update of that block).
//! - `compacted`, `event_msg` `turn_aborted`, `error`, `thread_goal_updated`,
//!   `sub_agent_activity`: a `system` block.
//!
//! Left out on purpose: `session_meta`, `turn_context` (its model goes on
//! the next assistant blocks), `world_state`, `inter_agent_communication_metadata`,
//! `response_item` `additional_tools`, and the `event_msg` records that
//! repeat a `response_item` or only count ([`SKIPPED_EVENTS`]). Any other
//! record is a `system` block with the raw type.

use serde_json::Value;
use terrarium_core::HarnessId;
use terrarium_core::tail::Lines;
use terrarium_core::transcript::{
    Block, BlockKind, BlockRole, BlockUsage, IMAGE_OMITTED, Origin, Transcript, json_or_string,
    unknown,
};

const H: HarnessId = HarnessId::Codex;

/// `event_msg` types that repeat a `response_item`, or carry counters and
/// settings.
pub const SKIPPED_EVENTS: &[&str] = &[
    "task_started",
    "task_complete",
    "token_count",
    "item_completed",
    "item_started",
    "thread_settings_applied",
    "user_message",
    "agent_message",
    "agent_message_delta",
    "agent_reasoning",
    "agent_reasoning_delta",
    "agent_reasoning_raw_content",
    "exec_command_begin",
    "exec_command_end",
    "exec_command_output_delta",
    "patch_apply_begin",
    "patch_apply_end",
    "web_search_begin",
    "web_search_end",
    "mcp_tool_call_begin",
    "mcp_tool_call_end",
    "turn_diff",
    "get_history_entry_response",
];

/// The start of a user message that Codex injects as context.
const INJECTED: &[&str] = &[
    "<environment_context>",
    "<user_instructions>",
    "<goal_context>",
    "<skill>",
    "<turn_aborted>",
    "<subagent_notification>",
    "<recommended_plugins>",
    "<user_shell_command>",
    "# AGENTS.md instructions",
];

/// The transcript parser for Codex rollout files.
#[derive(Debug, Default)]
pub struct CodexTranscript {
    lines: Lines,
    /// From the last `turn_context`.
    model: Option<String>,
    /// The last assistant block, for the usage of its response.
    last_assistant: Option<Block>,
}

impl CodexTranscript {
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
        let p = v.get("payload").cloned().unwrap_or(Value::Null);
        let id = s(&p, "id").unwrap_or_else(|| format!("L{no}"));
        let kind = v.get("type").and_then(Value::as_str).unwrap_or("");
        let out = match kind {
            "response_item" => self.item(&p, &id, no, at),
            "event_msg" => event(&p, &id, no, at),
            "turn_context" => {
                if let Some(m) = s(&p, "model") {
                    self.model = Some(m);
                }
                Vec::new()
            }
            "token_usage_record" => self.usage(&p),
            "compacted" => vec![system(id, no, at, "[context compacted]", "compacted")],
            "session_meta" | "world_state" | "inter_agent_communication_metadata" => Vec::new(),
            "" => vec![unknown(id, H, no, at, "untyped")],
            k => vec![unknown(id, H, no, at, k)],
        };
        if let Some(b) = out.iter().rev().find(|b| b.role == BlockRole::Assistant) {
            self.last_assistant = Some(b.clone());
        }
        out
    }

    fn assistant(&self, id: String, kind: BlockKind, no: u64, at: Option<String>) -> Block {
        Block::new(id, BlockRole::Assistant, kind, H, no, at).model(self.model.clone())
    }

    fn item(&self, p: &Value, id: &str, no: u64, at: Option<String>) -> Vec<Block> {
        let kind = p.get("type").and_then(Value::as_str).unwrap_or("");
        let call_id = s(p, "call_id").unwrap_or_else(|| id.to_owned());
        match kind {
            "message" => self.message(p, id, no, at),
            "reasoning" => {
                let summary = texts(p.get("summary"));
                let content = texts(p.get("content"));
                let text = if summary.is_empty() { content } else { summary };
                let mut b = self.assistant(format!("{id}:0"), BlockKind::Thinking, no, at);
                if text.trim().is_empty() {
                    b.redacted = true;
                    vec![b]
                } else {
                    vec![b.text(text)]
                }
            }
            "function_call" => {
                let args = s(p, "arguments").unwrap_or_default();
                vec![
                    self.assistant(format!("{id}:0"), BlockKind::ToolCall, no, at)
                        .tool(call_id, name(p), json_or_string(&args)),
                ]
            }
            "custom_tool_call" => vec![
                self.assistant(format!("{id}:0"), BlockKind::ToolCall, no, at)
                    .tool(
                        call_id,
                        name(p),
                        p.get("input").cloned().unwrap_or(Value::Null),
                    ),
            ],
            "local_shell_call" => vec![
                self.assistant(format!("{id}:0"), BlockKind::ToolCall, no, at)
                    .tool(
                        call_id,
                        "local_shell",
                        p.get("action").cloned().unwrap_or(Value::Null),
                    ),
            ],
            "web_search_call" => vec![
                self.assistant(format!("{id}:0"), BlockKind::ToolCall, no, at)
                    .tool(
                        call_id,
                        "web_search",
                        p.get("action").cloned().unwrap_or(Value::Null),
                    ),
            ],
            "function_call_output" | "custom_tool_call_output" => {
                let (output, is_error) = output(p.get("output"));
                vec![
                    Block::new(
                        format!("{id}:0"),
                        BlockRole::User,
                        BlockKind::ToolResult,
                        H,
                        no,
                        at,
                    )
                    .result(call_id, output, is_error),
                ]
            }
            "agent_message" => {
                let from = s(p, "author").unwrap_or_default();
                let text = texts(p.get("content"));
                vec![system(
                    format!("{id}:0"),
                    no,
                    at,
                    &format!("Message from {from}:\n{text}"),
                    "agent_message",
                )]
            }
            "additional_tools" => Vec::new(),
            other => vec![unknown(id, H, no, at, &format!("response_item:{other}"))],
        }
    }

    fn message(&self, p: &Value, id: &str, no: u64, at: Option<String>) -> Vec<Block> {
        let role = s(p, "role").unwrap_or_default();
        let parts = p
            .get("content")
            .and_then(Value::as_array)
            .cloned()
            .unwrap_or_default();
        parts
            .iter()
            .enumerate()
            .map(|(i, c)| {
                let bid = format!("{id}:{i}");
                let ctype = c.get("type").and_then(Value::as_str).unwrap_or("");
                if matches!(ctype, "input_image" | "output_image") {
                    return system(bid, no, at.clone(), IMAGE_OMITTED, "message:image");
                }
                let Some(text) = s(c, "text").or_else(|| s(c, "refusal")) else {
                    return unknown(bid, H, no, at.clone(), &format!("message:{ctype}"));
                };
                match role.as_str() {
                    "user" => {
                        let t = text.trim_start();
                        let origin = if INJECTED.iter().any(|p| t.starts_with(p)) {
                            Origin::System
                        } else {
                            Origin::User
                        };
                        Block::new(bid, BlockRole::User, BlockKind::Text, H, no, at.clone())
                            .text(text)
                            .origin(origin)
                    }
                    "assistant" => self
                        .assistant(bid, BlockKind::Text, no, at.clone())
                        .text(text),
                    other => system(bid, no, at.clone(), &text, &format!("message:{other}")),
                }
            })
            .collect()
    }

    /// The usage of one response, on the last assistant block.
    fn usage(&mut self, p: &Value) -> Vec<Block> {
        let Some(u) = p.get("usage") else {
            return Vec::new();
        };
        let n = |k: &str| u.get(k).and_then(Value::as_u64).unwrap_or(0);
        let cached = n("cached_input_tokens");
        let usage = BlockUsage {
            input: n("input_tokens").saturating_sub(cached),
            output: n("output_tokens"),
            cache_read: cached,
            cache_write: n("cache_write_input_tokens"),
        };
        match self.last_assistant.take() {
            Some(mut b) if b.usage.is_none() => {
                b.usage = Some(usage);
                vec![b]
            }
            _ => Vec::new(),
        }
    }
}

impl Transcript for CodexTranscript {
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

fn name(p: &Value) -> String {
    match (s(p, "namespace"), s(p, "name")) {
        (Some(ns), Some(n)) if !ns.is_empty() => format!("{ns}.{n}"),
        (_, n) => n.unwrap_or_default(),
    }
}

fn system(id: String, no: u64, at: Option<String>, text: &str, raw: &str) -> Block {
    Block::new(id, BlockRole::System, BlockKind::System, H, no, at)
        .text(text)
        .raw_type(raw)
}

/// The `text` of each part, joined: summaries, content, output parts.
fn texts(v: Option<&Value>) -> String {
    match v {
        Some(Value::String(t)) => t.clone(),
        Some(Value::Array(parts)) => parts
            .iter()
            .filter_map(|p| match p.get("type").and_then(Value::as_str) {
                Some("input_image" | "output_image") => Some(IMAGE_OMITTED.to_owned()),
                _ => s(p, "text"),
            })
            .collect::<Vec<_>>()
            .join("\n"),
        _ => String::new(),
    }
}

/// The text of a tool output, and whether it is an error. Codex writes a
/// string, an array of `input_text` parts, or `{content, success}`.
fn output(v: Option<&Value>) -> (String, bool) {
    match v {
        Some(Value::Object(o)) => {
            let ok = o.get("success").and_then(Value::as_bool).unwrap_or(true);
            let text = match o.get("content") {
                Some(c) => {
                    let t = texts(Some(c));
                    if t.is_empty() { c.to_string() } else { t }
                }
                None => Value::Object(o.clone()).to_string(),
            };
            (text, !ok)
        }
        other => (texts(other), false),
    }
}

fn event(p: &Value, id: &str, no: u64, at: Option<String>) -> Vec<Block> {
    let kind = p.get("type").and_then(Value::as_str).unwrap_or("");
    let raw = format!("event_msg:{kind}");
    match kind {
        k if SKIPPED_EVENTS.contains(&k) => Vec::new(),
        "turn_aborted" => {
            let reason = s(p, "reason").unwrap_or_default();
            vec![system(
                id.to_owned(),
                no,
                at,
                &format!("Turn aborted: {reason}"),
                &raw,
            )]
        }
        "error" | "stream_error" | "warning" => {
            let msg = s(p, "message").unwrap_or_default();
            vec![system(id.to_owned(), no, at, &msg, &raw)]
        }
        "thread_goal_updated" => {
            let goal = p
                .get("goal")
                .and_then(|g| s(g, "objective"))
                .unwrap_or_default();
            vec![system(
                id.to_owned(),
                no,
                at,
                &format!("Goal: {goal}"),
                &raw,
            )]
        }
        "sub_agent_activity" => {
            let path = s(p, "agent_path").unwrap_or_default();
            let what = s(p, "kind").unwrap_or_default();
            vec![system(
                id.to_owned(),
                no,
                at,
                &format!("Sub-agent {path}: {what}"),
                &raw,
            )]
        }
        _ => vec![unknown(id, H, no, at, &raw)],
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

    fn parse(path: &std::path::Path) -> TranscriptLog {
        let bytes = std::fs::read(path).expect("fixture");
        parse_all(&mut CodexTranscript::new(), &bytes)
    }

    #[test]
    fn transcript_matches_golden() {
        check_golden(
            &parse(&fixture("transcript.jsonl")),
            &fixture("transcript.golden.json"),
        );
    }

    /// The usage fixture (the spike capture of a two-turn session): every
    /// record maps.
    #[test]
    fn session_fixture_matches_golden() {
        let log = parse(&fixture("session.jsonl"));
        assert!(log.blocks().iter().all(|b| {
            b.raw_type.is_none()
                || b.raw_type
                    .as_deref()
                    .is_some_and(|r| r.starts_with("message:"))
        }));
        check_golden(&log, &fixture("session.transcript.golden.json"));
    }

    #[test]
    fn tool_calls_link_to_results_and_usage_updates_the_call() {
        let log = parse(&fixture("transcript.jsonl"));
        let b = log.blocks();
        let calls: Vec<&Block> = b.iter().filter(|b| b.kind == BlockKind::ToolCall).collect();
        assert!(calls.len() >= 8);
        for c in &calls {
            let id = &c.tool.as_ref().unwrap().id;
            if c.tool.as_ref().unwrap().name == "local_shell" {
                continue;
            }
            assert!(
                b.iter()
                    .any(|r| r.result.as_ref().is_some_and(|r| &r.tool_id == id)),
                "no result for {id}"
            );
        }
        let shell = calls
            .iter()
            .find(|c| c.tool.as_ref().unwrap().id == "call_fn_1")
            .unwrap();
        assert_eq!(
            shell.tool.as_ref().unwrap().summary,
            "shell: bash -lc ls -la"
        );
        assert_eq!(shell.model.as_deref(), Some("gpt-6-luna"));
        assert_eq!(
            shell.usage,
            Some(BlockUsage {
                input: 200,
                output: 12,
                cache_read: 800,
                cache_write: 0
            })
        );
        let failed = b
            .iter()
            .find(|r| r.result.as_ref().is_some_and(|r| r.tool_id == "call_fn_2"))
            .unwrap();
        assert!(failed.result.as_ref().unwrap().is_error);
        let thinking: Vec<&Block> = b.iter().filter(|b| b.kind == BlockKind::Thinking).collect();
        assert!(thinking.iter().any(|t| t.redacted));
        assert!(thinking.iter().any(|t| t.text.is_some()));
        let mail = b
            .iter()
            .find(|b| b.is_user_text() && b.text.as_deref().unwrap().starts_with("New mail from"))
            .unwrap();
        assert_eq!(mail.origin, Some(Origin::User));
        assert!(
            b.iter()
                .any(|b| b.is_user_text() && b.origin == Some(Origin::System))
        );
        for raw in [
            "event_msg:brand_new_event",
            "response_item:brand_new_item",
            "brand_new_record",
            "message:image",
        ] {
            assert!(
                b.iter().any(|b| b.raw_type.as_deref() == Some(raw)),
                "{raw}"
            );
        }
    }
}
