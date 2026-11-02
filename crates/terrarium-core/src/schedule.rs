//! Schedules (docs/api.md §19): a time spec and one action that the daemon
//! runs on that time. The daemon parses and checks the spec (`cron` and
//! the IANA zones live there); this module holds the shapes that the API,
//! the definition files, and fleet files share.
//!
//! A trigger (PLAN §12c) is a schedule whose spec is `on`: an event from
//! the log starts its run. [`ScheduleSpec::matches_event`] and [`glob`]
//! are the pure part of the match; the daemon keeps the dedupe and
//! escalation counters.

use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::template::{EventView, value_text};
pub use crate::webhook::WebhookAction;

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
    /// A trigger: an event of the log that matches starts a run.
    On {
        /// Kind globs, for example `ci.run.failed` or `ci.job.*`.
        kinds: Vec<String>,
        /// Every rule must hold.
        #[serde(default, rename = "match", skip_serializing_if = "Vec::is_empty")]
        matches: Vec<MatchRule>,
        /// One run per rendered key in a window.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        dedupe: Option<Dedupe>,
        /// After `after` matches in a row for one key, mail `to` once
        /// instead of the action.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        escalate: Option<Escalate>,
        /// The most runs in one hour; more matches are `skipped: rate
        /// limit`.
        #[serde(default = "default_max_per_hour")]
        max_per_hour: u32,
    },
}

/// The default `max_per_hour` of a trigger.
pub const DEFAULT_MAX_PER_HOUR: u32 = 30;

fn default_max_per_hour() -> u32 {
    DEFAULT_MAX_PER_HOUR
}

fn utc() -> String {
    "UTC".into()
}

impl ScheduleSpec {
    /// `cron`, `interval`, `at`, or `on`.
    pub fn kind(&self) -> &'static str {
        match self {
            Self::Cron { .. } => "cron",
            Self::Interval { .. } => "interval",
            Self::At { .. } => "at",
            Self::On { .. } => "on",
        }
    }

    /// Is it a trigger (`on`)?
    pub fn is_trigger(&self) -> bool {
        matches!(self, Self::On { .. })
    }

    /// Does the event match the kinds and every `match` rule? A time spec
    /// matches no event. The scope, the guards, the dedupe, and the
    /// escalation are the daemon's.
    pub fn matches_event(&self, e: &EventView<'_>) -> bool {
        match self {
            Self::On { kinds, matches, .. } => {
                kinds.iter().any(|k| glob(k, e.kind)) && matches.iter().all(|m| m.holds(e))
            }
            _ => false,
        }
    }
}

/// One `match` rule of a trigger: the value at `path` against `value`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct MatchRule {
    /// `project`, `kind`, `actor`, `summary`, or `data.<dotted path>`.
    pub path: String,
    #[serde(default)]
    pub op: MatchOp,
    /// A string for `eq`, `ne`, and `glob`; a list for `in`.
    pub value: Value,
}

/// The compare of a [`MatchRule`].
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum MatchOp {
    /// The value as text is `value` as text.
    #[default]
    Eq,
    /// Not `eq`. A missing value is not equal.
    Ne,
    /// `value` is a glob (`*`, `?`) over the value as text.
    Glob,
    /// The value as text is one of the list `value`.
    In,
}

impl MatchOp {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Eq => "eq",
            Self::Ne => "ne",
            Self::Glob => "glob",
            Self::In => "in",
        }
    }
}

impl MatchRule {
    /// Check the rule: a known path, and a value of the right shape.
    pub fn check(&self) -> Result<(), String> {
        let p = self.path.as_str();
        let known = matches!(p, "project" | "kind" | "actor" | "summary")
            || p.strip_prefix("data.")
                .is_some_and(|d| !d.is_empty() && d.split('.').all(|s| !s.is_empty()));
        if !known {
            return Err(format!(
                "bad path `{p}`: use project, kind, actor, summary, or data.<path>"
            ));
        }
        match (self.op, &self.value) {
            (MatchOp::In, Value::Array(items)) if !items.is_empty() => Ok(()),
            (MatchOp::In, _) => Err(format!("`in` on `{p}` needs a list of values")),
            (MatchOp::Glob, Value::String(s)) if !s.is_empty() => Ok(()),
            (MatchOp::Glob, _) => Err(format!("`glob` on `{p}` needs a pattern")),
            (_, Value::Array(_) | Value::Object(_)) => {
                Err(format!("`{}` on `{p}` needs one value", self.op.as_str()))
            }
            _ => Ok(()),
        }
    }

    /// Does the rule hold for the event?
    pub fn holds(&self, e: &EventView<'_>) -> bool {
        let have = e
            .get(&self.path)
            .filter(|v| !v.is_null())
            .map(|v| value_text(&v));
        let want = value_text(&self.value);
        match self.op {
            MatchOp::Eq => have.is_some_and(|h| h == want),
            MatchOp::Ne => have.is_none_or(|h| h != want),
            MatchOp::Glob => have.is_some_and(|h| glob(&want, &h)),
            MatchOp::In => have.is_some_and(|h| {
                self.value
                    .as_array()
                    .is_some_and(|a| a.iter().any(|v| value_text(v) == h))
            }),
        }
    }
}

