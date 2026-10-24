//! Map Claude Code hook payloads to Terrarium events (PLAN §7).
//!
//! Rules from `docs/compat.md`:
//!
//! - `UserPromptSubmit` opens a turn and `Stop` closes it. Text typed while
//!   a turn runs joins that turn: two `UserPromptSubmit`, one `Stop`. Do not
//!   count `Stop` events per delivery.
//! - `Notification` is the 60 s idle reminder (`idle_prompt`). It is not a
//!   "blocked" signal.
//! - `PermissionRequest` is the "needs input" signal. It fires for
//!   `AskUserQuestion` also in bypass mode.

use std::path::PathBuf;

use serde_json::Value;
use terrarium_core::{Event, HarnessSessionId, HookPayload};

/// The IDs that every Claude Code hook payload carries.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct HookContext {
    /// `hook_event_name`, else the name from `terrarium hook <name>`.
    pub event: String,
    pub session_id: Option<HarnessSessionId>,
    pub cwd: Option<PathBuf>,
    pub transcript_path: Option<PathBuf>,
    /// Turn hooks only. The same for every hook of one turn.
    pub prompt_id: Option<String>,
    /// Turn hooks only, for example `bypassPermissions`.
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
        prompt_id: s(b, "prompt_id").map(str::to_owned),
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
pub fn tool_summary(tool: &str, input: &Value) -> Option<String> {
    let key = match tool {
        "Bash" | "PowerShell" => "command",
        "Read" | "Write" | "Edit" | "MultiEdit" => "file_path",
        "NotebookEdit" => "notebook_path",
        "Glob" | "Grep" => "pattern",
        "WebFetch" => "url",
        "WebSearch" => "query",
        "Task" | "Agent" => "description",
        "AskUserQuestion" => {
            return first_question(input).map(|(q, _)| one_line(&q));
        }
        "TodoWrite" => {
            let n = input.get("todos").and_then(Value::as_array)?.len();
            return Some(format!("{n} todos"));
        }
        _ => "description",
    };
    s(input, key)
        .or_else(|| s(input, "description"))
        .map(one_line)
        .filter(|l| !l.is_empty())
}

/// The first question of an `AskUserQuestion` input, and its option labels
/// in screen order. A second question shows as a count in the text.
fn first_question(input: &Value) -> Option<(String, Vec<String>)> {
    let qs = input.get("questions")?.as_array()?;
    let q = qs.first()?;
    let mut text = s(q, "question")?.to_owned();
    if qs.len() > 1 {
        text.push_str(&format!(" (+{} more)", qs.len() - 1));
    }
    let options = q
        .get("options")
        .and_then(Value::as_array)
        .map(|opts| {
            opts.iter()
                .filter_map(|o| s(o, "label").map(str::to_owned))
                .collect()
        })
        .unwrap_or_default();
    Some((text, options))
}

