//! Map the Terrarium extension's events to Terrarium events (PLAN §7).
//!
//! The extension (`ext/terrarium.ts`) sends one hook message per pi event.
//! The hook name is the pi event name. Every payload carries
//! `hook_event_name`, `ts`, `pid`, `session_id`, `session_file`, `cwd`, and
//! `model` (`<provider>/<id>`).
//!
//! Rules from `docs/compat.md` and the adapter's live check:
//!
//! - `input` opens a turn. It fires 0.01 to 0.10 s after the CR. Text sent
//!   while a turn runs steers that turn: a second `input`
//!   (`streaming_behavior` = `steer`), one `agent_settled`.
//! - `agent_settled` closes the turn. `agent_end` and `turn_end` repeat for
//!   each model call and for each automatic retry; they map to nothing.
//! - An `input` that starts no agent run (no credential for the provider)
//!   gives no `agent_settled` from pi. The extension sends a synthetic one
//!   (`synthetic` = `no_agent_run`) after 1.5 s.
//! - `ui_prompt_start` is the "needs input" signal: an extension called
//!   `ctx.ui.select()`, `confirm()`, `input()`, `editor()`, or `custom()`.
//!   pi has no built-in question tool, and no tool approval.
//! - `ui_prompt_end` maps to nothing: the runner leaves `blocked` on the
//!   next tool event, the end of the turn, or its own answer.

use std::path::PathBuf;

use serde_json::Value;
use terrarium_core::{Event, HarnessSessionId, HookPayload};

/// The IDs that every payload of the extension carries.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct HookContext {
    /// `hook_event_name`, else the hook name of the message.
    pub event: String,
    pub session_id: Option<HarnessSessionId>,
    /// The session file. pi names it before the file exists: it is created
    /// at the first message.
    pub session_file: Option<PathBuf>,
    pub cwd: Option<PathBuf>,
    /// `<provider>/<id>`.
    pub model: Option<String>,
    /// Milliseconds since the epoch, from the extension.
    pub ts: Option<u64>,
}

fn s<'a>(v: &'a Value, key: &str) -> Option<&'a str> {
    v.get(key).and_then(Value::as_str)
}

/// Read the common IDs from a payload.
pub fn hook_context(hook: &HookPayload) -> HookContext {
    let b = &hook.body;
    HookContext {
        event: s(b, "hook_event_name").unwrap_or(&hook.hook).to_owned(),
        session_id: s(b, "session_id").map(|id| HarnessSessionId(id.to_owned())),
        session_file: s(b, "session_file").map(PathBuf::from),
        cwd: s(b, "cwd").map(PathBuf::from),
        model: s(b, "model").map(str::to_owned),
        ts: b.get("ts").and_then(Value::as_u64),
    }
}

const SUMMARY_MAX: usize = 120;

fn one_line(text: &str) -> String {
    let first = text
        .lines()
        .find(|l| !l.trim().is_empty())
        .unwrap_or("")
        .trim();
    let mut out: String = first.chars().take(SUMMARY_MAX).collect();
    if first.chars().count() > SUMMARY_MAX || text.trim().lines().count() > 1 {
        out.push('…');
    }
    out
}

/// One short line that says what a tool call does, for the dashboard.
/// pi's built-in tools: `bash` (`command`), `read`, `edit`, `write`, `ls`
/// (`path`), `grep`, `find` (`pattern`).
pub fn tool_summary(tool: &str, input: &Value) -> Option<String> {
    let keys: &[&str] = match tool {
        "bash" | "powershell" => &["command"],
        "read" | "edit" | "write" | "ls" => &["path", "file_path"],
        "grep" | "find" => &["pattern", "path"],
        _ => &["command", "path", "file_path", "pattern", "url", "query"],
    };
    keys.iter()
        .chain(["description"].iter())
        .find_map(|k| s(input, k))
        .map(one_line)
        .filter(|l| !l.is_empty())
}

