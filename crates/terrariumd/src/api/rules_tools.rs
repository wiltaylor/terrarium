//! The tools of the board rules (docs/api.md §19.12, TER-171): the rules
//! that run (the pack and the project's own), the lint, the migration of a
//! project to `board_flow = rules`, and one rule on or off.
//!
//! - `GET /api/projects/{name}/rules/effective`: whoever reads the project.
//! - `POST /api/projects/{name}/rules/lint`: whoever reads the project.
//! - `POST /api/projects/{name}/rules/migrate`: whoever reads the project
//!   for the plan; the operator, a project admin, or a sysadmin
//!   (`policy::admin_project`) with `apply`.
//! - `POST /api/projects/{name}/rules/{rule}/enable` and `.../disable`:
//!   `policy::admin_project`, as a put of the rules.
//!
//! The text work (`terrarium_core::rules_tools`) and each WCL parse run off
//! the DB lock; the write goes through `crate::store::rules::put`, which
//! checks the board and the hash again under the lock.

use axum::Json;
use axum::extract::{Path, State};
use axum::routing::{get, post};
use serde::Deserialize;
use serde_json::{Value, json};
use terrarium_core::config::BoardFlow;
use terrarium_core::rules_file::{self, BoardColumn, RuleSet};
use terrarium_core::rules_tools;

use super::{ApiJson, Shared};
use crate::actor::Actor;
use crate::error::{ApiError, ApiResult};
use crate::policy;
use crate::store::projects;
use crate::store::rules::{self, Rules};

pub fn routes() -> super::link_registry::RegisteredRouter {
    super::link_registry::RegisteredRouter::new()
        .route(
            "/projects/{name}/rules/effective",
            get(effective),
            &[(axum::http::Method::GET, super::link_registry::Access::Link)],
        )
        .route(
            "/projects/{name}/rules/lint",
            post(lint),
            &[(axum::http::Method::POST, super::link_registry::Access::Link)],
        )
        .route(
            "/projects/{name}/rules/migrate",
            post(migrate),
            &[(axum::http::Method::POST, super::link_registry::Access::Link)],
        )
        .route(
            "/projects/{name}/rules/{rule}/enable",
            post(enable),
            &[(axum::http::Method::POST, super::link_registry::Access::Link)],
        )
        .route(
            "/projects/{name}/rules/{rule}/disable",
            post(disable),
            &[(axum::http::Method::POST, super::link_registry::Access::Link)],
        )
}

/// What the tools read of a project: its rules file (with the rules read
/// off the lock), its flow, and the context of a check.
struct Read {
    rules: Rules,
    flow: BoardFlow,
    ctx: rules_file::Context,
}

impl Read {
    fn own(&self) -> RuleSet {
        RuleSet {
            rules: self.rules.rules.clone(),
        }
    }

    fn columns(&self) -> Vec<BoardColumn> {
        self.ctx.columns.clone().unwrap_or_default()
    }

    /// The hash to send as `base_hash`: the current one, `""` for none.
    fn base(&self) -> String {
        self.rules.hash.clone().unwrap_or_default()
    }
}

async fn read(state: &Shared, name: &str) -> ApiResult<Read> {
    let p = name.to_owned();
    let (r, flow, ctx) = state
        .db
        .read(move |c| {
            Ok((
                rules::get(c, &p)?,
                projects::board_flow(c, &p)?,
                rules::context(c, &p)?,
            ))
        })
        .await?;
    Ok(Read {
        rules: rules::with_rules(r).await?,
        flow,
        ctx,
    })
}

/// The pack when the project runs it.
fn pack_of(flow: BoardFlow) -> Option<&'static RuleSet> {
    (flow == BoardFlow::Rules).then(rules_file::default_pack)
}

/// The rules that the engine runs: the pack with `own`, or `own`.
fn running(flow: BoardFlow, own: &RuleSet) -> RuleSet {
    match pack_of(flow) {
        Some(p) => rules_file::with_pack(p, own),
        None => own.clone(),
    }
}

