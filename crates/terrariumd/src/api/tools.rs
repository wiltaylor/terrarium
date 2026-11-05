//! Tool proxies, P1a (GitHub #21, the wiki page `design/tool-proxies`):
//! the definitions and the grants. The run path comes later (P1c).
//!
//! | Route | Who |
//! |---|---|
//! | `GET /api/tools` | all: the operator and a sysadmin see every tool with its grants; an agent sees the tools granted to it (its own grants and its project's) |
//! | `GET /api/tools/{name}` | the same rule; a tool that an agent may not see is `404` |
//! | `POST /api/tools`, `PATCH`, `DELETE /api/tools/{name}` | the operator and sysadmins |
//! | `GET`, `POST`, `DELETE /api/tools/{name}/grants` | the operator and sysadmins (design Q2: no project_admin in P1) |
//!
//! Each change writes an audit row: `tool.create`, `tool.update`,
//! `tool.delete`, `tool.grant`, `tool.revoke`.

use std::path::PathBuf;

use axum::extract::{Path, State};
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use axum::routing::get;
use axum::{Json, Router};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use terrarium_core::tool::ToolDef;

use super::{ApiJson, ApiQuery, Shared};
use crate::actor::Actor;
use crate::error::{ApiError, ApiResult};
use crate::policy;
use crate::store::tools::{self, ToolGrant, ToolRow};

pub fn routes() -> Router<Shared> {
    Router::new()
        .route("/tools", get(list).post(create))
        .route("/tools/{name}", get(show).patch(update).delete(remove))
        .route(
            "/tools/{name}/grants",
            get(grants_list).post(grant).delete(revoke),
        )
}

/// A tool as the API shows it. `grants` only for the operator and sysadmins.
#[derive(Debug, Clone, Serialize)]
pub struct ToolView {
    #[serde(flatten)]
    pub row: ToolRow,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub grants: Option<Vec<ToolGrant>>,
}

/// May the actor define and grant tools? The operator and sysadmins.
fn manage(actor: &Actor, what: &str) -> ApiResult<()> {
    policy::sysadmin(actor, what)
}

fn manages(actor: &Actor) -> bool {
    manage(actor, "").is_ok()
}

/// More `PATH` dirs of the sandbox: the `bin/` of the tools dir.
fn extra_dirs(state: &Shared) -> Vec<PathBuf> {
    state
        .harness_tools
        .config()
        .map(|c| vec![c.bin_dir()])
        .unwrap_or_default()
}

async fn list(State(state): State<Shared>, actor: Actor) -> ApiResult<Json<Vec<ToolView>>> {
    let admin = manages(&actor);
    let agent = actor.agent().map(|a| (a.id.clone(), a.project.clone()));
    let views = state
        .db
        .read(move |c| {
            if admin {
                tools::list(c)?
                    .into_iter()
                    .map(|row| {
                        let grants = tools::grants_of(c, &row.def.name)?;
                        Ok(ToolView {
                            row,
                            grants: Some(grants),
                        })
                    })
                    .collect()
            } else if let Some((id, project)) = agent {
                Ok(tools::granted_to(c, &id, &project)?
                    .into_iter()
                    .map(|row| ToolView { row, grants: None })
                    .collect())
            } else {
                Ok(Vec::new())
            }
        })
        .await?;
    Ok(Json(views))
}

async fn show(
    State(state): State<Shared>,
    actor: Actor,
    Path(name): Path<String>,
) -> ApiResult<Json<ToolView>> {
    let admin = manages(&actor);
    let agent = actor.agent().map(|a| (a.id.clone(), a.project.clone()));
    let v = state
        .db
        .read(move |c| {
            if admin {
                let row = tools::get(c, &name)?;
                let grants = tools::grants_of(c, &name)?;
                return Ok(ToolView {
                    row,
                    grants: Some(grants),
                });
            }
            // An agent sees only a tool granted to it: another one is 404,
            // so the list of tools does not leak.
            let (id, project) =
                agent.ok_or_else(|| ApiError::not_found(format!("no tool `{name}`")))?;
            tools::granted_to(c, &id, &project)?
                .into_iter()
                .find(|r| r.def.name == name)
                .map(|row| ToolView { row, grants: None })
                .ok_or_else(|| ApiError::not_found(format!("no tool `{name}`")))
        })
        .await?;
    Ok(Json(v))
}