/// Map one hook payload to events.
pub fn hook_events(hook: &HookPayload) -> Vec<Event> {
    let ctx = hook_context(hook);
    let b = &hook.body;
    let tool = || s(b, "tool_name").unwrap_or("unknown").to_owned();
    let tool_id = || s(b, "tool_call_id").map(str::to_owned);
    match ctx.event.as_str() {
        "session_start" => ctx
            .session_id
            .map(|id| Event::SessionStarted {
                harness_session: id,
                transcript_path: ctx.session_file,
                source: s(b, "reason").map(str::to_owned),
            })
            .into_iter()
            .collect(),
        "input" => vec![Event::TurnStart {
            prompt: s(b, "prompt").map(str::to_owned),
        }],
        "agent_settled" => vec![Event::TurnEnd],
        "tool_execution_start" => {
            let input = b.get("tool_input").cloned().unwrap_or(Value::Null);
            vec![Event::ToolUse {
                summary: tool_summary(&tool(), &input),
                tool: tool(),
                input,
                id: tool_id(),
            }]
        }
        "tool_execution_end" => vec![Event::ToolFinished {
            tool: tool(),
            id: tool_id(),
            summary: None,
            ok: !b.get("is_error").and_then(Value::as_bool).unwrap_or(false),
        }],
        "ui_prompt_start" => {
            let kind = s(b, "kind").unwrap_or("prompt");
            let question = s(b, "title")
                .map(one_line)
                .filter(|t| !t.is_empty())
                .unwrap_or_else(|| format!("pi asks ({kind})"));
            // The event has no ID. The session and the time make one.
            let request_id = format!(
                "{}:{}",
                ctx.session_id.map(|id| id.0).unwrap_or_default(),
                ctx.ts.unwrap_or_default()
            );
            vec![Event::Question {
                request_id,
                tool: Some(format!("ui.{kind}")),
                question,
                // The event does not carry the options. The screen shows them.
                options: Vec::new(),
            }]
        }
        "message_end" if s(b, "stop_reason") == Some("error") => vec![Event::System {
            message: format!(
                "pi: the model call failed{}",
                s(b, "error_message")
                    .map(|e| format!(": {}", one_line(e)))
                    .unwrap_or_default()
            ),
        }],
        "session_before_compact" => vec![Event::CompactStarted {
            trigger: s(b, "reason").map(str::to_owned),
        }],
        "session_compact" => vec![
            Event::System {
                message: format!(
                    "pi: the context was compacted{}",
                    s(b, "reason")
                        .map(|r| format!(" ({r})"))
                        .unwrap_or_default()
                ),
            },
            Event::CompactFinished {
                ok: true,
                trigger: s(b, "reason").map(str::to_owned),
                message: None,
            },
        ],
        "session_compact_failed" => vec![Event::CompactFinished {
            ok: false,
            trigger: s(b, "reason").map(str::to_owned),
            message: Some(
                s(b, "error_message")
                    .map(one_line)
                    .unwrap_or_else(|| "the compaction failed".into()),
            ),
        }],
        "session_shutdown" => vec![Event::SessionEnded {
            reason: s(b, "reason").map(str::to_owned),
        }],
        _ => Vec::new(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    use terrarium_core::HarnessId;

    /// Read a fixture in the runner's hook message shape.
    pub(crate) fn captured(file: &str) -> Vec<HookPayload> {
        let path = format!("{}/tests/fixtures/hooks/{file}", env!("CARGO_MANIFEST_DIR"));
        std::fs::read_to_string(&path)
            .expect("fixture")
            .lines()
            .filter(|l| !l.trim().is_empty())
            .map(|l| {
                let v: Value = serde_json::from_str(l).expect("json line");
                assert_eq!(v["type"], "hook");
                HookPayload {
                    harness: HarnessId::Pi,
                    hook: v["hook"].as_str().expect("hook").to_owned(),
                    body: v["payload"].clone(),
                }
            })
            .collect()
    }

    fn hook(name: &str, body: Value) -> HookPayload {
        HookPayload {
            harness: HarnessId::Pi,
            hook: name.into(),
            body,
        }
    }

    #[test]
    fn every_captured_payload_has_the_ids() {
        for file in [
            "one-turn.jsonl",
            "typing.jsonl",
            "resume.jsonl",
            "ui-prompts.jsonl",
        ] {
            for h in captured(file) {
                let ctx = hook_context(&h);
                assert_eq!(ctx.event, h.hook, "{file}");
                assert!(ctx.session_id.is_some(), "{file} {}", h.hook);
                assert!(ctx.cwd.is_some(), "{file} {}", h.hook);
                assert!(ctx.ts.is_some(), "{file} {}", h.hook);
                let sf = ctx.session_file.expect("session_file");
                let id = ctx.session_id.expect("id").0;
                assert!(sf.to_string_lossy().ends_with(&format!("_{id}.jsonl")));
                assert!(ctx.model.expect("model").contains('/'));
            }
        }
    }

    #[test]
    fn one_turn_maps_in_order() {
        let events: Vec<Event> = captured("one-turn.jsonl")
            .iter()
            .flat_map(hook_events)
            .collect();
        let sid = "01a16ccf-8b68-73eb-9298-a210157d8f44";
        let call = "call_6de0a0ccc01440d8892b2490df21119f|fc_0f6ff4a2166ca5c0016adc5e1b88d88191be6fefd858b532fc";
        assert_eq!(
            events,
            [
                Event::SessionStarted {
                    harness_session: HarnessSessionId(sid.into()),
                    transcript_path: Some(PathBuf::from(format!(
                        "$HARNESS_HOME/sessions/<encoded-cwd>--/2026-10-24T07-28-23-401Z_{sid}.jsonl"
                    ))),
                    source: Some("startup".into()),
                },
                Event::TurnStart {
                    prompt: Some(
                        "Use the bash tool to run: echo terrarium-hook-test. Then reply with exactly: done"
                            .into()
                    ),
                },
                Event::ToolUse {
                    tool: "bash".into(),
                    input: json!({"command": "echo terrarium-hook-test"}),
                    id: Some(call.into()),
                    summary: Some("echo terrarium-hook-test".into()),
                },
                Event::ToolFinished {
                    tool: "bash".into(),
                    id: Some(call.into()),
                    summary: None,
                    ok: true,
                },
                Event::TurnEnd,
                Event::SessionEnded {
                    reason: Some("quit".into()),
                },
            ]
        );
    }

    #[test]
    fn steer_while_busy_gives_more_inputs_than_settles() {
        let events: Vec<Event> = captured("typing.jsonl")
            .iter()
            .flat_map(hook_events)
            .collect();
        let starts = events
            .iter()
            .filter(|e| matches!(e, Event::TurnStart { .. }))
            .count();
        let ends = events
            .iter()
            .filter(|e| matches!(e, Event::TurnEnd))
            .count();
        assert_eq!((starts, ends), (3, 2));
    }

    #[test]
    fn resume_keeps_the_session_id() {
        let ids: Vec<String> = captured("resume.jsonl")
            .iter()
            .flat_map(hook_events)
            .filter_map(|e| match e {
                Event::SessionStarted {
                    harness_session, ..
                } => Some(harness_session.0),
                _ => None,
            })
            .collect();
        assert_eq!(ids.len(), 6);
        assert!(
            ids.iter()
                .all(|i| i == "0aa4e42a-0404-423c-a41c-332c83e48e25")
        );
    }

    #[test]
    fn ui_prompts_are_questions_and_no_run_settles() {
        let events: Vec<Event> = captured("ui-prompts.jsonl")
            .iter()
            .flat_map(hook_events)
            .collect();
        let sid = "a6acb784-5d88-4161-8792-90143365c46a";
        assert_eq!(
            &events[1..3],
            [
                Event::Question {
                    request_id: format!("{sid}:1792828149674"),
                    tool: Some("ui.select".into()),
                    question: "Pick a colour".into(),
                    options: vec![],
                },
                Event::Question {
                    request_id: format!("{sid}:1792828152325"),
                    tool: Some("ui.confirm".into()),
                    question: "Really?".into(),
                    options: vec![],
                },
            ]
        );
        // "No API key found": `input`, then the extension's synthetic
        // `agent_settled`.
        assert_eq!(
            &events[3..5],
            [
                Event::TurnStart {
                    prompt: Some("Reply with exactly: pong".into())
                },
                Event::TurnEnd
            ]
        );
        assert!(matches!(
            events.last(),
            Some(Event::SessionEnded { reason: Some(r) }) if r == "quit"
        ));
        // The session file is in the agent's own session directory.
        assert!(matches!(
            &events[0],
            Event::SessionStarted { transcript_path: Some(p), .. }
                if p.starts_with("$AGENT_DIR/pi/sessions")
        ));
    }

    #[test]
    fn errors_compaction_and_unknown_events() {
        assert_eq!(
            hook_events(&hook(
                "message_end",
                json!({"stop_reason": "error", "error_message": "Connection error.\nmore"})
            )),
            [Event::System {
                message: "pi: the model call failed: Connection error.…".into()
            }]
        );
        assert!(hook_events(&hook("message_end", json!({"stop_reason": "stop"}))).is_empty());
        assert!(matches!(
            &hook_events(&hook("session_compact", json!({"reason": "threshold"})))[..],
            [Event::System { message }, Event::CompactFinished { ok: true, trigger: Some(t), .. }]
                if message.ends_with("(threshold)") && t == "threshold"
        ));
        // The extension events of the 1.4.0 bundle
        // (`dist/core/extensions/types.d.ts`), as `ext/terrarium.ts`
        // forwards them.
        assert_eq!(
            hook_events(&hook("session_before_compact", json!({"reason": "manual"}))),
            [Event::CompactStarted {
                trigger: Some("manual".into())
            }]
        );
        assert_eq!(
            hook_events(&hook(
                "session_compact_failed",
                json!({"reason": "manual", "error_message": "Nothing to compact (session too small)",
                       "aborted": false})
            )),
            [Event::CompactFinished {
                ok: false,
                trigger: Some("manual".into()),
                message: Some("Nothing to compact (session too small)".into())
            }]
        );
        assert!(matches!(
            &hook_events(&hook(
                "tool_execution_end",
                json!({"tool_name": "bash", "is_error": true})
            ))[..],
            [Event::ToolFinished { ok: false, .. }]
        ));
        for name in [
            "agent_start",
            "agent_end",
            "turn_start",
            "turn_end",
            "before_agent_start",
            "ui_prompt_end",
            "model_select",
            "project_trust",
        ] {
            assert!(hook_events(&hook(name, json!({}))).is_empty(), "{name}");
        }
        assert!(hook_events(&hook("session_start", json!({}))).is_empty());
        // The payload name wins over the message name.
        assert_eq!(
            hook_events(&hook("x", json!({"hook_event_name": "agent_settled"}))),
            [Event::TurnEnd]
        );
        assert_eq!(
            hook_events(&hook("ui_prompt_start", json!({"kind": "input"}))),
            [Event::Question {
                request_id: ":0".into(),
                tool: Some("ui.input".into()),
                question: "pi asks (input)".into(),
                options: vec![],
            }]
        );
    }

    #[test]
    fn summaries() {
        assert_eq!(
            tool_summary("edit", &json!({"path": "/w/a.rs"})).as_deref(),
            Some("/w/a.rs")
        );
        assert_eq!(
            tool_summary("grep", &json!({"pattern": "fn main"})).as_deref(),
            Some("fn main")
        );
        assert_eq!(tool_summary("mystery", &json!({})), None);
        let long = "x".repeat(200);
        let s = tool_summary("bash", &json!({"command": long})).expect("summary");
        assert_eq!(s.chars().count(), SUMMARY_MAX + 1);
    }
}
