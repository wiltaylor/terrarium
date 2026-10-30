//! `/api/templates/...` (docs/api.md §21): agent templates, and the
//! template step of `POST /api/agents`.
//!
//! Rights: members read their project's templates, every caller reads
//! the global ones; a project_admin manages its project's; a global
//! template needs a sysadmin. A template grants nothing: an agent create
//! from it runs the normal rights checks on the values it copies.

use axum::extract::{Path, State};
use axum::http::StatusCode;
use axum::routing::{get, post};
use axum::{Json, Router};
use serde::{Deserialize, Deserializer, Serialize};
use serde_json::Value;
use terrarium_core::agent_template::{self, Spec};
use terrarium_core::config::AgentDef;

use super::{ApiJson, ApiQuery, Shared};
use crate::actor::Actor;
use crate::error::{ApiError, ApiResult, Code};
use crate::policy;
use crate::store::projects::{SOURCE_API, SOURCE_FILE};
use crate::store::templates::{
    self, ListFilter, NewTemplate, Template, TemplatePatch, TemplateRef,
};

pub fn routes() -> Router<Shared> {
    Router::new()
        .route("/templates", get(list).post(create))
        .route("/templates/{id}", get(show).patch(update).delete(remove))
        .route("/templates/{id}/resolve", get(resolve))
        .route("/templates/from-agent/{agent}", post(from_agent))
}

/// `Some(None)` for an explicit `null`, `None` when the key is absent.
fn explicit<'de, D, T>(d: D) -> Result<Option<Option<T>>, D::Error>
where
    D: Deserializer<'de>,
    T: Deserialize<'de>,
{
    Option::<T>::deserialize(d).map(Some)
}

/// The scope of a new template: absent is the agent caller's project
/// (the operator: global), `null` or `""` is global.
fn scope_of(actor: &Actor, project: Option<Option<String>>) -> Option<String> {
    match project {
        Some(p) => p.filter(|p| !p.is_empty()),
        None => actor.project().map(str::to_owned),
    }
}

fn file_managed(t: &Template) -> ApiError {
    ApiError::new(
        Code::FileManaged,
        format!(
            "template `{}` is declared in projects/{}.toml; edit the file, then POST /api/reload",
            t.key(),
            t.project.as_deref().unwrap_or_default()
        ),
    )
}

#[derive(Debug, Default, Deserialize)]
struct ListQuery {
    project: Option<String>,
    /// `global`: the global templates only.
    scope: Option<String>,
}

async fn list(
    State(state): State<Shared>,
    actor: Actor,
    ApiQuery(q): ApiQuery<ListQuery>,
) -> ApiResult<Json<Vec<Template>>> {
    let sees_all = match &actor {
        Actor::Operator => true,
        Actor::Agent(a) => a.is_sysadmin(),
    };
    let f = match (
        q.scope.as_deref().filter(|s| !s.is_empty()),
        q.project.filter(|p| !p.is_empty()),
    ) {
        (Some("global"), _) => ListFilter {
            global: true,
            ..ListFilter::default()
        },
        (Some(other), _) => {
            return Err(ApiError::bad_request(format!(
                "bad `scope` `{other}`: use global, or ?project="
            )));
        }
        (None, Some(p)) => {
            policy::read_templates(&actor, Some(&p))?;
            ListFilter {
                project: Some(p),
                global: true,
                all: false,
            }
        }
        (None, None) if sees_all => ListFilter {
            all: true,
            ..ListFilter::default()
        },
        (None, None) => ListFilter {
            project: actor.project().map(str::to_owned),
            global: true,
            all: false,
        },
    };
    Ok(Json(state.db.read(move |c| templates::list(c, &f)).await?))
}

/// Find the template of the path and check that the actor may read it.
async fn load(state: &Shared, actor: &Actor, key: String) -> ApiResult<Template> {
    let context = actor.project().map(str::to_owned);
    let t = state
        .db
        .read(move |c| templates::find(c, &key, context.as_deref()))
        .await?;
    policy::read_templates(actor, t.project.as_deref())?;
    Ok(t)
}

/// A spec in a body: a JSON object.
fn spec_of(v: Value) -> ApiResult<Spec> {
    match v {
        Value::Object(m) => Ok(m),
        _ => Err(crate::store::schedules::field_error(
            "spec",
            "use a JSON object: an agent definition without name and project",
        )),
    }
}

