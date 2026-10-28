//! `/api/repos/...`: checkouts into agent workspaces and the git
//! credential helper (PLAN §9, §17, §21). `GET|POST
//! /api/projects/{name}/repos` stay in `projects.rs`. The secrets that the
//! repos name are in `crate::secrets` (PLAN §16b); `/api/credentials` is
//! in `secrets.rs`.

use std::collections::{HashMap, VecDeque};
use std::path::{Component, Path as FsPath};
use std::sync::{Mutex, OnceLock};
use std::time::{Duration, Instant};

use axum::extract::{Path, State};
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use axum::{Json, Router};
use serde::{Deserialize, Serialize};
use serde_json::json;

use super::{ApiJson, Shared};
use crate::actor::Actor;
use crate::error::{ApiError, ApiResult, Code};
use crate::events::EventDraft;
use crate::git::{self, Checkout, GitError, Outcome};
use crate::policy;
use crate::secrets::{self, RunDirs, Secret};
use crate::store::agents::{self, AgentView};
use crate::store::repos::{self, Repo};

pub fn routes() -> Router<Shared> {
    Router::new()
        .route("/repos/credential", post(credential_helper))
        .route("/repos/{id}", get(show))
        .route("/repos/{id}/checkout", post(checkout))
}

/// The credential helper answers at most this many times per agent in
/// [`HELPER_WINDOW`].
const HELPER_LIMIT: usize = 30;
const HELPER_WINDOW: Duration = Duration::from_secs(60);

/// Progress events: at most one per phase in this time.
const PROGRESS_EVERY: Duration = Duration::from_millis(1000);

async fn show(
    State(state): State<Shared>,
    actor: Actor,
    Path(id): Path<String>,
) -> ApiResult<Json<Repo>> {
    let context = actor.project().map(str::to_owned);
    let r = state
        .db
        .read(move |c| repos::resolve(c, &id, context.as_deref()))
        .await?;
    policy::read_project(&actor, &r.project)?;
    Ok(Json(r))
}

/// The body of `POST /api/repos/{id}/checkout`.
#[derive(Debug, Clone, Default, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CheckoutBody {
    /// A target name: the checkout runs on the target (its host agent, or
    /// ssh), not in a workspace.
    #[serde(default)]
    pub target: Option<String>,
    #[serde(default)]
    pub branch: Option<String>,
    /// The directory in the workspace. The default is the repo's `path`.
    #[serde(default)]
    pub path: Option<String>,
    #[serde(default)]
    pub depth: Option<u32>,
    /// The agent whose workspace gets the checkout. An agent caller
    /// defaults to itself; the operator names one.
    #[serde(default)]
    pub agent: Option<String>,
}

/// The answer of a checkout.
#[derive(Debug, Clone, Serialize)]
pub struct CheckoutResult {
    pub repo: String,
    pub repo_id: String,
    pub project: String,
    pub agent: String,
    pub agent_id: String,
    /// The directory under the workspace (`$HOME/work/<path>` in the
    /// sandbox).
    pub path: String,
    #[serde(flatten)]
    pub outcome: Outcome,
}

fn check_dir(dir: &str) -> ApiResult<()> {
    let p = FsPath::new(dir);
    let ok = !dir.is_empty()
        && !dir.starts_with('.')
        && p.components().count() == 1
        && matches!(p.components().next(), Some(Component::Normal(_)));
    if ok {
        Ok(())
    } else {
        Err(ApiError::invalid(format!(
            "bad checkout path `{dir}`: one directory name under the workspace"
        )))
    }
}

fn git_error(e: GitError) -> ApiError {
    match e {
        GitError::Refused(m) => ApiError::conflict(m),
        GitError::Failed {
            command,
            status,
            stderr,
        } => ApiError::invalid(format!("git {command} failed ({status})"))
            .with_details(json!({"command": command, "status": status, "stderr": stderr})),
        GitError::Timeout(d) => {
            ApiError::internal(format!("git did not finish in {}s", d.as_secs()))
        }
        GitError::Io(m) => ApiError::internal(m),
    }
}

fn checkout_event(a: &AgentView, summary: String, data: serde_json::Value) -> EventDraft {
    EventDraft::new("repo.checkout", summary, data)
        .agent(a.agent.id.to_string(), a.agent.name.clone())
        .project(a.agent.project.clone())
}

/// Record a failed checkout as a `repo.checkout` event. Best effort.
pub async fn record_failure(state: &Shared, a: &AgentView, repo: &str, err: &ApiError) {
    let draft = checkout_event(
        a,
        format!(
            "{}: checkout of {repo} failed: {}",
            a.agent.name, err.message
        ),
        json!({"repo": repo, "phase": "error", "error": err.message, "details": err.details}),
    );
    if let Err(e) = state.db.write("system", move |t| t.emit(draft)).await {
        tracing::warn!("cannot record a checkout failure: {e}");
    }
}

