//! Health, harness versions and updates, reload, and audit. Targets and hosts are in `targets.rs` and
//! `hosts.rs`.

use axum::Json;
use axum::extract::State;
use axum::response::Response;
use serde::{Deserialize, Serialize};
use terrarium_core::Role;
use terrarium_core::config::Definitions;

use super::{ApiQuery, Shared, paged};
use crate::actor::Actor;
use crate::error::{ApiError, ApiResult};
use crate::policy;
use crate::store::Page;
use crate::store::audit::{self, AuditFilter};
use crate::store::sync::SyncReport;

/// The body of `GET /api/health`.
#[derive(Debug, Serialize, Deserialize)]
pub struct Health {
    pub status: String,
    pub version: String,
    /// Seconds since the daemon started.
    pub uptime: u64,
    pub started_at: chrono::DateTime<chrono::Utc>,
    pub definitions: DefinitionCounts,
    /// The idle stop of an agent that sets none (`TERRARIUM_IDLE_STOP`):
    /// `off`, or a duration such as `10m`.
    #[serde(default)]
    pub idle_stop: String,
    /// The `compact_on_idle` of an agent that sets none
    /// (`TERRARIUM_COMPACT_ON_IDLE`).
    #[serde(default)]
    pub compact_on_idle: bool,
    /// The `compact_max_age` of an agent that sets none
    /// (`TERRARIUM_COMPACT_MAX_AGE`): `off`, or a duration such as `24h`.
    #[serde(default)]
    pub compact_max_age: String,
    /// The `card_reminder` of an agent that sets none
    /// (`TERRARIUM_CARD_REMINDER`).
    #[serde(default)]
    pub card_reminder: bool,
    /// The harnesses whose agents the daemon can compact (PLAN §18, "Auto
    /// compact"). The settings of an agent of another harness do nothing.
    #[serde(default)]
    pub compact_harnesses: Vec<String>,
}

#[derive(Debug, Serialize, Deserialize)]
pub struct DefinitionCounts {
    pub projects: usize,
    pub agents: usize,
    pub targets: usize,
    pub subscriptions: usize,
}

pub async fn health(State(state): State<Shared>) -> Response {
    use axum::response::IntoResponse;
    let failed = state.agent_calls.failure().is_some();
    let d = state.definitions();
    let body = Json(Health {
        status: if failed {
            "activation_failed"
        } else if state.agent_calls.in_probation() && !state.agent_calls.has_probation_marker() {
            "activating"
        } else {
            "ok"
        }
        .into(),
        version: terrarium_version::FULL.into(),
        uptime: state.started.elapsed().as_secs(),
        started_at: state.started_at,
        definitions: DefinitionCounts {
            projects: d.projects.len(),
            agents: d.agents.len(),
            targets: d.targets.len(),
            subscriptions: d.subscriptions.len(),
        },
        idle_stop: state.lifecycle.tuning().idle_stop.as_text(),
        compact_on_idle: state.lifecycle.tuning().compact_on_idle,
        compact_max_age: state.lifecycle.tuning().compact_max_age.as_text(),
        card_reminder: state.lifecycle.reminders.default_on(),
        compact_harnesses: terrarium_core::HarnessId::ALL
            .into_iter()
            .filter(|h| h.compact_command().is_some())
            .map(|h| h.as_str().to_owned())
            .collect(),
    });
    (
        if failed {
            axum::http::StatusCode::SERVICE_UNAVAILABLE
        } else {
            axum::http::StatusCode::OK
        },
        body,
    )
        .into_response()
}

/// The body of `GET /api/system/version` (docs/api.md §1): the build of
/// the daemon.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct VersionInfo {
    /// The full version, for example `0.1.0+6e31754`.
    pub version: String,
    /// The crate version, for example `0.1.0`.
    pub cargo_version: String,
    /// The short commit of the build, or `unknown`.
    pub git_ref: String,
    /// The build tree had changes to tracked files.
    pub dirty: bool,
    /// The build time, RFC 3339 UTC.
    pub build_time: String,
}