/// `GET .../rules/effective`: the rules that the engine runs for the
/// project, in its order, each with its origin and its WCL text.
async fn effective(
    State(state): State<Shared>,
    actor: Actor,
    Path(name): Path<String>,
) -> ApiResult<Json<Value>> {
    policy::read_project(&actor, &name)?;
    let r = read(&state, &name).await?;
    let list = rules_tools::effective(pack_of(r.flow), &r.own());
    Ok(Json(json!({
        "project": name,
        "board_flow": r.flow.as_str(),
        "exists": r.rules.exists,
        "hash": r.rules.hash,
        "source": r.rules.source,
        "rules": list,
        "problems": r.rules.problems,
    })))
}

/// The body of `POST .../rules/lint`.
#[derive(Debug, Default, Deserialize)]
#[serde(deny_unknown_fields)]
struct LintBody {
    /// A text to lint as the project's file instead of the stored one.
    #[serde(default)]
    text: Option<String>,
}

/// `POST .../rules/lint`: the findings of the rules that run (the stored
/// file, or `text`, with the pack when the project runs it) on the board.
async fn lint(
    State(state): State<Shared>,
    actor: Actor,
    Path(name): Path<String>,
    ApiJson(body): ApiJson<LintBody>,
) -> ApiResult<Json<Value>> {
    policy::read_project(&actor, &name)?;
    let r = read(&state, &name).await?;
    let (own, source) = match body.text {
        Some(text) => (rules::check(text, r.ctx.clone()).await?, "text"),
        None => (r.own(), "stored"),
    };
    let findings = rules_tools::lint(&running(r.flow, &own), &r.columns());
    Ok(Json(json!({
        "project": name,
        "board_flow": r.flow.as_str(),
        "source": source,
        "findings": findings,
        "problems": if source == "stored" { r.rules.problems } else { Vec::new() },
    })))
}

/// The body of `POST .../rules/migrate`.
#[derive(Debug, Default, Deserialize)]
#[serde(deny_unknown_fields)]
struct MigrateBody {
    /// Store the text and switch the project to `rules`. Absent: only the
    /// plan.
    #[serde(default)]
    apply: bool,
    /// The hash of the rules that the caller read (`""`: none). Absent:
    /// the hash read now.
    #[serde(default)]
    base_hash: Option<String>,
}