/// Each skill that a spec names is in the skill library (PLAN §8.6): an
/// unknown name is `422 invalid`. A spec with a `skills` that is not a
/// list of strings fails later, in the spec check.
fn check_skills(state: &Shared, spec: &Spec) -> ApiResult<()> {
    let Some(list) = spec
        .get("skills")
        .and_then(|v| serde_json::from_value::<Vec<String>>(v.clone()).ok())
    else {
        return Ok(());
    };
    crate::skills::check_assignment(super::skills::library_opt(state).as_deref(), &list)
}

/// The body of `POST /api/templates`.
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct NewBody {
    name: String,
    #[serde(default, deserialize_with = "explicit")]
    project: Option<Option<String>>,
    #[serde(default)]
    description: Option<String>,
    spec: Value,
}

async fn create(
    State(state): State<Shared>,
    actor: Actor,
    ApiJson(b): ApiJson<NewBody>,
) -> ApiResult<(StatusCode, Json<Template>)> {
    let project = scope_of(&actor, b.project);
    policy::manage_templates(&actor, project.as_deref(), "")?;
    let spec = spec_of(b.spec)?;
    check_skills(&state, &spec)?;
    let new = NewTemplate {
        name: b.name,
        project,
        description: b.description.unwrap_or_default(),
        spec,
        created_by: actor.label(),
        source: SOURCE_API.into(),
    };
    let t = state
        .db
        .write(actor.label(), move |t| templates::create(t, &new))
        .await?;
    Ok((StatusCode::CREATED, Json(t)))
}

async fn show(
    State(state): State<Shared>,
    actor: Actor,
    Path(id): Path<String>,
) -> ApiResult<Json<Template>> {
    Ok(Json(load(&state, &actor, id).await?))
}

/// The body of `PATCH /api/templates/{id}`.
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct PatchBody {
    #[serde(default)]
    name: Option<String>,
    #[serde(default)]
    description: Option<String>,
    #[serde(default)]
    spec: Option<Value>,
}

async fn update(
    State(state): State<Shared>,
    actor: Actor,
    Path(id): Path<String>,
    ApiJson(b): ApiJson<PatchBody>,
) -> ApiResult<Json<Template>> {
    let t = load(&state, &actor, id).await?;
    policy::manage_templates(&actor, t.project.as_deref(), "")?;
    if t.source == SOURCE_FILE {
        return Err(file_managed(&t));
    }
    let spec = b.spec.map(spec_of).transpose()?;
    if let Some(spec) = &spec {
        check_skills(&state, spec)?;
    }
    let patch = TemplatePatch {
        name: b.name,
        description: b.description,
        spec,
        source: None,
    };
    let v = state
        .db
        .write(actor.label(), move |tx| {
            templates::update(tx, &t.id, &patch)
        })
        .await?;
    Ok(Json(v))
}

async fn remove(
    State(state): State<Shared>,
    actor: Actor,
    Path(id): Path<String>,
) -> ApiResult<StatusCode> {
    let t = load(&state, &actor, id).await?;
    policy::manage_templates(&actor, t.project.as_deref(), "")?;
    if t.source == SOURCE_FILE {
        return Err(file_managed(&t));
    }
    state
        .db
        .write(actor.label(), move |tx| templates::delete(tx, &t.id))
        .await?;
    Ok(StatusCode::NO_CONTENT)
}

#[derive(Debug, Default, Deserialize)]
struct ResolveQuery {
    project: Option<String>,
}

/// The answer of `GET /api/templates/{id}/resolve`.
#[derive(Debug, Serialize)]
struct Resolved {
    template: TemplateRef,
    /// The create body that the template gives an agent of the project,
    /// without `name`.
    agent: Value,
}

