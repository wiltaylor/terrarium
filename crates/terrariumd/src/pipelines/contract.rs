//! The normal form of a pipeline run, and the JSON contract of the
//! `script` adapter (docs/api.md §31.2).
//!
//! Each adapter gives a list of [`RunInfo`]. The GitHub adapter maps the
//! GitHub Actions API to it (`super::github`); the script adapter reads it
//! from the stdout of a user script ([`parse_output`]). The store keeps
//! the runs (`crate::store::pipelines`).

use chrono::DateTime;
use serde::{Deserialize, Serialize};
use serde_json::Value;

/// The most runs that one poll gives; more are cut.
pub const MAX_RUNS_PER_POLL: usize = 100;
/// The most steps of one run; more are cut.
pub const MAX_STEPS: usize = 100;
/// The longest run ID.
pub const MAX_ID: usize = 200;
/// The longest name or title.
pub const MAX_NAME: usize = 200;
/// The longest summary.
pub const MAX_SUMMARY: usize = 2000;
/// The longest short text field (branch, commit, actor, event,
/// environment).
pub const MAX_SHORT: usize = 200;
/// The longest URL.
pub const MAX_URL: usize = 2000;

/// The status of a run or of a step, the same for every adapter.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RunStatus {
    /// Waits to start.
    Queued,
    /// Runs now.
    Running,
    /// Waits for a person: an approval, a manual step.
    Waiting,
    Success,
    Failed,
    Cancelled,
    Skipped,
    Unknown,
}

impl RunStatus {
    pub const ALL: [RunStatus; 8] = [
        Self::Queued,
        Self::Running,
        Self::Waiting,
        Self::Success,
        Self::Failed,
        Self::Cancelled,
        Self::Skipped,
        Self::Unknown,
    ];

    pub fn as_str(self) -> &'static str {
        match self {
            Self::Queued => "queued",
            Self::Running => "running",
            Self::Waiting => "waiting",
            Self::Success => "success",
            Self::Failed => "failed",
            Self::Cancelled => "cancelled",
            Self::Skipped => "skipped",
            Self::Unknown => "unknown",
        }
    }

    /// A status name, or one of its aliases, in any case. The aliases are
    /// the words of common CI/CD tools (GitHub, GitLab, Azure DevOps,
    /// Jenkins, Octopus Deploy).
    pub fn parse(s: &str) -> Option<Self> {
        let s = s.trim().to_ascii_lowercase().replace([' ', '-'], "_");
        Some(match s.as_str() {
            "queued"
            | "pending"
            | "requested"
            | "created"
            | "scheduled"
            | "not_started"
            | "notstarted"
            | "waiting_for_resource"
            | "preparing" => Self::Queued,
            "running" | "in_progress" | "inprogress" | "executing" | "started" | "cancelling"
            | "canceling" => Self::Running,
            "waiting" | "manual" | "action_required" | "approval" | "paused" | "blocked" => {
                Self::Waiting
            }
            "success" | "succeeded" | "successful" | "passed" | "ok" | "completed" | "done" => {
                Self::Success
            }
            "failed" | "failure" | "error" | "errored" | "timed_out" | "timedout" | "broken"
            | "startup_failure" => Self::Failed,
            "cancelled" | "canceled" | "aborted" | "stopped" => Self::Cancelled,
            "skipped" | "neutral" | "stale" | "not_run" => Self::Skipped,
            "unknown" => Self::Unknown,
            _ => return None,
        })
    }

    /// The run ended: it does not change any more.
    pub fn is_final(self) -> bool {
        matches!(
            self,
            Self::Success | Self::Failed | Self::Cancelled | Self::Skipped
        )
    }
}

/// One step of a run: a job, a stage, a deploy step, an environment.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Step {
    pub name: String,
    pub status: RunStatus,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub started_at: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub finished_at: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub url: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub summary: Option<String>,
}

/// One run of a pipeline, as an adapter gives it.
#[derive(Debug, Clone, PartialEq, Default, Serialize)]
pub struct RunInfo {
    /// The adapter's ID of the run, unique in the pipeline.
    pub id: String,
    pub name: Option<String>,
    pub status: Option<RunStatus>,
    pub url: Option<String>,
    pub branch: Option<String>,
    pub commit: Option<String>,
    pub actor: Option<String>,
    pub event: Option<String>,
    pub environment: Option<String>,
    pub started_at: Option<String>,
    pub finished_at: Option<String>,
    pub summary: Option<String>,
    /// `None`: the adapter did not read the steps; the store keeps the
    /// steps it has.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub steps: Option<Vec<Step>>,
}

