//! `/api/projects/{name}/rules` and `/api/rules/schema` (docs/api.md
//! §19.8, TER-166): the board rules file of a project.
//!
//! Read: whoever reads the project. Write (put, delete): the operator, a
//! project admin of the project, or a sysadmin (`policy::admin_project`,
//! as for the other project settings; docs/api.md §19.8). Each write
//! writes an audit row (`crate::store::rules`). Nothing runs the rules
//! yet.
//!
//! The WCL parse of a text never runs inside a DB closure: the handler
//! reads the context under the lock, parses on a blocking thread
//! (`rules::check`, `rules::with_rules`), then writes.

use axum::extract::{Path, State};
use axum::http::StatusCode;
use axum::routing::{get, post};
use axum::{Json, Router};
use serde::Deserialize;
use serde_json::{Value, json};
use terrarium_core::rules_file;

use super::{ApiJson, ApiQuery, Shared};
use crate::actor::Actor;
use crate::error::{ApiError, ApiResult};
use crate::policy;
use crate::store::rules::{self, Rules};

pub fn routes() -> Router<Shared> {
    Router::new()
        .route("/rules/schema", get(schema))
        .route("/projects/{name}/rules", get(show).put(put).delete(remove))
        .route("/projects/{name}/rules/check", post(check))
}

/// `GET /api/rules/schema`: the schema that the daemon checks each rules
/// file against, the verbs, and the limits.
async fn schema(_actor: Actor) -> Json<Value> {
    let verbs: Vec<Value> = rules_file::VERBS
        .iter()
        .map(|v| json!({"kind": v.kind, "verb": v.name}))
        .collect();
    Json(json!({
        "file": rules_file::FILE_NAME,
        "schema": rules_file::SCHEMA,
        "verbs": verbs,
        "max_rules": rules::max_per_project(),
        "max_bytes": rules_file::MAX_BYTES,
    }))
}

async fn show(
    State(state): State<Shared>,
    actor: Actor,
    Path(name): Path<String>,
) -> ApiResult<Json<Rules>> {
    policy::read_project(&actor, &name)?;
    let r = state.db.read(move |c| rules::get(c, &name)).await?;
    Ok(Json(rules::with_rules(r).await?))
}

/// The body of `PUT /api/projects/{name}/rules`.
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct PutBody {
    text: String,
    /// The hash that the caller read; `""`: no rules yet. Absent: no
    /// check.
    #[serde(default)]
    base_hash: Option<String>,
}

async fn put(
    State(state): State<Shared>,
    actor: Actor,
    Path(name): Path<String>,
    ApiJson(body): ApiJson<PutBody>,
) -> ApiResult<Json<Rules>> {
    policy::admin_project(&actor, &name, "editing the board rules")?;
    let project = name.clone();
    let ctx = state
        .db
        .read(move |c| rules::context(c, &project))
        .await?;
    let set = rules::check(body.text.clone(), ctx.clone()).await?;
    let r = state
        .db
        .write(actor.label(), move |t| {
            rules::put(t, &name, &body.text, set, &ctx, body.base_hash.as_deref())
        })
        .await?;
    Ok(Json(r))
}

#[derive(Debug, Default, Deserialize)]
struct DeleteQuery {
    base_hash: Option<String>,
}

async fn remove(
    State(state): State<Shared>,
    actor: Actor,
    Path(name): Path<String>,
    ApiQuery(q): ApiQuery<DeleteQuery>,
) -> ApiResult<StatusCode> {
    policy::admin_project(&actor, &name, "editing the board rules")?;
    let project = name.clone();
    let gone = state
        .db
        .write(actor.label(), move |t| {
            rules::delete(t, &name, q.base_hash.as_deref())
        })
        .await?;
    if !gone {
        return Err(ApiError::not_found(format!(
            "project `{project}` has no rules"
        )));
    }
    Ok(StatusCode::NO_CONTENT)
}

/// The body of `POST /api/projects/{name}/rules/check`.
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct CheckBody {
    text: String,
}

/// Check a text as the project's rules file, without a write: `200` with
/// the rules, or `422` with `details.problems`.
async fn check(
    State(state): State<Shared>,
    actor: Actor,
    Path(name): Path<String>,
    ApiJson(body): ApiJson<CheckBody>,
) -> ApiResult<Json<Value>> {
    policy::read_project(&actor, &name)?;
    let ctx = state.db.read(move |c| rules::context(c, &name)).await?;
    let set = rules::check(body.text, ctx).await?;
    Ok(Json(json!({"ok": true, "rules": set.rules})))
}
