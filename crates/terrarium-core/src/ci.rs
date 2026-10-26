//! Webhooks from a forge into standard events (PLAN §12c, docs/api.md
//! §20). Pure functions: the ingest listener of the daemon checks the
//! signature, then calls [`normalize`] with the event header and the JSON
//! body, and adds the result to the event log.
//!
//! The kinds:
//!
//! - `ci.run.started`, `ci.run.succeeded`, `ci.run.failed`,
//!   `ci.run.cancelled`, `ci.run.completed` (any other conclusion), and
//!   `ci.job.failed`.
//! - `git.push`; `pr.opened`, `pr.closed`, `pr.merged`, `pr.updated`.
//! - Any other webhook: `ext.<provider>.<event>`, with its `action`.
//!
//! The payload shapes are in `docs/compat.md`, "Webhooks (GitHub,
//! Forgejo)". Every text from a payload (a branch, a title, a commit
//! message) comes from whoever can push: [`clean`] takes the control
//! characters out and cuts it, but it stays data, never a command.

use serde_json::{Map, Value, json};

/// The most characters of an event summary.
pub const MAX_SUMMARY: usize = 300;
/// The most characters of one text field from a payload.
const MAX_TEXT: usize = 200;
/// The most characters of a URL from a payload.
const MAX_URL: usize = 500;

/// The kind prefixes that an agent and a generic source may emit. The
/// other kinds (`status`, `mail`, `card`, `schedule`, ...) belong to the
/// daemon.
pub const EXTERNAL_PREFIXES: &[&str] = &["ci.", "git.", "pr.", "ext.", "agent."];

/// Is `kind` in one of the [`EXTERNAL_PREFIXES`]?
pub fn external_kind(kind: &str) -> bool {
    EXTERNAL_PREFIXES
        .iter()
        .any(|p| kind.len() > p.len() && kind.starts_with(p))
}

/// A well-formed kind: 1-100 of `a-z`, `0-9`, `_`, `-`, in segments split
/// by `.`, starting with a letter. No empty segment.
pub fn valid_kind(kind: &str) -> bool {
    !kind.is_empty()
        && kind.len() <= 100
        && kind.starts_with(|c: char| c.is_ascii_lowercase())
        && kind.split('.').all(|seg| {
            !seg.is_empty()
                && seg
                    .chars()
                    .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '_' || c == '-')
        })
}

/// Text from a webhook, safe for one line: control characters become
/// spaces, runs of spaces become one, and the text stops at `max`
/// characters (with `…`).
pub fn clean(text: &str, max: usize) -> String {
    let mut out = String::with_capacity(text.len().min(max * 4));
    let mut space = false;
    let mut n = 0;
    for c in text.trim().chars() {
        let c = if c.is_control() || c.is_whitespace() {
            ' '
        } else {
            c
        };
        if c == ' ' {
            if space {
                continue;
            }
            space = true;
        } else {
            space = false;
        }
        if n == max {
            out.pop();
            out.push('…');
            break;
        }
        out.push(c);
        n += 1;
    }
    out.trim_end().to_owned()
}

/// The sender of a webhook.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Provider {
    Github,
    Forgejo,
}

impl Provider {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Github => "github",
            Self::Forgejo => "forgejo",
        }
    }
}

/// One standard event.
#[derive(Debug, Clone, PartialEq)]
pub struct CiEvent {
    pub kind: String,
    pub summary: String,
    pub data: Value,
}

/// What a webhook becomes.
#[derive(Debug, Clone, PartialEq)]
pub enum Webhook {
    /// A ping (GitHub, at the webhook create). No event.
    Ping,
    Event(CiEvent),
    /// A known webhook that makes no event, for example a job that
    /// succeeded, or a run that waits. The reason is for the log.
    Ignored(String),
}

/// A string at a JSON pointer, cleaned.
fn text(v: &Value, ptr: &str) -> Option<String> {
    v.pointer(ptr)
        .and_then(Value::as_str)
        .map(|s| clean(s, MAX_TEXT))
        .filter(|s| !s.is_empty())
}

fn url(v: &Value, ptr: &str) -> Option<String> {
    v.pointer(ptr)
        .and_then(Value::as_str)
        .map(|s| clean(s, MAX_URL))
        .filter(|s| !s.is_empty())
}