/// Clone or update `repo` in the workspace of `agent`, outside the sandbox.
/// Progress lines and the result are `repo.checkout` events; the result is
/// also an audit row. A failure is an event too.
pub async fn run_checkout(
    state: &Shared,
    actor: &Actor,
    repo: &Repo,
    agent: &AgentView,
    body: &CheckoutBody,
) -> ApiResult<CheckoutResult> {
    let result = checkout_inner(state, actor, repo, agent, body).await;
    if let Err(e) = &result {
        record_failure(state, agent, &repo.name, e).await;
    }
    result
}

async fn checkout_inner(
    state: &Shared,
    actor: &Actor,
    repo: &Repo,
    agent: &AgentView,
    body: &CheckoutBody,
) -> ApiResult<CheckoutResult> {
    let dir = body.path.clone().unwrap_or_else(|| repo.path.clone());
    check_dir(&dir)?;
    // The repo's secret: the project's first, then the global one.
    let secret = secrets::secret_of(state, repo.owner(), repo.credential.as_deref()).await?;
    // The workspace moves to the work root first (an agent made before it).
    let live = state
        .supervisor
        .status(agent.agent.id)
        .await
        .is_some_and(terrarium_core::AgentStatus::is_live);
    let workspace =
        crate::workspace::ensure(&state.state_dir, &state.work_dir, &agent.agent, !live)
            .ok_or_else(|| {
                ApiError::internal(format!("cannot create {}", agent.agent.workspace.display()))
            })?;
    let c = Checkout {
        url: repo.url.clone(),
        dest: workspace.join(&dir),
        branch: body.branch.clone().or_else(|| repo.default_branch.clone()),
        depth: body.depth,
        secret: secret.as_ref().map(|(_, s)| s.clone()),
        store: RunDirs::new(&state.state_dir),
        deny_local_under: vec![state.state_dir.clone(), state.work_dir.clone()],
        timeout: git::DEFAULT_TIMEOUT,
    };

    let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel::<String>();
    let pump = {
        let state = state.clone();
        let a = agent.clone();
        let repo_name = repo.name.clone();
        let repo_id = repo.id.clone();
        let dir = dir.clone();
        tokio::spawn(async move {
            let mut last: Option<(String, Instant)> = None;
            while let Some(line) = rx.recv().await {
                let phase = line.split(':').next().unwrap_or("").to_owned();
                let due = match &last {
                    Some((p, at)) => *p != phase || at.elapsed() >= PROGRESS_EVERY,
                    None => true,
                };
                if !due {
                    continue;
                }
                last = Some((phase, Instant::now()));
                let draft = checkout_event(
                    &a,
                    format!("{}: {repo_name}: {line}", a.agent.name),
                    json!({"repo": repo_name, "repo_id": repo_id, "path": dir,
                           "phase": "progress", "line": line}),
                );
                if let Err(e) = state.db.write("system", move |t| t.emit(draft)).await {
                    tracing::warn!("cannot record checkout progress: {e}");
                }
            }
        })
    };
    let outcome = git::clone_or_update_async(c, tx).await;
    let _ = pump.await;
    let outcome = outcome.map_err(git_error)?;

    let result = CheckoutResult {
        repo: repo.name.clone(),
        repo_id: repo.id.clone(),
        project: repo.project.clone(),
        agent: agent.agent.name.clone(),
        agent_id: agent.agent.id.to_string(),
        path: dir,
        outcome,
    };
    let draft = checkout_event(
        agent,
        format!(
            "{}: {} {} at {}",
            agent.agent.name,
            repo.name,
            json!(result.outcome.action)
                .as_str()
                .unwrap_or("checked out"),
            result.outcome.head.as_deref().unwrap_or("-")
        ),
        json!({"phase": "done", "result": result}),
    );
    let audit = json!({
        "agent": result.agent_id,
        "path": result.path,
        "action": result.outcome.action,
        "head": result.outcome.head,
        "branch": result.outcome.branch,
        "credential": secret.map(|(name, _)| name),
    });
    let target = format!("{}/{}", repo.project, repo.name);
    let project = repo.project.clone();
    state
        .db
        .write(actor.label(), move |t| {
            t.emit(draft)?;
            t.audit("repo.checkout", &target, Some(&project), audit)
        })
        .await?;
    Ok(result)
}

