//! `/api/fleet/...`: fleet files (PLAN §20, Phase 6; `docs/fleet.md`).
//!
//! The file content travels in the body, so the CLI works the same in
//! operator mode and through an agent's runner socket. The store work is
//! in `crate::fleet`; this layer adds the live-session check of a prune,
//! the clones of new agents, and `--start`.

use axum::extract::{Path, State};
use axum::routing::{get, post};
use axum::{Json, Router};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use terrarium_core::fleet::{self, Fleet, Issue, ObjectRef, References};

use super::repos::{CheckoutBody, run_checkout};
use super::{ApiJson, ApiQuery, Shared};
use crate::actor::Actor;
use crate::error::{ApiError, ApiResult, Code};
use crate::fleet::{self as store, Action, Origin, Plan};
use crate::policy;
use crate::store::{agents, repos};

pub fn routes() -> Router<Shared> {
    Router::new()
        .route("/fleet", get(list))
        .route("/fleet/validate", post(validate))
        .route("/fleet/plan", post(plan))
        .route("/fleet/apply", post(apply))
        .route("/fleet/export", get(export))
        .route("/fleet/{name}", get(show))
}

/// The body of `POST /api/fleet/{validate,plan,apply}`.
#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
struct Body {
    /// The fleet file, as TOML text.
    content: String,
    /// The file name, for a fleet without `name` (the stem names it).
    #[serde(default)]
    path: Option<String>,
    /// Delete the agents and targets, and release the projects and
    /// subscriptions, that the fleet owns and the file no longer declares.
    #[serde(default)]
    prune: bool,
    /// Needed when a prune deletes an agent with a live session.
    #[serde(default)]
    confirm: bool,
    /// Start each agent with `start = true` that does not run.
    #[serde(default)]
    start: bool,
}

impl Body {
    fn fallback(&self) -> Option<String> {
        self.path
            .as_deref()
            .and_then(|p| fleet::name_from_path(std::path::Path::new(p)))
    }
}

/// The answer of `POST /api/fleet/validate`.
#[derive(Debug, Serialize)]
struct Validation {
    fleet: Option<String>,
    valid: bool,
    errors: Vec<Issue>,
    warnings: Vec<Issue>,
    objects: Vec<ObjectRef>,
    references: References,
}

async fn validate(
    State(state): State<Shared>,
    ApiJson(body): ApiJson<Body>,
) -> ApiResult<Json<Validation>> {
    let fallback = body.fallback();
    let v = state
        .db
        .read(move |c| {
            let parsed = fleet::parse(&body.content, fallback.as_deref());
            let mut issues = parsed.issues;
            if let Some(f) = &parsed.fleet
                && !issues.iter().any(Issue::is_error)
            {
                issues.extend(store::check_against(c, f, &store::prompts_dir())?);
            }
            let (errors, warnings): (Vec<Issue>, Vec<Issue>) =
                issues.into_iter().partition(Issue::is_error);
            Ok(Validation {
                fleet: parsed.fleet.as_ref().map(|f| f.name.clone()),
                valid: errors.is_empty(),
                errors,
                warnings,
                objects: parsed
                    .fleet
                    .as_ref()
                    .map(Fleet::objects)
                    .unwrap_or_default(),
                references: parsed
                    .fleet
                    .as_ref()
                    .map(Fleet::references)
                    .unwrap_or_default(),
            })
        })
        .await?;
    Ok(Json(v))
}

/// Parse, check, plan, and mark the agents that run in the supervisor.
async fn plan_of(state: &Shared, body: &Body) -> ApiResult<(Option<Fleet>, Plan)> {
    let text = body.content.clone();
    let fallback = body.fallback();
    let (f, mut p) = state
        .db
        .read(move |c| store::plan_text(c, &text, fallback.as_deref()))
        .await?;
    for e in &mut p.entries {
        if let Some(id) = e.id.as_deref().and_then(|i| i.parse().ok())
            && state
                .supervisor
                .status(id)
                .await
                .is_some_and(|s| s.is_live())
        {
            e.live = true;
        }
    }
    Ok((f, p))
}

async fn plan(
    State(state): State<Shared>,
    actor: Actor,
    ApiJson(body): ApiJson<Body>,
) -> ApiResult<Json<Plan>> {
    let (f, p) = plan_of(&state, &body).await?;
    if p.valid {
        store::authorize(&actor, &p, body.prune)?;
        if let Some(f) = f {
            let a = actor.clone();
            state
                .db
                .read(move |c| store::check_overwrite(c, &a, &f, None, false))
                .await?;
        }
    }
    Ok(Json(p))
}