fn int(v: &Value, ptr: &str) -> Option<i64> {
    v.pointer(ptr).and_then(Value::as_i64)
}

/// The first line of a commit message.
fn title_line(v: &Value, ptr: &str) -> Option<String> {
    v.pointer(ptr)
        .and_then(Value::as_str)
        .and_then(|m| m.lines().next())
        .map(|l| clean(l, MAX_TEXT))
        .filter(|s| !s.is_empty())
}

fn short_sha(sha: Option<&String>) -> String {
    sha.map(|s| s.chars().take(7).collect()).unwrap_or_default()
}

/// The fields that every event has: absent ones are `null`, so a trigger
/// can match on any of them.
#[derive(Debug, Default)]
struct Fields {
    action: Option<String>,
    repo: Option<String>,
    repo_url: Option<String>,
    branch: Option<String>,
    commit: Option<String>,
    run_id: Option<i64>,
    run_url: Option<String>,
    workflow: Option<String>,
    job: Option<String>,
    conclusion: Option<String>,
    attempt: Option<i64>,
    sender: Option<String>,
    pr: Option<Value>,
}

impl Fields {
    /// The repository and the sender, from the usual places.
    fn base(p: &Value, action: Option<String>) -> Self {
        Self {
            action,
            repo: text(p, "/repository/full_name"),
            repo_url: url(p, "/repository/html_url"),
            sender: text(p, "/sender/login"),
            ..Self::default()
        }
    }

    fn data(self, provider: Provider, event: &str, extra: Map<String, Value>) -> Value {
        let mut m = Map::new();
        m.insert("provider".into(), json!(provider.as_str()));
        m.insert("event".into(), json!(event));
        m.insert("action".into(), json!(self.action));
        m.insert("repo".into(), json!(self.repo));
        m.insert("repo_url".into(), json!(self.repo_url));
        m.insert("branch".into(), json!(self.branch));
        m.insert("commit".into(), json!(self.commit));
        m.insert("run_id".into(), json!(self.run_id));
        m.insert("run_url".into(), json!(self.run_url));
        m.insert("workflow".into(), json!(self.workflow));
        m.insert("job".into(), json!(self.job));
        m.insert("conclusion".into(), json!(self.conclusion));
        m.insert("attempt".into(), json!(self.attempt));
        m.insert("sender".into(), json!(self.sender));
        m.insert("pr".into(), self.pr.unwrap_or(Value::Null));
        m.extend(extra);
        Value::Object(m)
    }

    fn repo_or(&self, provider: Provider) -> String {
        self.repo
            .clone()
            .unwrap_or_else(|| provider.as_str().to_owned())
    }
}

/// The run kind of a conclusion. `None`: no event (a run that is not done).
fn run_kind(conclusion: &str) -> &'static str {
    match conclusion {
        "success" => "ci.run.succeeded",
        "failure" | "timed_out" | "startup_failure" | "error" => "ci.run.failed",
        "cancelled" => "ci.run.cancelled",
        _ => "ci.run.completed",
    }
}

fn run_event(provider: Provider, event: &str, kind: &str, f: Fields) -> Webhook {
    let verb = match kind {
        "ci.run.started" => "started".to_owned(),
        "ci.run.succeeded" => "succeeded".to_owned(),
        "ci.run.failed" => "failed".to_owned(),
        "ci.run.cancelled" => "was cancelled".to_owned(),
        _ => format!(
            "completed ({})",
            f.conclusion.as_deref().unwrap_or("no conclusion")
        ),
    };
    let summary = format!(
        "{}: {} {verb} on {} ({})",
        f.repo_or(provider),
        f.workflow.as_deref().unwrap_or("workflow"),
        f.branch
            .as_deref()
            .or(f.pr.as_ref().map(|_| "a pull request"))
            .unwrap_or("?"),
        short_sha(f.commit.as_ref())
    );
    Webhook::Event(CiEvent {
        kind: kind.to_owned(),
        summary: clean(&summary, MAX_SUMMARY),
        data: f.data(provider, event, Map::new()),
    })
}