async fn checkout(
    State(state): State<Shared>,
    actor: Actor,
    Path(id): Path<String>,
    ApiJson(body): ApiJson<CheckoutBody>,
) -> ApiResult<Response> {
    let context = actor.project().map(str::to_owned);
    let repo = state
        .db
        .read(move |c| repos::resolve(c, &id, context.as_deref()))
        .await?;
    policy::read_project(&actor, &repo.project)?;
    if let Some(target) = body.target.clone() {
        return Ok(
            Json(checkout_target(&state, &actor, &repo, &target, &body).await?).into_response(),
        );
    }
    let key = match (&actor, &body.agent) {
        (_, Some(k)) => k.clone(),
        (Actor::Agent(a), None) => a.id.clone(),
        (Actor::Operator, None) => {
            return Err(ApiError::invalid(
                "name the agent whose workspace gets the checkout: body `agent`",
            ));
        }
    };
    let context = Some(repo.project.clone());
    let agent = state
        .db
        .read(move |c| agents::find(c, &key, context.as_deref()))
        .await?;
    policy::checkout_into(
        &actor,
        &repo.project,
        &agent.agent.id.to_string(),
        &agent.agent.project,
    )?;
    Ok(Json(run_checkout(&state, &actor, &repo, &agent, &body).await?).into_response())
}

/// Clone or update `repo` on a target (PLAN §9): the run happens on the
/// host agent (or over ssh), with the repo credential in the env of that
/// one run. The actor must be allowed to use the target, and the target
/// is global or in the repo's project.
async fn checkout_target(
    state: &Shared,
    actor: &Actor,
    repo: &Repo,
    target: &str,
    body: &CheckoutBody,
) -> ApiResult<serde_json::Value> {
    let row = crate::executor::usable(state, actor, target).await?;
    if let Some(p) = &row.def.project
        && *p != repo.project
    {
        return Err(ApiError::forbidden(format!(
            "target `{target}` belongs to project `{p}`; the repo is in `{}`",
            repo.project
        )));
    }
    let dir = body.path.clone().unwrap_or_else(|| repo.path.clone());
    let secret = secrets::secret_of(state, repo.owner(), repo.credential.as_deref()).await?;
    let branch = body.branch.clone().or_else(|| repo.default_branch.clone());
    let event = |summary: String, data: serde_json::Value| {
        let d = EventDraft::new("repo.checkout", summary, data).project(repo.project.clone());
        match actor.agent() {
            Some(a) => d.agent(a.id.clone(), a.name.clone()),
            None => d,
        }
    };
    let out = crate::executor::checkout_on_target(
        state,
        actor,
        &row,
        &repo.url,
        &dir,
        branch.as_deref(),
        body.depth,
        secret.as_ref().map(|(_, s)| s),
    )
    .await;
    let out = match out {
        Ok(v) => v,
        Err(e) => {
            let draft = event(
                format!(
                    "checkout of {} on {target} failed: {}",
                    repo.name, e.message
                ),
                json!({"repo": repo.name, "target": target, "phase": "error",
                       "error": e.message, "details": e.details}),
            );
            if let Err(w) = state.db.write("system", move |t| t.emit(draft)).await {
                tracing::warn!("cannot record a checkout failure: {w}");
            }
            return Err(e);
        }
    };
    let mut result = json!({
        "repo": repo.name, "repo_id": repo.id, "project": repo.project, "target": target,
        "path": dir, "remote_path": out["path"], "action": out["action"],
        "head": out["head"], "branch": out["branch"],
    });
    if let Some(n) = out.get("note") {
        result["note"] = n.clone();
    }
    let draft = event(
        format!(
            "{} {} on {target} at {}",
            repo.name,
            result["action"].as_str().unwrap_or("checked out"),
            result["head"].as_str().unwrap_or("-")
        ),
        json!({"phase": "done", "result": result}),
    );
    let audit = json!({
        "target": target, "path": dir, "action": result["action"], "head": result["head"],
        "branch": result["branch"], "credential": secret.map(|(name, _)| name),
    });
    let audit_target = format!("{}/{}", repo.project, repo.name);
    let project = repo.project.clone();
    state
        .db
        .write(actor.label(), move |t| {
            t.emit(draft)?;
            t.audit("repo.checkout", &audit_target, Some(&project), audit)
        })
        .await?;
    Ok(result)
}

/// The body of `POST /api/repos/credential`: what git sends to a
/// credential helper.
#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct HelperQuery {
    pub protocol: String,
    pub host: String,
    #[serde(default)]
    pub path: Option<String>,
}

/// The answer of the credential helper. The password is zeroed on drop
/// (PLAN §16b.2); the serialized body is not.
#[derive(Clone, Serialize)]
pub struct HelperAnswer {
    pub username: String,
    #[serde(serialize_with = "crate::secrets::serialize_zeroizing")]
    pub password: zeroize::Zeroizing<String>,
}