impl VersionInfo {
    /// The build of this binary.
    pub fn current() -> Self {
        Self {
            version: terrarium_version::FULL.into(),
            cargo_version: terrarium_version::VERSION.into(),
            git_ref: terrarium_version::GIT_REF.into(),
            dirty: terrarium_version::DIRTY,
            build_time: terrarium_version::BUILD_TIME.into(),
        }
    }
}

/// `GET /api/system/version`: any caller that passes auth.
pub async fn version() -> Json<VersionInfo> {
    Json(VersionInfo::current())
}

/// The query of `GET /api/harnesses`.
#[derive(Debug, Default, Deserialize)]
pub struct HarnessQuery {
    /// Run `<binary> --version` again, and ask npm again with `latest`;
    /// else cached answers (10 minutes, and 1 hour for npm).
    #[serde(default)]
    pub refresh: bool,
    /// Add the latest release on npm, and the update kind, to each row.
    #[serde(default)]
    pub latest: bool,
}

/// `GET /api/harnesses`: for each harness, the pin from `harnesses.toml`
/// and the installed version; with a tools dir, the active version dir,
/// the version dirs on disk, and the live agents with the version each
/// runs. Each row has its update
/// policy and its last auto-update check (`auto_update`), and the report
/// has the schedule of the check. With `latest`, also the
/// latest release on npm and the update kind; npm that does not answer
/// gives `latest: null` and `latest_error`, never an error status. Every
/// caller may read it.
pub async fn harnesses(
    State(state): State<Shared>,
    ApiQuery(q): ApiQuery<HarnessQuery>,
) -> ApiResult<Json<crate::harness_versions::HarnessReport>> {
    let mut report = state.supervisor.harness_versions(q.refresh).await;
    if q.latest {
        state.harness_latest.fill(&mut report, q.refresh).await;
    }
    let agents = agent_versions(&state).await?;
    let policies = state.db.read(crate::store::harness_updates::all).await?;
    let cfg = state.harness_tools.config();
    for row in &mut report.harnesses {
        if let Some(cfg) = &cfg {
            row.active = crate::harness_tools::active_version(&cfg.dir, row.harness.as_str())
                .map(|a| a.version);
            row.versions = cfg.versions(row.harness.as_str());
        }
        row.agents = agents
            .iter()
            .filter(|(h, _)| *h == row.harness)
            .map(|(_, a)| a.clone())
            .collect();
        let st = policies
            .iter()
            .find(|p| p.harness == row.harness.as_str())
            .cloned()
            .unwrap_or_else(|| {
                crate::store::harness_updates::UpdateState::new(row.harness.as_str())
            });
        row.auto_update = Some(crate::harness_auto_update::row_info(
            row.harness,
            &st,
            row.active.as_deref(),
            &row.versions,
        ));
    }
    report.auto_update = Some(crate::harness_auto_update::schedule_info(&state));
    Ok(Json(report))
}

/// The body of `POST /api/harnesses/{name}/update` and
/// `POST /api/harnesses/update`. Both fields are optional; an empty body is
/// `{"version": "latest"}`.
#[derive(Debug, Default, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct HarnessUpdateBody {
    /// `latest` (the default), or a version like `0.165.0`.
    #[serde(default)]
    pub version: Option<String>,
    /// Install the version again even when its version dir is complete on
    /// disk. Live agents never stop an update: each keeps the version it
    /// runs until its next start.
    #[serde(default)]
    pub force: bool,
}

/// One harness in the answer of an update.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct HarnessUpdateResult {
    /// The harness, or the extension name (`copilot-prompt`).
    pub harness: String,
    pub package: String,
    /// The version before; `null` when it was not installed.
    pub from: Option<String>,
    /// The version after; `null` when the update failed.
    pub to: Option<String>,
    /// npm's output (stdout, then stderr).
    pub output: String,
    /// True when the version dir was complete on disk, so npm did not run.
    #[serde(default)]
    pub reused: bool,
    /// The live agents on the harness that run another version than `to`.
    /// Each keeps its version until its next start.
    #[serde(default)]
    pub agents_on_old_version: Vec<crate::harness_versions::AgentOnVersion>,
    /// What happens to `agents_on_old_version`, in one line.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub note: Option<String>,
    /// The version dirs of this pin that the GC after the update removed.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub removed: Vec<String>,
    /// Only on a failure: why.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
}

