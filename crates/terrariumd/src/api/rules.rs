//! `/api/projects/{name}/rules` and `/api/rules/schema` (docs/api.md
//! §19.8, TER-166): the board rules file of a project.
//!
//! Read: whoever reads the project. Write (put, delete): the operator, a
//! project admin of the project, or a sysadmin (`policy::admin_project`,
//! as for the other project settings; docs/api.md §19.8). Each write
//! writes an audit row (`crate::store::rules`). The rule engine
//! (`crate::rules_engine`, TER-168) runs the stored rules.
//!
//! `POST .../rules/dry-run` plans what the rules would do for an event or
//! a `seq` range, on the current state, and writes nothing. `GET
//! .../rules/runs` is the run log. Both: whoever reads the project.
//!
//! The WCL parse of a text never runs inside a DB closure: the handler
//! reads the context under the lock, parses on a blocking thread
//! (`rules::check`, `rules::with_rules`), then writes.

use axum::Json;
use axum::extract::{Path, State};
use axum::http::StatusCode;
use axum::routing::{get, post};
use serde::Deserialize;
use serde_json::{Value, json};
use terrarium_core::rules_file;

use super::{ApiJson, ApiQuery, Shared};
use crate::actor::Actor;
use crate::error::{ApiError, ApiResult};
use crate::policy;
use crate::store::rules::{self, Rules};

pub fn routes() -> super::link_registry::RegisteredRouter {
    super::link_registry::RegisteredRouter::new()
        .route(
            "/rules/schema",
            get(schema),
            &[(axum::http::Method::GET, super::link_registry::Access::Link)],
        )
        .route(
            "/rules/catalog",
            get(catalog),
            &[(axum::http::Method::GET, super::link_registry::Access::Link)],
        )
        .route(
            "/projects/{name}/rules",
            get(show).put(put).delete(remove),
            &[
                (axum::http::Method::GET, super::link_registry::Access::Link),
                (axum::http::Method::PUT, super::link_registry::Access::Link),
                (
                    axum::http::Method::DELETE,
                    super::link_registry::Access::Link,
                ),
            ],
        )
        .route(
            "/projects/{name}/rules/check",
            post(check),
            &[(axum::http::Method::POST, super::link_registry::Access::Link)],
        )
        .route(
            "/projects/{name}/rules/dry-run",
            post(dry_run),
            &[(axum::http::Method::POST, super::link_registry::Access::Link)],
        )
        .route(
            "/projects/{name}/rules/runs",
            get(runs),
            &[(axum::http::Method::GET, super::link_registry::Access::Link)],
        )
}

/// `GET /api/rules/catalog` (TER-1120): the structured rule model of the
/// column options: the triggers, the conditions, the verbs with their
/// arguments, the placeholders, and the limits
/// ([`terrarium_core::board_rules::catalog`]). Any caller.
async fn catalog(_actor: Actor) -> Json<Value> {
    Json(terrarium_core::board_rules::catalog())
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
    let ctx = state.db.read(move |c| rules::context(c, &project)).await?;
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

/// The body of `POST /api/projects/{name}/rules/dry-run`: one event
/// (`seq`) or a range (`from`, `to`; `to` absent: the end of the log), and
/// optionally a `text` to plan instead of the stored rules.
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct DryRunBody {
    #[serde(default)]
    seq: Option<i64>,
    #[serde(default)]
    from: Option<i64>,
    #[serde(default)]
    to: Option<i64>,
    #[serde(default)]
    text: Option<String>,
}

/// Plan the runs of the rules for the events of the project in the range,
/// with the guards, the conditions on the current state, and the actions
/// filled in. Writes nothing.
async fn dry_run(
    State(state): State<Shared>,
    actor: Actor,
    Path(name): Path<String>,
    ApiJson(body): ApiJson<DryRunBody>,
) -> ApiResult<Json<Value>> {
    policy::read_project(&actor, &name)?;
    let (from, to) = match (body.seq, body.from, body.to) {
        (Some(s), None, None) => (s, s),
        (None, Some(f), t) => (f, t.unwrap_or(i64::MAX)),
        _ => {
            return Err(ApiError::invalid(
                "give `seq`, or `from` (and `to`), not both",
            ));
        }
    };
    if to < from {
        return Err(ApiError::invalid("`to` is before `from`"));
    }
    let project = name.clone();
    let (set, source) = match body.text {
        Some(text) => {
            let (ctx, flow) = state
                .db
                .read(move |c| {
                    Ok((
                        rules::context(c, &project)?,
                        crate::store::projects::board_flow(c, &project)?,
                    ))
                })
                .await?;
            let own = rules::check(text, ctx).await?;
            // As the lint with `text`: with `board_flow = rules` the text
            // runs with the pack, so plan both (TER-226).
            if flow == terrarium_core::config::BoardFlow::Rules {
                (
                    rules_file::with_pack(rules_file::default_pack(), &own),
                    "text",
                )
            } else {
                (own, "text")
            }
        }
        None => {
            let (r, flow) = state
                .db
                .read(move |c| {
                    Ok((
                        rules::get(c, &project)?,
                        crate::store::projects::board_flow(c, &project)?,
                    ))
                })
                .await?;
            let pack = flow == terrarium_core::config::BoardFlow::Rules;
            if !r.exists && !pack {
                return Err(ApiError::not_found(format!(
                    "project `{name}` has no rules; give a `text` to plan"
                )));
            }
            let r = rules::with_rules(r).await?;
            let own = terrarium_core::rules_file::RuleSet { rules: r.rules };
            // A project with `board_flow = rules` runs the pack with its
            // own rules (TER-171): plan what the engine runs.
            if pack {
                (
                    rules_file::with_pack(rules_file::default_pack(), &own),
                    "stored",
                )
            } else {
                (own, "stored")
            }
        }
    };
    // A script step is code: only an admin of the project runs one, also
    // in a dry run (TER-173 review).
    if set
        .rules
        .iter()
        .any(|r| r.actions.iter().any(|a| a.verb == rules_file::SCRIPT_VERB))
    {
        policy::admin_project(&actor, &name, "dry-run a rule with a script step")?;
    }
    let p = name.clone();
    let (events, mut plans) = state
        .db
        .read(move |c| crate::rules_engine::dry_run(c, &p, &set, from, to, chrono::Utc::now()))
        .await?;
    // The script steps run after the read, outside the database (TER-173).
    crate::rules_engine::dry_run_scripts(&mut plans).await;
    Ok(Json(json!({
        "project": name,
        "source": source,
        "from": from,
        "to": if to == i64::MAX { Value::Null } else { json!(to) },
        "events": events,
        "max_events": crate::rules_engine::MAX_DRY_RUN_EVENTS,
        "plans": plans,
    })))
}

#[derive(Debug, Default, Deserialize)]
struct RunsQuery {
    rule: Option<String>,
    limit: Option<usize>,
}

/// The run log of the project's rules, newest first.
async fn runs(
    State(state): State<Shared>,
    actor: Actor,
    Path(name): Path<String>,
    ApiQuery(q): ApiQuery<RunsQuery>,
) -> ApiResult<Json<Value>> {
    policy::read_project(&actor, &name)?;
    let limit = q.limit.unwrap_or(100).clamp(1, 500);
    let rows = state
        .db
        .read(move |c| {
            crate::store::projects::get(c, &name)?;
            crate::rules_engine::list_runs(c, &name, q.rule.as_deref(), limit)
        })
        .await?;
    Ok(Json(json!({"runs": rows})))
}
