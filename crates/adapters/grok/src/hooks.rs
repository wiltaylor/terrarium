//! Map Grok Build hook payloads to Terrarium events.
//!
//! The payload is JSON on stdin (docs.x.ai, "Hooks"). Every event has
//! `hookEventName`, `sessionId`, `cwd`, and `workspaceRoot`; the tool
//! events add `toolName` and `toolInput`. The other fields (the prompt of
//! `UserPromptSubmit`, the error of `StopFailure`, a tool call ID) are not
//! documented: the mapper reads the camelCase name first, then the Claude
//! Code name, and works without them. Grok also reads Claude Code hook
//! files, so a snake_case payload is possible. The signed-in capture
//! (TER-1192, release gate) records the real fields.
//!
//! - `UserPromptSubmit` opens a turn and `Stop` closes it. `StopFailure`
//!   closes it too, with a notice.
//! - `PermissionDenied` is a denial, NOT a permission request: it is a
//!   notice, never a question.
//! - `Notification` is a notice. A question shows on the screen.

use serde_json::Value;
use terrarium_core::{Event, HarnessSessionId, HookPayload};

fn s<'a>(v: &'a Value, keys: &[&str]) -> Option<&'a str> {
    keys.iter()
        .find_map(|k| v.get(*k).and_then(Value::as_str))
        .filter(|t| !t.is_empty())
}

/// The event name: `hookEventName`, else the name from
/// `terrarium hook <name>`.
pub fn event_name(hook: &HookPayload) -> &str {
    s(&hook.body, &["hookEventName", "hook_event_name"]).unwrap_or(&hook.hook)
}

