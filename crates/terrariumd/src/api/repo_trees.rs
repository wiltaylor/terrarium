//! `/api/repos/{id}/trees/...`: the repo browser (PLAN §9a, docs/api.md
//! §5.3). The trees of a repo are `main` (the daemon's bare mirror of the
//! repo's default branch) and the checkout of the repo in each agent
//! workspace of the project (`agent:<ULID>`). `crate::repo_browse` reads
//! them; this module finds them, refreshes the mirror, and shapes the
//! answers.
//!
//! The mirror is `<state>/repo-mirrors/<repo ULID>.git`. The daemon makes
//! it at the first request for `main`, and fetches every branch into it
//! when it is older than [`MIRROR_MAX_AGE`] (or at most each
//! [`MIRROR_MIN_GAP`] with `?refresh=true`). The fetch uses the repo's
//! secret as a checkout does (`crate::git`), outside every sandbox. No
//! agent can write the mirror.
//!
//! An agent tree is opened once for each request ([`CheckoutDir`]), and
//! git reads it only in the read sandbox (`crate::git::ReadGit`). With no
//! usable `bwrap`, a read of an agent tree is `503 unavailable`.
//!
//! An error answer has no stderr of a read command and no path of the
//! daemon ([`browse_error`], [`scrub`]): the API callers include agents.

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::{Arc, Mutex, OnceLock};
use std::time::{Duration, Instant};

use axum::extract::{Path, State};
use axum::http::{HeaderValue, header};
use axum::response::{IntoResponse, Response};
use axum::routing::get;
use axum::{Json, Router};
use serde::{Deserialize, Serialize};
use serde_json::json;

use super::repos::git_error;
use super::{ApiQuery, NEXT_CURSOR, Shared};
use crate::actor::Actor;
use crate::error::{ApiError, ApiResult, Code};
use crate::git::{self, Checkout, CheckoutDir, GitError, ReadGit};
use crate::policy;
use crate::repo_browse::{self as browse, BrowseError, LineBudget, Tree};
use crate::secrets::{self, RunDirs};
use crate::store::Page;
use crate::store::agents;
use crate::store::repos::{self, Repo};

pub fn routes() -> Router<Shared> {
    Router::new()
        .route("/repos/{id}/trees", get(trees))
        .route("/repos/{id}/trees/{tree}/files", get(files))
        .route("/repos/{id}/trees/{tree}/file", get(file))
        .route("/repos/{id}/trees/{tree}/diff", get(diff))
        .route("/repos/{id}/trees/{tree}/commits", get(commits))
        .route("/repos/{id}/trees/{tree}/commits/{sha}", get(commit))
}

/// A fetch into the mirror at most this often on a read.
pub const MIRROR_MAX_AGE: Duration = Duration::from_secs(60);
/// ... and at most this often on `?refresh=true`.
pub const MIRROR_MIN_GAP: Duration = Duration::from_secs(5);
/// The longest a mirror clone or fetch may take.
const MIRROR_TIMEOUT: Duration = Duration::from_secs(300);

/// The last fetch of one mirror.
#[derive(Debug, Clone, Default)]
struct MirrorState {
    at: Option<Instant>,
    fetched_at: Option<String>,
    error: Option<String>,
}

type MirrorLock = Arc<tokio::sync::Mutex<MirrorState>>;

/// One lock for each mirror, so that two requests do not fetch at once.
fn mirror_lock(repo: &str) -> MirrorLock {
    static LOCKS: OnceLock<Mutex<HashMap<String, MirrorLock>>> = OnceLock::new();
    let map = LOCKS.get_or_init(Default::default);
    let mut map = match map.lock() {
        Ok(m) => m,
        Err(p) => p.into_inner(),
    };
    map.entry(repo.to_owned()).or_default().clone()
}

/// `<state>/repo-mirrors/<repo ULID>.git`.
pub fn mirror_dir(state: &Shared, repo: &Repo) -> PathBuf {
    state
        .state_dir
        .join("repo-mirrors")
        .join(format!("{}.git", repo.id))
}