fn job_failed(provider: Provider, event: &str, f: Fields) -> Webhook {
    let summary = format!(
        "{}: job {} of {} failed on {} ({})",
        f.repo_or(provider),
        f.job.as_deref().unwrap_or("?"),
        f.workflow.as_deref().unwrap_or("workflow"),
        f.branch
            .as_deref()
            .or(f.pr.as_ref().map(|_| "a pull request"))
            .unwrap_or("?"),
        short_sha(f.commit.as_ref())
    );
    Webhook::Event(CiEvent {
        kind: "ci.job.failed".into(),
        summary: clean(&summary, MAX_SUMMARY),
        data: f.data(provider, event, Map::new()),
    })
}

/// Turn one webhook into an event. `event` is the event header
/// (`X-GitHub-Event`, `X-Forgejo-Event`); `payload` is the JSON body.
pub fn normalize(provider: Provider, event: &str, payload: &Value) -> Webhook {
    let event = event.trim().to_ascii_lowercase();
    let action = text(payload, "/action");
    match (provider, event.as_str()) {
        (Provider::Github, "ping") => Webhook::Ping,
        (Provider::Github, "workflow_run") => github_run(payload, action),
        (Provider::Github, "workflow_job") => github_job(payload, action),
        (Provider::Forgejo, e) if e.starts_with("workflow_run_") => {
            forgejo_run(payload, &event, action)
        }
        (Provider::Forgejo, e) if e.starts_with("workflow_job_") => {
            forgejo_job(payload, &event, action)
        }
        (Provider::Forgejo, "action_run_failure" | "action_run_success" | "action_run_recover") => {
            forgejo_action_run(payload, &event, action)
        }
        (_, "status") => status(provider, payload, action),
        (_, "push") => push(provider, payload, action),
        (_, "pull_request") => pull_request(provider, payload, action),
        _ => external(provider, &event, payload, action),
    }
}

// ---------------------------------------------------------------- GitHub

fn github_pr_ref(p: &Value, ptr: &str) -> Option<Value> {
    let pr = p.pointer(ptr)?;
    Some(json!({
        "number": int(pr, "/number"),
        "head": text(pr, "/head/ref"),
        "base": text(pr, "/base/ref"),
    }))
}

fn github_run(p: &Value, action: Option<String>) -> Webhook {
    let mut f = Fields::base(p, action.clone());
    f.branch = text(p, "/workflow_run/head_branch");
    f.commit = text(p, "/workflow_run/head_sha");
    f.run_id = int(p, "/workflow_run/id");
    f.run_url = url(p, "/workflow_run/html_url");
    f.workflow = text(p, "/workflow_run/name").or_else(|| text(p, "/workflow/name"));
    f.conclusion = text(p, "/workflow_run/conclusion");
    f.attempt = int(p, "/workflow_run/run_attempt");
    f.pr = github_pr_ref(p, "/workflow_run/pull_requests/0");
    let kind = match action.as_deref() {
        Some("requested") => "ci.run.started",
        Some("completed") => run_kind(f.conclusion.as_deref().unwrap_or_default()),
        other => {
            return Webhook::Ignored(format!(
                "workflow_run `{}`: not a start or an end",
                other.unwrap_or("-")
            ));
        }
    };
    run_event(Provider::Github, "workflow_run", kind, f)
}

fn github_job(p: &Value, action: Option<String>) -> Webhook {
    let mut f = Fields::base(p, action.clone());
    f.branch = text(p, "/workflow_job/head_branch");
    f.commit = text(p, "/workflow_job/head_sha");
    f.run_id = int(p, "/workflow_job/run_id");
    f.run_url = url(p, "/workflow_job/html_url");
    f.workflow = text(p, "/workflow_job/workflow_name");
    f.job = text(p, "/workflow_job/name");
    f.conclusion = text(p, "/workflow_job/conclusion");
    f.attempt = int(p, "/workflow_job/run_attempt");
    let failed = action.as_deref() == Some("completed")
        && matches!(f.conclusion.as_deref(), Some("failure" | "timed_out"));
    if !failed {
        return Webhook::Ignored(format!(
            "workflow_job `{}` ({}): only a failed job makes an event",
            action.as_deref().unwrap_or("-"),
            f.conclusion.as_deref().unwrap_or("no conclusion")
        ));
    }
    job_failed(Provider::Github, "workflow_job", f)
}

// ---------------------------------------------------------------- Forgejo

