//! `/api/system-prompts/...`: the system prompt library (PLAN §8.1,
//! docs/api.md §28, `crate::system_prompts`).
//!
//! Reads and creates: the operator, a sysadmin, and a project_admin (the
//! callers that can edit agents). A change or a delete of an entry that
//! agents or templates outside the caller's project use, or of `leader`
//! or `worker` (the personas by role of every project), needs sysadmin or
//! the operator. A write goes to the work tree of the config repo after
//! the secret scan, and stays uncommitted.

use axum::extract::{Path, Query, State};
use axum::http::StatusCode;
use axum::routing::get;
use axum::{Json};
use serde::Deserialize;
use serde_json::json;
use terrarium_core::Role;

use super::{ApiJson, Shared};
use crate::actor::Actor;
use crate::error::{ApiError, ApiResult};
use crate::events::EventDraft;
use crate::policy;
use crate::system_prompts::{self as lib, Detail, Dirs, Summary, User};

pub fn routes() -> super::link_registry::RegisteredRouter {
    super::link_registry::RegisteredRouter::new()
        .route("/system-prompts", get(list).post(create), &[(axum::http::Method::GET, super::link_registry::Access::Link), (axum::http::Method::POST, super::link_registry::Access::Link)])
        .route(
            "/system-prompts/{name}",
            get(show).put(update).delete(remove), &[(axum::http::Method::GET, super::link_registry::Access::Link), (axum::http::Method::PUT, super::link_registry::Access::Link), (axum::http::Method::DELETE, super::link_registry::Access::Link)])
}

/// The body of `POST /api/system-prompts`.
#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CreateBody {
    pub name: String,
    pub text: String,
    #[serde(default)]
    pub description: Option<String>,
    /// Write over an entry that exists (the rights of a `PUT`).
    #[serde(default)]
    pub replace: bool,
}

/// The body of `PUT /api/system-prompts/{name}`: only the fields given
/// change.
#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct UpdateBody {
    #[serde(default)]
    pub text: Option<String>,
    /// `""` removes the description.
    #[serde(default)]
    pub description: Option<String>,
}

#[derive(Debug, Default, Deserialize)]
pub struct DeleteQuery {
    #[serde(default)]
    pub force: bool,
}

/// May the caller read the library and add to it? The operator, a
/// sysadmin, and a project_admin.
fn allow_use(actor: &Actor, what: &str) -> ApiResult<()> {
    match actor {
        Actor::Operator => Ok(()),
        Actor::Agent(a) if a.is_sysadmin() || a.roles.contains(&Role::ProjectAdmin) => Ok(()),
        Actor::Agent(_) => Err(ApiError::forbidden(format!(
            "{what} needs project_admin, sysadmin, or the operator"
        ))),
    }
}

/// May the caller change or delete `name`, with these users? A
/// project_admin may when every user is in its own project; else, and
/// for `leader` and `worker`, sysadmin or the operator.
fn allow_change(actor: &Actor, name: &str, users: &[User], what: &str) -> ApiResult<()> {
    allow_use(actor, what)?;
    let Actor::Agent(a) = actor else {
        return Ok(());
    };
    if a.is_sysadmin() {
        return Ok(());
    }
    if lib::BUILTIN.contains(&name) {
        return policy::sysadmin(
            actor,
            &format!("{what} `{name}` (the persona by role of every project)"),
        );
    }
    let outside: Vec<&str> = users
        .iter()
        .filter(|u| u.project.as_deref() != Some(a.project.as_str()))
        .map(|u| u.name.as_str())
        .collect();
    if outside.is_empty() {
        return Ok(());
    }
    Err(ApiError::forbidden(format!(
        "{what} `{name}` needs sysadmin or the operator: it is used outside project `{}` ({})",
        a.project,
        outside.join(", ")
    )))
}