fn update_body(body: &[u8]) -> ApiResult<HarnessUpdateBody> {
    if body.iter().all(u8::is_ascii_whitespace) {
        return Ok(HarnessUpdateBody::default());
    }
    serde_json::from_slice(body).map_err(|e| ApiError::bad_request(e.to_string()))
}

/// The live agents (`project/name`, ID) on each harness.
async fn live_agents(state: &Shared) -> ApiResult<Vec<(String, String, String)>> {
    state
        .db
        .read(|c| {
            let mut st = c.prepare(
                "SELECT harness, project || '/' || name, id FROM agents
                 WHERE status IN ('starting', 'idle', 'waiting', 'working', 'blocked', 'restarting')
                 ORDER BY project, name",
            )?;
            let rows = st
                .query_map([], |r| {
                    Ok((
                        r.get::<_, String>(0)?,
                        r.get::<_, String>(1)?,
                        r.get::<_, String>(2)?,
                    ))
                })?
                .collect::<Result<Vec<_>, _>>()?;
            Ok(rows)
        })
        .await
}

/// Each live agent with its harness and the version it runs: the store
/// gives the live agents, the supervisor the versions.
async fn agent_versions(
    state: &Shared,
) -> ApiResult<
    Vec<(
        terrarium_core::HarnessId,
        crate::harness_versions::AgentOnVersion,
    )>,
> {
    let runs = state.supervisor.harness_runs();
    Ok(live_agents(state)
        .await?
        .into_iter()
        .filter_map(|(h, agent, id)| {
            let harness = h.parse().ok()?;
            let version = runs
                .iter()
                .find(|r| r.agent.to_string() == id)
                .and_then(|r| r.version.clone());
            Some((
                harness,
                crate::harness_versions::AgentOnVersion { agent, id, version },
            ))
        })
        .collect())
}

/// The tools dir, or `501 not_available`.
fn tools(state: &Shared) -> ApiResult<std::sync::Arc<crate::harness_tools::ToolsConfig>> {
    state.harness_tools.config().ok_or_else(|| {
        ApiError::new(
            crate::error::Code::NotAvailable,
            "this daemon has no tools dir (TERRARIUM_TOOLS_DIR): update the harnesses where they \
             are installed",
        )
    })
}

/// Remove the version dirs of `cfg` that are not active, that no live
/// agent runs, and that are not the previous or the pinned version of
/// their harness ([`crate::harness_tools::ToolsConfig::gc`]). When no live
/// agent started on the flat layout of before the version dirs, remove
/// that too. The caller holds the update lock. Answers the removed
/// versions (`<name> <version>`).
pub async fn collect_garbage(
    state: &Shared,
    cfg: std::sync::Arc<crate::harness_tools::ToolsConfig>,
) -> Vec<String> {
    let mut in_use = crate::harness_tools::InUse::new();
    // The rollback target and the pin of each harness stay on disk.
    match state.db.read(crate::store::harness_updates::all).await {
        Ok(rows) => {
            for r in rows {
                let keep = in_use.entry(r.harness.clone()).or_default();
                keep.extend(r.previous);
                keep.extend(r.pinned_version);
            }
        }
        Err(e) => {
            tracing::warn!(
                "the harness GC cannot read the update policies: {e}; it removes nothing"
            );
            return Vec::new();
        }
    }
    let mut flat = false;
    for r in state.supervisor.harness_runs() {
        match r.version {
            Some(v) => {
                in_use.entry(r.harness.to_string()).or_default().insert(v);
            }
            None => flat = true,
        }
    }
    let result = tokio::task::spawn_blocking(move || {
        let removed = cfg.gc(Some(&in_use));
        if !flat && let Err(e) = cfg.remove_flat() {
            tracing::warn!("cannot remove the flat harness layout: {e}");
        }
        removed
    })
    .await;
    result.unwrap_or_else(|e| {
        tracing::warn!("the harness GC did not finish: {e}");
        Vec::new()
    })
}

