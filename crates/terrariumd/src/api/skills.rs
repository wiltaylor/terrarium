//! `/api/skills/...`: the skill library (PLAN §8.6, docs/api.md §26).
//!
//! The operator and a sysadmin read and write the whole library. Any other
//! agent reads only its own skills (its `skills` list against the
//! library). A write goes to the work tree of the config repo and stays
//! uncommitted (`crate::skills`).

use axum::extract::{Path, State};
use axum::http::StatusCode;
use axum::routing::get;
use axum::{Json, Router};
use serde::Serialize;
use serde_json::json;

use super::{ApiJson, Shared};
use crate::actor::Actor;
use crate::error::{ApiError, ApiResult};
use crate::policy;
use crate::skills::{self, Detail, Summary, WriteBody};

pub fn routes() -> Router<Shared> {
    Router::new()
        .route("/skills", get(list).post(create))
        .route("/skills/{name}", get(show).put(update).delete(remove))
        .route("/skills/{name}/usage", get(usage))
}

fn allow_write(actor: &Actor) -> ApiResult<()> {
    policy::sysadmin(actor, "changing the skill library")
}

/// The library dir, or `501 not_available` when the daemon has no config
/// repo.
fn library(state: &Shared) -> ApiResult<std::path::PathBuf> {
    Ok(skills::library_dir(&state.config_repo.get()?.dir))
}

/// The library dir when the daemon has one, for the checks at assignment.
pub fn library_opt(state: &Shared) -> Option<std::path::PathBuf> {
    state
        .config_repo
        .get()
        .ok()
        .map(|r| skills::library_dir(&r.dir))
}

/// The skills an agent caller may read: `None` for all (the operator, a
/// sysadmin), else the names its own `skills` list picks.
async fn visible(
    state: &Shared,
    actor: &Actor,
    lib: &std::path::Path,
) -> ApiResult<Option<Vec<String>>> {
    let Actor::Agent(a) = actor else {
        return Ok(None);
    };
    if a.is_sysadmin() {
        return Ok(None);
    }
    let id = a.id.clone();
    let assigned = state
        .db
        .read(move |c| crate::store::agents::get(c, &id))
        .await?
        .agent
        .skills;
    let names = terrarium_core::skills::library_names(lib);
    Ok(Some(
        terrarium_core::skills::resolve(&assigned, &names).wanted,
    ))
}

async fn blocking<T, F>(f: F) -> ApiResult<T>
where
    T: Send + 'static,
    F: FnOnce() -> ApiResult<T> + Send + 'static,
{
    tokio::task::spawn_blocking(f)
        .await
        .map_err(|e| ApiError::internal(format!("skill library task: {e}")))?
}

async fn list(State(state): State<Shared>, actor: Actor) -> ApiResult<Json<Vec<Summary>>> {
    let lib = library(&state)?;
    let only = visible(&state, &actor, &lib).await?;
    let mut all = blocking(move || Ok(skills::list(&lib))).await?;
    if let Some(names) = only {
        all.retain(|s| names.contains(&s.name));
    }
    Ok(Json(all))
}

async fn show(
    State(state): State<Shared>,
    actor: Actor,
    Path(name): Path<String>,
) -> ApiResult<Json<Detail>> {
    let lib = library(&state)?;
    if let Some(names) = visible(&state, &actor, &lib).await?
        && !names.contains(&name)
    {
        return Err(ApiError::forbidden(format!(
            "the skill `{name}` is not one of your skills; reading the whole library needs \
             sysadmin"
        )));
    }
    Ok(Json(blocking(move || skills::get(&lib, &name)).await?))
}

async fn write(
    state: &Shared,
    actor: &Actor,
    name: String,
    body: WriteBody,
    create: bool,
) -> ApiResult<Detail> {
    allow_write(actor)?;
    let lib = library(state)?;
    let _busy = state.config_repo.busy.lock().await;
    let n = name.clone();
    let d = blocking(move || skills::write(&lib, &n, &body, create)).await?;
    let what = if create {
        "skill.create"
    } else {
        "skill.update"
    };
    audit(state, actor, what, &name).await;
    Ok(d)
}