/// The mirror of `repo`, fetched when it is old (or `force`). A failed
/// fetch keeps the old mirror and says so in `error`; with no mirror, it
/// is the error of the request.
pub(crate) async fn ensure_mirror(
    state: &Shared,
    repo: &Repo,
    force: bool,
) -> ApiResult<MirrorView> {
    let dest = mirror_dir(state, repo);
    let lock = mirror_lock(&repo.id);
    let mut m = lock.lock().await;
    let exists = dest.join("HEAD").exists();
    let age = m.at.map(|t| t.elapsed());
    let due = !exists
        || match age {
            None => true,
            Some(a) if force => a >= MIRROR_MIN_GAP,
            Some(a) => a >= MIRROR_MAX_AGE,
        };
    if due {
        let secret = secrets::secret_of(state, repo.owner(), repo.credential.as_deref()).await?;
        let c = Checkout {
            url: repo.url.clone(),
            dest: dest.clone(),
            branch: None,
            depth: None,
            secret: secret.map(|(_, s)| s),
            store: RunDirs::new(&state.state_dir),
            deny_local_under: vec![state.state_dir.clone(), state.work_dir.clone()],
            timeout: MIRROR_TIMEOUT,
            hooks: git::Hooks::default(),
            origin: git::Origin::default(),
            fetch_from: None,
        };
        let r = tokio::task::spawn_blocking(move || git::refresh_mirror(&c))
            .await
            .map_err(|e| ApiError::internal(format!("the mirror task failed: {e}")))?;
        m.at = Some(Instant::now());
        match r {
            Ok(()) => {
                m.fetched_at = Some(crate::db::now());
                m.error = None;
            }
            Err(e) => {
                let msg = e.to_string();
                tracing::warn!(repo = %repo.id, "mirror fetch failed: {msg}");
                if !dest.join("HEAD").exists() {
                    let mut err = git_error(e);
                    err.message = scrub(state, &err.message);
                    err.details = err
                        .details
                        .map(|d| serde_json::Value::String(scrub(state, &d.to_string())));
                    return Err(err);
                }
                m.error = Some(scrub(state, &msg));
            }
        }
    }
    Ok(MirrorView {
        fetched_at: m.fetched_at.clone(),
        error: m.error.clone(),
    })
}

#[derive(Debug, Clone, Serialize)]
pub struct MirrorView {
    /// The last good fetch since the daemon started.
    pub fetched_at: Option<String>,
    /// The last fetch failed: the mirror is older.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
}

/// Replace the state dir and the work dir in `m` with `<state>` and
/// `<work>`: a message of a mirror fetch can name the mirror dir.
fn scrub(state: &Shared, m: &str) -> String {
    let mut m = m.to_owned();
    for (dir, name) in [(&state.state_dir, "<state>"), (&state.work_dir, "<work>")] {
        let d = dir.to_string_lossy();
        if !d.is_empty() && d != "/" {
            m = m.replace(d.as_ref(), name);
        }
    }
    m
}

/// The answer to a failed read. The refusals of the browser and of the
/// checks in `crate::git` name paths relative to the checkout only. A
/// failed git command is a fixed message with no stderr (`ReadGit` keeps
/// it out already), and an I/O error is a fixed message (it is logged).
fn browse_error(e: BrowseError) -> ApiError {
    match e {
        BrowseError::BadRequest(m) => ApiError::invalid(m),
        BrowseError::NotFound(m) => ApiError::not_found(m),
        BrowseError::Git(GitError::Refused(m)) => ApiError::conflict(m),
        BrowseError::Git(GitError::Failed {
            command, status, ..
        }) => ApiError::invalid(format!("git {command} failed ({status})")),
        BrowseError::Git(GitError::Timeout(d)) => {
            ApiError::internal(format!("git did not finish in {}s", d.as_secs()))
        }
        BrowseError::Git(GitError::Unavailable(m)) => ApiError::new(Code::Unavailable, m),
        BrowseError::Git(GitError::Io(m)) | BrowseError::Io(m) => {
            tracing::warn!("repo browser: {m}");
            ApiError::internal("the repo browser cannot read the tree (see the daemon log)")
        }
    }
}

