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
/// sysadmin), else its own effective skills (the default-on skills and
/// its `skills`, less `skills_off`).
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
    let agent = state
        .db
        .read(move |c| crate::store::agents::get(c, &id))
        .await?
        .agent;
    Ok(Some(
        agent
            .skill_selection()
            .effective_in(lib)
            .into_iter()
            .map(|e| e.name)
            .collect(),
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
    /// `true` when the user gets the skill only because it is default on.
    #[serde(skip_serializing_if = "std::ops::Not::not")]
    default: bool,
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
    let index = skills::LibraryIndex::read(library_opt(&state).as_deref());
    let u = state
        .db
        .read(move |c| {
            use terrarium_core::skills::{ALL, Selection, Source};
            // `Some((all, default))` when the selection gives the skill.
            let pick = |sel: Selection<'_>| -> Option<(bool, bool)> {
                let e = index.effective(sel).into_iter().find(|e| e.name == name)?;
                Some((
                    !sel.skills.contains(&name) && sel.skills.iter().any(|n| n == ALL),
                    e.source == Source::Default,
                ))
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
                    if let Some((all, default)) = pick(v.agent.skill_selection()) {
                        agents.push(User {
                            id: v.agent.id.to_string(),
                            name: format!("{}/{}", v.agent.project, v.agent.name),
                            all,
                            default,
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
                let (list, off, default_skills) = t.skill_settings();
                let sel = Selection {
                    skills: &list,
                    off: &off,
                    default_skills,
                };
                if let Some((all, default)) = pick(sel) {
                    templates.push(User {
                        id: t.id.clone(),
                        name: match &t.project {
                            Some(p) => format!("{p}/{}", t.name),
                            None => t.name.clone(),
                        },
                        all,
                        default,
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
