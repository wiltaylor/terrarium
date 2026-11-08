//! `/api/config/...`: the config repo (PLAN §8.3, docs/api.md §25). The
//! operator or a sysadmin only.
//!
//! The git work is in `crate::config_repo` and runs off the async runtime,
//! one operation at a time. The export of the store (`export/`) is written
//! before each commit, unless a pull brought `export/` changes that wait
//! for `pull --apply`.

use axum::extract::State;
use axum::routing::{get, post};
use axum::{Json, Router};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

use super::{ApiJson, ApiQuery, Shared};
use crate::actor::Actor;
use crate::config_repo::{
    self as repo, Committed, ConfigRepo, EXPORT_DIR, LogEntry, Merged, Remote, Status,
};
use crate::defaults::{self, Pending};
use crate::error::{ApiError, ApiResult};
use crate::fleet::Plan;
use crate::policy;
use crate::secrets::export::{self as secrets_export, Report as SecretsReport};

pub fn routes() -> Router<Shared> {
    Router::new()
        .route("/config/status", get(status))
        .route("/config/diff", get(diff))
        .route("/config/log", get(log))
        .route("/config/commit", post(commit))
        .route("/config/pull", post(pull))
        .route("/config/push", post(push))
        .route("/config/sync", post(sync))
        .route("/config/abort", post(abort))
        .route(
            "/config/remote",
            axum::routing::put(remote_set).delete(remote_clear),
        )
        .route("/config/defaults", get(defaults_pending))
        .route("/config/defaults/accept", post(defaults_accept))
        .route("/config/defaults/keep", post(defaults_keep))
}

fn allow(actor: &Actor, what: &str) -> ApiResult<()> {
    policy::sysadmin(actor, what)
}

/// Run blocking repo work off the async runtime.
async fn blocking<T, F>(r: ConfigRepo, f: F) -> ApiResult<T>
where
    T: Send + 'static,
    F: FnOnce(&ConfigRepo) -> ApiResult<T> + Send + 'static,
{
    tokio::task::spawn_blocking(move || f(&r))
        .await
        .map_err(|e| ApiError::internal(format!("config repo task: {e}")))?
}