impl RunInfo {
    pub fn status(&self) -> RunStatus {
        self.status.unwrap_or(RunStatus::Unknown)
    }
}

/// Cut `s` to at most `max` characters.
pub fn cut(s: &str, max: usize) -> String {
    match s.char_indices().nth(max) {
        None => s.to_owned(),
        Some((i, _)) => format!("{}…", &s[..i]),
    }
}

/// A URL that a page may link to: `http` or `https` only.
pub fn safe_url(s: &str) -> bool {
    let l = s.trim().to_ascii_lowercase();
    (l.starts_with("https://") || l.starts_with("http://")) && !s.chars().any(char::is_whitespace)
}

/// A time as RFC 3339 in UTC with milliseconds, or the error.
pub fn norm_time(s: &str) -> Result<String, String> {
    DateTime::parse_from_rfc3339(s.trim())
        .map(|t| {
            t.with_timezone(&chrono::Utc)
                .to_rfc3339_opts(chrono::SecondsFormat::Millis, true)
        })
        .map_err(|e| format!("`{}` is not an RFC 3339 time: {e}", cut(s, 60)))
}

// ------------------------------------------------------- the script output

/// The output of a script: an object with `runs`, or the array of runs.
#[derive(Deserialize)]
#[serde(untagged)]
enum Doc {
    Object { runs: Vec<Value> },
    List(Vec<Value>),
}

fn text_of(v: &Value, path: &str, max: usize) -> Result<Option<String>, String> {
    match v {
        Value::Null => Ok(None),
        Value::String(s) if s.trim().is_empty() => Ok(None),
        Value::String(s) => Ok(Some(cut(s.trim(), max))),
        Value::Number(n) => Ok(Some(n.to_string())),
        Value::Bool(b) => Ok(Some(b.to_string())),
        _ => Err(format!("{path}: give a string")),
    }
}

fn time_of(v: &Value, path: &str) -> Result<Option<String>, String> {
    match text_of(v, path, 100)? {
        None => Ok(None),
        Some(s) => norm_time(&s).map(Some).map_err(|e| format!("{path}: {e}")),
    }
}

fn url_of(v: &Value, path: &str) -> Result<Option<String>, String> {
    match text_of(v, path, MAX_URL)? {
        None => Ok(None),
        Some(u) if safe_url(&u) => Ok(Some(u)),
        Some(_) => Err(format!("{path}: give an http or https URL")),
    }
}

fn status_of(v: &Value, path: &str) -> Result<RunStatus, String> {
    let s = v
        .as_str()
        .ok_or_else(|| format!("{path}: give a status (queued, running, waiting, success, failed, cancelled, skipped, unknown)"))?;
    RunStatus::parse(s).ok_or_else(|| {
        format!(
            "{path}: `{}` is not a status; use queued, running, waiting, success, failed, \
             cancelled, skipped, or unknown",
            cut(s, 40)
        )
    })
}

const RUN_KEYS: [&str; 14] = [
    "id",
    "name",
    "status",
    "url",
    "branch",
    "commit",
    "actor",
    "event",
    "environment",
    "started_at",
    "finished_at",
    "summary",
    "steps",
    "number",
];
const STEP_KEYS: [&str; 6] = [
    "name",
    "status",
    "started_at",
    "finished_at",
    "url",
    "summary",
];

fn step_of(v: &Value, path: &str) -> Result<Step, String> {
    let o = v
        .as_object()
        .ok_or_else(|| format!("{path}: a step is an object"))?;
    if let Some(k) = o.keys().find(|k| !STEP_KEYS.contains(&k.as_str())) {
        return Err(format!("{path}: unknown key `{}`", cut(k, 40)));
    }
    Ok(Step {
        name: text_of(&v["name"], &format!("{path}.name"), MAX_NAME)?
            .ok_or_else(|| format!("{path}.name: a step needs a name"))?,
        status: status_of(&v["status"], &format!("{path}.status"))?,
        started_at: time_of(&v["started_at"], &format!("{path}.started_at"))?,
        finished_at: time_of(&v["finished_at"], &format!("{path}.finished_at"))?,
        url: url_of(&v["url"], &format!("{path}.url"))?,
        summary: text_of(&v["summary"], &format!("{path}.summary"), MAX_SUMMARY)?,
    })
}