/// The live agents on the harness of `pin` that run another version than
/// `to`, and the note about them.
async fn old_agents(
    state: &Shared,
    pin: &crate::harness_tools::Pin,
    to: &str,
) -> ApiResult<(Vec<crate::harness_versions::AgentOnVersion>, Option<String>)> {
    let old: Vec<crate::harness_versions::AgentOnVersion> = agent_versions(state)
        .await?
        .into_iter()
        .filter(|(h, a)| *h == pin.harness && a.version.as_deref() != Some(to))
        .map(|(_, a)| a)
        .collect();
    let note = (!old.is_empty()).then(|| {
        format!(
            "{} live agent(s) run another version of {}: {}. Each keeps its version and moves \
             to {to} at its next restart",
            old.len(),
            pin.name,
            old.iter()
                .map(|a| a.agent.as_str())
                .collect::<Vec<_>>()
                .join(", ")
        )
    });
    Ok((old, note))
}

/// Run the update of one pin, then the audit row and the `harness` event.
/// The version cache is not refreshed here.
async fn update_one(
    state: &Shared,
    actor: &Actor,
    cfg: &crate::harness_tools::ToolsConfig,
    pin: &crate::harness_tools::Pin,
    version: &str,
    force: bool,
) -> ApiResult<HarnessUpdateResult> {
    let r = cfg.update(&pin.name, version, force).await;
    let (from, to, output, reused, error) = match &r {
        Ok(u) => (
            u.from.clone(),
            Some(u.to.clone()),
            u.output.clone(),
            u.reused,
            None,
        ),
        Err(f) => (
            f.from.clone(),
            None,
            f.output.clone(),
            false,
            Some(f.message.clone()),
        ),
    };
    let (old, note) = match &to {
        Some(to) if !pin.extension => old_agents(state, pin, to).await?,
        _ => (Vec::new(), None),
    };
    let name = pin.name.clone();
    let package = pin.package.clone();
    let old_names: Vec<&str> = old.iter().map(|a| a.agent.as_str()).collect();
    let detail = serde_json::json!({
        "package": package, "version": version, "from": from, "to": to,
        "force": force, "reused": reused, "ok": error.is_none(), "error": error,
        "agents_on_old_version": old_names,
    });
    let event = to.as_ref().map(|to| {
        crate::events::EventDraft::new(
            "harness",
            format!(
                "{name} {} -> {to}",
                from.as_deref().unwrap_or("(not installed)")
            ),
            serde_json::json!({
                "change": "updated", "harness": name, "package": package,
                "from": from, "to": to, "forced": force,
                "agents_on_old_version": old_names,
            }),
        )
    });
    let target = pin.name.clone();
    let switch = (!pin.extension)
        .then(|| to.clone().map(|to| (from.clone(), to)))
        .flatten();
    state
        .db
        .write(actor.label(), move |t| {
            t.audit("harness.update", &target, None, detail)?;
            if let Some(e) = event {
                t.emit(e)?;
            }
            // The rollback target; a pinned harness is now pinned to the
            // new version.
            if let Some((from, to)) = &switch {
                crate::store::harness_updates::record_switch(t, &target, from.as_deref(), to)?;
            }
            Ok(())
        })
        .await?;
    Ok(HarnessUpdateResult {
        harness: pin.name.clone(),
        package: pin.package.clone(),
        from,
        to,
        output,
        reused,
        agents_on_old_version: old,
        note,
        removed: Vec::new(),
        error,
    })
}

