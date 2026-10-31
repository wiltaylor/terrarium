//! Map Copilot CLI hook payloads to Terrarium events (PLAN §7).
//!
//! Rules from `docs/compat.md`, "Copilot CLI 1.0.103":
//!
//! - Two dialects. PascalCase hooks (`SessionStart`, `UserPromptSubmit`,
//!   `PreToolUse`, `PostToolUse`, `Stop`, `SessionEnd`, `Notification`)
//!   have snake_case keys. camelCase hooks (`sessionStart`,
//!   `userPromptSubmitted`, `preToolUse`, `postToolUse`, `agentStop`,
//!   `sessionEnd`) have camelCase keys. `PermissionRequest` and
//!   `Notification` have camelCase IDs (`sessionId`). Terrarium registers
//!   the PascalCase names; the mapper reads both.
//! - `UserPromptSubmit` opens a turn and `Stop` closes it.
//! - `PermissionRequest` fires for every tool call, also in Allow All mode.
//!   It is not a "needs input" signal. It maps to [`Event::ToolUse`] only
//!   when no `PreToolUse` reported the call (normally it maps to nothing,
//!   so that one call is not two tool events).
//! - "Needs input" is `Notification` with `notification_type` =
//!   `elicitation_dialog`. It comes just after `PreToolUse` with
//!   `tool_name` = `AskUserQuestion`; that payload has the question and the
//!   choices. It maps to [`Event::Question`].
//! - A shell command can move to the background. Then `Stop` fires while
//!   it runs, and Copilot starts a second turn later by itself. That turn
//!   had its own `UserPromptSubmit` (`<system_notification>...`) in the
//!   capture. If a later turn starts with no `UserPromptSubmit`, the first
//!   tool hook after a `Stop` opens it: the mapper adds a
//!   [`Event::TurnStart`] with no prompt.
//! - `SessionStart` fires at the first prompt, not at start-up.
//! - `PreCompact` fires when a compaction starts, also for a `/compact`
//!   before the first prompt (verified live with 1.0.105 on 2026-10-31:
//!   `hook.start` with `hookType: "preCompact"` in `events.jsonl`, before
//!   the `sessionStart` hook). It maps to [`Event::CompactStarted`]. When
//!   no `SessionStart` reported the session yet, it maps to an
//!   [`Event::SessionStarted`] with no `source` first, so that the usage
//!   collector reads the session file, where the end of the compaction
//!   is (docs/compat.md, "Compaction").
//!
//! The mapper keeps a little state per session ID: whether a turn is open,
//! the time of the last `Stop`, the open tool calls, the last question, and
//! whether the session was reported.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Mutex;

use chrono::DateTime;
use serde_json::Value;
use terrarium_core::{Event, HarnessSessionId, HookPayload};

/// The tool name of Copilot's question tool (`ask_user`) in the
/// PascalCase dialect.
pub const ASK_TOOL: &str = "AskUserQuestion";
/// The `notification_type` of the question dialog.
pub const ELICITATION: &str = "elicitation_dialog";
/// The start of a prompt that Copilot makes itself, for example when a
/// background shell command ends.
pub const SYSTEM_PROMPT_PREFIX: &str = "<system_notification>";

/// True when a `UserPromptSubmit` prompt came from Copilot itself, not
/// from typed input. Do not count it as a delivery.
pub fn is_system_prompt(prompt: &str) -> bool {
    prompt.trim_start().starts_with(SYSTEM_PROMPT_PREFIX)
}

fn s<'a>(v: &'a Value, key: &str) -> Option<&'a str> {
    v.get(key).and_then(Value::as_str)
}

/// The first of `keys` that the body has, as a string.
fn field<'a>(b: &'a Value, keys: &[&str]) -> Option<&'a str> {
    keys.iter().find_map(|k| s(b, k))
}

fn value<'a>(b: &'a Value, keys: &[&str]) -> Option<&'a Value> {
    keys.iter().find_map(|k| b.get(*k)).filter(|v| !v.is_null())
}