async fn create(
    State(state): State<Shared>,
    actor: Actor,
    ApiJson(def): ApiJson<ToolDef>,
) -> ApiResult<Response> {
    manage(&actor, "defining a tool")?;
    let dirs = extra_dirs(&state);
    let row = state
        .db
        .write(actor.label(), move |t| {
            let refs: Vec<&std::path::Path> = dirs.iter().map(PathBuf::as_path).collect();
            let row = tools::create(t, &def, &refs)?;
            t.audit(
                "tool.create",
                &row.def.name,
                row.def.project.as_deref(),
                serde_json::to_value(&row.def)?,
            )?;
            Ok(row)
        })
        .await?;
    Ok((
        StatusCode::CREATED,
        Json(ToolView {
            row,
            grants: Some(Vec::new()),
        }),
    )
        .into_response())
}

async fn update(
    State(state): State<Shared>,
    actor: Actor,
    Path(name): Path<String>,
    ApiJson(patch): ApiJson<Value>,
) -> ApiResult<Json<ToolView>> {
    manage(&actor, "changing a tool")?;
    let dirs = extra_dirs(&state);
    let v = state
        .db
        .write(actor.label(), move |t| {
            let refs: Vec<&std::path::Path> = dirs.iter().map(PathBuf::as_path).collect();
            let row = tools::update(t, &name, &patch, &refs)?;
            t.audit("tool.update", &name, row.def.project.as_deref(), patch)?;
            let grants = tools::grants_of(t, &name)?;
            Ok(ToolView {
                row,
                grants: Some(grants),
            })
        })
        .await?;
    Ok(Json(v))
}

async fn remove(
    State(state): State<Shared>,
    actor: Actor,
    Path(name): Path<String>,
) -> ApiResult<StatusCode> {
    manage(&actor, "deleting a tool")?;
    state
        .db
        .write(actor.label(), move |t| {
            let row = tools::delete(t, &name)?;
            t.audit("tool.delete", &name, row.def.project.as_deref(), json!({}))
        })
        .await?;
    Ok(StatusCode::NO_CONTENT)
}

async fn grants_list(
    State(state): State<Shared>,
    actor: Actor,
    Path(name): Path<String>,
) -> ApiResult<Json<Vec<ToolGrant>>> {
    manage(&actor, "reading the grants of a tool")?;
    Ok(Json(
        state.db.read(move |c| tools::grants_of(c, &name)).await?,
    ))
}

/// The body of `POST /api/tools/{name}/grants`.
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct GrantBody {
    /// `agent:<project>/<name>`, `agent:<ulid>`, or `project:<name>`.
    grantee: String,
}

async fn grant(
    State(state): State<Shared>,
    actor: Actor,
    Path(name): Path<String>,
    ApiJson(b): ApiJson<GrantBody>,
) -> ApiResult<Response> {
    manage(&actor, "granting a tool")?;
    let by = actor.label();
    let g = state
        .db
        .write(actor.label(), move |t| {
            let (g, who) = tools::grant(t, &name, &b.grantee, &by)?;
            let project = tools::get(t, &name)?.def.project;
            t.audit(
                "tool.grant",
                &name,
                project.as_deref().or(Some(who.project())),
                json!({"grantee": g.grantee, "grantee_label": g.grantee_label}),
            )?;
            Ok(g)
        })
        .await?;
    Ok((StatusCode::CREATED, Json(g)).into_response())
}

/// The query of `DELETE /api/tools/{name}/grants`.
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct RevokeQuery {
    grantee: String,
}

async fn revoke(
    State(state): State<Shared>,
    actor: Actor,
    Path(name): Path<String>,
    ApiQuery(q): ApiQuery<RevokeQuery>,
) -> ApiResult<StatusCode> {
    manage(&actor, "revoking a tool")?;
    state
        .db
        .write(actor.label(), move |t| {
            let who = tools::revoke(t, &name, &q.grantee)?;
            let project = tools::get(t, &name)?.def.project;
            t.audit(
                "tool.revoke",
                &name,
                project.as_deref().or(Some(who.project())),
                json!({"grantee": who.key(), "grantee_label": who.label()}),
            )
        })
        .await?;
    Ok(StatusCode::NO_CONTENT)
}