/// One run per rendered `key` in `window`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Dedupe {
    /// A template, for example `{{event.data.repo}}:{{event.data.branch}}`.
    pub key: String,
    /// A duration: `90s`, `10m`, `1h`.
    pub window: String,
}

/// The escalation of a trigger: see [`ScheduleSpec::On`].
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Escalate {
    /// A template; the count is per rendered key.
    pub key: String,
    /// The matches in a row that escalate (1 or more).
    pub after: u32,
    /// Kind globs whose events with the same key set the count to zero.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub reset_on: Vec<String>,
    /// An agent, `project:<name>`, or `operator` (the default).
    #[serde(default = "operator")]
    pub to: String,
}

fn operator() -> String {
    "operator".into()
}

/// A glob over the whole text: `*` is any run of characters, `?` is one.
pub fn glob(pattern: &str, text: &str) -> bool {
    let p: Vec<char> = pattern.chars().collect();
    let t: Vec<char> = text.chars().collect();
    let (mut pi, mut ti) = (0, 0);
    // The last `*` and the text position it matched up to.
    let mut star: Option<(usize, usize)> = None;
    while ti < t.len() {
        if pi < p.len() && (p[pi] == '?' || p[pi] == t[ti]) {
            pi += 1;
            ti += 1;
        } else if pi < p.len() && p[pi] == '*' {
            star = Some((pi, ti));
            pi += 1;
        } else if let Some((sp, st)) = star {
            pi = sp + 1;
            ti = st + 1;
            star = Some((sp, st + 1));
        } else {
            return false;
        }
    }
    while pi < p.len() && p[pi] == '*' {
        pi += 1;
    }
    pi == p.len()
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
    /// Open a card on the project's board, or comment on the open card
    /// with the same key; or close (move to done) the open card.
    Card {
        op: CardOp,
        /// The card's trigger key (a template): one open card per key.
        key: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        title: Option<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        body: Option<String>,
        /// `open`: the column of a new card (default `backlog`). `close`:
        /// the column to move to (default `done`).
        #[serde(default, skip_serializing_if = "Option::is_none")]
        column: Option<String>,
    },
    /// Send an HTTP request about the event (docs/api.md §19.5). The
    /// daemon queues a delivery and sends it with retries.
    Webhook(Box<WebhookAction>),
}

/// The `op` of a `card` action.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum CardOp {
    Open,
    Close,
}

