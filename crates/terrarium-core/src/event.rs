use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

use crate::ids::{AgentId, EventId, HarnessSessionId, MessageId, ThreadId};

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
    /// The harness called a tool.
    ToolUse {
        tool: String,
        #[serde(default, skip_serializing_if = "serde_json::Value::is_null")]
        input: serde_json::Value,
    },
    /// The harness raised a notification. When `question` is true, the
    /// harness waits for the user, and the agent is `blocked`.
    Notification { message: String, question: bool },
    /// The harness told us its session ID.
    SessionStarted { harness_session: HarnessSessionId },
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
}

/// An event with its identity, agent, and timestamp.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct EventRecord {
    pub id: EventId,
    pub agent: AgentId,
    pub at: DateTime<Utc>,
    #[serde(flatten)]
    pub event: Event,
}

impl EventRecord {
    pub fn now(agent: AgentId, event: Event) -> Self {
        Self {
            id: EventId::new(),
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
            },
        );
        let json = serde_json::to_value(&rec).expect("serialise");
        assert_eq!(json["type"], "tool_use");
        let back: EventRecord = serde_json::from_value(json).expect("deserialise");
        assert_eq!(back, rec);
    }
}
