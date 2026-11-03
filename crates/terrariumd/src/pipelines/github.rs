//! The `github` adapter (docs/api.md §31.3): the runs of GitHub Actions
//! from the REST API.
//!
//! - The list: `GET {api}/repos/{owner}/{repo}/actions/runs`, or
//!   `.../actions/workflows/{workflow}/runs` with a workflow, with
//!   `per_page` = `max_runs`, `branch`, and `exclude_pull_requests`.
//! - The steps of a run are its jobs: `GET
//!   {api}/repos/{owner}/{repo}/actions/runs/{id}/jobs`. A poll reads them
//!   for the runs that changed (at most [`JOBS_PER_POLL`]); the run detail
//!   reads them when the store has none.
//!
//! The token is the main value of a secret (`token_secret`), sent as
//! `Authorization: Bearer`. Without one, GitHub allows 60 requests an
//! hour from one address, public repos only.
//!
//! The requests keep to the address rules of the webhooks
//! (`TERRARIUM_WEBHOOK_PRIVATE`, `crate::webhooks::net`): one DNS answer
//! for each request, no proxy, no redirect.

use std::net::SocketAddr;
use std::time::Duration;

use reqwest::Url;
use serde_json::Value;

use super::contract::{self, RunInfo, RunStatus, Step};
use crate::webhooks::net::{self, PrivateMode, Resolver};

/// The API of github.com.
pub const DEFAULT_API: &str = "https://api.github.com";
/// The time limit of one request.
pub const TIMEOUT: Duration = Duration::from_secs(20);
/// The most bytes of an answer that a request reads.
pub const READ_MAX: usize = 4 * 1024 * 1024;
/// The most runs whose jobs one poll reads.
pub const JOBS_PER_POLL: usize = 5;

/// The address rules of a request.
#[derive(Clone, Default)]
pub struct NetRules {
    pub mode: PrivateMode,
    pub own_ports: Vec<u16>,
    pub resolver: Option<Resolver>,
}

/// The repo and the API of a `github` source.
pub struct Api<'a> {
    pub base: &'a str,
    pub repo: &'a str,
    pub token: Option<&'a str>,
    pub rules: &'a NetRules,
}

/// Check `owner/name`.
pub fn check_repo(repo: &str) -> Result<(), String> {
    let ok = |s: &str| {
        !s.is_empty()
            && s.len() <= 100
            && s.chars()
                .all(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '.'))
            && s != "."
            && s != ".."
    };
    match repo.split_once('/') {
        Some((o, n)) if ok(o) && ok(n) => Ok(()),
        _ => Err(format!(
            "`{}` is not `owner/name` (a-z, 0-9, `-`, `_`, `.`)",
            contract::cut(repo, 120)
        )),
    }
}

/// Check the API base URL: `http` or `https`, no user, no query.
pub fn check_api(base: &str) -> Result<Url, String> {
    let u = net::check_url(base)?;
    if u.query().is_some() || u.fragment().is_some() {
        return Err("give the API base URL with no query".into());
    }
    Ok(u)
}

impl Api<'_> {
    fn url(&self, tail: &[&str], query: &[(&str, String)]) -> Result<Url, String> {
        let mut u = check_api(self.base)?;
        let (owner, name) = self
            .repo
            .split_once('/')
            .ok_or_else(|| "the repo is not owner/name".to_owned())?;
        {
            let mut seg = u
                .path_segments_mut()
                .map_err(|()| "the API URL cannot take a path".to_owned())?;
            seg.pop_if_empty().extend(["repos", owner, name]);
            seg.extend(tail);
        }
        if !query.is_empty() {
            let mut q = u.query_pairs_mut();
            for (k, v) in query {
                q.append_pair(k, v);
            }
        }
        Ok(u)
    }

    /// The newest runs, newest first.
    pub async fn runs(
        &self,
        workflow: Option<&str>,
        branch: Option<&str>,
        per_page: usize,
    ) -> Result<Vec<RunInfo>, String> {
        let mut query = vec![
            ("per_page", per_page.clamp(1, 100).to_string()),
            ("exclude_pull_requests", "true".to_owned()),
        ];
        if let Some(b) = branch {
            query.push(("branch", b.to_owned()));
        }
        let u = match workflow {
            Some(w) => self.url(&["actions", "workflows", w, "runs"], &query)?,
            None => self.url(&["actions", "runs"], &query)?,
        };
        let v = get_json(u, self.token, self.rules).await?;
        let list = v["workflow_runs"]
            .as_array()
            .ok_or_else(|| "the GitHub answer has no `workflow_runs`".to_owned())?;
        Ok(list.iter().filter_map(map_run).collect())
    }

    /// The jobs of run `id`, as steps.
    pub async fn jobs(&self, id: &str) -> Result<Vec<Step>, String> {
        let u = self.url(
            &["actions", "runs", id, "jobs"],
            &[("per_page", "100".to_owned())],
        )?;
        let v = get_json(u, self.token, self.rules).await?;
        Ok(map_jobs(&v))
    }
}

