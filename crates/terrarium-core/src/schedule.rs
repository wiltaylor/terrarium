//! Schedules (docs/api.md §19): a time spec and one action that the daemon
//! runs on that time. The daemon parses and checks the spec (`cron` and
//! the IANA zones live there); this module holds the shapes that the API,
//! the definition files, and fleet files share.

use serde::{Deserialize, Serialize};

/// When a schedule runs.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "lowercase", deny_unknown_fields)]
pub enum ScheduleSpec {
    /// A five-field cron expression (`*/15 * * * *`) or `@hourly`,
    /// `@daily`, `@weekly`, `@monthly`, `@yearly`, in the zone `tz`.
    Cron {
        expr: String,
        /// `UTC` or an IANA zone, for example `Europe/London`.
        #[serde(default = "utc")]
        tz: String,
    },
    /// A fixed interval: `90s`, `30m`, `2h`, `1d`, or a sum like `1h30m`.
    Interval { every: String },
    /// One run at an RFC 3339 time.
    At { at: String },
}

fn utc() -> String {
    "UTC".into()
}

/// What a schedule does when it runs.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "lowercase", deny_unknown_fields)]
pub enum ScheduleAction {
    /// Send mail as the schedule's `run_as` actor.
    Mail {
        /// An agent, `project:<name>`, or `operator`.
        to: String,
        #[serde(default)]
        subject: String,
        body: String,
        /// Post every run on this thread. The loop guard counts these
        /// messages as any other mail on the thread.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        thread: Option<String>,
    },
    /// Type a turn into an agent, as `POST /api/agents/{id}/send`.
    Turn {
        agent: String,
        text: String,
        /// Start a stopped agent first. Without it, a stopped agent makes
        /// the run `skipped`.
        #[serde(default, skip_serializing_if = "is_false")]
        start_if_stopped: bool,
    },
    /// Run a command on a target through the executor.
    Exec {
        target: String,
        cmd: Vec<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        cwd: Option<String>,
        /// The run limit: default 300, at most 3600.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        timeout_secs: Option<u64>,
    },
    /// Start, stop, or restart an agent.
    Agent { agent: String, verb: AgentVerb },
    /// Run the quota probe of a subscription.
    Probe { subscription: String },
}

fn is_false(b: &bool) -> bool {
    !*b
}

impl ScheduleAction {
    /// The `type` word.
    pub fn kind(&self) -> &'static str {
        match self {
            Self::Mail { .. } => "mail",
            Self::Turn { .. } => "turn",
            Self::Exec { .. } => "exec",
            Self::Agent { .. } => "agent",
            Self::Probe { .. } => "probe",
        }
    }
}

/// The verb of an `agent` action.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum AgentVerb {
    Start,
    Stop,
    Restart,
}

impl AgentVerb {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Start => "start",
            Self::Stop => "stop",
            Self::Restart => "restart",
        }
    }
}

/// What the daemon does at start with a run that it missed while it was
/// down.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum CatchUp {
    /// Compute the next run from now; the missed runs do not happen.
    #[default]
    Skip,
    /// One run (trigger `catch_up`), then the next from now.
    Once,
}

impl CatchUp {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Skip => "skip",
            Self::Once => "once",
        }
    }
}

fn yes() -> bool {
    true
}

fn is_true(b: &bool) -> bool {
    *b
}

/// A schedule in a definition file (`[[schedules]]` in
/// `projects/<name>.toml`) or a fleet file (`[[projects.schedules]]`).
/// The project is the enclosing project.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ScheduleDef {
    pub name: String,
    pub spec: ScheduleSpec,
    pub action: ScheduleAction,
    #[serde(default = "yes", skip_serializing_if = "is_true")]
    pub enabled: bool,
    #[serde(default, skip_serializing_if = "is_skip")]
    pub catch_up: CatchUp,
}

fn is_skip(c: &CatchUp) -> bool {
    *c == CatchUp::Skip
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::*;

    #[test]
    fn toml_and_json_shapes() {
        let d: ScheduleDef = toml::from_str(
            r#"
name = "standup"
catch_up = "once"
spec = { kind = "cron", expr = "0 9 * * MON-FRI", tz = "Europe/London" }
action = { type = "mail", to = "lead", subject = "standup", body = "post your status" }
"#,
        )
        .unwrap();
        assert_eq!(d.catch_up, CatchUp::Once);
        assert!(d.enabled);
        assert_eq!(
            d.spec,
            ScheduleSpec::Cron {
                expr: "0 9 * * MON-FRI".into(),
                tz: "Europe/London".into()
            }
        );
        let j = serde_json::to_value(&d.action).unwrap();
        assert_eq!(j["type"], "mail");
        let a: ScheduleAction = serde_json::from_value(serde_json::json!(
            {"type": "exec", "target": "box", "cmd": ["make", "test"], "timeout_secs": 60}
        ))
        .unwrap();
        assert_eq!(a.kind(), "exec");
        let s: ScheduleSpec =
            serde_json::from_value(serde_json::json!({"kind": "cron", "expr": "* * * * *"}))
                .unwrap();
        assert_eq!(
            s,
            ScheduleSpec::Cron {
                expr: "* * * * *".into(),
                tz: "UTC".into()
            }
        );
        // Unknown fields are errors, so a typo does not pass quietly.
        assert!(
            serde_json::from_value::<ScheduleSpec>(
                serde_json::json!({"kind": "interval", "every": "1h", "evry": "x"})
            )
            .is_err()
        );
        assert!(
            serde_json::from_value::<ScheduleAction>(
                serde_json::json!({"type": "agent", "agent": "w", "verb": "pause"})
            )
            .is_err()
        );
    }
}