/// Run a browser call on the blocking pool.
async fn blocking<T: Send + 'static>(
    f: impl FnOnce(&ReadGit) -> Result<T, BrowseError> + Send + 'static,
) -> ApiResult<T> {
    tokio::task::spawn_blocking(move || f(&ReadGit::new(browse::CMD_TIMEOUT)))
        .await
        .map_err(|e| ApiError::internal(format!("the repo browser task failed: {e}")))?
        .map_err(browse_error)
}

async fn load_repo(state: &Shared, actor: &Actor, id: String) -> ApiResult<Repo> {
    let context = actor.project().map(str::to_owned);
    let r = state
        .db
        .read(move |c| repos::resolve(c, &id, context.as_deref()))
        .await?;
    policy::read_project(actor, &r.project)?;
    Ok(r)
}

/// A tree, ready to read.
enum Opened {
    Main {
        tree: Tree,
        branch: String,
        head: String,
        mirror: MirrorView,
    },
    Agent {
        tree: Tree,
        dir: Arc<CheckoutDir>,
        agent_id: String,
        agent: String,
    },
}

impl Opened {
    fn tree(&self) -> Tree {
        match self {
            Opened::Main { tree, .. } | Opened::Agent { tree, .. } => tree.clone(),
        }
    }
}

/// The mirror's default branch: the repo's `default_branch`, else the
/// remote's HEAD.
async fn main_tree(state: &Shared, repo: &Repo, force: bool) -> ApiResult<Opened> {
    let mirror = ensure_mirror(state, repo, force).await?;
    let git_dir = mirror_dir(state, repo);
    let wanted = repo.default_branch.clone();
    let gd = git_dir.clone();
    let (branch, head) = blocking(move |g| {
        let branch = wanted
            .or_else(|| browse::mirror_default_branch(g, &gd))
            .ok_or_else(|| BrowseError::NotFound("the mirror has no default branch".into()))?;
        let head = browse::resolve(g, &gd, &format!("refs/heads/{branch}")).ok_or_else(|| {
            BrowseError::NotFound(format!("the mirror has no branch `{branch}` (empty repo?)"))
        })?;
        Ok((branch, head))
    })
    .await?;
    Ok(Opened::Main {
        tree: Tree::Mirror {
            git_dir,
            rev: head.clone(),
        },
        branch,
        head,
        mirror,
    })
}

/// The checkout of `repo` in the workspace of an agent of its project.
fn checkout_dir(repo: &Repo, a: &agents::AgentView) -> PathBuf {
    a.agent.workspace.join(&repo.path)
}

/// An agent tree needs the read sandbox: with none, every read of it is
/// `503 unavailable` (docs/api.md §5.3, security review N1), also the
/// reads that run no git.
fn need_read_sandbox(g: &ReadGit) -> Result<(), BrowseError> {
    if g.reads_checkouts() {
        Ok(())
    } else {
        Err(BrowseError::Git(GitError::Unavailable(
            git::NO_READ_SANDBOX.into(),
        )))
    }
}

/// Open the checkout of `repo` for `a` and check it, on the blocking pool.
fn open_checkout(
    g: &ReadGit,
    repo: &Repo,
    a: &agents::AgentView,
    store: &RunDirs,
) -> Result<CheckoutDir, BrowseError> {
    need_read_sandbox(g)?;
    let c = CheckoutDir::open(
        &a.agent.workspace,
        &repo.path,
        format!("agent:{}/{}", a.agent.id, repo.path),
    )?;
    g.check_checkout(&c, store)?;
    Ok(c)
}

async fn agent_tree(state: &Shared, repo: &Repo, ulid: &str) -> ApiResult<Opened> {
    blocking(need_read_sandbox).await?;
    let missing = || ApiError::not_found(format!("no tree `agent:{ulid}` for this repo"));
    if ulid.parse::<terrarium_core::AgentId>().is_err() {
        return Err(missing());
    }
    let key = ulid.to_owned();
    let a = state
        .db
        .read(move |c| agents::get(c, &key))
        .await
        .map_err(|_| missing())?;
    if a.agent.project != repo.project {
        return Err(missing());
    }
    let dir = checkout_dir(repo, &a);
    if !std::fs::symlink_metadata(dir.join(".git")).is_ok_and(|m| m.is_dir()) {
        return Err(missing());
    }
    let store = RunDirs::new(&state.state_dir);
    let (r, view) = (repo.clone(), a.clone());
    let dir = Arc::new(blocking(move |g| open_checkout(g, &r, &view, &store)).await?);
    Ok(Opened::Agent {
        tree: Tree::Checkout { dir: dir.clone() },
        dir,
        agent_id: a.agent.id.to_string(),
        agent: a.agent.name.clone(),
    })
}