fn s(v: &Value) -> Option<String> {
    v.as_str()
        .filter(|x| !x.is_empty())
        .map(|x| contract::cut(x, contract::MAX_SHORT))
}

fn time(v: &Value) -> Option<String> {
    v.as_str().and_then(|t| contract::norm_time(t).ok())
}

fn url(v: &Value) -> Option<String> {
    v.as_str()
        .filter(|u| contract::safe_url(u))
        .map(str::to_owned)
}

/// The status of a run or a job: `status`, then `conclusion` when it is
/// `completed`.
pub fn status_of(status: &Value, conclusion: &Value) -> RunStatus {
    match status.as_str() {
        Some("completed") => conclusion
            .as_str()
            .and_then(RunStatus::parse)
            .unwrap_or(RunStatus::Unknown),
        Some(st) => RunStatus::parse(st).unwrap_or(RunStatus::Unknown),
        None => RunStatus::Unknown,
    }
}

/// One workflow run of the API.
pub fn map_run(v: &Value) -> Option<RunInfo> {
    let id = v["id"].as_u64()?.to_string();
    let status = status_of(&v["status"], &v["conclusion"]);
    let name = match (v["name"].as_str(), v["run_number"].as_u64()) {
        (Some(n), Some(k)) => Some(format!("{n} #{k}")),
        (Some(n), None) => Some(n.to_owned()),
        (None, Some(k)) => Some(format!("#{k}")),
        (None, None) => None,
    }
    .map(|n| contract::cut(&n, contract::MAX_NAME));
    let actor = s(&v["triggering_actor"]["login"]).or_else(|| s(&v["actor"]["login"]));
    Some(RunInfo {
        id,
        name,
        status: Some(status),
        url: url(&v["html_url"]),
        branch: s(&v["head_branch"]),
        commit: s(&v["head_sha"]),
        actor,
        event: s(&v["event"]),
        environment: None,
        started_at: time(&v["run_started_at"]).or_else(|| time(&v["created_at"])),
        finished_at: if status.is_final() {
            time(&v["updated_at"])
        } else {
            None
        },
        summary: v["display_title"]
            .as_str()
            .map(|t| contract::cut(t, contract::MAX_SUMMARY)),
        steps: None,
    })
}

/// The jobs of a run (`{"jobs": [...]}`) as steps.
pub fn map_jobs(v: &Value) -> Vec<Step> {
    v["jobs"]
        .as_array()
        .map(|jobs| {
            jobs.iter()
                .take(contract::MAX_STEPS)
                .map(|j| Step {
                    name: j["name"].as_str().map_or_else(
                        || "job".to_owned(),
                        |n| contract::cut(n, contract::MAX_NAME),
                    ),
                    status: status_of(&j["status"], &j["conclusion"]),
                    started_at: time(&j["started_at"]),
                    finished_at: time(&j["completed_at"]),
                    url: url(&j["html_url"]),
                    summary: None,
                })
                .collect()
        })
        .unwrap_or_default()
}

/// The deepest cause of a reqwest error, with no URL.
fn source_text(e: &reqwest::Error) -> String {
    let mut s: &dyn std::error::Error = e;
    while let Some(next) = s.source() {
        s = next;
    }
    s.to_string()
}