/// `POST .../rules/migrate`: map the built-in pack to the project's board
/// (`rules_tools::migrate`), check the new text, and lint the rules that
/// would run. With `apply`, store the text (when it changed) and set
/// `board_flow = rules` in one write, with the audit row `rules.migrate`.
async fn migrate(
    State(state): State<Shared>,
    actor: Actor,
    Path(name): Path<String>,
    ApiJson(body): ApiJson<MigrateBody>,
) -> ApiResult<Json<Value>> {
    if body.apply {
        policy::admin_project(&actor, &name, "migrating the board to rules")?;
    } else {
        policy::read_project(&actor, &name)?;
    }
    let r = read(&state, &name).await?;
    if body.apply && r.flow == BoardFlow::Columns {
        // A converted board runs its column rules (TER-1113): the pack
        // does not come back.
        return Err(ApiError::conflict(format!(
            "project `{name}` runs the rules of its board columns (`board_flow = columns`); \
             the built-in pack does not apply"
        )));
    }
    let own = r.own();
    let columns = r.columns();
    let m = tokio::task::spawn_blocking({
        let text = r.rules.text.clone();
        let own = own.clone();
        let columns = columns.clone();
        move || rules_tools::migrate(rules_file::default_pack(), &text, &own, &columns)
    })
    .await
    .map_err(|e| ApiError::internal(format!("rules migrate: {e}")))?;
    let changed = m.text != r.rules.text;
    let new_own = if changed {
        rules::check(m.text.clone(), r.ctx.clone()).await?
    } else {
        own
    };
    let findings = rules_tools::lint(
        &rules_file::with_pack(rules_file::default_pack(), &new_own),
        &columns,
    );
    let mut flow = r.flow;
    let mut hash = r.rules.hash.clone();
    if body.apply {
        let p = name.clone();
        let ctx = r.ctx.clone();
        let text = m.text.clone();
        let base = body.base_hash.clone().unwrap_or_else(|| r.base());
        let before = r.flow;
        let changes: Vec<Value> = m
            .changes
            .iter()
            .map(|c| json!({"rule": c.rule, "change": c.change}))
            .collect();
        let set = new_own.clone();
        hash = state
            .db
            .write(actor.label(), move |t| {
                let hash = if changed {
                    rules::put(t, &p, &text, set, &ctx, Some(&base))?.hash
                } else {
                    // The text stays: still check that nobody changed it.
                    let have = rules::get(t, &p)?.hash;
                    if base != have.clone().unwrap_or_default() {
                        return Err(ApiError::conflict(format!(
                            "the rules of project `{p}` changed: read them again"
                        )));
                    }
                    have
                };
                projects::set_board_flow(t, &p, BoardFlow::Rules)?;
                t.audit(
                    "rules.migrate",
                    &p,
                    Some(p.as_str()),
                    json!({
                        "board_flow": "rules",
                        "previous_board_flow": before.as_str(),
                        "hash": hash,
                        "changes": changes,
                    }),
                )?;
                Ok(hash)
            })
            .await?;
        flow = BoardFlow::Rules;
    }
    Ok(Json(json!({
        "project": name,
        "applied": body.apply,
        "board_flow": flow.as_str(),
        "changed": changed,
        "text": m.text,
        "hash": hash,
        "changes": m.changes,
        "findings": findings,
    })))
}

async fn enable(
    State(state): State<Shared>,
    actor: Actor,
    Path((name, rule)): Path<(String, String)>,
) -> ApiResult<Json<Value>> {
    set_enabled(state, actor, name, rule, true).await
}

async fn disable(
    State(state): State<Shared>,
    actor: Actor,
    Path((name, rule)): Path<(String, String)>,
) -> ApiResult<Json<Value>> {
    set_enabled(state, actor, name, rule, false).await
}

/// Switch one rule of the project's file, or of the pack, on or off by an
/// edit of the file's text (`rules_tools::set_enabled`), then a put.
async fn set_enabled(
    state: Shared,
    actor: Actor,
    name: String,
    rule: String,
    enabled: bool,
) -> ApiResult<Json<Value>> {
    policy::admin_project(&actor, &name, "editing the board rules")?;
    let r = read(&state, &name).await?;
    let own = r.own();
    let new = rules_tools::set_enabled(
        &r.rules.text,
        &own,
        rules_file::default_pack(),
        &rule,
        enabled,
    )
    .map_err(|e| {
        if e.starts_with("no rule") {
            ApiError::not_found(e)
        } else {
            ApiError::conflict(e)
        }
    })?;
    let Some(text) = new else {
        let findings = rules_tools::lint(&running(r.flow, &own), &r.columns());
        return Ok(Json(json!({
            "project": name,
            "rule": rule,
            "enabled": enabled,
            "changed": false,
            "hash": r.rules.hash,
            "findings": findings,
        })));
    };
    let set = rules::check(text.clone(), r.ctx.clone()).await?;
    // The lint of the rules that run after the change: an enable of a
    // pack rule that the migration turned off can name a missing column
    // again (TER-226).
    let findings = rules_tools::lint(&running(r.flow, &set), &r.columns());
    let base = r.base();
    let ctx = r.ctx.clone();
    let p = name.clone();
    let stored = state
        .db
        .write(actor.label(), move |t| {
            rules::put(t, &p, &text, set, &ctx, Some(&base))
        })
        .await?;
    Ok(Json(json!({
        "project": name,
        "rule": rule,
        "enabled": enabled,
        "changed": true,
        "hash": stored.hash,
        "findings": findings,
    })))
}