/// The fields of a Forgejo `ActionRun` (`run` in the payload).
/// `prettyref` is the branch or tag, or `#<n>` for a pull request run.
fn forgejo_run_fields(p: &Value, action: Option<String>) -> Fields {
    let mut f = Fields {
        action,
        repo: text(p, "/run/repository/full_name").or_else(|| text(p, "/repository/full_name")),
        repo_url: url(p, "/run/repository/html_url").or_else(|| url(p, "/repository/html_url")),
        sender: text(p, "/run/trigger_user/login").or_else(|| text(p, "/sender/login")),
        ..Fields::default()
    };
    match text(p, "/run/prettyref") {
        Some(r) if r.starts_with('#') => {
            f.pr = Some(json!({"number": r[1..].parse::<i64>().ok(), "head": null, "base": null}));
        }
        other => f.branch = other,
    }
    f.commit = text(p, "/run/commit_sha");
    f.run_id = int(p, "/run/id");
    f.run_url = url(p, "/run/html_url");
    f.workflow = text(p, "/run/workflow_id");
    f.conclusion = text(p, "/run/status");
    f
}

fn forgejo_run(p: &Value, event: &str, action: Option<String>) -> Webhook {
    let f = forgejo_run_fields(p, action);
    let kind = match event.trim_start_matches("workflow_run_") {
        "running" => "ci.run.started",
        "success" => "ci.run.succeeded",
        "failure" => "ci.run.failed",
        "cancelled" => "ci.run.cancelled",
        "skipped" => "ci.run.completed",
        other => {
            return Webhook::Ignored(format!("{event}: a run that is `{other}` is not done"));
        }
    };
    run_event(Provider::Forgejo, event, kind, f)
}

fn forgejo_job(p: &Value, event: &str, action: Option<String>) -> Webhook {
    let mut f = forgejo_run_fields(p, action);
    f.job = text(p, "/job/name");
    f.conclusion = text(p, "/job/status");
    f.attempt = int(p, "/job/attempt");
    f.run_url = url(p, "/job/html_url").or(f.run_url);
    if event != "workflow_job_failure" {
        return Webhook::Ignored(format!("{event}: only a failed job makes an event"));
    }
    job_failed(Provider::Forgejo, event, f)
}

/// The Forgejo 12-16 events `action_run_failure`, `action_run_success`,
/// and `action_run_recover` (`ActionPayload`). Forgejo 17 still sends
/// them, but calls them deprecated.
fn forgejo_action_run(p: &Value, event: &str, action: Option<String>) -> Webhook {
    let f = forgejo_run_fields(p, action);
    let kind = if event == "action_run_failure" {
        "ci.run.failed"
    } else {
        "ci.run.succeeded"
    };
    run_event(Provider::Forgejo, event, kind, f)
}

// ---------------------------------------------------------------- both

/// A commit status (GitHub `status`; Gitea 1.23+ `status`). Forgejo does
/// not send this event. One status is one run of an outside CI; its
/// `context` is the workflow.
fn status(provider: Provider, p: &Value, action: Option<String>) -> Webhook {
    let mut f = Fields::base(p, action);
    f.commit = text(p, "/sha");
    f.branch = text(p, "/branches/0/name");
    f.run_url = url(p, "/target_url");
    f.workflow = text(p, "/context");
    f.conclusion = text(p, "/state");
    let kind = match f.conclusion.as_deref() {
        Some("pending") => "ci.run.started",
        Some(c) => run_kind(c),
        None => return Webhook::Ignored("status without a state".into()),
    };
    run_event(provider, "status", kind, f)
}

