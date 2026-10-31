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
        /// The harness's ID for this tool call, if it has one. On the wire
        /// it is `tool_id`: an `EventRecord` flattens the event, and the
        /// record's own `id` (a ULID) owns the key `id`. A bare `Event`
        /// with the older key `id` still parses.
        #[serde(
            default,
            rename = "tool_id",
            alias = "id",
            skip_serializing_if = "Option::is_none"
        )]
        id: Option<String>,
        /// One short line for the dashboard, for example the command.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        summary: Option<String>,
    },
    /// A tool call finished.
    ToolFinished {
        tool: String,
        /// The harness's ID for the tool call. `tool_id` on the wire, as
        /// for `ToolUse`.
        #[serde(
            default,
            rename = "tool_id",
            alias = "id",
            skip_serializing_if = "Option::is_none"
        )]
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
    /// A supervisor or daemon notice that needs the operator's attention,
    /// for example a harness version outside the adapter's tested range.
    Warning { message: String },
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
    /// The provider refused a turn because of a usage limit or a rate
    /// limit of the plan. The daemon moves the agent to the next account
    /// of its pool, or holds its new turns (`terrariumd::usage_limit`).
    UsageLimit {
        /// The provider's message, on one line.
        message: String,
        /// When the limit resets, if the message gives the time.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        resets_at: Option<DateTime<Utc>>,
    },
    /// The harness started to compact the context of its session: a
    /// `/compact` command, or a compaction that the harness started itself
    /// (docs/compat.md, "Compaction").
    CompactStarted {
        /// `manual` or `auto`, when the harness says it.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        trigger: Option<String>,
    },
    /// The harness finished a compaction. `ok` is false when the harness
    /// reports that the compaction failed.
    CompactFinished {
        #[serde(default = "default_true")]
        ok: bool,
        /// `manual` or `auto`, when the harness says it.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        trigger: Option<String>,
        /// The error of a failed compaction, on one line.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        message: Option<String>,
    },
}

fn default_true() -> bool {
    true
}

/// An event with its identity, agent, and timestamp.
///
/// The event is flattened into the record, so no `Event` field may use the
/// keys `id`, `seq`, `agent`, `at`, or `type` (see the tests). Parsing is
/// tolerant of a bad `id`: see the `Deserialize` impl.
#[derive(Debug, Clone, PartialEq, Serialize)]
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

/// Parse a record. A bad or missing `id` never drops the event: the parser
/// makes a new ULID and logs a warning (once per process). Records from
/// before the `tool_id` key could carry two `id` keys (the ULID and the tool
/// call ID); the last one wins in JSON, so `id` holds the tool call ID. When
/// the event is a tool event without a `tool_id`, the parser moves that
/// value into the tool call ID.
impl<'de> Deserialize<'de> for EventRecord {
    fn deserialize<D: serde::Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        use serde::de::Error;

        let mut map = serde_json::Map::<String, serde_json::Value>::deserialize(d)?;
        let raw_id = map.remove("id");
        let seq = match map.remove("seq") {
            None | Some(serde_json::Value::Null) => None,
            Some(v) => Some(serde_json::from_value::<u64>(v).map_err(D::Error::custom)?),
        };
        let agent: AgentId = serde_json::from_value(
            map.remove("agent")
                .ok_or_else(|| D::Error::missing_field("agent"))?,
        )
        .map_err(D::Error::custom)?;
        let at: DateTime<Utc> = serde_json::from_value(
            map.remove("at")
                .ok_or_else(|| D::Error::missing_field("at"))?,
        )
        .map_err(D::Error::custom)?;
        let mut event: Event =
            serde_json::from_value(serde_json::Value::Object(map)).map_err(D::Error::custom)?;

        let parsed = raw_id
            .as_ref()
            .and_then(|v| serde_json::from_value::<EventId>(v.clone()).ok());
        let id = match parsed {
            Some(id) => id,
            None => {
                let stray = raw_id.as_ref().and_then(|v| v.as_str()).map(str::to_owned);
                if let (
                    Some(stray),
                    Event::ToolUse {
                        id: tool_id @ None, ..
                    }
                    | Event::ToolFinished {
                        id: tool_id @ None, ..
                    },
                ) = (stray, &mut event)
                {
                    *tool_id = Some(stray);
                }
                warn_bad_id_once(raw_id.as_ref());
                EventId::new()
            }
        };
        Ok(Self {
            id,
            seq,
            agent,
            at,
            event,
        })
    }
}