/// Run `<binary> --version` again for each harness, so the version
/// cache sees the new versions.
async fn refresh(state: &Shared) {
    state.supervisor.harness_versions(true).await;
}

/// The GC after an update; each result gets the removed versions of its
/// pin.
async fn gc_after(
    state: &Shared,
    cfg: std::sync::Arc<crate::harness_tools::ToolsConfig>,
    results: &mut [HarnessUpdateResult],
) {
    let removed = collect_garbage(state, cfg).await;
    for r in results {
        let prefix = format!("{} ", r.harness);
        r.removed = removed
            .iter()
            .filter_map(|x| x.strip_prefix(&prefix).map(str::to_owned))
            .collect();
    }
}

/// `POST /api/harnesses/{name}/update`: install the version into its own
/// version dir in the tools dir (`npm install -g` in the container; no npm
/// run when the dir is complete on disk), check `--version`, and switch to
/// it. Live agents keep the version they run; each moves to the new one at
/// its next start (`agents_on_old_version`). Then the GC removes the
/// version dirs that nothing uses. Needs sysadmin (or the operator).
/// `409 conflict` while another update runs; `422 invalid` for a bad name
/// or version, and for an npm failure (`details.output`).
pub async fn harness_update(
    State(state): State<Shared>,
    actor: Actor,
    axum::extract::Path(name): axum::extract::Path<String>,
    body: axum::body::Bytes,
) -> ApiResult<Json<HarnessUpdateResult>> {
    policy::sysadmin(&actor, "updating a harness")?;
    let body = update_body(&body)?;
    let cfg = tools(&state)?;
    let pin = cfg.pin(&name).cloned().ok_or_else(|| {
        let known: Vec<&str> = cfg.pins.iter().map(|p| p.name.as_str()).collect();
        ApiError::invalid(format!(
            "unknown harness `{name}`: use one of {}",
            known.join(", ")
        ))
    })?;
    let version = body.version.as_deref().unwrap_or("latest");
    if !crate::harness_tools::valid_version(version) {
        return Err(ApiError::invalid(format!(
            "`{version}` is not `latest` or a version like 1.2.3"
        )));
    }
    let _busy = state.harness_tools.busy.try_lock().map_err(|_| {
        ApiError::conflict("another harness update runs now; try again when it ends")
    })?;
    let mut results = vec![update_one(&state, &actor, &cfg, &pin, version, body.force).await?];
    refresh(&state).await;
    if results[0].error.is_none() {
        gc_after(&state, cfg.clone(), &mut results).await;
    }
    let r = results.remove(0);
    if let Some(e) = &r.error {
        return Err(
            ApiError::invalid(e.clone()).with_details(serde_json::json!({
                "output": r.output, "package": r.package, "version": version, "from": r.from,
            })),
        );
    }
    Ok(Json(r))
}

/// The answer of `POST /api/harnesses/update`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct HarnessUpdateAll {
    pub results: Vec<HarnessUpdateResult>,
}

/// `POST /api/harnesses/update`: every harness and extension to the latest
/// release, one after another. A failure does not stop the others; any
/// failure makes the answer `422 invalid` with `details.results`. Live
/// agents keep their versions, as for one update.
pub async fn harness_update_all(
    State(state): State<Shared>,
    actor: Actor,
    body: axum::body::Bytes,
) -> ApiResult<Json<HarnessUpdateAll>> {
    policy::sysadmin(&actor, "updating the harnesses")?;
    let body = update_body(&body)?;
    let version = body.version.as_deref().unwrap_or("latest");
    if version != "latest" {
        return Err(ApiError::invalid(
            "`version` of an update of every harness can only be `latest`",
        ));
    }
    let cfg = tools(&state)?;
    let _busy = state.harness_tools.busy.try_lock().map_err(|_| {
        ApiError::conflict("another harness update runs now; try again when it ends")
    })?;
    let mut results = Vec::new();
    for pin in &cfg.pins {
        results.push(update_one(&state, &actor, &cfg, pin, version, body.force).await?);
    }
    refresh(&state).await;
    gc_after(&state, cfg.clone(), &mut results).await;
    let failed = results.iter().filter(|r| r.error.is_some()).count();
    if failed > 0 {
        return Err(
            ApiError::invalid(format!("{failed} of {} updates failed", results.len()))
                .with_details(serde_json::json!({ "results": results })),
        );
    }
    Ok(Json(HarnessUpdateAll { results }))
}