impl std::fmt::Debug for HelperAnswer {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("HelperAnswer")
            .field("username", &self.username)
            .finish_non_exhaustive()
    }
}

/// A sliding window per agent.
fn rate_ok(agent: &str) -> bool {
    static SEEN: OnceLock<Mutex<HashMap<String, VecDeque<Instant>>>> = OnceLock::new();
    let map = SEEN.get_or_init(Default::default);
    let mut map = match map.lock() {
        Ok(m) => m,
        Err(p) => p.into_inner(),
    };
    let now = Instant::now();
    let q = map.entry(agent.to_owned()).or_default();
    while q
        .front()
        .is_some_and(|t| now.duration_since(*t) > HELPER_WINDOW)
    {
        q.pop_front();
    }
    if q.len() >= HELPER_LIMIT {
        return false;
    }
    q.push_back(now);
    true
}

async fn credential_helper(
    State(state): State<Shared>,
    actor: Actor,
    ApiJson(q): ApiJson<HelperQuery>,
) -> ApiResult<Json<HelperAnswer>> {
    let me = policy::credential_helper(&actor)?.clone();
    if !rate_ok(&me.id) {
        return Err(ApiError::new(
            Code::RateLimited,
            format!(
                "the credential helper answers at most {HELPER_LIMIT} times a minute per agent"
            ),
        ));
    }
    let none = || ApiError::not_found("no credential for this host and path in your project");
    let protocol = q.protocol.to_ascii_lowercase();
    let host = q.host.to_ascii_lowercase();
    // First a repo of the project (git sends the path with
    // `credential.useHttpPath`), then a granted `token` or `github_token`
    // for the host (PLAN §16b.4).
    let found = match q.path.as_deref().map(git::normalize_path) {
        Some(path) => {
            let project = me.project.clone();
            let all = state
                .db
                .read(move |c| repos::all_for_project(c, &project))
                .await?;
            all.into_iter().find(|r| {
                r.credential.is_some()
                    && git::http_location(&r.url)
                        .is_some_and(|l| l.protocol == protocol && l.host == host && l.path == path)
            })
        }
        None => None,
    };
    let Some(repo) = found else {
        if protocol != "https" {
            return Err(none());
        }
        let granted =
            super::secret_grants::helper_answer(&state, &actor, &me.id, &me.project, &host).await?;
        let Some((username, token)) = granted else {
            return Err(none());
        };
        return Ok(Json(HelperAnswer {
            username,
            password: token,
        }));
    };
    let name = repo.credential.clone().unwrap_or_default();
    // A `token` or a `github_token` of the project, else a global one.
    let Ok(Secret::Token { username, token }) = secrets::load(&state, repo.owner(), &name).await
    else {
        return Err(none());
    };
    let target = format!("{}/{}", repo.project, repo.name);
    let detail = json!({"agent": me.id, "host": host, "credential": name});
    let project = repo.project.clone();
    state
        .db
        .write(actor.label(), move |t| {
            t.audit("repo.credential", &target, Some(&project), detail)
        })
        .await?;
    Ok(Json(HelperAnswer {
        username,
        password: token,
    }))
}

/// Clone each repo of the project's `checkout.on_agent_create` list (the
/// project definition file) into the new agent's workspace. Each entry of
/// the answer is a [`CheckoutResult`] or `{repo, error}`.
pub async fn checkout_on_create(
    state: &Shared,
    actor: &Actor,
    agent: &AgentView,
) -> Vec<serde_json::Value> {
    let names: Vec<String> = state
        .definitions()
        .projects
        .iter()
        .find(|p| p.name == agent.agent.project)
        .map(|p| p.checkout.on_agent_create.clone())
        .unwrap_or_default();
    let mut out = Vec::new();
    for name in names {
        let project = agent.agent.project.clone();
        let n = name.clone();
        let found = state
            .db
            .read(move |c| repos::find_by_name(c, &project, &n))
            .await;
        let result = match found {
            Ok(Some(repo)) => {
                run_checkout(state, actor, &repo, agent, &CheckoutBody::default()).await
            }
            Ok(None) => {
                let e = ApiError::invalid(format!(
                    "checkout.on_agent_create names `{name}`, which is not a repo of the project"
                ));
                record_failure(state, agent, &name, &e).await;
                Err(e)
            }
            Err(e) => {
                record_failure(state, agent, &name, &e).await;
                Err(e)
            }
        };
        out.push(match result {
            Ok(r) => serde_json::to_value(&r).unwrap_or_else(|_| json!({"repo": name})),
            Err(e) => json!({"repo": name, "error": e.message, "details": e.details}),
        });
    }
    out
}