impl CardOp {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Open => "open",
            Self::Close => "close",
        }
    }
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
            Self::Card { .. } => "card",
            Self::Webhook(_) => "webhook",
        }
    }

    /// May a trigger (spec `on`) run this action? Only `mail`, `exec`,
    /// `card`, and `webhook`: a trigger notifies an agent by mail, never
    /// with a turn.
    pub fn allowed_on_trigger(&self) -> bool {
        matches!(
            self,
            Self::Mail { .. } | Self::Exec { .. } | Self::Card { .. } | Self::Webhook(_)
        )
    }

    /// The strings of the action that take `{{event.*}}` templates, with
    /// their field names.
    pub fn templated(&self) -> Vec<(&'static str, &str)> {
        match self {
            Self::Mail { subject, body, .. } => {
                vec![
                    ("action.subject", subject.as_str()),
                    ("action.body", body.as_str()),
                ]
            }
            Self::Exec { cmd, .. } => cmd.iter().map(|c| ("action.cmd", c.as_str())).collect(),
            Self::Card {
                key, title, body, ..
            } => {
                let mut v = vec![("action.key", key.as_str())];
                if let Some(t) = title {
                    v.push(("action.title", t.as_str()));
                }
                if let Some(b) = body {
                    v.push(("action.body", b.as_str()));
                }
                v
            }
            _ => Vec::new(),
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

    fn ev(data: &serde_json::Value) -> EventView<'_> {
        EventView {
            seq: 3,
            kind: "ci.run.failed",
            summary: "CI failed",
            project: Some("terra"),
            actor: Some("source:terra/gh"),
            data,
        }
    }

    #[test]
    fn globs() {
        assert!(glob("ci.run.failed", "ci.run.failed"));
        assert!(glob("ci.*", "ci.run.failed"));
        assert!(glob("ci.*.failed", "ci.job.failed"));
        assert!(glob("*", ""));
        assert!(glob("ci.?ob.*", "ci.job.failed"));
        assert!(glob("release/*", "release/1.2"));
        assert!(glob("*a*b*", "xxaxxbxx"));
        assert!(!glob("ci.run", "ci.run.failed"));
        assert!(!glob("ci.*.failed", "ci.run.succeeded"));
        assert!(!glob("main", "main2"));
        assert!(!glob("?", ""));
    }

    #[test]
    fn trigger_spec_shapes_and_matching() {
        let s: ScheduleSpec = serde_json::from_value(serde_json::json!({
            "kind": "on",
            "kinds": ["ci.run.failed", "ci.job.*"],
            "match": [
                {"path": "data.branch", "op": "glob", "value": "main*"},
                {"path": "data.repo", "value": "wil/terrarium"},
                {"path": "data.attempt", "op": "in", "value": [1, 2]},
                {"path": "data.sender", "op": "ne", "value": "bot"}
            ],
            "dedupe": {"key": "{{event.data.repo}}", "window": "10m"},
            "escalate": {"key": "{{event.data.repo}}", "after": 3, "reset_on": ["ci.run.succeeded"]}
        }))
        .unwrap();
        assert_eq!(s.kind(), "on");
        assert!(s.is_trigger());
        let ScheduleSpec::On {
            max_per_hour,
            escalate,
            ..
        } = &s
        else {
            panic!("not on")
        };
        assert_eq!(*max_per_hour, DEFAULT_MAX_PER_HOUR);
        assert_eq!(escalate.as_ref().unwrap().to, "operator");
        let good = serde_json::json!({"branch": "main", "repo": "wil/terrarium", "attempt": 2});
        assert!(s.matches_event(&ev(&good)));
        // Each rule can fail the match.
        for (k, v) in [
            ("branch", serde_json::json!("dev")),
            ("repo", serde_json::json!("other/repo")),
            ("attempt", serde_json::json!(3)),
            ("sender", serde_json::json!("bot")),
        ] {
            let mut d = good.clone();
            d[k] = v;
            assert!(!s.matches_event(&ev(&d)), "{k}");
        }
        // A missing value fails eq, glob, and in; it passes ne.
        let mut d = good.clone();
        d.as_object_mut().unwrap().remove("branch");
        assert!(!s.matches_event(&ev(&d)));
        let mut other = ev(&good);
        other.kind = "ci.run.succeeded";
        assert!(!s.matches_event(&other));
        other.kind = "ci.job.failed";
        assert!(s.matches_event(&other));
        // A time spec matches no event.
        let t = ScheduleSpec::Interval { every: "1h".into() };
        assert!(!t.matches_event(&ev(&good)));
        // The rules are checked.
        let rule = |p: &str, op: MatchOp, v: serde_json::Value| MatchRule {
            path: p.into(),
            op,
            value: v,
        };
        assert!(
            rule("data.x", MatchOp::Eq, serde_json::json!("a"))
                .check()
                .is_ok()
        );
        assert!(
            rule("project", MatchOp::In, serde_json::json!(["a"]))
                .check()
                .is_ok()
        );
        assert!(
            rule("agent", MatchOp::Eq, serde_json::json!("a"))
                .check()
                .is_err()
        );
        assert!(
            rule("data.", MatchOp::Eq, serde_json::json!("a"))
                .check()
                .is_err()
        );
        assert!(
            rule("kind", MatchOp::In, serde_json::json!("a"))
                .check()
                .is_err()
        );
        assert!(
            rule("kind", MatchOp::Glob, serde_json::json!(1))
                .check()
                .is_err()
        );
        assert!(
            rule("kind", MatchOp::Eq, serde_json::json!(["a"]))
                .check()
                .is_err()
        );
        // A trigger in a project file, as TOML.
        let d: ScheduleDef = toml::from_str(
            r#"
name = "ci-failed"
catch_up = "once"
spec = { kind = "on", kinds = ["ci.run.failed"], match = [{ path = "data.branch", op = "glob", value = "main" }], max_per_hour = 10 }
action = { type = "card", op = "open", key = "{{event.data.repo}}", title = "CI failed: {{event.data.repo}}" }
"#,
        )
        .unwrap();
        assert!(d.spec.is_trigger());
        assert_eq!(d.action.kind(), "card");
        assert!(d.action.allowed_on_trigger());
        let back: ScheduleDef = toml::from_str(&toml::to_string(&d).unwrap()).unwrap();
        assert_eq!(back, d);
        let turn: ScheduleAction =
            serde_json::from_value(serde_json::json!({"type": "turn", "agent": "w", "text": "x"}))
                .unwrap();
        assert!(!turn.allowed_on_trigger());
        // A webhook, in JSON and in TOML.
        let w: ScheduleAction = serde_json::from_value(serde_json::json!(
            {"type": "webhook", "url": "https://hooks.example/x", "format": "slack"}
        ))
        .unwrap();
        assert_eq!(w.kind(), "webhook");
        assert!(w.allowed_on_trigger());
        let ScheduleAction::Webhook(a) = &w else {
            panic!("not webhook")
        };
        assert_eq!(a.format, crate::webhook::WebhookFormat::Slack);
        assert_eq!(serde_json::to_value(&w).unwrap()["type"], "webhook");
        assert!(
            serde_json::from_value::<ScheduleAction>(
                serde_json::json!({"type": "webhook", "url": "https://x", "bogus": 1})
            )
            .is_err()
        );
        let d: ScheduleDef = toml::from_str(
            r#"
name = "page"
spec = { kind = "on", kinds = ["monitor.status"] }
action = { type = "webhook", url = "https://ntfy.example/alerts", format = "ntfy" }
"#,
        )
        .unwrap();
        assert_eq!(d.action.kind(), "webhook");
        let back: ScheduleDef = toml::from_str(&toml::to_string(&d).unwrap()).unwrap();
        assert_eq!(back, d);
    }
}