/// `POST /api/harnesses/check`: run the auto-update check now, as the
/// background task does (`crate::harness_auto_update`): ask npm for each
/// harness, and install a newer release of each harness with the policy
/// `auto`. Needs sysadmin (or the operator). `409 conflict` while another
/// check runs; `501 not_available` without a tools dir.
pub async fn harness_check(
    State(state): State<Shared>,
    actor: Actor,
) -> ApiResult<Json<crate::harness_auto_update::CheckReport>> {
    policy::sysadmin(&actor, "checking the harnesses for updates")?;
    tools(&state)?;
    let _running = state
        .harness_auto_update
        .running
        .try_lock()
        .map_err(|_| ApiError::conflict("a harness check runs now; try again when it ends"))?;
    Ok(Json(
        crate::harness_auto_update::run_check(&state, chrono::Utc::now()).await,
    ))
}

/// The body of `POST /api/harnesses/{name}/pin`. An empty body pins the
/// previous version.
#[derive(Debug, Default, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct HarnessPinBody {
    /// A version like `0.164.0`. Absent: the previous version (a
    /// rollback).
    #[serde(default)]
    pub version: Option<String>,
}

/// The answer of `POST /api/harnesses/{name}/pin` and `unpin`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct HarnessPinResult {
    pub harness: String,
    pub policy: crate::store::harness_updates::UpdatePolicy,
    /// The version of the pin; `null` after an unpin.
    pub pinned_version: Option<String>,
    /// The active version before.
    pub from: Option<String>,
    /// The active version after.
    pub to: Option<String>,
    /// npm's output, when the pin installed the version.
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub output: String,
    /// The live agents on another version than `to`. Each keeps its
    /// version until its next start.
    #[serde(default)]
    pub agents_on_old_version: Vec<crate::harness_versions::AgentOnVersion>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub note: Option<String>,
    /// The version dirs that the GC after the switch removed.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub removed: Vec<String>,
}

/// The harness pin `name` of `cfg` (not an extension), or `422 invalid`.
fn harness_pin_of(
    cfg: &crate::harness_tools::ToolsConfig,
    name: &str,
) -> ApiResult<crate::harness_tools::Pin> {
    cfg.pins
        .iter()
        .find(|p| p.name == name && !p.extension)
        .cloned()
        .ok_or_else(|| {
            let known: Vec<&str> = cfg
                .pins
                .iter()
                .filter(|p| !p.extension)
                .map(|p| p.name.as_str())
                .collect();
            ApiError::invalid(format!(
                "unknown harness `{name}`: use one of {}",
                known.join(", ")
            ))
        })
}