/// `GET` one API URL and read the JSON. The error is a sentence for the
/// operator, with no token.
async fn get_json(u: Url, token: Option<&str>, rules: &NetRules) -> Result<Value, String> {
    let addr: SocketAddr =
        net::resolve(&u, rules.mode, &rules.own_ports, rules.resolver.clone()).await?;
    let mut b = reqwest::Client::builder()
        .timeout(TIMEOUT)
        .connect_timeout(TIMEOUT)
        .redirect(reqwest::redirect::Policy::none())
        .no_proxy()
        .user_agent(concat!("terrarium/", env!("CARGO_PKG_VERSION")));
    if let Some(url::Host::Domain(d)) = u.host() {
        b = b.resolve(d, SocketAddr::new(addr.ip(), 0));
    }
    let client = b
        .build()
        .map_err(|e| format!("cannot make the HTTP client: {e}"))?;
    let mut r = client
        .get(u.clone())
        .header("Accept", "application/vnd.github+json")
        .header("X-GitHub-Api-Version", "2022-11-28");
    if let Some(t) = token {
        r = r.bearer_auth(t);
    }
    let mut resp = r.send().await.map_err(|e| {
        if e.is_timeout() {
            format!("GitHub did not answer in {} s", TIMEOUT.as_secs())
        } else {
            format!("cannot reach {}: {}", net::log_url(&u), source_text(&e))
        }
    })?;
    let status = resp.status();
    let remaining = resp
        .headers()
        .get("x-ratelimit-remaining")
        .and_then(|v| v.to_str().ok())
        .map(str::to_owned);
    let reset = resp
        .headers()
        .get("x-ratelimit-reset")
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.parse::<i64>().ok())
        .and_then(|t| chrono::DateTime::from_timestamp(t, 0));
    let mut body: Vec<u8> = Vec::new();
    loop {
        match resp.chunk().await {
            Ok(Some(c)) => {
                if body.len() + c.len() > READ_MAX {
                    return Err(format!(
                        "the GitHub answer is larger than {} MiB",
                        READ_MAX / 1024 / 1024
                    ));
                }
                body.extend_from_slice(&c);
            }
            Ok(None) => break,
            Err(e) => {
                return Err(format!(
                    "reading the GitHub answer failed: {}",
                    source_text(&e)
                ));
            }
        }
    }
    let json: Option<Value> = serde_json::from_slice(&body).ok();
    if status.is_success() {
        return json.ok_or_else(|| "the GitHub answer is not JSON".to_owned());
    }
    let msg = json
        .as_ref()
        .and_then(|j| j["message"].as_str())
        .map(|m| format!(": {}", contract::cut(m, 200)))
        .unwrap_or_default();
    let code = status.as_u16();
    Err(match code {
        401 => format!("GitHub refused the token (401{msg}); check the secret of token_secret"),
        403 | 429 if remaining.as_deref() == Some("0") => format!(
            "GitHub rate limit ({code}){}; give a token_secret for a higher limit, or poll less often",
            reset
                .map(|t| format!(
                    ", it resets at {}",
                    t.to_rfc3339_opts(chrono::SecondsFormat::Secs, true)
                ))
                .unwrap_or_default()
        ),
        404 => format!(
            "GitHub has no such repo or workflow (404{msg}); a private repo needs a token_secret"
        ),
        c if (300..400).contains(&c) => format!(
            "GitHub answered with a redirect ({c}); the repo may have moved: give its new name"
        ),
        c => format!("GitHub answered {c}{msg}"),
    })
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn repos_and_urls() {
        check_repo("wiltaylor/terrarium").unwrap();
        check_repo("a.b/c_d-e").unwrap();
        for bad in ["terrarium", "a/b/c", "/x", "a/..", "a b/c", ""] {
            assert!(check_repo(bad).is_err(), "{bad}");
        }
        let rules = NetRules::default();
        let api = Api {
            base: "https://ghe.example/api/v3/",
            repo: "o/r",
            token: None,
            rules: &rules,
        };
        let u = api
            .url(
                &["actions", "workflows", "ci.yml", "runs"],
                &[("branch", "feat/x y".into())],
            )
            .unwrap();
        assert_eq!(
            u.as_str(),
            "https://ghe.example/api/v3/repos/o/r/actions/workflows/ci.yml/runs?branch=feat%2Fx+y"
        );
        assert!(check_api("https://api.github.com?x=1").is_err());
        assert!(check_api("ftp://x").is_err());
    }

    #[test]
    fn runs_and_jobs_map() {
        let run = json!({"id": 99, "name": "CI", "run_number": 12, "status": "completed",
            "conclusion": "failure", "html_url": "https://github.com/o/r/actions/runs/99",
            "head_branch": "main", "head_sha": "deadbeef", "event": "push",
            "display_title": "Fix the build", "actor": {"login": "wil"},
            "run_started_at": "2026-11-03T00:00:00Z", "updated_at": "2026-11-03T00:05:00Z"});
        let r = map_run(&run).unwrap();
        assert_eq!(r.id, "99");
        assert_eq!(r.name.as_deref(), Some("CI #12"));
        assert_eq!(r.status(), RunStatus::Failed);
        assert_eq!(r.actor.as_deref(), Some("wil"));
        assert_eq!(r.summary.as_deref(), Some("Fix the build"));
        assert_eq!(r.finished_at.as_deref(), Some("2026-11-03T00:05:00.000Z"));
        let mut live = run.clone();
        live["status"] = json!("in_progress");
        live["conclusion"] = Value::Null;
        let r = map_run(&live).unwrap();
        assert_eq!(r.status(), RunStatus::Running);
        assert!(r.finished_at.is_none());
        live["status"] = json!("waiting");
        assert_eq!(map_run(&live).unwrap().status(), RunStatus::Waiting);
        assert!(map_run(&json!({"name": "no id"})).is_none());
        let jobs = map_jobs(&json!({"jobs": [
            {"name": "build", "status": "completed", "conclusion": "success",
             "started_at": "2026-11-03T00:00:00Z", "completed_at": "2026-11-03T00:01:00Z",
             "html_url": "https://github.com/o/r/actions/runs/99/job/1"},
            {"name": "test", "status": "queued", "conclusion": null}]}));
        assert_eq!(jobs.len(), 2);
        assert_eq!(jobs[0].status, RunStatus::Success);
        assert_eq!(jobs[1].status, RunStatus::Queued);
        assert_eq!(
            status_of(&json!("completed"), &json!("timed_out")),
            RunStatus::Failed
        );
        assert_eq!(
            status_of(&json!("completed"), &json!("cancelled")),
            RunStatus::Cancelled
        );
        assert_eq!(
            status_of(&json!("completed"), &json!("skipped")),
            RunStatus::Skipped
        );
    }
}
