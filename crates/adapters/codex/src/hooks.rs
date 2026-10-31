//! Map Codex hook payloads to Terrarium events (PLAN §7).
//!
//! The hook format and the payload fields are the Claude Code ones. Rules
//! from `docs/compat.md` ("Codex 0.160.0", "Hooks"):
//!
//! - `SessionStart` fires at the first prompt, just before
//!   `UserPromptSubmit`, not at start-up. It carries the session ID and the
//!   session file (`transcript_path`). `source` is `startup` or `resume`.
//! - `UserPromptSubmit` opens a turn and `Stop` closes it. Text typed while
//!   a turn runs steers that turn: the same `turn_id`, one `Stop` for both
//!   prompts. Do not count `Stop` events per delivery.
//! - `PostToolUse.tool_response` is a string (the output). The payload has
//!   no error flag, so a finished tool is always `ok`.
//! - There is no `Notification` event, and `PermissionRequest` did not fire
//!   in yolo mode. Codex gives no hook for "needs input": the runner finds
//!   a question by the screen ([`crate::screen`], `dialog`).
//! - `SessionEnd.reason` is `other` after `/exit`.

use std::path::PathBuf;

use serde_json::Value;
use terrarium_core::{Event, HarnessSessionId, HookPayload};

/// The IDs that every Codex hook payload carries.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct HookContext {
    /// `hook_event_name`, else the name from `terrarium hook <name>`.
    pub event: String,
    pub session_id: Option<HarnessSessionId>,
    pub cwd: Option<PathBuf>,
    pub transcript_path: Option<PathBuf>,
    /// Turn hooks only. The same for every hook of one turn, and for a
    /// prompt that steers a running turn.
    pub turn_id: Option<String>,
    /// Not in `SessionEnd`.
    pub model: Option<String>,
    /// `bypassPermissions` in yolo mode. Not in `SessionEnd`.
    pub permission_mode: Option<String>,
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
        cwd: s(b, "cwd").map(PathBuf::from),
        transcript_path: s(b, "transcript_path").map(PathBuf::from),
        turn_id: s(b, "turn_id").map(str::to_owned),
        model: s(b, "model").map(str::to_owned),
        permission_mode: s(b, "permission_mode").map(str::to_owned),
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
/// Codex 0.160.0 names its shell tool `Bash` with `tool_input.command`
/// (captured). The other keys are guesses for tools the spikes did not
/// call.
pub fn tool_summary(input: &Value) -> Option<String> {
    [
        "command",
        "file_path",
        "path",
        "pattern",
        "url",
        "query",
        "description",
    ]
    .iter()
    .find_map(|k| s(input, k))
    .map(one_line)
    .filter(|l| !l.is_empty())
}