/// The session ID of a payload.
pub fn session_id(hook: &HookPayload) -> Option<HarnessSessionId> {
    s(&hook.body, &["sessionId", "session_id"]).map(|id| HarnessSessionId(id.to_owned()))
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

/// One short line that says what a tool call does: the first string of
/// the usual input keys.
pub fn tool_summary(input: &Value) -> Option<String> {
    s(
        input,
        &[
            "command",
            "file_path",
            "filePath",
            "path",
            "pattern",
            "url",
            "query",
            "description",
        ],
    )
    .map(one_line)
    .filter(|l| !l.is_empty())
}

fn tool(b: &Value) -> String {
    s(b, &["toolName", "tool_name"])
        .unwrap_or("unknown")
        .to_owned()
}

fn tool_input(b: &Value) -> Value {
    b.get("toolInput")
        .or_else(|| b.get("tool_input"))
        .cloned()
        .unwrap_or(Value::Null)
}

fn tool_id(b: &Value) -> Option<String> {
    s(b, &["toolUseId", "toolCallId", "tool_use_id"]).map(str::to_owned)
}

/// Map one hook payload to events.
pub fn hook_events(hook: &HookPayload) -> Vec<Event> {
    let b = &hook.body;
    match event_name(hook) {
        "SessionStart" => session_id(hook)
            .map(|id| Event::SessionStarted {
                harness_session: id,
                transcript_path: None,
                source: s(b, &["source"]).map(str::to_owned),
            })
            .into_iter()
            .collect(),
        "UserPromptSubmit" => vec![Event::TurnStart {
            prompt: s(b, &["prompt", "userPrompt", "user_prompt"]).map(str::to_owned),
        }],
        "Stop" => vec![Event::TurnEnd],
        "StopFailure" => vec![
            Event::System {
                message: format!(
                    "grok: the turn ended with an error{}",
                    s(b, &["error", "reason", "message"])
                        .map(|e| format!(": {}", one_line(e)))
                        .unwrap_or_default()
                ),
            },
            Event::TurnEnd,
        ],
        "PreToolUse" => {
            let input = tool_input(b);
            vec![Event::ToolUse {
                summary: tool_summary(&input),
                tool: tool(b),
                input,
                id: tool_id(b),
            }]
        }
        name @ ("PostToolUse" | "PostToolUseFailure") => vec![Event::ToolFinished {
            summary: tool_summary(&tool_input(b)),
            tool: tool(b),
            id: tool_id(b),
            ok: name == "PostToolUse",
        }],
        "PermissionDenied" => vec![Event::Notification {
            message: format!("grok denied the tool call {}", tool(b)),
            question: false,
        }],
        "Notification" => vec![Event::Notification {
            message: s(b, &["message", "title"]).unwrap_or_default().to_owned(),
            question: false,
        }],
        "SessionEnd" => vec![Event::SessionEnded {
            reason: s(b, &["reason"]).map(str::to_owned),
        }],
        "PreCompact" => vec![Event::CompactStarted {
            trigger: s(b, &["trigger"]).map(str::to_owned),
        }],
        "PostCompact" => vec![Event::CompactFinished {
            ok: true,
            trigger: s(b, &["trigger"]).map(str::to_owned),
            message: None,
        }],
        _ => Vec::new(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::Path;
    use terrarium_core::HarnessId;

    /// A fixture of `tests/fixtures/hooks/`. The fixtures follow the
    /// documented fields; they are not captured from a signed-in grok.
    fn fixture(name: &str) -> HookPayload {
        let path = Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("tests/fixtures/hooks")
            .join(format!("{name}.json"));
        let body: Value =
            serde_json::from_str(&std::fs::read_to_string(&path).expect("fixture")).expect("json");
        HookPayload {
            harness: HarnessId::Grok,
            hook: name.into(),
            body,
        }
    }

    #[test]
    fn the_turn_hooks_open_and_close_a_turn() {
        assert_eq!(
            hook_events(&fixture("SessionStart")),
            [Event::SessionStarted {
                harness_session: HarnessSessionId("0f8e4c2a-7b1d-4e3f-9a6c-5d2b8e1f4a7c".into()),
                transcript_path: None,
                source: Some("startup".into()),
            }]
        );
        assert_eq!(
            hook_events(&fixture("UserPromptSubmit")),
            [Event::TurnStart {
                prompt: Some("fix the bug".into())
            }]
        );
        assert_eq!(hook_events(&fixture("Stop")), [Event::TurnEnd]);
        let failed = hook_events(&fixture("StopFailure"));
        assert_eq!(failed.len(), 2);
        assert!(
            matches!(&failed[0], Event::System { message } if message.contains("rate limited"))
        );
        assert_eq!(failed[1], Event::TurnEnd);
        assert_eq!(
            hook_events(&fixture("SessionEnd")),
            [Event::SessionEnded {
                reason: Some("exit".into())
            }]
        );
    }

    #[test]
    fn tool_hooks_carry_the_tool() {
        let ev = hook_events(&fixture("PreToolUse"));
        let [
            Event::ToolUse {
                tool,
                summary,
                input,
                ..
            },
        ] = ev.as_slice()
        else {
            panic!("{ev:?}");
        };
        assert_eq!(tool, "Bash");
        assert_eq!(summary.as_deref(), Some("cargo test"));
        assert_eq!(input["command"], "cargo test");
        assert!(matches!(
            hook_events(&fixture("PostToolUse")).as_slice(),
            [Event::ToolFinished { ok: true, .. }]
        ));
        assert!(matches!(
            hook_events(&fixture("PostToolUseFailure")).as_slice(),
            [Event::ToolFinished { ok: false, .. }]
        ));
    }

    /// `PermissionDenied` and `Notification` never block the agent: a
    /// question is read from the screen.
    #[test]
    fn denials_and_notifications_are_notices() {
        for name in ["PermissionDenied", "Notification"] {
            let ev = hook_events(&fixture(name));
            assert!(
                matches!(ev.as_slice(), [Event::Notification { question: false, .. }]),
                "{name}: {ev:?}"
            );
        }
    }

    /// The name from `terrarium hook <name>` counts when the body has
    /// none, and the Claude Code field names work too.
    #[test]
    fn the_cli_name_and_snake_case_fields_work() {
        let hook = HookPayload {
            harness: HarnessId::Grok,
            hook: "SessionStart".into(),
            body: serde_json::json!({"session_id": "s1"}),
        };
        assert!(matches!(
            hook_events(&hook).as_slice(),
            [Event::SessionStarted { harness_session, .. }] if harness_session.0 == "s1"
        ));
        let unknown = HookPayload {
            harness: HarnessId::Grok,
            hook: "SubagentStart".into(),
            body: serde_json::json!({"hookEventName": "SubagentStart"}),
        };
        assert!(hook_events(&unknown).is_empty());
        // No session ID: no event, no panic.
        let bare = HookPayload {
            harness: HarnessId::Grok,
            hook: "SessionStart".into(),
            body: serde_json::json!({}),
        };
        assert!(hook_events(&bare).is_empty());
    }
}