fn push(provider: Provider, p: &Value, action: Option<String>) -> Webhook {
    let mut f = Fields::base(p, action);
    let reference = text(p, "/ref").unwrap_or_default();
    let (branch, tag) = match (
        reference.strip_prefix("refs/heads/"),
        reference.strip_prefix("refs/tags/"),
    ) {
        (Some(b), _) => (Some(b.to_owned()), None),
        (_, Some(t)) => (None, Some(t.to_owned())),
        _ => (None, None),
    };
    f.branch = branch;
    f.commit = text(p, "/after");
    let deleted = p["deleted"].as_bool().unwrap_or(false)
        || f.commit
            .as_deref()
            .is_some_and(|c| c.chars().all(|c| c == '0'));
    let commits = p["commits"].as_array().map_or(0, Vec::len);
    let total = int(p, "/total_commits").unwrap_or(commits as i64);
    let pusher = text(p, "/pusher/login")
        .or_else(|| text(p, "/pusher/name"))
        .or_else(|| f.sender.clone());
    let message = title_line(p, "/head_commit/message");
    let to = f
        .branch
        .clone()
        .or_else(|| tag.as_ref().map(|t| format!("tag {t}")))
        .unwrap_or_else(|| reference.clone());
    let summary = if deleted {
        format!(
            "{}: {} deleted {to}",
            f.repo_or(provider),
            pusher.as_deref().unwrap_or("?")
        )
    } else {
        format!(
            "{}: {} pushed {total} commit{} to {to} ({})",
            f.repo_or(provider),
            pusher.as_deref().unwrap_or("?"),
            if total == 1 { "" } else { "s" },
            short_sha(f.commit.as_ref())
        )
    };
    let mut extra = Map::new();
    extra.insert("ref".into(), json!(reference));
    extra.insert("tag".into(), json!(tag));
    extra.insert("before".into(), json!(text(p, "/before")));
    extra.insert("deleted".into(), json!(deleted));
    extra.insert("commits".into(), json!(total));
    extra.insert("message".into(), json!(message));
    extra.insert("pusher".into(), json!(pusher));
    extra.insert(
        "compare_url".into(),
        json!(url(p, "/compare").or_else(|| url(p, "/compare_url"))),
    );
    Webhook::Event(CiEvent {
        kind: "git.push".into(),
        summary: clean(&summary, MAX_SUMMARY),
        data: f.data(provider, "push", extra),
    })
}

fn pull_request(provider: Provider, p: &Value, action: Option<String>) -> Webhook {
    let mut f = Fields::base(p, action.clone());
    let pr = &p["pull_request"];
    let merged = pr["merged"].as_bool().unwrap_or(false);
    let number = int(pr, "/number").or_else(|| int(p, "/number"));
    let title = text(pr, "/title");
    f.branch = text(pr, "/head/ref");
    f.commit = text(pr, "/head/sha");
    f.pr = Some(json!({
        "number": number,
        "title": title,
        "url": url(pr, "/html_url"),
        "head": f.branch,
        "base": text(pr, "/base/ref"),
        "author": text(pr, "/user/login"),
        "merged": merged,
        "merged_by": text(pr, "/merged_by/login"),
        "draft": pr["draft"].as_bool(),
    }));
    // GitHub says `synchronize`, Forgejo `synchronized`.
    let (kind, verb) = match action.as_deref() {
        Some("opened" | "reopened") => ("pr.opened", action.clone().unwrap_or_default()),
        Some("closed") if merged => ("pr.merged", "merged".to_owned()),
        Some("closed") => ("pr.closed", "closed".to_owned()),
        Some(
            "synchronize" | "synchronized" | "edited" | "ready_for_review" | "converted_to_draft",
        ) => ("pr.updated", "updated".to_owned()),
        _ => return external(provider, "pull_request", p, action),
    };
    let summary = format!(
        "{}: PR #{} {verb}: {}",
        f.repo_or(provider),
        number.map(|n| n.to_string()).unwrap_or_else(|| "?".into()),
        title.as_deref().unwrap_or("")
    );
    Webhook::Event(CiEvent {
        kind: kind.into(),
        summary: clean(&summary, MAX_SUMMARY),
        data: f.data(provider, "pull_request", Map::new()),
    })
}