/// `POST /api/harnesses/{name}/pin`: switch the harness to a version and
/// stop its auto-update. Without `version`, the version is the previous
/// one (the version before the last switch, else the newest version dir
/// under the active one): a rollback. A version that is not on disk is
/// installed first. The switch is for new starts; running agents keep
/// their version until their next start. Needs sysadmin (or the
/// operator). `409 conflict` while an update runs; `422 invalid` for a
/// bad name or version, no previous version, or an npm failure.
pub async fn harness_pin(
    State(state): State<Shared>,
    actor: Actor,
    axum::extract::Path(name): axum::extract::Path<String>,
    body: axum::body::Bytes,
) -> ApiResult<Json<HarnessPinResult>> {
    use crate::store::harness_updates::{self as hu, UpdatePolicy};
    policy::sysadmin(&actor, "pinning a harness")?;
    let body: HarnessPinBody = if body.iter().all(u8::is_ascii_whitespace) {
        HarnessPinBody::default()
    } else {
        serde_json::from_slice(&body).map_err(|e| ApiError::bad_request(e.to_string()))?
    };
    let cfg = tools(&state)?;
    let pin = harness_pin_of(&cfg, &name)?;
    let _busy = state
        .harness_tools
        .busy
        .try_lock()
        .map_err(|_| ApiError::conflict("a harness update runs now; try again when it ends"))?;
    let active = cfg.active(&pin).map(|a| a.version);
    let key = name.clone();
    let st = state.db.read(move |c| hu::get(c, &key)).await?;
    let version = match body.version {
        Some(v) => v,
        None => crate::harness_auto_update::previous_version(
            &st,
            active.as_deref(),
            &cfg.versions(&name),
        )
        .ok_or_else(|| {
            ApiError::invalid(format!(
                "{name} has no previous version on disk: give a version (`terrarium admin \
                 harness pin {name} <version>`)"
            ))
        })?,
    };
    if semver::Version::parse(&version).is_err() {
        return Err(ApiError::invalid(format!(
            "`{version}` is not a version like 1.2.3"
        )));
    }
    let u = match cfg.update(&name, &version, false).await {
        Ok(u) => u,
        Err(f) => {
            let detail = serde_json::json!({
                "version": version, "from": f.from, "ok": false, "error": f.message,
            });
            let target = name.clone();
            state
                .db
                .write(actor.label(), move |t| {
                    t.audit("harness.pin", &target, None, detail)
                })
                .await?;
            return Err(
                ApiError::invalid(f.message).with_details(serde_json::json!({
                    "output": f.output, "package": pin.package, "version": version,
                    "from": f.from,
                })),
            );
        }
    };
    let (old, note) = old_agents(&state, &pin, &u.to).await?;
    let old_names: Vec<String> = old.iter().map(|a| a.agent.clone()).collect();
    let (target, from, to, package) = (
        name.clone(),
        u.from.clone(),
        u.to.clone(),
        pin.package.clone(),
    );
    let rollback = st.policy == UpdatePolicy::Auto && from.as_deref() != Some(to.as_str());
    state
        .db
        .write(actor.label(), move |t| {
            hu::record_switch(t, &target, from.as_deref(), &to)?;
            hu::set_policy(t, &target, UpdatePolicy::Pinned, Some(&to))?;
            t.audit(
                "harness.pin",
                &target,
                None,
                serde_json::json!({"version": to, "from": from, "ok": true,
                                   "agents_on_old_version": old_names}),
            )?;
            t.emit(crate::events::EventDraft::new(
                "harness",
                format!(
                    "{target} pinned to {to}{}",
                    if rollback { " (rollback)" } else { "" }
                ),
                serde_json::json!({"change": "pinned", "harness": target, "package": package,
                                   "from": from, "to": to,
                                   "agents_on_old_version": old_names}),
            ))?;
            Ok(())
        })
        .await?;
    let _ = state.supervisor.harness_versions(true).await;
    let prefix = format!("{name} ");
    let removed = collect_garbage(&state, cfg.clone())
        .await
        .into_iter()
        .filter_map(|x| x.strip_prefix(&prefix).map(str::to_owned))
        .collect();
    Ok(Json(HarnessPinResult {
        harness: name,
        policy: UpdatePolicy::Pinned,
        pinned_version: Some(u.to.clone()),
        from: u.from,
        to: Some(u.to),
        output: if u.reused { String::new() } else { u.output },
        agents_on_old_version: old,
        note,
        removed,
    }))
}

