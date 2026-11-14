//! Map Grok Build hook payloads to Terrarium events.
//!
//! The payload is JSON on stdin (docs.x.ai, "Hooks"). Grok 1.0.77 sends
//! every field twice, camelCase and the Claude Code snake_case name
//! (signed-in capture, TER-1312, `tests/fixtures/hooks/captured/`).
//! The event name differs between the two: `hookEventName` is snake_case
//! (`pre_tool_use`) and `hook_event_name` is PascalCase (`PreToolUse`).
//! [`event_name`] takes the PascalCase name and turns a snake_case one
//! into it. Every event has `sessionId`, `cwd`, `workspaceRoot`,
//! `timestamp`, `permissionMode`, and from the first turn
//! `transcriptPath`; the tool events add `toolName`, `toolUseId`,
//! `toolInput`, and `toolResult` after the call.
//!
//! - `UserPromptSubmit` opens a turn and `Stop` closes it (`reason`
//!   `end_turn`). `StopFailure` closes it too, with a notice. A turn
//!   cancelled with Ctrl+C sends NO `Stop`: the runner ends it from the
//!   screen. `/quit` sends `SessionEnd` and then a `Stop` with `reason`
//!   `shutdown`.
//! - `PermissionDenied` is a denial, NOT a permission request: it is a
//!   notice, never a question.
//! - `Notification` is a notice, except `notificationType`
//!   `permission_prompt`: grok shows a permission question and waits.
//!   `idle_prompt` comes about a minute after a turn ends without a
//!   reply.

use std::borrow::Cow;

use serde_json::Value;
use terrarium_core::{Event, HarnessSessionId, HookPayload};

fn s<'a>(v: &'a Value, keys: &[&str]) -> Option<&'a str> {
    keys.iter()
        .find_map(|k| v.get(*k).and_then(Value::as_str))
        .filter(|t| !t.is_empty())
}

/// The event name in PascalCase: `hook_event_name`, else `hookEventName`
/// (snake_case in grok 1.0.77, so `pre_tool_use` becomes `PreToolUse`),
/// else the name from `terrarium hook <name>`.
pub fn event_name(hook: &HookPayload) -> Cow<'_, str> {
    if let Some(name) = s(&hook.body, &["hook_event_name"]) {
        return Cow::Borrowed(name);
    }
    match s(&hook.body, &["hookEventName"]) {
        Some(name) if name.contains('_') || name.starts_with(|c: char| c.is_ascii_lowercase()) => {
            Cow::Owned(pascal_case(name))
        }
        Some(name) => Cow::Borrowed(name),
        None => Cow::Borrowed(&hook.hook),
    }
}