/// The answer of `POST /api/fleet/apply`.
#[derive(Debug, Serialize)]
struct ApplyResult {
    #[serde(flatten)]
    plan: Plan,
    prune: bool,
    /// The clones of `checkout.on_agent_create` for the new agents.
    #[serde(skip_serializing_if = "Vec::is_empty")]
    checkout: Vec<Value>,
    /// The agents that `start` started.
    #[serde(skip_serializing_if = "Vec::is_empty")]
    started: Vec<String>,
    /// Starts and clones that failed. The apply itself stays.
    #[serde(skip_serializing_if = "Vec::is_empty")]
    failures: Vec<String>,
}

async fn apply(
    State(state): State<Shared>,
    actor: Actor,
    ApiJson(body): ApiJson<Body>,
) -> ApiResult<Json<ApplyResult>> {
    let (f, p) = plan_of(&state, &body).await?;
    let Some(f) = f.filter(|_| p.valid) else {
        let list: Vec<String> = p.errors.iter().map(ToString::to_string).collect();
        return Err(ApiError::invalid(format!(
            "the fleet file has {} error(s): {}",
            list.len(),
            list.join("; ")
        ))
        .with_details(json!({"errors": p.errors, "warnings": p.warnings})));
    };
    store::authorize(&actor, &p, body.prune)?;
    {
        let (a, fl, confirm) = (actor.clone(), f.clone(), body.confirm);
        state
            .db
            .read(move |c| store::check_overwrite(c, &a, &fl, Some("api"), confirm))
            .await?;
    }
    if p.conflicts().next().is_some() {
        return Err(ApiError::conflict(format!(
            "fleet `{}` has conflicts; run `fleet plan` to see them",
            p.fleet
        ))
        .with_details(serde_json::to_value(&p)?));
    }
    // A prune that deletes a live agent needs `confirm` (PLAN §10). The
    // daemon stops the agent before the delete.
    let live: Vec<&store::Entry> = if body.prune {
        p.removals()
            .filter(|e| e.action == Action::Delete && e.live)
            .collect()
    } else {
        Vec::new()
    };
    if !live.is_empty() && !body.confirm {
        let names: Vec<&str> = live.iter().map(|e| e.key.as_str()).collect();
        return Err(ApiError::new(
            Code::ConfirmRequired,
            format!(
                "the prune deletes agent(s) with a live session: {}; add confirm to stop and delete them",
                names.join(", ")
            ),
        )
        .with_details(json!({"agents": names})));
    }
    for e in &live {
        if let Some(id) = e.id.as_deref().and_then(|i| i.parse().ok()) {
            crate::trash::stop_agent(&state, id)
                .await
                .map_err(super::agents::supervisor_error)?;
        }
    }

    let origin = Origin {
        origin: "api".into(),
        content: body.content.clone(),
    };
    let state_dir = state.work_dir.clone();
    let (prune, confirm, a, fl) = (body.prune, body.confirm, actor.clone(), f.clone());
    let applied = state
        .db
        .write(actor.label(), move |t| {
            store::apply(t, &fl, &origin, &a, prune, confirm, &state_dir)
        })
        .await?;
    // Like `DELETE /api/agents/{id}`: each pruned agent dir goes to the
    // trash (`crate::trash`).
    for id in &applied.deleted_agents {
        if let Ok(id) = id.parse() {
            crate::trash::retire_agent(&state, id).await;
        }
    }
    let mut result = ApplyResult {
        plan: applied.plan.unwrap_or(p),
        prune: body.prune,
        checkout: Vec::new(),
        started: Vec::new(),
        failures: Vec::new(),
    };

    // PLAN §9: clone `checkout.on_agent_create` into each new workspace.
    for (id, project) in &applied.created_agents {
        let wanted: Vec<String> = f
            .projects
            .iter()
            .find(|p| p.def.name == *project)
            .map(|p| p.def.checkout.on_agent_create.clone())
            .unwrap_or_default();
        if wanted.is_empty() {
            continue;
        }
        let key = id.clone();
        let view = state.db.read(move |c| agents::get(c, &key)).await?;
        for repo in wanted {
            let (p, n) = (project.clone(), repo.clone());
            let found = state
                .db
                .read(move |c| repos::find_by_name(c, &p, &n))
                .await?;
            let Some(r) = found else { continue };
            match run_checkout(&state, &actor, &r, &view, &CheckoutBody::default()).await {
                Ok(c) => result
                    .checkout
                    .push(serde_json::to_value(&c).unwrap_or_else(|_| json!({"repo": repo}))),
                Err(e) => {
                    result.failures.push(format!(
                        "clone of {repo} into {}/{}: {}",
                        project, view.agent.name, e.message
                    ));
                    result
                        .checkout
                        .push(json!({"repo": repo, "agent": view.agent.name, "error": e.message}));
                }
            }
        }
    }

    if body.start {
        for a in f.agents().filter(|a| a.start) {
            let key = fleet::agent_key(&a.def);
            match start_agent(&state, &actor, &a.def.project, &a.def.name).await {
                Ok(true) => result.started.push(key),
                Ok(false) => {}
                Err(e) => result
                    .failures
                    .push(format!("start of {key}: {}", e.message)),
            }
        }
    }
    Ok(Json(result))
}