/// `GET /api/templates/{id}/resolve?project=P`: the template that a
/// create of an agent in `P` with `template: "{id}"` uses, and the
/// values it copies. Without `project`: the caller's project.
async fn resolve(
    State(state): State<Shared>,
    actor: Actor,
    Path(id): Path<String>,
    ApiQuery(q): ApiQuery<ResolveQuery>,
) -> ApiResult<Json<Resolved>> {
    let Some(project) = q
        .project
        .filter(|p| !p.is_empty())
        .or_else(|| actor.project().map(str::to_owned))
    else {
        return Err(ApiError::bad_request("give ?project="));
    };
    policy::read_project(&actor, &project)?;
    let p = project.clone();
    let t = state
        .db
        .read(move |c| {
            crate::store::projects::get(c, &p)?;
            templates::resolve(c, &p, &id)
        })
        .await?;
    policy::read_templates(&actor, t.project.as_deref())?;
    let mut agent = t.spec.clone();
    agent.insert("project".into(), Value::String(project));
    Ok(Json(Resolved {
        template: t.reference(),
        agent: Value::Object(agent),
    }))
}

/// The body of `POST /api/templates/from-agent/{agent}`.
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct FromAgentBody {
    name: String,
    /// Absent: the agent's project. `null` or `""`: global.
    #[serde(default, deserialize_with = "explicit")]
    project: Option<Option<String>>,
    #[serde(default)]
    description: Option<String>,
}

/// `POST /api/templates/from-agent/{agent}`: a template with the
/// settings of an agent now.
async fn from_agent(
    State(state): State<Shared>,
    actor: Actor,
    Path(agent): Path<String>,
    ApiJson(b): ApiJson<FromAgentBody>,
) -> ApiResult<(StatusCode, Json<Template>)> {
    let a = super::agents::load(&state, &actor, agent).await?;
    let project = match b.project {
        Some(p) => p.filter(|p| !p.is_empty()),
        None => Some(a.agent.project.clone()),
    };
    policy::manage_templates(&actor, project.as_deref(), "")?;
    let def = AgentDef {
        name: a.agent.name.clone(),
        project: a.agent.project.clone(),
        harness: a.agent.harness,
        subscription: a.agent.subscription.clone(),
        subscription_pool: a.agent.subscription_pool.clone(),
        model: a.agent.model.clone(),
        prompt: a.agent.prompt.clone(),
        system_prompt: a.agent.system_prompt.clone(),
        persona: a.agent.persona.clone(),
        roles: a.agent.roles.clone(),
        policy: a.agent.policy.clone(),
        restart: a.agent.restart,
        idle_stop: a.agent.idle_stop,
        skills: a.agent.skills.clone(),
        new_session_on_prompt_change: a.agent.new_session_on_prompt_change,
    };
    let new = NewTemplate {
        name: b.name,
        project,
        description: b
            .description
            .unwrap_or_else(|| format!("from agent {}/{}", def.project, def.name)),
        spec: agent_template::spec_of(&def),
        created_by: actor.label(),
        source: SOURCE_API.into(),
    };
    let t = state
        .db
        .write(actor.label(), move |t| templates::create(t, &new))
        .await?;
    Ok((StatusCode::CREATED, Json(t)))
}

/// The template step of `POST /api/agents`: with `template`, the
/// definition is the template's spec with the body's fields over it
/// (`agent_template::merge`). Returns the definition and the template.
/// The caller then runs the normal create checks on the definition.
pub async fn create_body(
    state: &Shared,
    actor: &Actor,
    body: Value,
) -> ApiResult<(AgentDef, Option<TemplateRef>)> {
    let Value::Object(mut body) = body else {
        return Err(ApiError::bad_request("the body is not a JSON object"));
    };
    let key = match body.remove("template") {
        None | Some(Value::Null) => None,
        Some(Value::String(s)) if s.trim().is_empty() => None,
        Some(Value::String(s)) => Some(s),
        Some(_) => return Err(ApiError::bad_request("`template` must be a string")),
    };
    let Some(key) = key else {
        return Ok((parse_def(Value::Object(body))?, None));
    };
    let project = match body.get("project") {
        Some(Value::String(p)) if !p.is_empty() => p.clone(),
        _ => return Err(ApiError::bad_request("give `project` with `template`")),
    };
    let p = project.clone();
    let t = state
        .db
        .read(move |c| templates::resolve(c, &p, &key))
        .await?;
    policy::read_templates(actor, t.project.as_deref())?;
    // A `null` in the body clears the template's value.
    let merged = agent_template::merge(&t.spec, &body);
    Ok((parse_def(Value::Object(merged))?, Some(t.reference())))
}

fn parse_def(v: Value) -> ApiResult<AgentDef> {
    serde_json::from_value(v)
        .map_err(|e| ApiError::bad_request(format!("bad agent definition: {e}")))
}