/// Map one hook payload to events.
pub fn hook_events(hook: &HookPayload) -> Vec<Event> {
    let ctx = hook_context(hook);
    let b = &hook.body;
    let tool = || s(b, "tool_name").unwrap_or("unknown").to_owned();
    let input = || b.get("tool_input").cloned().unwrap_or(Value::Null);
    let tool_id = || s(b, "tool_use_id").map(str::to_owned);
    match ctx.event.as_str() {
        "SessionStart" => ctx
            .session_id
            .map(|id| Event::SessionStarted {
                harness_session: id,
                transcript_path: ctx.transcript_path,
                source: s(b, "source").map(str::to_owned),
            })
            .into_iter()
            .collect(),
        "UserPromptSubmit" => vec![Event::TurnStart {
            prompt: s(b, "prompt").map(str::to_owned),
        }],
        "Stop" => vec![Event::TurnEnd],
        "PreToolUse" => {
            let input = input();
            vec![Event::ToolUse {
                summary: tool_summary(&input),
                tool: tool(),
                input,
                id: tool_id(),
            }]
        }
        "PostToolUse" => vec![Event::ToolFinished {
            summary: tool_summary(&input()),
            tool: tool(),
            id: tool_id(),
            ok: true,
        }],
        // Not captured: it did not fire in yolo mode. The shape is assumed
        // to be the Claude Code one (`tool_name`, `tool_input`).
        "PermissionRequest" => {
            let tool = tool();
            let question = match tool_summary(&input()) {
                Some(sum) => format!("Allow {tool}: {sum}?"),
                None => format!("Allow {tool}?"),
            };
            let request_id = tool_id()
                .or(ctx.turn_id)
                .or_else(|| ctx.session_id.map(|id| id.0))
                .unwrap_or_else(|| "unknown".into());
            vec![Event::Question {
                request_id,
                tool: Some(tool),
                question,
                options: Vec::new(),
            }]
        }
        "SessionEnd" => vec![Event::SessionEnded {
            reason: s(b, "reason").map(str::to_owned),
        }],
        // A compaction: `/compact` (`trigger` `manual`) or a full context
        // (`auto`). The payloads have no error field (docs/compat.md,
        // "Compaction"): a failure is a missing PostCompact.
        "PreCompact" => vec![Event::CompactStarted {
            trigger: s(b, "trigger").map(str::to_owned),
        }],
        "PostCompact" => vec![Event::CompactFinished {
            ok: true,
            trigger: s(b, "trigger").map(str::to_owned),
            message: None,
        }],
        // SubagentStart, SubagentStop: not captured, and no status change.
        // `SubagentStop` must not close the turn of the main thread.
        _ => Vec::new(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    use terrarium_core::HarnessId;

    fn captured(file: &str) -> Vec<HookPayload> {
        let path = format!("{}/tests/fixtures/hooks/{file}", env!("CARGO_MANIFEST_DIR"));
        std::fs::read_to_string(&path)
            .expect("fixture")
            .lines()
            .filter(|l| !l.trim().is_empty())
            .map(|l| {
                let v: Value = serde_json::from_str(l).expect("json line");
                HookPayload {
                    harness: HarnessId::Codex,
                    hook: v["hook"].as_str().expect("hook").to_owned(),
                    body: v["payload"].clone(),
                }
            })
            .collect()
    }

    fn hook(name: &str, body: Value) -> HookPayload {
        HookPayload {
            harness: HarnessId::Codex,
            hook: name.into(),
            body,
        }
    }

    #[test]
    fn every_captured_payload_has_the_ids() {
        for file in ["one-turn.jsonl", "typing.jsonl", "resume.jsonl"] {
            for h in captured(file) {
                let ctx = hook_context(&h);
                assert_eq!(ctx.event, h.hook);
                assert!(ctx.session_id.is_some(), "{}", h.hook);
                assert_eq!(ctx.cwd.as_deref(), Some(std::path::Path::new("$CWD")));
                let tp = ctx.transcript_path.expect("transcript_path");
                let name = tp.file_name().expect("name").to_string_lossy().into_owned();
                assert!(name.starts_with("rollout-") && name.ends_with(".jsonl"));
                if h.hook == "SessionEnd" {
                    assert!(ctx.model.is_none() && ctx.permission_mode.is_none());
                } else {
                    assert_eq!(ctx.permission_mode.as_deref(), Some("bypassPermissions"));
                }
                let turn_hook = !matches!(h.hook.as_str(), "SessionStart" | "SessionEnd");
                assert_eq!(ctx.turn_id.is_some(), turn_hook, "{}", h.hook);
            }
        }
    }

    #[test]
    fn one_turn_maps_in_order() {
        let events: Vec<Event> = captured("one-turn.jsonl")
            .iter()
            .flat_map(hook_events)
            .collect();
        let sid = "01a16cb0-f46c-7ea2-a000-55803cbcdad5";
        let tool_id = "exec-d52e759a-0e79-4de9-8c0a-6ba61a3a57a0";
        assert_eq!(
            events,
            [
                Event::SessionStarted {
                    harness_session: HarnessSessionId(sid.into()),
                    transcript_path: Some(PathBuf::from(format!(
                        "$HARNESS_HOME/sessions/2026/10/24/rollout-2026-10-24T16-54-58-{sid}.jsonl"
                    ))),
                    source: Some("startup".into()),
                },
                Event::TurnStart {
                    prompt: Some(
                        "Run the shell command `echo terrarium-hook-test`. Then reply with exactly: done"
                            .into()
                    ),
                },
                Event::ToolUse {
                    tool: "Bash".into(),
                    input: json!({"command": "echo terrarium-hook-test"}),
                    id: Some(tool_id.into()),
                    summary: Some("echo terrarium-hook-test".into()),
                },
                Event::ToolFinished {
                    tool: "Bash".into(),
                    id: Some(tool_id.into()),
                    summary: Some("echo terrarium-hook-test".into()),
                    ok: true,
                },
                Event::TurnEnd,
                Event::SessionEnded {
                    reason: Some("other".into()),
                },
            ]
        );
    }

    #[test]
    fn resume_reports_the_same_session() {
        let starts: Vec<Event> = captured("resume.jsonl")
            .iter()
            .flat_map(hook_events)
            .filter(|e| matches!(e, Event::SessionStarted { .. }))
            .collect();
        let sid = HarnessSessionId("01a16cb2-0d95-7b40-894d-e19fc1b66c24".into());
        let sources: Vec<(HarnessSessionId, Option<String>)> = starts
            .into_iter()
            .map(|e| match e {
                Event::SessionStarted {
                    harness_session,
                    source,
                    ..
                } => (harness_session, source),
                _ => unreachable!(),
            })
            .collect();
        assert_eq!(
            sources,
            [
                (sid.clone(), Some("startup".into())),
                (sid, Some("resume".into()))
            ]
        );
    }

    #[test]
    fn steered_prompt_gives_more_submits_than_stops() {
        let hooks = captured("typing.jsonl");
        let events: Vec<Event> = hooks.iter().flat_map(hook_events).collect();
        let starts = events
            .iter()
            .filter(|e| matches!(e, Event::TurnStart { .. }))
            .count();
        let ends = events
            .iter()
            .filter(|e| matches!(e, Event::TurnEnd))
            .count();
        assert_eq!(starts, ends + 1, "{events:?}");
        // The steered prompt has the turn ID of the running turn.
        let last_two: Vec<String> = hooks
            .iter()
            .filter(|h| h.hook == "UserPromptSubmit")
            .rev()
            .take(2)
            .filter_map(|h| hook_context(h).turn_id)
            .collect();
        assert_eq!(last_two.len(), 2);
        assert_eq!(last_two[0], last_two[1]);
    }

    #[test]
    fn other_hooks() {
        assert!(matches!(
            &hook_events(&hook(
                "PermissionRequest",
                json!({"session_id": "s", "turn_id": "t", "tool_name": "Bash",
                       "tool_input": {"command": "rm -rf build\necho done"}})
            ))[..],
            [Event::Question { request_id, question, .. }]
                if request_id == "t" && question == "Allow Bash: rm -rf build…?"
        ));
        for name in ["SubagentStart", "SubagentStop", "Notification"] {
            assert!(hook_events(&hook(name, json!({"session_id": "s"}))).is_empty());
        }
        // The `pre-compact.command.input` and `post-compact.command.input`
        // schemas of the 0.165.0 binary.
        let compact = |name: &str| {
            hook_events(&hook(
                name,
                json!({"session_id": "s", "turn_id": "t", "hook_event_name": name,
                       "trigger": "manual", "model": "gpt-5"}),
            ))
        };
        assert_eq!(
            compact("PreCompact"),
            [Event::CompactStarted {
                trigger: Some("manual".into())
            }]
        );
        assert_eq!(
            compact("PostCompact"),
            [Event::CompactFinished {
                ok: true,
                trigger: Some("manual".into()),
                message: None
            }]
        );
        assert!(hook_events(&hook("SessionStart", json!({}))).is_empty());
        // The payload name wins over the CLI argument.
        assert_eq!(
            hook_events(&hook("x", json!({"hook_event_name": "Stop"}))),
            [Event::TurnEnd]
        );
        let long = "x".repeat(200);
        let sum = tool_summary(&json!({"command": long})).expect("summary");
        assert_eq!(sum.chars().count(), SUMMARY_MAX + 1);
        assert_eq!(tool_summary(&json!({})), None);
    }
}