/// Start an agent that does not run. False: it runs already.
async fn start_agent(state: &Shared, actor: &Actor, project: &str, name: &str) -> ApiResult<bool> {
    policy::admin_project(actor, project, "starting an agent")?;
    let key = format!("{project}/{name}");
    let a = state.db.read(move |c| agents::find(c, &key, None)).await?;
    let agent_id = a.agent.id;
    if state
        .supervisor
        .status(agent_id)
        .await
        .is_some_and(|s| s.is_live())
    {
        return Ok(false);
    }
    let id = agent_id.to_string();
    let project = project.to_owned();
    let resume = state
        .db
        .write(actor.label(), move |t| {
            agents::begin_session(t, &id, "superseded")?;
            t.audit(
                "agent.start",
                &id,
                Some(&project),
                json!({"source": "fleet"}),
            )?;
            agents::last_harness_session(t, &id)
        })
        .await?;
    if let Err(e) = state.supervisor.start(a.agent.clone(), resume).await {
        let id = agent_id.to_string();
        state
            .db
            .write("system", move |t| {
                agents::end_session(t, &id, "start_failed")
            })
            .await?;
        return Err(super::agents::supervisor_error(e));
    }
    Ok(true)
}

/// May the actor see this fleet? The operator and sysadmins see every
/// fleet; an agent sees the fleets that own objects in its project.
fn visible(actor: &Actor, row: &store::FleetRow) -> bool {
    match actor {
        Actor::Operator => true,
        Actor::Agent(a) => a.is_sysadmin() || row.projects.contains(&a.project),
    }
}

async fn list(State(state): State<Shared>, actor: Actor) -> ApiResult<Json<Vec<store::FleetRow>>> {
    let rows = state.db.read(store::list).await?;
    Ok(Json(
        rows.into_iter().filter(|r| visible(&actor, r)).collect(),
    ))
}

/// The answer of `GET /api/fleet/{name}`: the fleet, its last applied
/// content, and the plan of that content now (drift shows there).
#[derive(Debug, Serialize)]
struct Shown {
    #[serde(flatten)]
    row: store::FleetRow,
    plan: Plan,
}

async fn show(
    State(state): State<Shared>,
    actor: Actor,
    Path(name): Path<String>,
) -> ApiResult<Json<Shown>> {
    let row = state.db.read(move |c| store::get(c, &name)).await?;
    if !visible(&actor, &row) {
        return Err(ApiError::not_found(format!("no fleet `{}`", row.name)));
    }
    let content = row.content.clone().unwrap_or_default();
    let fallback = row.name.clone();
    let (_, plan) = state
        .db
        .read(move |c| store::plan_text(c, &content, Some(&fallback)))
        .await?;
    Ok(Json(Shown { row, plan }))
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct ExportQuery {
    project: String,
    /// The fleet name. The default is the project name.
    #[serde(default)]
    name: Option<String>,
}

/// The answer of `GET /api/fleet/export`.
#[derive(Debug, Serialize)]
struct Exported {
    fleet: String,
    project: String,
    /// The fleet file, as TOML text.
    content: String,
}

async fn export(
    State(state): State<Shared>,
    actor: Actor,
    ApiQuery(q): ApiQuery<ExportQuery>,
) -> ApiResult<Json<Exported>> {
    policy::read_project(&actor, &q.project)?;
    let name = q.name.clone().unwrap_or_else(|| q.project.clone());
    if !fleet::valid_name(&name) || fleet::RESERVED_NAMES.contains(&name.as_str()) {
        return Err(ApiError::invalid(format!("bad fleet name `{name}`")));
    }
    // The checkout list of a file project is in its definition file.
    let checkout: Vec<String> = state
        .definitions()
        .projects
        .iter()
        .find(|p| p.name == q.project)
        .map(|p| p.checkout.on_agent_create.clone())
        .unwrap_or_default();
    let project = q.project.clone();
    let n = name.clone();
    let f = state
        .db
        .read(move |c| store::export(c, &project, &n, checkout))
        .await?;
    Ok(Json(Exported {
        fleet: name,
        project: q.project,
        content: f.to_toml(),
    }))
}