async fn open(state: &Shared, repo: &Repo, tree: &str) -> ApiResult<Opened> {
    match tree {
        "main" => main_tree(state, repo, false).await,
        t => match t.strip_prefix("agent:") {
            Some(ulid) => agent_tree(state, repo, ulid).await,
            None => Err(ApiError::not_found(format!(
                "no tree `{t}`: `main` or `agent:<ULID>`"
            ))),
        },
    }
}

// --- GET /api/repos/{id}/trees ---------------------------------------------

#[derive(Debug, Default, Deserialize)]
#[serde(deny_unknown_fields)]
struct TreesQuery {
    #[serde(default)]
    refresh: bool,
}

async fn trees(
    State(state): State<Shared>,
    actor: Actor,
    Path(id): Path<String>,
    ApiQuery(q): ApiQuery<TreesQuery>,
) -> ApiResult<Json<serde_json::Value>> {
    let repo = load_repo(&state, &actor, id).await?;
    let mut out = Vec::new();
    let mut default_branch = repo.default_branch.clone();
    match main_tree(&state, &repo, q.refresh).await {
        Ok(Opened::Main {
            branch,
            head,
            mirror,
            ..
        }) => {
            default_branch = Some(branch.clone());
            out.push(json!({
                "id": "main", "kind": "main", "label": "main", "branch": branch,
                "head": head, "mirror": mirror,
            }));
        }
        Ok(Opened::Agent { .. }) => {}
        Err(e) => out.push(json!({
            "id": "main", "kind": "main", "label": "main", "branch": default_branch,
            "error": e.message, "details": e.details,
        })),
    }

    let project = repo.project.clone();
    let all = state
        .db
        .read(move |c| {
            let mut all = Vec::new();
            let mut page = Page {
                limit: Some(Page::MAX),
                cursor: None,
            };
            loop {
                let p = agents::list(c, Some(&project), &page)?;
                all.extend(p.items);
                match p.next {
                    Some(n) => page.cursor = Some(n),
                    None => break,
                }
            }
            Ok(all)
        })
        .await?;
    let store = RunDirs::new(&state.state_dir);
    for a in all {
        let dir = checkout_dir(&repo, &a);
        if !std::fs::symlink_metadata(dir.join(".git")).is_ok_and(|m| m.is_dir()) {
            continue;
        }
        let id = format!("agent:{}", a.agent.id);
        let name = a.agent.name.clone();
        let agent_id = a.agent.id.to_string();
        let store = store.clone();
        let default = default_branch.clone();
        let r = repo.clone();
        let r = blocking(move |g| {
            let c = open_checkout(g, &r, &a, &store)?;
            let info = browse::checkout_info(g, &c, default.as_deref());
            let dirty = browse::dirty(g, &c, browse::LIST_BUDGET).ok();
            Ok((info, dirty))
        })
        .await;
        let label = |branch: Option<&str>| format!("{name} · {}", branch.unwrap_or("detached"));
        out.push(match r {
            Ok((info, dirty)) => json!({
                "id": id, "kind": "agent", "agent": name, "agent_id": agent_id,
                "label": label(info.branch.as_deref()), "branch": info.branch, "head": info.head,
                "base_ref": info.base_ref, "base": info.base, "ahead": info.ahead,
                "behind": info.behind, "dirty": dirty,
            }),
            Err(e) => json!({
                "id": id, "kind": "agent", "agent": name, "agent_id": agent_id,
                "label": label(None), "error": e.message,
            }),
        });
    }
    Ok(Json(json!({
        "repo": repo.name, "repo_id": repo.id, "project": repo.project,
        "default_branch": default_branch, "trees": out,
    })))
}