/// `pre_tool_use` to `PreToolUse`.
fn pascal_case(name: &str) -> String {
    name.split('_')
        .map(|part| {
            let mut c = part.chars();
            c.next()
                .map(|f| f.to_ascii_uppercase().to_string() + c.as_str())
                .unwrap_or_default()
        })
        .collect()
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
    match event_name(hook).as_ref() {
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
            question: s(b, &["notificationType", "notification_type"]) == Some("permission_prompt"),
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
                matches!(
                    ev.as_slice(),
                    [Event::Notification {
                        question: false,
                        ..
                    }]
                ),
                "{name}: {ev:?}"
            );
        }
    }

    /// The payloads of the signed-in grok 1.0.77 check (TER-1312), in
    /// order: a new session with three turns (a read-only command, a
    /// `touch`, a write that asked for permission), a turn cancelled with
    /// Ctrl+C, `/quit`, then `--resume` with one turn and `/quit`. The
    /// hook name passed to the mapper is a dummy, so the body alone must
    /// name the event.
    fn captured() -> Vec<(String, Vec<Event>)> {
        let dir = Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/hooks/captured");
        let mut files: Vec<_> = std::fs::read_dir(&dir)
            .expect("captured dir")
            .map(|e| e.expect("entry").path())
            .filter(|p| p.extension().is_some_and(|e| e == "json"))
            .collect();
        files.sort();
        files
            .iter()
            .map(|path| {
                let body: Value =
                    serde_json::from_str(&std::fs::read_to_string(path).expect("fixture"))
                        .expect("json");
                let hook = HookPayload {
                    harness: HarnessId::Grok,
                    hook: "unknown".into(),
                    body,
                };
                (event_name(&hook).into_owned(), hook_events(&hook))
            })
            .collect()
    }

    #[test]
    fn the_captured_event_names_are_snake_case_and_map() {
        let names: Vec<String> = captured().into_iter().map(|(n, _)| n).collect();
        assert_eq!(
            names,
            [
                "SessionStart",
                "UserPromptSubmit",
                "PreToolUse",
                "PostToolUse",
                "Stop",
                "UserPromptSubmit",
                "PreToolUse",
                "PostToolUse",
                "Stop",
                "UserPromptSubmit",
                "PreToolUse",
                "Notification",
                "PostToolUse",
                "Stop",
                "UserPromptSubmit",
                "Notification",
                "SessionEnd",
                "Stop",
                "SessionStart",
                "UserPromptSubmit",
                "Stop",
                "SessionEnd",
                "Stop",
            ]
        );
        // Only the camelCase name: snake_case, made PascalCase.
        let hook = HookPayload {
            harness: HarnessId::Grok,
            hook: "x".into(),
            body: serde_json::json!({"hookEventName": "post_tool_use_failure"}),
        };
        assert_eq!(event_name(&hook), "PostToolUseFailure");
    }

    #[test]
    fn the_captured_session_maps_to_turns_tools_and_one_question() {
        let all = captured();
        let session = HarnessSessionId("01a1d8b7-fce7-7fd1-a82b-ea116c408859".into());
        assert_eq!(
            all[0].1,
            [Event::SessionStarted {
                harness_session: session.clone(),
                transcript_path: None,
                source: Some("new".into()),
            }]
        );
        // A resume names the same session, with `source` `load`.
        assert_eq!(
            all[18].1,
            [Event::SessionStarted {
                harness_session: session,
                transcript_path: None,
                source: Some("load".into()),
            }]
        );
        assert_eq!(
            all[1].1,
            [Event::TurnStart {
                prompt: Some(
                    "Run the shell command: wc -c note.txt   and tell me the number.".into()
                )
            }]
        );
        let [
            Event::ToolUse {
                tool, summary, id, ..
            },
        ] = all[2].1.as_slice()
        else {
            panic!("{:?}", all[2].1);
        };
        assert_eq!(tool, "run_terminal_command");
        assert_eq!(summary.as_deref(), Some("wc -c note.txt"));
        assert_eq!(
            id.as_deref(),
            Some("call-80d1d45a-3661-414b-a4d8-823e883a8471-0")
        );
        assert!(matches!(
            all[3].1.as_slice(),
            [Event::ToolFinished {
                ok: true,
                id: Some(_),
                ..
            }]
        ));
        assert_eq!(all[4].1, [Event::TurnEnd]);
        // The permission question blocks; the later idle prompt does not.
        assert_eq!(
            all[11].1,
            [Event::Notification {
                message: "Tool permission requested".into(),
                question: true,
            }]
        );
        assert_eq!(
            all[15].1,
            [Event::Notification {
                message: "Waiting for your next prompt".into(),
                question: false,
            }]
        );
        // The Ctrl+C turn (index 14) has no Stop before the idle prompt.
        assert!(matches!(all[14].1.as_slice(), [Event::TurnStart { .. }]));
        assert_eq!(
            all[16].1,
            [Event::SessionEnded {
                reason: Some("shutdown".into())
            }]
        );
        // Each event of the capture maps to at least one event.
        for (i, (name, events)) in all.iter().enumerate() {
            assert!(!events.is_empty(), "{i} {name}");
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