/// The lookup dirs and the config repo, or `501 not_available` when the
/// daemon has no config repo.
fn dirs(state: &Shared) -> ApiResult<(Dirs, std::path::PathBuf)> {
    let repo = state.config_repo.get()?.dir;
    let mut dirs = Dirs::of_repo(&repo);
    // `TERRARIUM_PROMPTS_DIR`, as the start reads it.
    if let Some(p) = std::env::var_os("TERRARIUM_PROMPTS_DIR").filter(|v| !v.is_empty()) {
        dirs.prompts = p.into();
    }
    Ok((dirs, repo))
}

async fn blocking<T, F>(f: F) -> ApiResult<T>
where
    T: Send + 'static,
    F: FnOnce() -> ApiResult<T> + Send + 'static,
{
    tokio::task::spawn_blocking(f)
        .await
        .map_err(|e| ApiError::internal(format!("system prompt library task: {e}")))?
}

async fn users(state: &Shared, name: &str) -> ApiResult<Vec<User>> {
    let n = name.to_owned();
    state.db.read(move |c| lib::users(c, &n)).await
}

async fn list(State(state): State<Shared>, actor: Actor) -> ApiResult<Json<Vec<Summary>>> {
    allow_use(&actor, "reading the system prompt library")?;
    let (dirs, repo) = dirs(&state)?;
    Ok(Json(
        blocking(move || Ok(lib::list(&dirs, Some(&repo)))).await?,
    ))
}

async fn detail(state: &Shared, name: &str) -> ApiResult<Detail> {
    let (dirs, repo) = dirs(state)?;
    let n = name.to_owned();
    let mut d = blocking(move || lib::get(&dirs, Some(&repo), &n)).await?;
    d.users = users(state, name).await?;
    Ok(d)
}

async fn show(
    State(state): State<Shared>,
    actor: Actor,
    Path(name): Path<String>,
) -> ApiResult<Json<Detail>> {
    allow_use(&actor, "reading the system prompt library")?;
    Ok(Json(detail(&state, &name).await?))
}

/// Write the file of `name` in the transaction of its audit row and its
/// event. `change`: `created` or `updated`.
async fn write_file(
    state: &Shared,
    actor: &Actor,
    name: &str,
    description: &str,
    text: &str,
    change: &'static str,
) -> ApiResult<()> {
    let (rel, bytes) = lib::checked_file(name, description, text)?;
    let repo = state.config_repo.get()?.dir;
    let path = repo.join(&rel);
    let n = name.to_owned();
    let size = bytes.len();
    state
        .db
        .write(actor.label(), move |t| {
            t.stage_file(path, Some(bytes));
            let action = if change == "created" {
                "system_prompt.create"
            } else {
                "system_prompt.update"
            };
            t.audit(
                action,
                &n,
                None,
                json!({"name": n, "path": rel, "size": size}),
            )?;
            t.emit(EventDraft::new(
                "system_prompt",
                format!("system prompt {n} {change}"),
                json!({"change": change, "name": n, "path": rel}),
            ))?;
            Ok(())
        })
        .await
}

/// True when `name` has a file in the library dir (not only in the
/// prompts dir of an older config repo).
fn in_library(state: &Shared, name: &str) -> ApiResult<Option<Detail>> {
    let (dirs, repo) = dirs(state)?;
    match lib::get(&dirs, Some(&repo), name) {
        Ok(d) => Ok(Some(d)),
        Err(e) if e.code == crate::error::Code::NotFound => Ok(None),
        Err(e) => Err(e),
    }
}