fn tool_ok(b: &Value) -> bool {
    let resp = b.get("tool_response");
    let flag = |k: &str| resp.and_then(|r| r.get(k)).and_then(Value::as_bool);
    !(flag("interrupted").unwrap_or(false) || flag("is_error").unwrap_or(false))
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
        "StopFailure" => vec![
            Event::System {
                message: format!(
                    "claude: the turn ended with an error{}",
                    s(b, "error")
                        .or_else(|| s(b, "reason"))
                        .map(|e| format!(": {e}"))
                        .unwrap_or_default()
                ),
            },
            Event::TurnEnd,
        ],
        "PreToolUse" => {
            let input = input();
            vec![Event::ToolUse {
                summary: tool_summary(&tool(), &input),
                tool: tool(),
                input,
                id: tool_id(),
            }]
        }
        "PostToolUse" | "PostToolUseFailure" => vec![Event::ToolFinished {
            summary: tool_summary(&tool(), &input()),
            tool: tool(),
            id: tool_id(),
            ok: ctx.event == "PostToolUse" && tool_ok(b),
        }],
        "PermissionRequest" => {
            let tool = tool();
            let input = input();
            let (question, options) = match first_question(&input) {
                Some(q) => q,
                None => (
                    match tool_summary(&tool, &input) {
                        Some(sum) => format!("Allow {tool}: {sum}?"),
                        None => format!("Allow {tool}?"),
                    },
                    Vec::new(),
                ),
            };
            // The payload has no request ID. The tool call ID is there in
            // some versions; the prompt ID is always there for a turn.
            let request_id = tool_id()
                .or(ctx.prompt_id)
                .or_else(|| ctx.session_id.map(|id| id.0))
                .unwrap_or_else(|| "unknown".into());
            vec![Event::Question {
                request_id,
                tool: Some(tool),
                question,
                options,
            }]
        }
        "Notification" => vec![Event::Notification {
            message: s(b, "message").unwrap_or_default().to_owned(),
            question: false,
        }],
        "SessionEnd" => vec![Event::SessionEnded {
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

    fn captured(file: &str) -> Vec<HookPayload> {
        let path = format!("{}/tests/fixtures/hooks/{file}", env!("CARGO_MANIFEST_DIR"));
        std::fs::read_to_string(&path)
            .expect("fixture")
            .lines()
            .filter(|l| !l.trim().is_empty())
            .map(|l| {
                let v: Value = serde_json::from_str(l).expect("json line");
                HookPayload {
                    harness: HarnessId::Claude,
                    hook: v["hook"].as_str().expect("hook").to_owned(),
                    body: v["payload"].clone(),
                }
            })
            .collect()
    }

    fn by_name<'a>(hooks: &'a [HookPayload], name: &str) -> &'a HookPayload {
        hooks.iter().find(|h| h.hook == name).expect(name)
    }

    #[test]
    fn every_captured_payload_has_the_ids() {
        for file in ["one-turn.jsonl", "typing.jsonl"] {
            for h in captured(file) {
                let ctx = hook_context(&h);
                assert_eq!(ctx.event, h.hook);
                assert!(ctx.session_id.is_some(), "{}", h.hook);
                assert_eq!(ctx.cwd.as_deref(), Some(std::path::Path::new("$CWD")));
                let tp = ctx.transcript_path.expect("transcript_path");
                assert!(tp.to_string_lossy().ends_with(".jsonl"));
            }
        }
    }

    #[test]
    fn one_turn_maps_in_order() {
        let hooks = captured("one-turn.jsonl");
        let events: Vec<Event> = hooks.iter().flat_map(hook_events).collect();
        let sid = "20997765-ed25-4336-99ed-c4be65c8e439";
        assert_eq!(
            events,
            [
                Event::SessionStarted {
                    harness_session: HarnessSessionId(sid.into()),
                    transcript_path: Some(PathBuf::from(format!(
                        "$CLAUDE_CONFIG_DIR/projects/<encoded-cwd>/{sid}.jsonl"
                    ))),
                    source: Some("startup".into()),
                },
                Event::TurnStart {
                    prompt: Some(
                        "Use the Bash tool to run: echo terrarium-hook-test. Then reply with exactly: done"
                            .into()
                    ),
                },
                Event::ToolUse {
                    tool: "Bash".into(),
                    input: json!({"command": "echo terrarium-hook-test", "description": "Echo the hook test string"}),
                    id: Some("toolu_01MUR36SJ79jLxY12kLPVwbf".into()),
                    summary: Some("echo terrarium-hook-test".into()),
                },
                Event::ToolFinished {
                    tool: "Bash".into(),
                    id: Some("toolu_01MUR36SJ79jLxY12kLPVwbf".into()),
                    summary: Some("echo terrarium-hook-test".into()),
                    ok: true,
                },
                // PostToolBatch and MessageDisplay map to nothing.
                Event::TurnEnd,
                Event::Notification {
                    message: "Claude is waiting for your input".into(),
                    question: false,
                },
                Event::SessionEnded {
                    reason: Some("prompt_input_exit".into()),
                },
            ]
        );
    }

    #[test]
    fn ask_user_question_is_a_question() {
        let hooks = captured("typing.jsonl");
        let ev = hook_events(by_name(&hooks, "PermissionRequest"));
        assert_eq!(
            ev,
            [Event::Question {
                request_id: "0d95d8a9-bd16-442e-9ddb-43723e2f0af5".into(),
                tool: Some("AskUserQuestion".into()),
                question: "Which colour do you pick?".into(),
                options: vec!["Red".into(), "Green".into(), "Blue".into()],
            }]
        );
        let pre = hooks
            .iter()
            .find(|h| h.hook == "PreToolUse" && h.str_field("tool_name") == Some("AskUserQuestion"))
            .expect("pre");
        assert!(matches!(
            &hook_events(pre)[..],
            [Event::ToolUse { summary: Some(s), .. }] if s == "Which colour do you pick?"
        ));
    }

    #[test]
    fn typed_while_busy_gives_more_submits_than_stops() {
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
        assert_eq!((starts, ends), (8, 7));
    }

    fn hook(name: &str, body: Value) -> HookPayload {
        HookPayload {
            harness: HarnessId::Claude,
            hook: name.into(),
            body,
        }
    }

    #[test]
    fn permission_prompt_for_a_plain_tool() {
        let ev = hook_events(&hook(
            "PermissionRequest",
            json!({"session_id": "s", "prompt_id": "p", "tool_name": "Bash",
                   "tool_input": {"command": "rm -rf build\necho done"}}),
        ));
        assert_eq!(
            ev,
            [Event::Question {
                request_id: "p".into(),
                tool: Some("Bash".into()),
                question: "Allow Bash: rm -rf build…?".into(),
                options: vec![],
            }]
        );
    }

    #[test]
    fn failures_and_unknown_hooks() {
        assert!(matches!(
            &hook_events(&hook("PostToolUseFailure", json!({"tool_name": "Bash"})))[..],
            [Event::ToolFinished { ok: false, .. }]
        ));
        assert!(matches!(
            &hook_events(&hook(
                "PostToolUse",
                json!({"tool_name": "Bash", "tool_response": {"interrupted": true}})
            ))[..],
            [Event::ToolFinished { ok: false, .. }]
        ));
        assert!(matches!(
            &hook_events(&hook("StopFailure", json!({"error": "overloaded"})))[..],
            [Event::System { .. }, Event::TurnEnd]
        ));
        assert!(hook_events(&hook("MessageDisplay", json!({}))).is_empty());
        assert!(hook_events(&hook("SessionStart", json!({}))).is_empty());
        // The payload name wins over the CLI argument.
        assert_eq!(
            hook_events(&hook("x", json!({"hook_event_name": "Stop"}))),
            [Event::TurnEnd]
        );
    }

    #[test]
    fn summaries() {
        assert_eq!(
            tool_summary("Edit", &json!({"file_path": "/w/a.rs"})).as_deref(),
            Some("/w/a.rs")
        );
        assert_eq!(
            tool_summary("TodoWrite", &json!({"todos": [1, 2]})).as_deref(),
            Some("2 todos")
        );
        assert_eq!(tool_summary("Mystery", &json!({})), None);
        let long = "x".repeat(200);
        let s = tool_summary("Bash", &json!({"command": long})).expect("summary");
        assert_eq!(s.chars().count(), SUMMARY_MAX + 1);
    }
}