fn warn_bad_id_once(raw: Option<&serde_json::Value>) {
    use std::sync::atomic::{AtomicBool, Ordering};
    static WARNED: AtomicBool = AtomicBool::new(false);
    if !WARNED.swap(true, Ordering::Relaxed) {
        tracing::warn!(
            "an event record has a bad or missing id ({}); a new id is used \
             (this warning shows once)",
            raw.map(|v| v.to_string()).unwrap_or_else(|| "none".into())
        );
    }
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

    /// Regression: a real tool call has an ID. It went out as a second `id`
    /// key, which clobbered the record's ULID, and the reader dropped it.
    #[test]
    fn tool_event_with_a_real_id_round_trips() {
        let mut rec = EventRecord::now(
            AgentId::new(),
            Event::ToolUse {
                tool: "Bash".into(),
                input: serde_json::json!({"command": "ls"}),
                id: Some("toolu_01ABC".into()),
                summary: Some("ls".into()),
            },
        );
        rec.seq = Some(7);
        let line = serde_json::to_string(&rec).expect("serialise");
        assert_eq!(line.matches("\"id\"").count(), 1, "one id key: {line}");
        assert!(line.contains("\"tool_id\":\"toolu_01ABC\""), "{line}");
        let back: EventRecord = serde_json::from_str(&line).expect("deserialise");
        assert_eq!(back, rec);

        let fin = EventRecord::now(
            AgentId::new(),
            Event::ToolFinished {
                tool: "Bash".into(),
                id: Some("toolu_01ABC".into()),
                summary: None,
                ok: false,
            },
        );
        let line = serde_json::to_string(&fin).expect("serialise");
        let back: EventRecord = serde_json::from_str(&line).expect("deserialise");
        assert_eq!(back, fin);
    }

    /// A line from before `tool_id`: two `id` keys, and the tool call ID
    /// won. The record still parses, with a new ULID and the tool call ID.
    #[test]
    fn an_old_line_with_two_id_keys_still_parses() {
        let agent = AgentId::new();
        let line = format!(
            "{{\"id\":\"{}\",\"agent\":\"{agent}\",\"at\":\"2026-10-25T00:00:00Z\",\
             \"type\":\"tool_use\",\"tool\":\"Bash\",\"id\":\"toolu_01ABC\"}}",
            EventId::new()
        );
        let rec: EventRecord = serde_json::from_str(&line).expect("parse");
        assert_eq!(rec.agent, agent);
        assert!(
            matches!(&rec.event, Event::ToolUse { id: Some(t), .. } if t == "toolu_01ABC"),
            "{rec:?}"
        );
    }

    #[test]
    fn a_bad_or_missing_record_id_does_not_drop_the_event() {
        let agent = AgentId::new();
        for id in [r#""id":"not-a-ulid","#, ""] {
            let line = format!(
                r#"{{{id}"agent":"{agent}","at":"2026-10-25T00:00:00Z","type":"turn_end"}}"#
            );
            let rec: EventRecord = serde_json::from_str(&line).expect("parse");
            assert_eq!(rec.event, Event::TurnEnd);
        }
    }

    #[test]
    fn a_bare_event_takes_the_old_id_key() {
        let ev: Event = serde_json::from_value(serde_json::json!({
            "type": "tool_finished", "tool": "Bash", "id": "toolu_1"
        }))
        .expect("tool_finished");
        assert!(matches!(ev, Event::ToolFinished { id: Some(ref t), .. } if t == "toolu_1"));
    }

    /// No flattened event field may reuse a key of the record.
    #[test]
    fn no_event_field_collides_with_a_record_key() {
        let s = |v: &str| v.to_owned();
        let all = [
            Event::TurnStart {
                prompt: Some(s("p")),
            },
            Event::TurnEnd,
            Event::ToolUse {
                tool: s("t"),
                input: serde_json::json!({}),
                id: Some(s("i")),
                summary: Some(s("s")),
            },
            Event::ToolFinished {
                tool: s("t"),
                id: Some(s("i")),
                summary: Some(s("s")),
                ok: true,
            },
            Event::Notification {
                message: s("m"),
                question: true,
            },
            Event::Question {
                request_id: s("r"),
                tool: Some(s("t")),
                question: s("q"),
                options: vec![s("a")],
            },
            Event::SessionStarted {
                harness_session: HarnessSessionId(s("h")),
                transcript_path: Some(PathBuf::from("/t")),
                source: Some(s("startup")),
            },
            Event::SessionEnded {
                reason: Some(s("r")),
            },
            Event::Exit { code: Some(0) },
            Event::Mail {
                message: MessageId::new(),
                from: s("f"),
                thread: ThreadId::new(),
            },
            Event::Card {
                card: s("c"),
                change: s("x"),
            },
            Event::System { message: s("m") },
            Event::Warning { message: s("w") },
            Event::StatusChanged {
                from: AgentStatus::Idle,
                to: AgentStatus::Working,
                reason: Some(s("r")),
            },
            Event::DeliveryFailed {
                text: s("t"),
                reason: s("r"),
            },
            Event::UsageLimit {
                message: s("m"),
                resets_at: Some(Utc::now()),
            },
            Event::CompactStarted {
                trigger: Some(s("manual")),
            },
            Event::CompactFinished {
                ok: false,
                trigger: None,
                message: Some(s("m")),
            },
        ];
        for ev in all {
            let v = serde_json::to_value(&ev).expect("serialise");
            for key in ["id", "seq", "agent", "at"] {
                assert!(v.get(key).is_none(), "{key} in {v}");
            }
        }
    }

    #[test]
    fn a_compaction_end_is_ok_unless_it_says_otherwise() {
        let ev: Event =
            serde_json::from_value(serde_json::json!({"type": "compact_finished"})).expect("parse");
        assert_eq!(
            ev,
            Event::CompactFinished {
                ok: true,
                trigger: None,
                message: None
            }
        );
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