fn run_of(v: &Value, path: &str) -> Result<RunInfo, String> {
    let o = v
        .as_object()
        .ok_or_else(|| format!("{path}: a run is an object"))?;
    if let Some(k) = o.keys().find(|k| !RUN_KEYS.contains(&k.as_str())) {
        return Err(format!("{path}: unknown key `{}`", cut(k, 40)));
    }
    let id = match &v["id"] {
        Value::String(s) if !s.trim().is_empty() && s.trim().chars().count() <= MAX_ID => {
            s.trim().to_owned()
        }
        Value::Number(n) => n.to_string(),
        Value::String(s) if !s.trim().is_empty() => {
            return Err(format!("{path}.id: longer than {MAX_ID} characters"));
        }
        _ => return Err(format!("{path}.id: give the run ID (a string or a number)")),
    };
    let mut name = text_of(&v["name"], &format!("{path}.name"), MAX_NAME)?;
    if name.is_none() && !v["number"].is_null() {
        name = text_of(&v["number"], &format!("{path}.number"), MAX_NAME)?.map(|n| format!("#{n}"));
    }
    let steps = match &v["steps"] {
        Value::Null => None,
        Value::Array(a) => Some(
            a.iter()
                .take(MAX_STEPS)
                .enumerate()
                .map(|(i, s)| step_of(s, &format!("{path}.steps[{i}]")))
                .collect::<Result<Vec<_>, _>>()?,
        ),
        _ => return Err(format!("{path}.steps: give an array")),
    };
    Ok(RunInfo {
        id,
        name,
        status: Some(status_of(&v["status"], &format!("{path}.status"))?),
        url: url_of(&v["url"], &format!("{path}.url"))?,
        branch: text_of(&v["branch"], &format!("{path}.branch"), MAX_SHORT)?,
        commit: text_of(&v["commit"], &format!("{path}.commit"), MAX_SHORT)?,
        actor: text_of(&v["actor"], &format!("{path}.actor"), MAX_SHORT)?,
        event: text_of(&v["event"], &format!("{path}.event"), MAX_SHORT)?,
        environment: text_of(&v["environment"], &format!("{path}.environment"), MAX_SHORT)?,
        started_at: time_of(&v["started_at"], &format!("{path}.started_at"))?,
        finished_at: time_of(&v["finished_at"], &format!("{path}.finished_at"))?,
        summary: text_of(&v["summary"], &format!("{path}.summary"), MAX_SUMMARY)?,
        steps,
    })
}