async fn create(
    State(state): State<Shared>,
    actor: Actor,
    ApiJson(body): ApiJson<WriteBody>,
) -> ApiResult<(StatusCode, Json<Detail>)> {
    let name = body
        .name
        .clone()
        .map(|n| n.trim().to_owned())
        .filter(|n| !n.is_empty())
        .ok_or_else(|| ApiError::invalid("a new skill needs `name`"))?;
    let d = write(&state, &actor, name, body, true).await?;
    Ok((StatusCode::CREATED, Json(d)))
}

async fn update(
    State(state): State<Shared>,
    actor: Actor,
    Path(name): Path<String>,
    ApiJson(body): ApiJson<WriteBody>,
) -> ApiResult<Json<Detail>> {
    if body.name.as_deref().is_some_and(|n| n.trim() != name) {
        return Err(ApiError::invalid(
            "`name` differs from the path; a skill cannot be renamed",
        ));
    }
    Ok(Json(write(&state, &actor, name, body, false).await?))
}

async fn remove(
    State(state): State<Shared>,
    actor: Actor,
    Path(name): Path<String>,
) -> ApiResult<StatusCode> {
    allow_write(&actor)?;
    let lib = library(&state)?;
    let _busy = state.config_repo.busy.lock().await;
    let n = name.clone();
    blocking(move || skills::remove(&lib, &n)).await?;
    audit(&state, &actor, "skill.delete", &name).await;
    Ok(StatusCode::NO_CONTENT)
}

/// One user of a skill in `GET /api/skills/{name}/usage`.
#[derive(Debug, Serialize)]
struct User {
    id: String,
    /// `project/name` for an agent, the name (`project/name` for a
    /// project template) for a template.
    name: String,
    /// `true` when the list names the skill through `*`.
    all: bool,
}

/// `GET /api/skills/{name}/usage`.
#[derive(Debug, Serialize)]
struct Usage {
    agents: Vec<User>,
    templates: Vec<User>,
}

async fn usage(
    State(state): State<Shared>,
    actor: Actor,
    Path(name): Path<String>,
) -> ApiResult<Json<Usage>> {
    policy::sysadmin(&actor, "reading the users of a skill")?;
    terrarium_core::skills::check_name(&name).map_err(ApiError::invalid)?;
    let u = state
        .db
        .read(move |c| {
            let pick = |list: &[String]| -> Option<bool> {
                if list.contains(&name) {
                    Some(false)
                } else if list.iter().any(|n| n == terrarium_core::skills::ALL) {
                    Some(true)
                } else {
                    None
                }
            };
            let page = crate::store::Page {
                limit: Some(crate::store::Page::MAX),
                cursor: None,
            };
            let mut agents = Vec::new();
            let mut cursor = None;
            loop {
                let p = crate::store::Page {
                    cursor: cursor.clone(),
                    ..page.clone()
                };
                let rows = crate::store::agents::list(c, None, &p)?;
                for v in &rows.items {
                    if let Some(all) = pick(&v.agent.skills) {
                        agents.push(User {
                            id: v.agent.id.to_string(),
                            name: format!("{}/{}", v.agent.project, v.agent.name),
                            all,
                        });
                    }
                }
                match rows.next {
                    Some(n) => cursor = Some(n),
                    None => break,
                }
            }
            let mut templates = Vec::new();
            let all = crate::store::templates::ListFilter {
                all: true,
                ..Default::default()
            };
            for t in crate::store::templates::list(c, &all)? {
                let list: Vec<String> = t
                    .spec
                    .get("skills")
                    .and_then(|v| serde_json::from_value(v.clone()).ok())
                    .unwrap_or_default();
                if let Some(all) = pick(&list) {
                    templates.push(User {
                        id: t.id.clone(),
                        name: match &t.project {
                            Some(p) => format!("{p}/{}", t.name),
                            None => t.name.clone(),
                        },
                        all,
                    });
                }
            }
            Ok(Usage { agents, templates })
        })
        .await?;
    Ok(Json(u))
}

async fn audit(state: &Shared, actor: &Actor, what: &'static str, name: &str) {
    let name = name.to_owned();
    let r = state
        .db
        .write(actor.label(), move |t| {
            t.audit(what, &name, None, json!({"skill": name}))
        })
        .await;
    if let Err(e) = r {
        tracing::warn!("cannot write the audit row of {what}: {}", e.message);
    }
}
