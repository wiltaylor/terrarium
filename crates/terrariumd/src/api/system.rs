//! Health, reload, audit, and the read-only target and host lists.

use axum::Json;
use axum::extract::State;
use axum::response::Response;
use serde::{Deserialize, Serialize};
use terrarium_core::Role;
use terrarium_core::config::{Definitions, TargetDef};

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

/// Host agents (PLAN §19). None connect yet, so the list is empty.
pub async fn hosts() -> Json<Vec<serde_json::Value>> {
    Json(Vec::new())
}

/// A target definition with its live state. Executors come in a later
/// phase, so `state` is `unknown`.
#[derive(Debug, Clone, Serialize)]
pub struct TargetView {
    /// The target name. Targets have no other ID yet.
    pub id: String,
    #[serde(flatten)]
    pub def: TargetDef,
    /// `global` or `project`.
    pub scope: &'static str,
    pub state: &'static str,
    pub snapshots: Vec<String>,
}

pub fn target_views(defs: &Definitions, project: Option<&str>, globals: bool) -> Vec<TargetView> {
    defs.targets
        .iter()
        .filter(|t| match (&t.project, project) {
            (None, _) => globals,
            (Some(_), None) => true,
            (Some(p), Some(want)) => p == want,
        })
        .map(|t| TargetView {
            id: t.name.clone(),
            def: t.clone(),
            scope: if t.project.is_some() {
                "project"
            } else {
                "global"
            },
            state: "unknown",
            snapshots: Vec::new(),
        })
        .collect()
}

/// `GET /api/targets`: every target for the operator and sysadmins; for an
/// agent, the global targets and those of its project.
pub async fn targets(State(state): State<Shared>, actor: Actor) -> Json<Vec<TargetView>> {
    let defs = state.definitions();
    let project = match &actor {
        Actor::Agent(a) if !a.is_sysadmin() => Some(a.project.as_str()),
        _ => None,
    };
    Json(target_views(&defs, project, true))
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
    let rows = state
        .db
        .read(move |c| audit::list(c, &filter, &page))
        .await?;
    Ok(paged(rows))
}