/// The hook names of both dialects, as one name.
fn canonical(name: &str) -> &str {
    match name {
        "sessionStart" => "SessionStart",
        "userPromptSubmitted" => "UserPromptSubmit",
        "preToolUse" => "PreToolUse",
        "postToolUse" => "PostToolUse",
        "agentStop" => "Stop",
        "sessionEnd" => "SessionEnd",
        "errorOccurred" => "ErrorOccurred",
        "subagentStop" => "SubagentStop",
        "permissionRequest" => "PermissionRequest",
        "preCompact" => "PreCompact",
        other => other,
    }
}

/// The IDs that the Copilot hook payloads carry.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct HookContext {
    /// The canonical hook name: `hook_event_name`, else `hookName`, else
    /// the name from `terrarium hook <name>`.
    pub event: String,
    pub session_id: Option<HarnessSessionId>,
    pub cwd: Option<PathBuf>,
    /// `Stop` only.
    pub transcript_path: Option<PathBuf>,
    /// Milliseconds since the epoch. PascalCase payloads have an RFC 3339
    /// string; camelCase payloads have a number.
    pub timestamp_ms: Option<i64>,
}

/// Read the common IDs from a payload.
pub fn hook_context(hook: &HookPayload) -> HookContext {
    let b = &hook.body;
    let name = field(b, &["hook_event_name", "hookName"]).unwrap_or(&hook.hook);
    HookContext {
        event: canonical(name).to_owned(),
        session_id: field(b, &["session_id", "sessionId"])
            .map(|id| HarnessSessionId(id.to_owned())),
        cwd: field(b, &["cwd"]).map(PathBuf::from),
        transcript_path: field(b, &["transcript_path", "transcriptPath"]).map(PathBuf::from),
        timestamp_ms: b.get("timestamp").and_then(|t| match t {
            Value::Number(n) => n.as_i64(),
            Value::String(s) => DateTime::parse_from_rfc3339(s)
                .ok()
                .map(|d| d.timestamp_millis()),
            _ => None,
        }),
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
/// Only `Bash` and `AskUserQuestion` were captured; the other keys are
/// guesses that fall back to `description`.
pub fn tool_summary(tool: &str, input: &Value) -> Option<String> {
    let keys: &[&str] = match tool {
        "Bash" | "bash" | "PowerShell" | "powershell" => &["command"],
        ASK_TOOL | "ask_user" => &["message", "question"],
        _ => &["path", "file_path", "pattern", "url", "query", "prompt"],
    };
    keys.iter()
        .find_map(|k| s(input, k))
        .or_else(|| s(input, "description"))
        .map(one_line)
        .filter(|l| !l.is_empty())
}

/// The question text and its choices (screen order) from the
/// `AskUserQuestion` input: `message`, and the `enum` of the first
/// property of `requestedSchema`. The screen also shows "Other (type your
/// answer)" after the choices; it is not in the list, so the indexes stay
/// right.
pub fn ask_question(input: &Value) -> Option<(String, Vec<String>)> {
    let text = s(input, "message")?.to_owned();
    let options = input
        .get("requestedSchema")
        .and_then(|r| r.get("properties"))
        .and_then(Value::as_object)
        .and_then(|props| props.values().next())
        .and_then(|p| p.get("enum"))
        .and_then(Value::as_array)
        .map(|e| {
            e.iter()
                .map(|v| match v {
                    Value::String(s) => s.clone(),
                    other => other.to_string(),
                })
                .collect()
        })
        .unwrap_or_default();
    Some((text, options))
}

fn tool_ok(b: &Value) -> bool {
    let result = value(b, &["tool_result", "toolResult"]);
    match result.and_then(|r| s(r, "result_type").or_else(|| s(r, "resultType"))) {
        Some(t) => t == "success",
        None => true,
    }
}

/// The per-session state of the mapper.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct SessionState {
    /// `None` until the first `UserPromptSubmit` or `Stop`.
    pub turn_open: Option<bool>,
    /// The time of the last `Stop`, in milliseconds.
    pub last_stop_ms: Option<i64>,
    /// Tool calls with a `PreToolUse` and no `PostToolUse` yet.
    pub open_tools: usize,
    /// The last `AskUserQuestion` input: the question and its choices.
    pub question: Option<(String, Vec<String>)>,
    /// True after an `Event::SessionStarted` for this session.
    pub reported: bool,
}

impl SessionState {
    /// The `TurnStart` for a turn that began with no `UserPromptSubmit`:
    /// a tool hook after a `Stop`. A hook with a time before the `Stop` is
    /// late, and opens nothing.
    fn implicit_turn(&mut self, ts: Option<i64>) -> Option<Event> {
        let late = matches!((ts, self.last_stop_ms), (Some(t), Some(stop)) if t <= stop);
        if self.turn_open == Some(false) && !late {
            self.turn_open = Some(true);
            Some(Event::TurnStart { prompt: None })
        } else {
            None
        }
    }
}

/// Map one hook payload to events. `state` is the state of the payload's
/// session; `config_dir` is `COPILOT_HOME` as the harness sees it (for the
/// session file path).
pub fn map_hook(hook: &HookPayload, state: &mut SessionState, config_dir: &Path) -> Vec<Event> {
    let ctx = hook_context(hook);
    let b = &hook.body;
    let tool = || {
        field(b, &["tool_name", "toolName"])
            .unwrap_or("unknown")
            .to_owned()
    };
    let input = || {
        value(b, &["tool_input", "toolInput", "toolArgs"])
            .cloned()
            .unwrap_or(Value::Null)
    };
    let mut out = Vec::new();
    match ctx.event.as_str() {
        "SessionStart" => {
            *state = SessionState::default();
            if let Some(id) = ctx.session_id {
                state.reported = true;
                out.push(Event::SessionStarted {
                    transcript_path: Some(crate::session::events_file(config_dir, &id)),
                    harness_session: id,
                    source: s(b, "source").map(str::to_owned),
                });
            }
        }
        "UserPromptSubmit" => {
            state.turn_open = Some(true);
            out.push(Event::TurnStart {
                prompt: s(b, "prompt").map(str::to_owned),
            });
        }
        "Stop" => {
            state.turn_open = Some(false);
            state.last_stop_ms = ctx.timestamp_ms.or(state.last_stop_ms);
            state.open_tools = 0;
            state.question = None;
            out.push(Event::TurnEnd);
        }
        "PreToolUse" => {
            out.extend(state.implicit_turn(ctx.timestamp_ms));
            let (tool, input) = (tool(), input());
            if tool == ASK_TOOL || tool == "ask_user" {
                state.question = ask_question(&input);
            }
            state.open_tools += 1;
            out.push(Event::ToolUse {
                summary: tool_summary(&tool, &input),
                tool,
                input,
                id: None,
            });
        }
        "PostToolUse" | "PostToolUseFailure" => {
            out.extend(state.implicit_turn(ctx.timestamp_ms));
            state.open_tools = state.open_tools.saturating_sub(1);
            let tool = tool();
            out.push(Event::ToolFinished {
                summary: tool_summary(&tool, &input()),
                tool,
                id: None,
                ok: ctx.event == "PostToolUse" && tool_ok(b),
            });
        }
        "PermissionRequest" => {
            out.extend(state.implicit_turn(ctx.timestamp_ms));
            if state.open_tools == 0 {
                let (tool, input) = (tool(), input());
                out.push(Event::ToolUse {
                    summary: tool_summary(&tool, &input),
                    tool,
                    input,
                    id: None,
                });
            }
        }
        "Notification" => {
            let message = s(b, "message").unwrap_or_default().to_owned();
            if s(b, "notification_type") == Some(ELICITATION) {
                out.extend(state.implicit_turn(ctx.timestamp_ms));
                let (question, options) = state.question.take().unwrap_or((message, Vec::new()));
                let request_id = match (&ctx.session_id, ctx.timestamp_ms) {
                    (Some(id), Some(ts)) => format!("{}:{ts}", id.0),
                    (Some(id), None) => id.0.clone(),
                    (None, _) => "unknown".into(),
                };
                out.push(Event::Question {
                    request_id,
                    tool: Some(ASK_TOOL.into()),
                    question,
                    options,
                });
            } else {
                out.push(Event::Notification {
                    message,
                    question: false,
                });
            }
        }
        // Registered, never captured: the payload shape is a guess.
        "ErrorOccurred" => out.push(Event::System {
            message: format!(
                "copilot: an error occurred{}",
                field(b, &["message", "error"])
                    .or_else(|| b.get("error").and_then(|e| s(e, "message")))
                    .map(|e| format!(": {e}"))
                    .unwrap_or_default()
            ),
        }),
        "SessionEnd" => out.push(Event::SessionEnded {
            reason: s(b, "reason").map(str::to_owned),
        }),
        "PreCompact" => {
            if let Some(id) = ctx.session_id.filter(|_| !state.reported) {
                state.reported = true;
                out.push(Event::SessionStarted {
                    transcript_path: Some(crate::session::events_file(config_dir, &id)),
                    harness_session: id,
                    source: None,
                });
            }
            out.push(Event::CompactStarted {
                trigger: s(b, "trigger").map(str::to_owned),
            });
        }
        _ => {}
    }
    out
}

/// The mapper with its state for every session. One adapter serves many
/// agents, so the state is keyed by the session ID. `SessionEnd` drops it.
#[derive(Debug, Default)]
pub struct HookMapper {
    sessions: Mutex<HashMap<String, SessionState>>,
}

impl HookMapper {
    pub fn new() -> Self {
        Self::default()
    }

    /// Map one hook payload, with the state of its session.
    pub fn map(&self, hook: &HookPayload, config_dir: &Path) -> Vec<Event> {
        let key = hook_context(hook)
            .session_id
            .map(|id| id.0)
            .unwrap_or_default();
        let mut sessions = match self.sessions.lock() {
            Ok(g) => g,
            Err(poisoned) => poisoned.into_inner(),
        };
        let state = sessions.entry(key.clone()).or_default();
        let events = map_hook(hook, state, config_dir);
        if events
            .iter()
            .any(|e| matches!(e, Event::SessionEnded { .. }))
        {
            sessions.remove(&key);
        }
        events
    }

    /// The number of sessions with state, for tests.
    pub fn tracked(&self) -> usize {
        self.sessions.lock().map(|g| g.len()).unwrap_or(0)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    use terrarium_core::HarnessId;

    const HOME: &str = "/home/agent/.copilot";

    fn captured(file: &str) -> Vec<HookPayload> {
        let path = format!("{}/tests/fixtures/hooks/{file}", env!("CARGO_MANIFEST_DIR"));
        std::fs::read_to_string(&path)
            .expect("fixture")
            .lines()
            .filter(|l| !l.trim().is_empty())
            .map(|l| {
                let v: Value = serde_json::from_str(l).expect("json line");
                HookPayload {
                    harness: HarnessId::Copilot,
                    hook: v["hook"].as_str().expect("hook").to_owned(),
                    body: v["payload"].clone(),
                }
            })
            .collect()
    }

    fn map_all(hooks: &[HookPayload]) -> Vec<Event> {
        let m = HookMapper::new();
        hooks
            .iter()
            .flat_map(|h| m.map(h, Path::new(HOME)))
            .collect()
    }

    fn hook(name: &str, body: Value) -> HookPayload {
        HookPayload {
            harness: HarnessId::Copilot,
            hook: name.into(),
            body,
        }
    }

    #[test]
    fn every_captured_payload_has_the_ids() {
        for file in ["one-turn.jsonl", "typing.jsonl", "camel.jsonl"] {
            for h in captured(file) {
                let ctx = hook_context(&h);
                assert_eq!(ctx.event, canonical(&h.hook), "{file}");
                assert!(ctx.session_id.is_some(), "{file} {}", h.hook);
                assert_eq!(ctx.cwd.as_deref(), Some(Path::new("$CWD")));
                assert!(ctx.timestamp_ms.is_some(), "{file} {}", h.hook);
                assert_eq!(ctx.transcript_path.is_some(), ctx.event == "Stop");
            }
        }
    }

    #[test]
    fn one_turn_maps_in_order() {
        let sid = "3c27d166-9952-489f-8b96-969b46c533c1";
        let events = map_all(&captured("one-turn.jsonl"));
        assert_eq!(
            events,
            [
                Event::SessionStarted {
                    harness_session: HarnessSessionId(sid.into()),
                    transcript_path: Some(PathBuf::from(format!(
                        "{HOME}/session-state/{sid}/events.jsonl"
                    ))),
                    source: Some("new".into()),
                },
                Event::TurnStart {
                    prompt: Some(
                        "Run the shell command `echo terrarium-hook-test`. Then reply with exactly: done"
                            .into()
                    ),
                },
                Event::ToolUse {
                    tool: "Bash".into(),
                    input: json!({"command": "echo terrarium-hook-test", "description": "Echo test command"}),
                    id: None,
                    summary: Some("echo terrarium-hook-test".into()),
                },
                // PermissionRequest for the same call adds nothing.
                Event::ToolFinished {
                    tool: "Bash".into(),
                    id: None,
                    summary: Some("echo terrarium-hook-test".into()),
                    ok: true,
                },
                Event::TurnEnd,
                Event::SessionEnded {
                    reason: Some("user_exit".into()),
                },
            ]
        );
    }

    #[test]
    fn camel_dialect_maps_the_same() {
        let one: Vec<Event> = map_all(&captured("one-turn.jsonl"))
            .into_iter()
            .filter(|e| !matches!(e, Event::SessionStarted { .. }))
            .collect();
        let camel: Vec<Event> = map_all(&captured("camel.jsonl"))
            .into_iter()
            .filter(|e| !matches!(e, Event::SessionStarted { .. }))
            .map(|e| match e {
                // camelCase names the tool `bash`.
                Event::ToolUse {
                    input, id, summary, ..
                } => Event::ToolUse {
                    tool: "Bash".into(),
                    input,
                    id,
                    summary,
                },
                Event::ToolFinished {
                    id, summary, ok, ..
                } => Event::ToolFinished {
                    tool: "Bash".into(),
                    id,
                    summary,
                    ok,
                },
                e => e,
            })
            .collect();
        assert_eq!(one, camel);
    }

    #[test]
    fn typing_capture_turns_and_question() {
        let events = map_all(&captured("typing.jsonl"));
        let starts: Vec<Option<&str>> = events
            .iter()
            .filter_map(|e| match e {
                Event::TurnStart { prompt } => Some(prompt.as_deref()),
                _ => None,
            })
            .collect();
        let ends = events
            .iter()
            .filter(|e| matches!(e, Event::TurnEnd))
            .count();
        // Five typed turns plus Copilot's own system-notification turn; the
        // text typed while busy steered and fired no hook.
        assert_eq!((starts.len(), ends), (5, 5));
        assert!(is_system_prompt(starts[3].expect("prompt")));
        assert!(!is_system_prompt(starts[2].expect("prompt")));
        // No synthetic turn: every turn here had a UserPromptSubmit.
        assert!(starts.iter().all(Option::is_some));

        let q: Vec<&Event> = events
            .iter()
            .filter(|e| matches!(e, Event::Question { .. }))
            .collect();
        assert_eq!(
            q,
            [&Event::Question {
                request_id: "e5c28995-ddc5-4a53-a9d9-f534b2a95029:1792826213970".into(),
                tool: Some(ASK_TOOL.into()),
                question: "Please pick one colour: red or blue.".into(),
                options: vec!["red".into(), "blue".into()],
            }]
        );
        assert!(events.iter().any(|e| matches!(
            e,
            Event::Notification { question: false, message } if message.contains("has completed")
        )));
        // PermissionRequest never blocks.
        assert!(
            !events
                .iter()
                .any(|e| matches!(e, Event::Notification { question: true, .. }))
        );
    }

    #[test]
    fn tool_hook_after_stop_opens_a_turn() {
        let m = HookMapper::new();
        let home = Path::new(HOME);
        let sid = json!("s1");
        let body = |extra: Value| {
            let mut v = json!({"session_id": sid});
            for (k, x) in extra.as_object().expect("obj") {
                v[k] = x.clone();
            }
            v
        };
        m.map(
            &hook(
                "UserPromptSubmit",
                body(json!({"prompt": "go", "timestamp": "2026-10-24T07:16:38.629Z"})),
            ),
            home,
        );
        assert_eq!(
            m.map(
                &hook(
                    "Stop",
                    body(json!({"timestamp": "2026-10-24T07:16:43.499Z"}))
                ),
                home
            ),
            [Event::TurnEnd]
        );
        // A late PostToolUse from before the Stop opens nothing.
        let late = m.map(
            &hook(
                "PostToolUse",
                body(json!({"tool_name": "Bash", "timestamp": "2026-10-24T07:16:41.834Z"})),
            ),
            home,
        );
        assert!(matches!(&late[..], [Event::ToolFinished { .. }]));
        // A tool call after the Stop, with no prompt hook: a new turn.
        let ev = m.map(
            &hook("PreToolUse", body(json!({"tool_name": "Bash", "tool_input": {"command": "ls"}, "timestamp": "2026-10-24T07:16:46.000Z"}))),
            home,
        );
        assert!(matches!(
            &ev[..],
            [Event::TurnStart { prompt: None }, Event::ToolUse { .. }]
        ));
        // Only once.
        let ev = m.map(
            &hook(
                "PermissionRequest",
                json!({"sessionId": "s1", "toolName": "bash", "timestamp": 1792826206100_i64}),
            ),
            home,
        );
        assert!(ev.is_empty(), "{ev:?}");
        assert_eq!(m.tracked(), 1);
        m.map(
            &hook("SessionEnd", body(json!({"reason": "user_exit"}))),
            home,
        );
        assert_eq!(m.tracked(), 0);
    }

    #[test]
    fn permission_request_alone_is_a_tool_use() {
        let mut st = SessionState::default();
        let ev = map_hook(
            &hook(
                "PermissionRequest",
                json!({"hookName": "permissionRequest", "sessionId": "s", "toolName": "bash",
                       "toolInput": {"command": "rm -rf build"}, "permissionSuggestions": []}),
            ),
            &mut st,
            Path::new(HOME),
        );
        assert_eq!(
            ev,
            [Event::ToolUse {
                tool: "bash".into(),
                input: json!({"command": "rm -rf build"}),
                id: None,
                summary: Some("rm -rf build".into()),
            }]
        );
    }

    #[test]
    fn failures_errors_and_unknown_hooks() {
        let mut st = SessionState::default();
        let home = Path::new(HOME);
        assert!(matches!(
            &map_hook(
                &hook("PostToolUseFailure", json!({"tool_name": "Bash"})),
                &mut st,
                home
            )[..],
            [Event::ToolFinished { ok: false, .. }]
        ));
        assert!(matches!(
            &map_hook(
                &hook(
                    "PostToolUse",
                    json!({"tool_name": "Bash", "tool_result": {"result_type": "failure"}})
                ),
                &mut st,
                home
            )[..],
            [Event::ToolFinished { ok: false, .. }]
        ));
        assert!(matches!(
            &map_hook(&hook("ErrorOccurred", json!({"error": {"message": "rate limited"}})), &mut st, home)[..],
            [Event::System { message }] if message.ends_with("rate limited")
        ));
        // An elicitation with no question before it uses the notification text.
        assert!(matches!(
            &map_hook(
                &hook("Notification", json!({"sessionId": "s", "message": "Information requested",
                                             "notification_type": "elicitation_dialog"})),
                &mut st,
                home
            )[..],
            [Event::Question { question, options, request_id, .. }]
                if question == "Information requested" && options.is_empty() && request_id == "s"
        ));
        for name in ["SubagentStart", "SubagentStop", "Mystery"] {
            assert!(
                map_hook(&hook(name, json!({})), &mut st, home).is_empty(),
                "{name}"
            );
        }
        assert!(map_hook(&hook("SessionStart", json!({})), &mut st, home).is_empty());
        // The payload name wins over the CLI argument.
        assert_eq!(
            map_hook(
                &hook("x", json!({"hook_event_name": "Stop"})),
                &mut st,
                home
            ),
            [Event::TurnEnd]
        );
    }

    /// The `input` of the `preCompact` `hook.start` lines in a live
    /// 1.0.105 `events.jsonl` (2026-10-31; the session ID is replaced).
    /// The first `/compact` came before the first prompt, so before the
    /// `sessionStart` hook.
    #[test]
    fn pre_compact_starts_the_compaction_and_reports_the_session() {
        let sid = "00000000-0000-4000-8000-000000000105";
        let pre = |ts: i64| {
            hook(
                "PreCompact",
                json!({"transcriptPath": format!("{HOME}/session-state/{sid}/events.jsonl"),
                       "trigger": "manual", "customInstructions": "", "sessionId": sid,
                       "timestamp": ts, "cwd": "/home/agent/work"}),
            )
        };
        let started = Event::SessionStarted {
            harness_session: HarnessSessionId(sid.into()),
            transcript_path: Some(PathBuf::from(format!(
                "{HOME}/session-state/{sid}/events.jsonl"
            ))),
            source: None,
        };
        let compact = Event::CompactStarted {
            trigger: Some("manual".into()),
        };
        let m = HookMapper::new();
        let home = Path::new(HOME);
        assert_eq!(
            m.map(&pre(1_793_422_901_807), home),
            [started, compact.clone()]
        );
        // The session is reported once.
        assert_eq!(
            m.map(&pre(1_793_422_901_900), home),
            std::slice::from_ref(&compact)
        );
        let start = hook(
            "SessionStart",
            json!({"source": "new", "initialPrompt": "[User] Say ok.", "sessionId": sid,
                   "timestamp": 1_793_422_908_367_i64, "cwd": "/home/agent/work"}),
        );
        assert!(matches!(
            &m.map(&start, home)[..],
            [Event::SessionStarted { source: Some(s), .. }] if s == "new"
        ));
        assert_eq!(m.map(&pre(1_793_423_351_488), home), [compact]);
        // The camelCase name, and a payload with no session.
        let mut st = SessionState::default();
        assert_eq!(
            map_hook(
                &hook("preCompact", json!({"trigger": "auto"})),
                &mut st,
                home
            ),
            [Event::CompactStarted {
                trigger: Some("auto".into())
            }]
        );
    }

    #[test]
    fn summaries() {
        assert_eq!(
            tool_summary("view", &json!({"path": "/w/a.rs"})).as_deref(),
            Some("/w/a.rs")
        );
        assert_eq!(tool_summary("Mystery", &json!({})), None);
        let long = "x".repeat(200);
        let s = tool_summary("Bash", &json!({"command": long})).expect("summary");
        assert_eq!(s.chars().count(), SUMMARY_MAX + 1);
        assert_eq!(
            ask_question(&json!({"message": "Free text?"})),
            Some(("Free text?".into(), vec![]))
        );
    }
}