// --- Files -------------------------------------------------------------------

#[derive(Debug, Default, Deserialize)]
#[serde(deny_unknown_fields)]
struct PathQuery {
    #[serde(default)]
    path: String,
}

async fn files(
    State(state): State<Shared>,
    actor: Actor,
    Path((id, tree)): Path<(String, String)>,
    ApiQuery(q): ApiQuery<PathQuery>,
) -> ApiResult<Json<browse::Listing>> {
    let path = browse::clean_path(&q.path).map_err(browse_error)?;
    let repo = load_repo(&state, &actor, id).await?;
    let t = open(&state, &repo, &tree).await?.tree();
    Ok(Json(
        blocking(move |g| browse::list_dir(g, &t, &path)).await?,
    ))
}

/// The raw bytes of one file, with a type that no browser runs: text is
/// `text/plain; charset=utf-8`, anything else `application/octet-stream`
/// as an attachment. `X-Terrarium-Size` is the size of the whole file;
/// `X-Terrarium-Truncated: true` says the body is the first 1 MiB only.
async fn file(
    State(state): State<Shared>,
    actor: Actor,
    Path((id, tree)): Path<(String, String)>,
    ApiQuery(q): ApiQuery<PathQuery>,
) -> ApiResult<Response> {
    let path = browse::clean_path(&q.path).map_err(browse_error)?;
    let repo = load_repo(&state, &actor, id).await?;
    let t = open(&state, &repo, &tree).await?.tree();
    let f = blocking(move |g| browse::read_file(g, &t, &path)).await?;
    let text = browse::is_text(&f.bytes);
    let mut resp = f.bytes.into_response();
    let h = resp.headers_mut();
    h.insert(
        header::CONTENT_TYPE,
        HeaderValue::from_static(if text {
            "text/plain; charset=utf-8"
        } else {
            "application/octet-stream"
        }),
    );
    if !text {
        h.insert(
            header::CONTENT_DISPOSITION,
            HeaderValue::from_static("attachment"),
        );
    }
    h.insert(
        header::X_CONTENT_TYPE_OPTIONS,
        HeaderValue::from_static("nosniff"),
    );
    h.insert(header::CACHE_CONTROL, HeaderValue::from_static("no-store"));
    if let Ok(v) = HeaderValue::from_str(&f.size.to_string()) {
        h.insert("x-terrarium-size", v);
    }
    h.insert(
        "x-terrarium-truncated",
        HeaderValue::from_static(if f.truncated { "true" } else { "false" }),
    );
    Ok(resp)
}

// --- Changes -------------------------------------------------------------------

async fn diff(
    State(state): State<Shared>,
    actor: Actor,
    Path((id, tree)): Path<(String, String)>,
) -> ApiResult<Json<serde_json::Value>> {
    let repo = load_repo(&state, &actor, id).await?;
    let default = repo.default_branch.clone();
    match open(&state, &repo, &tree).await? {
        Opened::Main { branch, head, .. } => Ok(Json(json!({
            "tree": "main", "base": head, "head": head, "base_ref": format!("refs/heads/{branch}"),
            "committed": browse::DiffSet::default(), "uncommitted": null,
            "note": "main is the base",
        }))),
        Opened::Agent {
            tree: t,
            dir,
            agent,
            agent_id,
        } => {
            let tree_id = tree.clone();
            let v = blocking(move |g| {
                let info = browse::checkout_info(g, &dir, default.as_deref());
                let mut budget = LineBudget::default();
                let (committed, note) = match (&info.base, &info.head) {
                    (Some(b), Some(h)) => (browse::diff_commits(g, &t, b, h, &mut budget)?, None),
                    _ => (
                        browse::DiffSet::default(),
                        Some("no base: the checkout has no commit, or no default branch"),
                    ),
                };
                let uncommitted = browse::uncommitted(g, &dir, &mut budget)?;
                Ok(json!({
                    "tree": tree_id, "agent": agent, "agent_id": agent_id,
                    "base": info.base, "head": info.head, "base_ref": info.base_ref,
                    "branch": info.branch, "committed": committed, "uncommitted": uncommitted,
                    "note": note,
                }))
            })
            .await?;
            Ok(Json(v))
        }
    }
}

