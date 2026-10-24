use std::path::PathBuf;

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

use crate::ids::{AgentId, EventId, HarnessSessionId, MessageId, ThreadId};
use crate::status::AgentStatus;

/// One normalised event. Produced from hooks, the PTY, and the supervisor.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum Event {
    /// A prompt-submit hook fired. A turn is open.
    TurnStart {
        #[serde(default, skip_serializing_if = "Option::is_none")]
        prompt: Option<String>,
    },
    /// A stop hook fired. The turn is closed.
    TurnEnd,
    /// The harness called a tool. The tool call starts now.
    ToolUse {
        tool: String,
        #[serde(default, skip_serializing_if = "serde_json::Value::is_null")]
        input: serde_json::Value,
        /// The harness's ID for this tool call, if it has one.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        id: Option<String>,
        /// One short line for the dashboard, for example the command.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        summary: Option<String>,
    },
    /// A tool call finished.
    ToolFinished {
        tool: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        id: Option<String>,
        /// One short line for the dashboard.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        summary: Option<String>,
        /// False when the harness reports an error or an interrupt.
        #[serde(default = "default_true")]
        ok: bool,
    },
    /// The harness raised a notification. When `question` is true, the
    /// harness waits for the user, and the agent is `blocked`.
    Notification { message: String, question: bool },
    /// The harness asks the user a question and waits. The agent is
    /// `blocked` until the runner types an answer.
    Question {
        /// An ID for the answer path. Opaque to Terrarium.
        request_id: String,
        /// The tool that asks, for example `AskUserQuestion`.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        tool: Option<String>,
        question: String,
        /// The choices, in screen order. Empty for a free-text question.
        #[serde(default, skip_serializing_if = "Vec::is_empty")]
        options: Vec<String>,
    },
    /// The harness told us its session ID.
    SessionStarted {
        harness_session: HarnessSessionId,
        /// The session file of the harness, for usage accounting.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        transcript_path: Option<PathBuf>,
        /// Why the session started, for example `startup` or `resume`.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        source: Option<String>,
    },
    /// The harness closed its session. The process exits soon after.
    SessionEnded {
        #[serde(default, skip_serializing_if = "Option::is_none")]
        reason: Option<String>,
    },
    /// The harness process exited.
    Exit {
        #[serde(default, skip_serializing_if = "Option::is_none")]
        code: Option<i32>,
    },
    /// Mail arrived for the agent.
    Mail {
        message: MessageId,
        from: String,
        thread: ThreadId,
    },
    /// A board card changed in a way that concerns the agent.
    Card { card: String, change: String },
    /// A supervisor or daemon notice.
    System { message: String },
    /// The agent status changed. The runner raises one per transition.
    StatusChanged {
        from: AgentStatus,
        to: AgentStatus,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        reason: Option<String>,
    },
    /// The runner typed a turn, but the prompt-submit hook did not fire and
    /// the screen did not go busy, even after the retry. `text` is the turn
    /// that was not delivered.
    DeliveryFailed { text: String, reason: String },
}

fn default_true() -> bool {
    true
}

/// An event with its identity, agent, and timestamp.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct EventRecord {
    pub id: EventId,
    /// The position of the record in the agent's `events.jsonl`, from 1.
    /// The runner sets it; it goes on across runner restarts, so a reader
    /// can ask for the records after a seq (`docs/runner.md`). `None` in
    /// records from before the field existed, and in records that did not
    /// come from a runner.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub seq: Option<u64>,
    pub agent: AgentId,
    pub at: DateTime<Utc>,
    #[serde(flatten)]
    pub event: Event,
}

impl EventRecord {
    pub fn now(agent: AgentId, event: Event) -> Self {
        Self {
            id: EventId::new(),
            seq: None,
            agent,
            at: Utc::now(),
            event,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn event_record_round_trips_as_json() {
        let rec = EventRecord::now(
            AgentId::new(),
            Event::ToolUse {
                tool: "Bash".into(),
                input: serde_json::json!({"command": "ls"}),
                id: None,
                summary: Some("ls".into()),
            },
        );
        let json = serde_json::to_value(&rec).expect("serialise");
        assert_eq!(json["type"], "tool_use");
        let back: EventRecord = serde_json::from_value(json).expect("deserialise");
        assert_eq!(back, rec);
    }

    #[test]
    fn seq_is_optional_on_the_wire() {
        let mut rec = EventRecord::now(AgentId::new(), Event::TurnEnd);
        let json = serde_json::to_value(&rec).expect("serialise");
        assert!(json.get("seq").is_none());
        rec.seq = Some(42);
        let json = serde_json::to_value(&rec).expect("serialise");
        assert_eq!(json["seq"], 42);
        let back: EventRecord = serde_json::from_value(json).expect("deserialise");
        assert_eq!(back, rec);
    }

    #[test]
    fn older_wire_forms_still_parse() {
        let ev: Event = serde_json::from_value(serde_json::json!({
            "type": "session_started", "harness_session": "abc"
        }))
        .expect("session_started");
        assert_eq!(
            ev,
            Event::SessionStarted {
                harness_session: HarnessSessionId("abc".into()),
                transcript_path: None,
                source: None,
            }
        );
        let ev: Event = serde_json::from_value(serde_json::json!({
            "type": "tool_finished", "tool": "Bash"
        }))
        .expect("tool_finished");
        assert!(matches!(ev, Event::ToolFinished { ok: true, .. }));
    }
}