/// `POST /api/harnesses/{name}/unpin`: set the policy of the harness back
/// to `auto`. The active version stays; the next check may update it.
/// Needs sysadmin (or the operator).
pub async fn harness_unpin(
    State(state): State<Shared>,
    actor: Actor,
    axum::extract::Path(name): axum::extract::Path<String>,
) -> ApiResult<Json<HarnessPinResult>> {
    use crate::store::harness_updates::{self as hu, UpdatePolicy};
    policy::sysadmin(&actor, "unpinning a harness")?;
    let cfg = tools(&state)?;
    let pin = harness_pin_of(&cfg, &name)?;
    let active = cfg.active(&pin).map(|a| a.version);
    let target = name.clone();
    let was = state
        .db
        .write(actor.label(), move |t| {
            let was = hu::get(t, &target)?.pinned_version;
            hu::set_policy(t, &target, UpdatePolicy::Auto, None)?;
            t.audit(
                "harness.unpin",
                &target,
                None,
                serde_json::json!({"was": was}),
            )?;
            t.emit(crate::events::EventDraft::new(
                "harness",
                format!("{target} unpinned: auto-update"),
                serde_json::json!({"change": "unpinned", "harness": target, "was": was}),
            ))?;
            Ok(was)
        })
        .await?;
    tracing::info!(harness = %name, was = ?was, "harness unpinned");
    Ok(Json(HarnessPinResult {
        harness: name,
        policy: UpdatePolicy::Auto,
        pinned_version: None,
        from: active.clone(),
        to: active,
        output: String::new(),
        agents_on_old_version: Vec::new(),
        note: None,
        removed: Vec::new(),
    }))
}

/// `POST /api/reload`: read the definition files again and sync them.
pub async fn reload(State(state): State<Shared>, actor: Actor) -> ApiResult<Json<SyncReport>> {
    policy::sysadmin(&actor, "reloading definitions")?;
    let dir = state.config_dir.clone();
    let (defs, errors) = tokio::task::spawn_blocking(move || Definitions::load(&dir))
        .await
        .map_err(|e| ApiError::internal(format!("reload task failed: {e}")))?;
    let errors = errors.iter().map(ToString::to_string).collect();
    let report = crate::apply_definitions(&state, defs, errors, actor.label()).await?;
    Ok(Json(report))
}

/// Which audit rows `actor` may read: `Ok(None)` every row (the operator
/// and a sysadmin), `Ok(Some(project))` the rows of one project (a project
/// admin), or `403` (any other agent). `GET /api/audit` and the `audit`
/// frames of the firehose (docs/api.md §9.1) both use this rule.
pub fn audit_scope(actor: &Actor) -> ApiResult<Option<String>> {
    match actor {
        Actor::Operator => Ok(None),
        Actor::Agent(a) if a.reads_all() => Ok(None),
        Actor::Agent(a) if a.roles.contains(&Role::ProjectAdmin) => Ok(Some(a.project.clone())),
        Actor::Agent(_) => Err(ApiError::forbidden(
            "reading the audit log needs project_admin, sysadmin, or the operator",
        )),
    }
}

/// `GET /api/audit`: the operator and sysadmins see every row; a project
/// admin sees the rows of its project.
pub async fn audit(
    State(state): State<Shared>,
    actor: Actor,
    ApiQuery(page): ApiQuery<Page>,
    ApiQuery(mut filter): ApiQuery<AuditFilter>,
) -> ApiResult<Response> {
    if let Some(own) = audit_scope(&actor)? {
        if filter.project.as_deref().is_some_and(|p| p != own) {
            return Err(ApiError::forbidden(
                "a project admin reads the audit rows of its own project only",
            ));
        }
        filter.project = Some(own);
    }
    match filter.order.as_deref() {
        None | Some("" | "asc" | "desc") => {}
        Some(other) => {
            return Err(ApiError::bad_request(format!(
                "bad order `{other}`: use asc or desc"
            )));
        }
    }
    filter.since = crate::time_param::opt_time_param("since", filter.since.as_deref())?;
    let mut rows = state
        .db
        .read(move |c| {
            let mut rows = audit::list(c, &filter, &page)?;
            audit::attach_links(c, &mut rows.items)?;
            Ok(rows)
        })
        .await?;
    let mut linker = super::session_links::Linker::new(&state, &actor);
    for e in &mut rows.items {
        linker.show(&mut e.link);
    }
    Ok(paged(rows))
}