/// `ext.<provider>.<event>`: any other webhook, with its `action`.
fn external(provider: Provider, event: &str, p: &Value, action: Option<String>) -> Webhook {
    let seg: String = event
        .chars()
        .map(|c| {
            if c.is_ascii_lowercase() || c.is_ascii_digit() || c == '_' || c == '-' {
                c
            } else {
                '_'
            }
        })
        .take(60)
        .collect();
    let seg = if seg.is_empty() {
        "unknown".into()
    } else {
        seg
    };
    let f = Fields::base(p, action.clone());
    let summary = format!(
        "{}: {} {seg}{}",
        f.repo_or(provider),
        provider.as_str(),
        action.map(|a| format!(" {a}")).unwrap_or_default()
    );
    Webhook::Event(CiEvent {
        kind: format!("ext.{}.{seg}", provider.as_str()),
        summary: clean(&summary, MAX_SUMMARY),
        data: f.data(provider, event, Map::new()),
    })
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::*;

    fn fixture(rel: &str) -> Value {
        let path = format!("{}/tests/fixtures/ci/{rel}", env!("CARGO_MANIFEST_DIR"));
        serde_json::from_str(&std::fs::read_to_string(&path).unwrap()).unwrap()
    }

    fn event(provider: Provider, header: &str, rel: &str) -> CiEvent {
        match normalize(provider, header, &fixture(rel)) {
            Webhook::Event(e) => e,
            other => panic!("{rel}: {other:?}"),
        }
    }

    fn ignored(provider: Provider, header: &str, rel: &str) {
        let w = normalize(provider, header, &fixture(rel));
        assert!(matches!(w, Webhook::Ignored(_)), "{rel}: {w:?}");
    }

    const GH: Provider = Provider::Github;
    const FJ: Provider = Provider::Forgejo;

    #[test]
    fn kinds_and_cleaning() {
        assert!(valid_kind("ci.run.failed"));
        assert!(valid_kind("ext.github.check_run"));
        assert!(!valid_kind("ci..failed"));
        assert!(!valid_kind("ci.run."));
        assert!(!valid_kind("Ci.run"));
        assert!(!valid_kind("1ci"));
        assert!(!valid_kind(""));
        assert!(!valid_kind("ci run"));
        assert!(external_kind("ci.run.failed") && external_kind("agent.note"));
        assert!(!external_kind("status") && !external_kind("mail") && !external_kind("ci."));
        assert!(!external_kind("schedule") && !external_kind("memory.created"));
        assert_eq!(clean("a\n\tb   c\u{1b}[31m", 100), "a b c [31m");
        assert_eq!(clean("abcdef", 4), "abc…");
        assert_eq!(clean("  x  ", 4), "x");
    }

    #[test]
    fn github_workflow_runs() {
        let e = event(GH, "workflow_run", "github/workflow_run_failure.json");
        assert_eq!(e.kind, "ci.run.failed");
        let d = &e.data;
        assert_eq!(d["provider"], "github");
        assert_eq!(d["repo"], "octo-org/hello-world");
        assert_eq!(d["repo_url"], "https://github.com/octo-org/hello-world");
        assert_eq!(d["branch"], "feature/login");
        assert_eq!(d["commit"], "acb5820ced9479c074f688cc328bf03f341a511d");
        assert_eq!(d["run_id"], 30433642);
        assert_eq!(
            d["run_url"],
            "https://github.com/octo-org/hello-world/actions/runs/30433642"
        );
        assert_eq!(d["workflow"], "CI");
        assert_eq!(d["conclusion"], "failure");
        assert_eq!(d["attempt"], 1);
        assert_eq!(d["sender"], "octocat");
        assert_eq!(d["action"], "completed");
        assert!(d["pr"].is_null() && d["job"].is_null());
        assert_eq!(
            e.summary,
            "octo-org/hello-world: CI failed on feature/login (acb5820)"
        );
        for (file, kind) in [
            ("workflow_run_success.json", "ci.run.succeeded"),
            ("workflow_run_cancelled.json", "ci.run.cancelled"),
            ("workflow_run_skipped.json", "ci.run.completed"),
            ("workflow_run_requested.json", "ci.run.started"),
        ] {
            assert_eq!(
                event(GH, "workflow_run", &format!("github/{file}")).kind,
                kind
            );
        }
        ignored(GH, "workflow_run", "github/workflow_run_in_progress.json");
    }

    #[test]
    fn github_jobs_and_status() {
        let e = event(GH, "workflow_job", "github/workflow_job_failure.json");
        assert_eq!(e.kind, "ci.job.failed");
        assert_eq!(e.data["job"], "test (ubuntu-latest)");
        assert_eq!(e.data["workflow"], "CI");
        assert_eq!(e.data["run_id"], 30433642);
        assert!(e.summary.contains("job test (ubuntu-latest) of CI failed"));
        ignored(GH, "workflow_job", "github/workflow_job_success.json");
        ignored(GH, "workflow_job", "github/workflow_job_queued.json");

        let e = event(GH, "status", "github/status_failure.json");
        assert_eq!(e.kind, "ci.run.failed");
        assert_eq!(e.data["workflow"], "ci/woodpecker/push");
        assert_eq!(e.data["branch"], "feature/login");
        assert_eq!(e.data["run_url"], "https://ci.example.com/builds/1234");
    }

    #[test]
    fn github_push_pr_ping_and_others() {
        let e = event(GH, "push", "github/push.json");
        assert_eq!(e.kind, "git.push");
        assert_eq!(e.data["branch"], "main");
        assert_eq!(e.data["commits"], 1);
        assert_eq!(e.data["message"], "Add the login form");
        assert_eq!(e.data["pusher"], "mona");
        assert_eq!(e.data["deleted"], false);
        assert!(
            e.data["compare_url"]
                .as_str()
                .unwrap()
                .contains("/compare/")
        );
        assert_eq!(
            e.summary,
            "octo-org/hello-world: mona pushed 1 commit to main (acb5820)"
        );

        let e = event(GH, "pull_request", "github/pull_request_opened.json");
        assert_eq!(e.kind, "pr.opened");
        assert_eq!(e.data["pr"]["number"], 42);
        assert_eq!(e.data["pr"]["base"], "main");
        assert_eq!(e.data["branch"], "feature/login");
        assert_eq!(
            e.summary,
            "octo-org/hello-world: PR #42 opened: Add the login form"
        );
        let e = event(GH, "pull_request", "github/pull_request_merged.json");
        assert_eq!(e.kind, "pr.merged");
        assert_eq!(e.data["pr"]["merged_by"], "octocat");
        assert_eq!(
            event(GH, "pull_request", "github/pull_request_closed.json").kind,
            "pr.closed"
        );
        assert_eq!(
            event(GH, "pull_request", "github/pull_request_synchronize.json").kind,
            "pr.updated"
        );
        let e = event(GH, "pull_request", "github/pull_request_labeled.json");
        assert_eq!(e.kind, "ext.github.pull_request");
        assert_eq!(e.data["action"], "labeled");

        assert_eq!(
            normalize(GH, "ping", &fixture("github/ping.json")),
            Webhook::Ping
        );
        let e = event(GH, "issues", "github/issues_opened.json");
        assert_eq!(e.kind, "ext.github.issues");
        assert_eq!(e.data["action"], "opened");
        assert_eq!(e.summary, "octo-org/hello-world: github issues opened");
        // A strange header stays one safe kind segment.
        let e = event(GH, "Weird Event!", "github/issues_opened.json");
        assert_eq!(e.kind, "ext.github.weird_event_");
        assert!(valid_kind(&e.kind));
    }

    #[test]
    fn forgejo_runs_and_jobs() {
        let e = event(
            FJ,
            "workflow_run_failure",
            "forgejo/workflow_run_failure.json",
        );
        assert_eq!(e.kind, "ci.run.failed");
        let d = &e.data;
        assert_eq!(d["provider"], "forgejo");
        assert_eq!(d["repo"], "org/app");
        assert_eq!(d["repo_url"], "https://forgejo.example/org/app");
        assert_eq!(d["branch"], "main");
        assert_eq!(d["commit"], "9f2c1e0d4b6a8c0e2f4a6b8c0d2e4f6a8b0c2d4e");
        assert_eq!(d["run_id"], 4711);
        assert_eq!(
            d["run_url"],
            "https://forgejo.example/org/app/actions/runs/42"
        );
        assert_eq!(d["workflow"], "ci.yml");
        assert_eq!(d["conclusion"], "failure");
        assert_eq!(d["sender"], "wil");
        assert_eq!(d["action"], "run_completed");
        assert_eq!(e.summary, "org/app: ci.yml failed on main (9f2c1e0)");
        for (header, kind) in [
            ("workflow_run_success", "ci.run.succeeded"),
            ("workflow_run_cancelled", "ci.run.cancelled"),
            ("workflow_run_skipped", "ci.run.completed"),
            ("workflow_run_running", "ci.run.started"),
        ] {
            let file = format!("forgejo/{header}.json");
            assert_eq!(event(FJ, header, &file).kind, kind);
        }
        ignored(
            FJ,
            "workflow_run_waiting",
            "forgejo/workflow_run_waiting.json",
        );

        // A pull request run: `prettyref` is `#7`, not a branch.
        let e = event(
            FJ,
            "workflow_run_failure",
            "forgejo/workflow_run_failure_pr.json",
        );
        assert!(e.data["branch"].is_null());
        assert_eq!(e.data["pr"]["number"], 7);
        assert!(e.summary.contains("on a pull request"));

        let e = event(
            FJ,
            "workflow_job_failure",
            "forgejo/workflow_job_failure.json",
        );
        assert_eq!(e.kind, "ci.job.failed");
        assert_eq!(e.data["job"], "test");
        assert_eq!(e.data["attempt"], 2);
        assert_eq!(
            e.data["run_url"],
            "https://forgejo.example/org/app/actions/runs/42/jobs/1/attempt/2"
        );
        ignored(
            FJ,
            "workflow_job_success",
            "forgejo/workflow_job_success.json",
        );

        assert_eq!(
            event(FJ, "action_run_failure", "forgejo/action_run_failure.json").kind,
            "ci.run.failed"
        );
        let e = event(FJ, "action_run_success", "forgejo/action_run_recover.json");
        assert_eq!(e.kind, "ci.run.succeeded");
        assert_eq!(e.data["action"], "recover");
        assert_eq!(
            event(FJ, "action_run_recover", "forgejo/action_run_recover.json").kind,
            "ci.run.succeeded"
        );
    }

    #[test]
    fn forgejo_push_and_prs() {
        let e = event(FJ, "push", "forgejo/push.json");
        assert_eq!(e.kind, "git.push");
        assert_eq!(e.data["branch"], "main");
        assert_eq!(e.data["pusher"], "wil");
        assert_eq!(e.data["commits"], 1);
        assert!(
            e.data["compare_url"]
                .as_str()
                .unwrap()
                .starts_with("https://forgejo.example/org/app/compare/")
        );
        assert_eq!(e.summary, "org/app: wil pushed 1 commit to main (9f2c1e0)");

        let e = event(FJ, "pull_request", "forgejo/pull_request_opened.json");
        assert_eq!(e.kind, "pr.opened");
        assert_eq!(e.data["pr"]["number"], 7);
        assert_eq!(e.data["pr"]["author"], "wil");
        assert_eq!(
            event(FJ, "pull_request", "forgejo/pull_request_merged.json").kind,
            "pr.merged"
        );
        assert_eq!(
            event(FJ, "pull_request", "forgejo/pull_request_closed.json").kind,
            "pr.closed"
        );
        assert_eq!(
            event(FJ, "pull_request", "forgejo/pull_request_synchronized.json").kind,
            "pr.updated"
        );
        // Forgejo has no ping: an unknown event is ext.
        assert_eq!(
            event(FJ, "release", "forgejo/push.json").kind,
            "ext.forgejo.release"
        );
    }

    #[test]
    fn branch_deletion_and_tags() {
        let mut p = fixture("github/push.json");
        p["after"] = json!("0000000000000000000000000000000000000000");
        p["deleted"] = json!(true);
        let Webhook::Event(e) = normalize(GH, "push", &p) else {
            panic!()
        };
        assert_eq!(e.data["deleted"], true);
        assert_eq!(e.summary, "octo-org/hello-world: mona deleted main");
        let mut p = fixture("forgejo/push.json");
        p["ref"] = json!("refs/tags/v1.2.0");
        let Webhook::Event(e) = normalize(FJ, "push", &p) else {
            panic!()
        };
        assert!(e.data["branch"].is_null());
        assert_eq!(e.data["tag"], "v1.2.0");
        assert!(e.summary.contains("to tag v1.2.0"));
    }

    #[test]
    fn text_from_the_payload_is_one_clean_line() {
        let mut p = fixture("github/workflow_run_failure.json");
        p["workflow_run"]["head_branch"] = json!("evil\nbranch\u{7}; rm -rf /");
        p["workflow_run"]["name"] = json!("x".repeat(1000));
        let Webhook::Event(e) = normalize(GH, "workflow_run", &p) else {
            panic!()
        };
        assert_eq!(e.data["branch"], "evil branch ; rm -rf /");
        assert_eq!(
            e.data["workflow"].as_str().unwrap().chars().count(),
            MAX_TEXT
        );
        assert!(!e.summary.contains('\n'));
        assert!(e.summary.chars().count() <= MAX_SUMMARY);
    }
}