async fn status(State(state): State<Shared>, actor: Actor) -> ApiResult<Json<Status>> {
    allow(&actor, "reading the config repo")?;
    let r = state.config_repo.get()?;
    Ok(Json(blocking(r, ConfigRepo::status).await?))
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct DiffQuery {
    #[serde(default)]
    path: Option<String>,
}

#[derive(Debug, Serialize)]
struct Diff {
    diff: String,
}

async fn diff(
    State(state): State<Shared>,
    actor: Actor,
    ApiQuery(q): ApiQuery<DiffQuery>,
) -> ApiResult<Json<Diff>> {
    allow(&actor, "reading the config repo")?;
    let r = state.config_repo.get()?;
    let diff = blocking(r, move |r| r.diff(q.path.as_deref())).await?;
    Ok(Json(Diff { diff }))
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct LogQuery {
    #[serde(default)]
    limit: Option<u32>,
}

async fn log(
    State(state): State<Shared>,
    actor: Actor,
    ApiQuery(q): ApiQuery<LogQuery>,
) -> ApiResult<Json<Vec<LogEntry>>> {
    allow(&actor, "reading the config repo")?;
    let r = state.config_repo.get()?;
    let limit = q.limit.unwrap_or(20);
    Ok(Json(blocking(r, move |r| r.log(limit)).await?))
}

/// Write `export/` from the store. While a pulled export waits, only the
/// files that are not in the repo yet (`force`: all of them), so the
/// pulled files stay for `pull --apply` and each project of this instance
/// still reaches `export/` (TER-193). The answer is the export paths
/// written.
async fn refresh_export(state: &Shared, r: &ConfigRepo, force: bool) -> ApiResult<Vec<String>> {
    let files = state.db.read(repo::export_files).await?;
    if r.state().export_pending && !force {
        tracing::warn!(
            "pulled export pending: existing export files are not refreshed until a successful pull --apply or force_export"
        );
        return blocking(r.clone(), move |r| {
            r.ensure()?;
            r.write_export_new(&files)
        })
        .await;
    }
    let paths = files.iter().map(|f| f.path.clone()).collect();
    blocking(r.clone(), move |r| {
        r.ensure()?;
        r.write_export(&files)?;
        r.set_export_pending(false)
    })
    .await?;
    Ok(paths)
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct CommitBody {
    message: String,
    /// Write `export/` first (default true).
    #[serde(default)]
    export: Option<bool>,
    /// Write `export/` also when a pulled export waits: the store wins.
    #[serde(default)]
    force_export: bool,
}

#[derive(Debug, Serialize)]
struct CommitResult {
    #[serde(flatten)]
    committed: Committed,
    /// The export files that the commit wrote first.
    export: Vec<String>,
}

async fn commit(
    State(state): State<Shared>,
    actor: Actor,
    ApiJson(b): ApiJson<CommitBody>,
) -> ApiResult<Json<CommitResult>> {
    allow(&actor, "committing the config repo")?;
    let r = state.config_repo.get()?;
    let _busy = state.config_repo.busy.lock().await;
    let export = if b.export.unwrap_or(true) {
        refresh_export(&state, &r, b.force_export).await?
    } else {
        Vec::new()
    };
    let message = b.message;
    let committed = blocking(r, move |r| r.commit(&message)).await?;
    audit(
        &state,
        &actor,
        "config.commit",
        json!({"commit": committed.commit}),
    )
    .await;
    Ok(Json(CommitResult { committed, export }))
}

/// The plan of each export file, or its apply.
#[derive(Debug, Serialize)]
struct ExportPlan {
    path: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    plan: Option<Plan>,
    #[serde(skip_serializing_if = "Option::is_none")]
    applied: Option<Value>,
    #[serde(skip_serializing_if = "Option::is_none")]
    error: Option<String>,
}

/// The secrets of a pulled export (PLAN §16b.5): what their import does,
/// or did.
#[derive(Debug, Default, Serialize)]
struct SecretsImport {
    #[serde(flatten)]
    report: SecretsReport,
    /// The import did not run (a database error).
    #[serde(skip_serializing_if = "Option::is_none")]
    error: Option<String>,
}

/// Plan, or run, the import of the secrets files of the export. A run
/// writes the store in one transaction, then builds the redaction matcher
/// again. A new or wider grant of a secret with a value runs only with
/// `approve` (the operator, S8); else the report shows it as held.
async fn import_secrets(
    state: &Shared,
    actor: &Actor,
    r: &ConfigRepo,
    apply: bool,
    approve: bool,
) -> ApiResult<SecretsReport> {
    let (files, projects) = blocking(r.clone(), |r| {
        Ok((r.secrets_in_repo(), r.exported_projects()))
    })
    .await?;
    if !apply {
        return state
            .db
            .read(move |c| Ok(secrets_export::plan(c, &files, &projects)?.report))
            .await;
    }
    let by = actor.label();
    let report = state
        .db
        .write(actor.label(), move |t| {
            let plan = secrets_export::plan(t, &files, &projects)?;
            secrets_export::apply(t, plan, &by, approve)
        })
        .await?;
    if !report.changes.is_empty() {
        crate::secrets::redact::rebuild(state).await;
    }
    Ok(report)
}

/// Apply the fleet files `files`; the answer has a row for each, and
/// whether one failed.
async fn apply_fleets(
    state: &Shared,
    actor: &Actor,
    files: Vec<(String, String)>,
) -> (Vec<ExportPlan>, bool) {
    let mut out = Vec::new();
    let mut failed = false;
    for (path, content) in files {
        let name = Some(path.clone());
        let row = match super::fleet::apply_file(state, actor, content, name).await {
            Ok(v) => ExportPlan {
                path,
                plan: None,
                applied: Some(v),
                error: None,
            },
            Err(e) => {
                failed = true;
                ExportPlan {
                    path,
                    plan: None,
                    applied: None,
                    error: Some(e.message),
                }
            }
        };
        out.push(row);
    }
    (out, failed)
}

/// Apply the export: the secrets first (a target names its login, so the
/// login must exist), then the fleets, then the secrets again (the
/// project secrets and the grants need the projects and the agents of the
/// fleets). A fleet that failed gets one more try after that.
async fn apply_export(
    state: &Shared,
    actor: &Actor,
    r: &ConfigRepo,
    files: Vec<(String, String)>,
    approve: bool,
) -> (Vec<ExportPlan>, SecretsImport, bool) {
    let mut secrets = SecretsImport::default();
    let mut changes: Vec<secrets_export::Change> = Vec::new();
    let mut run = |res: ApiResult<SecretsReport>, s: &mut SecretsImport| match res {
        Ok(rep) => {
            // A held grant comes back in each pass: show it once.
            for c in &rep.changes {
                if !changes.contains(c) {
                    changes.push(c.clone());
                }
            }
            s.report = rep;
            true
        }
        Err(e) => {
            s.error = Some(e.message);
            false
        }
    };
    let mut ok = run(
        import_secrets(state, actor, r, true, approve).await,
        &mut secrets,
    );
    let (mut rows, failed) = apply_fleets(state, actor, files.clone()).await;
    ok &= run(
        import_secrets(state, actor, r, true, approve).await,
        &mut secrets,
    );
    let mut failed = failed;
    if failed {
        let again: Vec<(String, String)> = files
            .into_iter()
            .filter(|(p, _)| rows.iter().any(|x| x.path == *p && x.error.is_some()))
            .collect();
        let (retry, still) = apply_fleets(state, actor, again).await;
        for row in retry {
            if let Some(slot) = rows.iter_mut().find(|x| x.path == row.path) {
                *slot = row;
            }
        }
        failed = still;
        ok &= run(
            import_secrets(state, actor, r, true, approve).await,
            &mut secrets,
        );
    }
    secrets.report.changes = changes;
    (rows, secrets, failed || !ok)
}

/// True when the report holds a grant that waits for the operator.
fn holds_grants(report: &SecretsReport) -> bool {
    report
        .changes
        .iter()
        .any(|c| c.approval.as_deref() == Some(secrets_export::APPROVAL_HELD))
}

/// `approve_grants` is for the operator only: a sysadmin agent cannot
/// approve the grants of a config import (S8).
fn check_approve(actor: &Actor, approve: bool) -> ApiResult<()> {
    if approve && !matches!(actor, Actor::Operator) {
        return Err(ApiError::forbidden(
            "`approve_grants` is for the operator only: an agent cannot approve the grants of \
             a config import",
        ));
    }
    Ok(())
}

/// After a merge: plan (or apply) the export files when `export/`
/// changed or a pulled export waits.
async fn export_plans(
    state: &Shared,
    actor: &Actor,
    r: &ConfigRepo,
    merged: &Merged,
    apply: bool,
    approve: bool,
) -> ApiResult<(Vec<ExportPlan>, Option<SecretsImport>)> {
    let changed = merged
        .changed
        .iter()
        .any(|p| p.starts_with(&format!("{EXPORT_DIR}/")));
    if !changed && !r.state().export_pending {
        return Ok((Vec::new(), None));
    }
    let files = blocking(r.clone(), |r| Ok(r.export_in_repo())).await?;
    if apply {
        let (rows, secrets, failed) = apply_export(state, actor, r, files, approve).await;
        // A held grant keeps the export pending: the next `pull --apply
        // --approve-grants` plans it again.
        let pending = failed || holds_grants(&secrets.report);
        blocking(r.clone(), move |r| r.set_export_pending(pending)).await?;
        if !pending {
            // Refresh from the resulting store only after all imports succeed.
            refresh_export(state, r, false).await?;
        }
        return Ok((rows, Some(secrets)));
    }
    let mut out = Vec::new();
    for (path, content) in files {
        let name = Some(path.clone());
        let row = match super::fleet::plan_file(state, actor, content, name).await {
            Ok(p) => ExportPlan {
                path,
                plan: Some(p),
                applied: None,
                error: None,
            },
            Err(e) => ExportPlan {
                path,
                plan: None,
                applied: None,
                error: Some(e.message),
            },
        };
        out.push(row);
    }
    let secrets = match import_secrets(state, actor, r, false, false).await {
        Ok(report) => SecretsImport {
            report,
            error: None,
        },
        Err(e) => SecretsImport {
            report: SecretsReport::default(),
            error: Some(e.message),
        },
    };
    blocking(r.clone(), move |r| r.set_export_pending(true)).await?;
    Ok((out, Some(secrets)))
}

#[derive(Debug, Default, Deserialize)]
#[serde(deny_unknown_fields)]
struct PullBody {
    /// Apply the `export/` changes as fleets. Without it, the answer has
    /// their plans, and they wait.
    #[serde(default)]
    apply: bool,
    /// With `apply`: make the new and wider grants of secrets that have a
    /// value (S8). The operator only.
    #[serde(default)]
    approve_grants: bool,
}

#[derive(Debug, Serialize)]
struct PullResult {
    #[serde(flatten)]
    merged: Merged,
    export: Vec<ExportPlan>,
    /// The secrets of the export: the plan of their import, or what it
    /// did. Absent when `export/` did not change.
    #[serde(skip_serializing_if = "Option::is_none")]
    secrets: Option<SecretsImport>,
}

fn conflict_error(merged: &Merged) -> ApiError {
    ApiError::conflict(format!(
        "the merge stopped on a conflict in {}: edit the files, then commit; or abort. \
         No local commit is lost",
        merged.conflicts.join(", ")
    ))
    .with_details(json!({ "conflicts": merged.conflicts }))
}

fn body_or_default<T: Default + serde::de::DeserializeOwned>(bytes: &[u8]) -> ApiResult<T> {
    if bytes.iter().all(u8::is_ascii_whitespace) {
        return Ok(T::default());
    }
    serde_json::from_slice(bytes).map_err(|e| ApiError::bad_request(e.to_string()))
}

async fn pull(
    State(state): State<Shared>,
    actor: Actor,
    body: axum::body::Bytes,
) -> ApiResult<Json<PullResult>> {
    allow(&actor, "pulling the config repo")?;
    let b: PullBody = body_or_default(&body)?;
    check_approve(&actor, b.approve_grants)?;
    let r = state.config_repo.get()?;
    let _busy = state.config_repo.busy.lock().await;
    let merged = blocking(r.clone(), |r| {
        r.fetch()?;
        r.merge()
    })
    .await?;
    if !merged.conflicts.is_empty() {
        return Err(conflict_error(&merged));
    }
    // The pulled template files, before the export: a fleet agent of the
    // export may name one.
    crate::template_files::refresh_quiet(&state).await;
    let (export, secrets) =
        export_plans(&state, &actor, &r, &merged, b.apply, b.approve_grants).await?;
    audit(
        &state,
        &actor,
        "config.pull",
        json!({"changed": merged.changed}),
    )
    .await;
    Ok(Json(PullResult {
        merged,
        export,
        secrets,
    }))
}

#[derive(Debug, Serialize)]
struct Pushed {
    pushed: bool,
}

async fn push(State(state): State<Shared>, actor: Actor) -> ApiResult<Json<Pushed>> {
    allow(&actor, "pushing the config repo")?;
    let r = state.config_repo.get()?;
    let _busy = state.config_repo.busy.lock().await;
    let pushed = blocking(r, ConfigRepo::push).await?;
    audit(&state, &actor, "config.push", json!({})).await;
    Ok(Json(Pushed { pushed }))
}

#[derive(Debug, Default, Deserialize)]
#[serde(deny_unknown_fields)]
struct SyncBody {
    #[serde(default)]
    message: Option<String>,
    #[serde(default)]
    apply: bool,
    /// As in a pull (S8). The operator only.
    #[serde(default)]
    approve_grants: bool,
}

#[derive(Debug, Serialize)]
struct SyncResult {
    committed: Committed,
    #[serde(flatten)]
    merged: Merged,
    pushed: bool,
    export: Vec<ExportPlan>,
    #[serde(skip_serializing_if = "Option::is_none")]
    secrets: Option<SecretsImport>,
}

/// One sync: commit the local changes (with the scan), fetch, merge, push.
pub(crate) async fn run_sync(state: &Shared, actor: &Actor, b: SyncBodyIn) -> ApiResult<Value> {
    let r = state.config_repo.get()?;
    let _busy = state.config_repo.busy.lock().await;
    refresh_export(state, &r, false).await?;
    let message = b
        .message
        .filter(|m| !m.trim().is_empty())
        .unwrap_or_else(|| "Terrarium config sync".to_owned());
    let committed = blocking(r.clone(), move |r| r.commit(&message)).await?;
    let merged = blocking(r.clone(), |r| {
        r.fetch()?;
        r.merge()
    })
    .await?;
    if !merged.conflicts.is_empty() {
        return Err(conflict_error(&merged));
    }
    let pushed = blocking(r.clone(), ConfigRepo::push).await?;
    crate::template_files::refresh_quiet(state).await;
    check_approve(actor, b.approve_grants)?;
    let (export, secrets) =
        export_plans(state, actor, &r, &merged, b.apply, b.approve_grants).await?;
    audit(
        state,
        actor,
        "config.sync",
        json!({"commit": committed.commit, "changed": merged.changed}),
    )
    .await;
    Ok(serde_json::to_value(SyncResult {
        committed,
        merged,
        pushed,
        export,
        secrets,
    })?)
}

/// The input of [`run_sync`].
#[derive(Debug, Default)]
pub(crate) struct SyncBodyIn {
    pub message: Option<String>,
    pub apply: bool,
    pub approve_grants: bool,
}

async fn sync(
    State(state): State<Shared>,
    actor: Actor,
    body: axum::body::Bytes,
) -> ApiResult<Json<Value>> {
    allow(&actor, "syncing the config repo")?;
    let b: SyncBody = body_or_default(&body)?;
    run_sync(
        &state,
        &actor,
        SyncBodyIn {
            message: b.message,
            apply: b.apply,
            approve_grants: b.approve_grants,
        },
    )
    .await
    .map(Json)
}

/// Start the automatic sync (`TERRARIUM_CONFIG_SYNC`): every `every`, a
/// pull or a sync, as the operator. The `export/` changes wait for
/// `pull --apply`. `None` when it is off.
pub fn spawn_config_sync(
    state: Shared,
    mode: repo::AutoSync,
    every: std::time::Duration,
) -> Option<tokio::task::JoinHandle<()>> {
    if mode == repo::AutoSync::Off {
        return None;
    }
    let every = every.max(std::time::Duration::from_secs(60));
    tracing::info!(mode = mode.as_str(), every = ?every, "config repo auto-sync");
    Some(tokio::spawn(async move {
        loop {
            tokio::time::sleep(every).await;
            let result = match mode {
                repo::AutoSync::Off => return,
                repo::AutoSync::Pull => run_pull(&state).await.map(|m| json!(m)),
                repo::AutoSync::Sync => {
                    run_sync(&state, &Actor::Operator, SyncBodyIn::default()).await
                }
            };
            match result {
                Ok(v) => tracing::debug!(result = %v, "config repo auto-sync"),
                Err(e) => tracing::warn!("config repo auto-sync: {}", e.message),
            }
        }
    }))
}

/// The automatic pull (`TERRARIUM_CONFIG_SYNC=pull`): fetch and merge; the
/// `export/` changes wait.
pub(crate) async fn run_pull(state: &Shared) -> ApiResult<Merged> {
    let r = state.config_repo.get()?;
    let _busy = state.config_repo.busy.lock().await;
    let merged = blocking(r.clone(), |r| {
        r.fetch()?;
        r.merge()
    })
    .await?;
    if !merged.conflicts.is_empty() {
        return Err(conflict_error(&merged));
    }
    if merged
        .changed
        .iter()
        .any(|p| p.starts_with(&format!("{EXPORT_DIR}/")))
    {
        blocking(r, |r| r.set_export_pending(true)).await?;
    }
    drop(_busy);
    crate::template_files::refresh_quiet(state).await;
    Ok(merged)
}

async fn abort(State(state): State<Shared>, actor: Actor) -> ApiResult<Json<Value>> {
    allow(&actor, "aborting a config repo merge")?;
    let r = state.config_repo.get()?;
    let _busy = state.config_repo.busy.lock().await;
    blocking(r, ConfigRepo::abort).await?;
    Ok(Json(json!({"aborted": true})))
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct RemoteBody {
    url: String,
    /// The name of a `token` or `ssh_key` credential.
    #[serde(default)]
    credential: Option<String>,
    #[serde(default)]
    branch: Option<String>,
}

async fn remote_set(
    State(state): State<Shared>,
    actor: Actor,
    ApiJson(b): ApiJson<RemoteBody>,
) -> ApiResult<Json<Status>> {
    allow(&actor, "setting the config repo remote")?;
    policy::manage_secrets(&actor, crate::secrets::GLOBAL)?;
    let r = state.config_repo.get()?;
    let _busy = state.config_repo.busy.lock().await;
    let remote = Remote {
        url: b.url,
        credential: b.credential.filter(|c| !c.is_empty()),
        branch: b
            .branch
            .filter(|b| !b.is_empty())
            .unwrap_or_else(|| repo::DEFAULT_BRANCH.to_owned()),
    };
    let shown = repo::redact_url(&remote.url);
    let st = blocking(r, move |r| {
        r.set_remote(Some(remote))?;
        r.ensure()?;
        r.status()
    })
    .await?;
    audit(&state, &actor, "config.remote", json!({"url": shown})).await;
    Ok(Json(st))
}

async fn remote_clear(State(state): State<Shared>, actor: Actor) -> ApiResult<Json<Status>> {
    allow(&actor, "clearing the config repo remote")?;
    let r = state.config_repo.get()?;
    let _busy = state.config_repo.busy.lock().await;
    let st = blocking(r, |r| {
        r.set_remote(None)?;
        r.status()
    })
    .await?;
    audit(&state, &actor, "config.remote", json!({"url": null})).await;
    Ok(Json(st))
}

async fn defaults_pending(
    State(state): State<Shared>,
    actor: Actor,
) -> ApiResult<Json<Vec<Pending>>> {
    allow(&actor, "reading the config repo")?;
    let r = state.config_repo.get()?;
    Ok(Json(blocking(r, |r| Ok(defaults::pending(&r.dir))).await?))
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct PathBody {
    path: String,
}

async fn defaults_accept(
    State(state): State<Shared>,
    actor: Actor,
    ApiJson(b): ApiJson<PathBody>,
) -> ApiResult<Json<Vec<Pending>>> {
    allow(&actor, "changing the config repo")?;
    let r = state.config_repo.get()?;
    let path = b.path.clone();
    let left = blocking(r, move |r| {
        defaults::accept(&r.dir, &path).map_err(ApiError::invalid)?;
        Ok(defaults::pending(&r.dir))
    })
    .await?;
    // A new default of a template file.
    crate::template_files::refresh_quiet(&state).await;
    audit(
        &state,
        &actor,
        "config.defaults.accept",
        json!({"path": b.path}),
    )
    .await;
    Ok(Json(left))
}

async fn defaults_keep(
    State(state): State<Shared>,
    actor: Actor,
    ApiJson(b): ApiJson<PathBody>,
) -> ApiResult<Json<Vec<Pending>>> {
    allow(&actor, "changing the config repo")?;
    let r = state.config_repo.get()?;
    let path = b.path.clone();
    let left = blocking(r, move |r| {
        defaults::keep(&r.dir, &path).map_err(ApiError::invalid)?;
        Ok(defaults::pending(&r.dir))
    })
    .await?;
    audit(
        &state,
        &actor,
        "config.defaults.keep",
        json!({"path": b.path}),
    )
    .await;
    Ok(Json(left))
}

async fn audit(state: &Shared, actor: &Actor, what: &'static str, detail: Value) {
    let r = state
        .db
        .write(actor.label(), move |t| {
            t.audit(what, "config", None, detail)
        })
        .await;
    if let Err(e) = r {
        tracing::warn!("cannot write the audit row of {what}: {}", e.message);
    }
}