async fn create(
    State(state): State<Shared>,
    actor: Actor,
    ApiJson(body): ApiJson<CreateBody>,
) -> ApiResult<(StatusCode, Json<Detail>)> {
    allow_use(&actor, "adding a system prompt")?;
    let name = body.name.trim().to_owned();
    lib::check_name(&name)?;
    let _busy = state.config_repo.busy.lock().await;
    let existing = in_library(&state, &name)?;
    let change = match &existing {
        Some(_) if !body.replace => {
            return Err(ApiError::conflict(format!(
                "the system prompt `{name}` exists already; change it with PUT \
                 /api/system-prompts/{name}, or send `replace: true`"
            )));
        }
        Some(_) => {
            allow_change(
                &actor,
                &name,
                &users(&state, &name).await?,
                "changing the system prompt",
            )?;
            "updated"
        }
        None => "created",
    };
    let description = body.description.clone().unwrap_or_default();
    write_file(&state, &actor, &name, &description, &body.text, change).await?;
    let status = if change == "created" {
        StatusCode::CREATED
    } else {
        StatusCode::OK
    };
    Ok((status, Json(detail(&state, &name).await?)))
}

async fn update(
    State(state): State<Shared>,
    actor: Actor,
    Path(name): Path<String>,
    ApiJson(body): ApiJson<UpdateBody>,
) -> ApiResult<Json<Detail>> {
    allow_use(&actor, "changing the system prompt")?;
    let _busy = state.config_repo.busy.lock().await;
    let have = in_library(&state, &name)?
        .ok_or_else(|| ApiError::not_found(format!("no system prompt `{name}` in the library")))?;
    allow_change(
        &actor,
        &name,
        &users(&state, &name).await?,
        "changing the system prompt",
    )?;
    if body.text.is_none() && body.description.is_none() {
        return Err(ApiError::invalid("give `text` or `description`"));
    }
    // A text with its own description line keeps it when `description`
    // is not given.
    let text = body.text.clone().unwrap_or_else(|| have.text.clone());
    let description = match (&body.description, &body.text) {
        (Some(d), _) => d.clone(),
        (None, Some(t)) if !lib::description(t).is_empty() => lib::description(t),
        (None, _) => have.summary.description.clone(),
    };
    write_file(&state, &actor, &name, &description, &text, "updated").await?;
    Ok(Json(detail(&state, &name).await?))
}

async fn remove(
    State(state): State<Shared>,
    actor: Actor,
    Path(name): Path<String>,
    Query(q): Query<DeleteQuery>,
) -> ApiResult<StatusCode> {
    allow_use(&actor, "deleting a system prompt")?;
    terrarium_core::config::check_persona(&name).map_err(ApiError::invalid)?;
    if lib::BUILTIN.contains(&name.as_str()) {
        return Err(ApiError::conflict(format!(
            "`{name}` is the persona by role of every project: it cannot be deleted; edit it \
             instead"
        )));
    }
    let _busy = state.config_repo.busy.lock().await;
    let have = in_library(&state, &name)?
        .ok_or_else(|| ApiError::not_found(format!("no system prompt `{name}` in the library")))?;
    let who = users(&state, &name).await?;
    allow_change(&actor, &name, &who, "deleting the system prompt")?;
    if !who.is_empty() && !q.force {
        let list: Vec<&str> = who.iter().map(|u| u.name.as_str()).collect();
        return Err(ApiError::conflict(format!(
            "the system prompt `{name}` is in use by {} ({}): pick another system prompt for \
             them first, or delete with `force=true` (their next start then fails)",
            who.len(),
            list.join(", ")
        ))
        .with_details(json!({ "users": who })));
    }
    let repo = state.config_repo.get()?.dir;
    let rel = have.summary.path.clone();
    let path = repo.join(&rel);
    let users = who.len();
    state
        .db
        .write(actor.label(), move |t| {
            t.stage_file(path, None);
            t.audit(
                "system_prompt.delete",
                &name,
                None,
                json!({"name": name, "path": rel, "users": users}),
            )?;
            t.emit(EventDraft::new(
                "system_prompt",
                format!("system prompt {name} deleted"),
                json!({"change": "deleted", "name": name, "path": rel}),
            ))?;
            Ok(())
        })
        .await?;
    Ok(StatusCode::NO_CONTENT)
}
