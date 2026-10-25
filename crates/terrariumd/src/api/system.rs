//! Health, harness versions, reload, and audit. Targets and hosts are in `targets.rs` and
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
}

#[derive(Debug, Serialize, Deserialize)]
pub struct DefinitionCounts {
    pub projects: usize,
    pub agents: usize,
    pub targets: usize,
    pub subscriptions: usize,
}

pub async fn health(State(state): State<Shared>) -> Json<Health> {
    let d = state.definitions();
    Json(Health {
        status: "ok".into(),
        version: terrarium_core::VERSION.into(),
        uptime: state.started.elapsed().as_secs(),
        started_at: state.started_at,
        definitions: DefinitionCounts {
            projects: d.projects.len(),
            agents: d.agents.len(),
            targets: d.targets.len(),
            subscriptions: d.subscriptions.len(),
        },
    })
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

/// `GET /api/harnesses`: for each harness, the pin from `harnesses.toml`,
/// the adapter's tested range, the installed version, and whether it is in
/// the range; and the start policy (`TERRARIUM_HARNESS_VERSION_POLICY`).
/// With `latest`, also the latest release on npm and the update kind; npm
/// that does not answer gives `latest: null` and `latest_error`, never an
/// error status. Every caller may read it.
pub async fn harnesses(
    State(state): State<Shared>,
    ApiQuery(q): ApiQuery<HarnessQuery>,
) -> Json<crate::harness_versions::HarnessReport> {
    let mut report = state.supervisor.harness_versions(q.refresh).await;
    if q.latest {
        state.harness_latest.fill(&mut report, q.refresh).await;
    }
    Json(report)
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

/// `GET /api/audit`: the operator and sysadmins see every row; a project
/// admin sees the rows of its project.
pub async fn audit(
    State(state): State<Shared>,
    actor: Actor,
    ApiQuery(page): ApiQuery<Page>,
    ApiQuery(mut filter): ApiQuery<AuditFilter>,
) -> ApiResult<Response> {
    match &actor {
        Actor::Operator => {}
        Actor::Agent(a) if a.is_sysadmin() => {}
        Actor::Agent(a) if a.roles.contains(&Role::ProjectAdmin) => {
            if filter.project.as_deref().is_some_and(|p| p != a.project) {
                return Err(ApiError::forbidden(
                    "a project admin reads the audit rows of its own project only",
                ));
            }
            filter.project = Some(a.project.clone());
        }
        Actor::Agent(_) => {
            return Err(ApiError::forbidden(
                "reading the audit log needs project_admin, sysadmin, or the operator",
            ));
        }
    }
    filter.since = crate::time_param::opt_time_param("since", filter.since.as_deref())?;
    let rows = state
        .db
        .read(move |c| audit::list(c, &filter, &page))
        .await?;
    Ok(paged(rows))
}