/// Read the stdout of a script adapter: one JSON document, `{"runs":
/// [...]}` or `[...]`, newest run first. A run ID that comes twice keeps
/// the first. At most [`MAX_RUNS_PER_POLL`] runs. The error names the
/// path (`runs[2].status`).
pub fn parse_output(stdout: &str) -> Result<Vec<RunInfo>, String> {
    let t = stdout.trim();
    if t.is_empty() {
        return Err("the script printed nothing; print {\"runs\": [...]} as JSON".into());
    }
    let doc: Doc = serde_json::from_str(t).map_err(|e| {
        format!("stdout is not the pipeline JSON ({{\"runs\": [...]}} or [...]): {e}")
    })?;
    let (items, prefix) = match doc {
        Doc::Object { runs } => (runs, "runs"),
        Doc::List(l) => (l, ""),
    };
    let mut out: Vec<RunInfo> = Vec::new();
    for (i, v) in items.iter().enumerate() {
        if out.len() >= MAX_RUNS_PER_POLL {
            break;
        }
        let r = run_of(v, &format!("{prefix}[{i}]"))?;
        if !out.iter().any(|o| o.id == r.id) {
            out.push(r);
        }
    }
    Ok(out)
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn statuses_and_aliases() {
        for s in RunStatus::ALL {
            assert_eq!(RunStatus::parse(s.as_str()), Some(s));
        }
        assert_eq!(RunStatus::parse("Executing"), Some(RunStatus::Running));
        assert_eq!(RunStatus::parse("in progress"), Some(RunStatus::Running));
        assert_eq!(RunStatus::parse("Canceled"), Some(RunStatus::Cancelled));
        assert_eq!(RunStatus::parse("TimedOut"), Some(RunStatus::Failed));
        assert_eq!(RunStatus::parse("Succeeded"), Some(RunStatus::Success));
        assert_eq!(
            RunStatus::parse("action_required"),
            Some(RunStatus::Waiting)
        );
        assert_eq!(RunStatus::parse("bogus"), None);
        assert!(RunStatus::Failed.is_final());
        assert!(!RunStatus::Waiting.is_final());
    }

    #[test]
    fn the_object_form_with_steps() {
        let out = json!({"runs": [
            {"id": 42, "name": "Release 1.2.3 to Production", "status": "Executing",
             "url": "https://octopus.example/app#/deployments/42",
             "environment": "Production", "branch": "main", "commit": "abc123",
             "started_at": "2026-11-03T10:00:00+10:00",
             "steps": [{"name": "Deploy web", "status": "success",
                        "finished_at": "2026-11-03T00:01:00Z"},
                       {"name": "Smoke test", "status": "running"}]},
            {"id": "41", "number": 41, "status": "failed", "summary": "step 2 failed"},
            {"id": "41", "status": "success"}
        ]})
        .to_string();
        let runs = parse_output(&out).unwrap();
        assert_eq!(runs.len(), 2, "a repeated ID keeps the first");
        let r = &runs[0];
        assert_eq!(r.id, "42");
        assert_eq!(r.status(), RunStatus::Running);
        assert_eq!(r.started_at.as_deref(), Some("2026-11-03T00:00:00.000Z"));
        assert_eq!(r.environment.as_deref(), Some("Production"));
        let steps = r.steps.as_ref().unwrap();
        assert_eq!(steps.len(), 2);
        assert_eq!(steps[1].status, RunStatus::Running);
        assert_eq!(runs[1].name.as_deref(), Some("#41"));
        assert_eq!(runs[1].status(), RunStatus::Failed);
        assert!(runs[1].steps.is_none());
    }

    #[test]
    fn the_array_form_and_errors() {
        let runs = parse_output(r#"[{"id": "a", "status": "ok"}]"#).unwrap();
        assert_eq!(runs[0].status(), RunStatus::Success);
        assert!(parse_output(r#"{"runs": []}"#).unwrap().is_empty());
        let e = |s: &str| parse_output(s).unwrap_err();
        assert!(e("").contains("printed nothing"));
        assert!(e("hello").contains("not the pipeline JSON"));
        assert!(e(r#"{"runs": [{"status": "ok"}]}"#).contains("runs[0].id"));
        assert!(e(r#"{"runs": [{"id": 1}]}"#).contains("runs[0].status"));
        assert!(e(r#"[{"id": 1, "status": "meh"}]"#).contains("[0].status: `meh`"));
        assert!(
            e(r#"[{"id": 1, "status": "ok", "url": "javascript:alert(1)"}]"#).contains("[0].url")
        );
        assert!(
            e(r#"[{"id": 1, "status": "ok", "started_at": "yesterday"}]"#)
                .contains("[0].started_at")
        );
        assert!(e(r#"[{"id": 1, "status": "ok", "colour": "red"}]"#).contains("unknown key"));
        assert!(
            e(r#"[{"id": 1, "status": "ok", "steps": [{"status": "ok"}]}]"#)
                .contains("[0].steps[0].name")
        );
        assert!(e(r#"[{"id": 1, "status": "ok", "steps": {}}]"#).contains("[0].steps"));
    }

    #[test]
    fn limits() {
        let many: Vec<Value> = (0..150)
            .map(|i| json!({"id": i, "status": "success", "name": "x".repeat(300)}))
            .collect();
        let runs = parse_output(&Value::Array(many).to_string()).unwrap();
        assert_eq!(runs.len(), MAX_RUNS_PER_POLL);
        assert_eq!(runs[0].name.as_ref().unwrap().chars().count(), MAX_NAME + 1);
        assert!(safe_url("https://x.example/a"));
        assert!(!safe_url("data:text/html,x"));
        assert!(!safe_url("https://x y"));
    }
}