// --- Commits -------------------------------------------------------------------

#[derive(Debug, Default, Deserialize)]
#[serde(deny_unknown_fields)]
struct CommitsQuery {
    #[serde(default)]
    cursor: Option<String>,
    #[serde(default)]
    limit: Option<usize>,
}

async fn commits(
    State(state): State<Shared>,
    actor: Actor,
    Path((id, tree)): Path<(String, String)>,
    ApiQuery(q): ApiQuery<CommitsQuery>,
) -> ApiResult<Response> {
    let skip = match q.cursor.as_deref().filter(|c| !c.is_empty()) {
        Some(c) => c
            .parse::<usize>()
            .map_err(|_| ApiError::bad_request(format!("bad cursor `{c}`")))?,
        None => 0,
    };
    let limit = q.limit.unwrap_or(50);
    let repo = load_repo(&state, &actor, id).await?;
    let default = repo.default_branch.clone();
    let opened = open(&state, &repo, &tree).await?;
    let (list, next) = match opened {
        Opened::Main { tree: t, head, .. } => {
            blocking(move |g| browse::commits(g, &t, &head, skip, limit)).await?
        }
        Opened::Agent { tree: t, dir, .. } => {
            blocking(move |g| {
                let info = browse::checkout_info(g, &dir, default.as_deref());
                let Some(head) = info.head else {
                    return Ok((Vec::new(), None));
                };
                let range = match info.base {
                    Some(b) => format!("{b}..{head}"),
                    None => head,
                };
                browse::commits(g, &t, &range, skip, limit)
            })
            .await?
        }
    };
    let mut resp = Json(list).into_response();
    if let Some(n) = next
        && let Ok(v) = HeaderValue::from_str(&n.to_string())
    {
        resp.headers_mut().insert(NEXT_CURSOR, v);
    }
    Ok(resp)
}

async fn commit(
    State(state): State<Shared>,
    actor: Actor,
    Path((id, tree, sha)): Path<(String, String, String)>,
) -> ApiResult<Json<browse::CommitDetail>> {
    browse::check_sha(&sha).map_err(browse_error)?;
    let repo = load_repo(&state, &actor, id).await?;
    let t = open(&state, &repo, &tree).await?.tree();
    Ok(Json(blocking(move |g| browse::commit(g, &t, &sha)).await?))
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;

    /// N3 (docs/security-review.md §15): the answer of a failed read has
    /// no stderr and no path of the daemon.
    #[test]
    fn a_read_error_has_no_stderr_and_no_daemon_path() {
        let secret = "/state/credentials/x: TOPSECRET";
        for e in [
            BrowseError::Git(GitError::Failed {
                command: "log".into(),
                status: "exit 128".into(),
                stderr: secret.into(),
            }),
            BrowseError::Git(GitError::Io(secret.into())),
            BrowseError::Io(secret.into()),
        ] {
            let a = browse_error(e);
            let text = format!("{} {:?}", a.message, a.details);
            assert!(!text.contains("TOPSECRET"), "{text}");
            assert!(!text.contains("/state"), "{text}");
            assert!(a.details.is_none(), "{text}");
        }
        let a = browse_error(BrowseError::Git(GitError::Unavailable(
            git::NO_READ_SANDBOX.into(),
        )));
        assert_eq!(a.code, Code::Unavailable);
        assert_eq!(a.code.status().as_u16(), 503);
        assert!(a.message.contains("bwrap"));
    }

    /// N1: with no read sandbox, an agent tree is `503 unavailable`, also
    /// for the reads that run no git (the file list of the work tree).
    #[test]
    fn an_agent_tree_needs_the_read_sandbox() {
        let none = ReadGit::without_sandbox(browse::CMD_TIMEOUT);
        let a = browse_error(need_read_sandbox(&none).unwrap_err());
        assert_eq!(a.code.status().as_u16(), 503);
        assert_eq!(a.message, git::NO_READ_SANDBOX);
        assert!(need_read_sandbox(&ReadGit::on_host(browse::CMD_TIMEOUT)).is_ok());
    }
}
