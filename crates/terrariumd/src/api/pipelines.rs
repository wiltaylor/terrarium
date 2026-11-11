//! `/api/projects/{name}/pipelines` and `/api/pipelines/...` (docs/api.md
//! §31).
//!
//! Members of a project (and its agents) read the pipelines and their
//! runs. A write (create, change, delete, refresh, test) needs
//! project_admin of the project, sysadmin, or the operator. A `script`
//! source also needs a target that the caller may use (as for a monitor),
//! and each secret that a source names must be one the caller may use
//! (as for a webhook).

use axum::extract::{Path, State};
use axum::http::StatusCode;
use axum::routing::{get, post};
use axum::{Json};
use chrono::Utc;
use serde::Deserialize;
use serde_json::{Value, json};

use super::monitors::check_target;
use super::{ApiJson, ApiQuery, Shared};
use crate::actor::Actor;
use crate::error::{ApiError, ApiResult};
use crate::pipelines::{self as engine, contract};
use crate::policy;
use crate::store::pipelines::{self, Pipeline, PipelineDef};
use crate::webhooks;

pub fn routes() -> super::link_registry::RegisteredRouter {
    super::link_registry::RegisteredRouter::new()
        .route("/projects/{name}/pipelines", get(list).post(create), &[(axum::http::Method::GET, super::link_registry::Access::Link), (axum::http::Method::POST, super::link_registry::Access::Link)])
        .route("/projects/{name}/pipelines/test", post(test_poll), &[(axum::http::Method::POST, super::link_registry::Access::Link)])
        .route("/pipelines/{id}", get(show).patch(update).delete(remove), &[(axum::http::Method::GET, super::link_registry::Access::Link), (axum::http::Method::PATCH, super::link_registry::Access::Link), (axum::http::Method::DELETE, super::link_registry::Access::Link)])
        .route("/pipelines/{id}/refresh", post(refresh), &[(axum::http::Method::POST, super::link_registry::Access::Link)])
        .route("/pipelines/{id}/runs", get(runs), &[(axum::http::Method::GET, super::link_registry::Access::Link)])
        .route("/pipelines/{id}/runs/{run}", get(run_detail), &[(axum::http::Method::GET, super::link_registry::Access::Link)])
}

/// Fill `running` from the runtime.
fn live(state: &Shared, mut p: Pipeline) -> Pipeline {
    p.running = state.pipelines.is_running(&p.id);
    p
}

async fn load(state: &Shared, key: String) -> ApiResult<Pipeline> {
    state.db.read(move |c| pipelines::find(c, &key)).await
}

fn bad_def(e: serde_json::Error) -> ApiError {
    ApiError::invalid(format!("bad pipeline: {e}"))
}

/// The checks of a source for `actor`: the target of a script, and each
/// secret.
async fn check_source(
    state: &Shared,
    actor: &Actor,
    project: &str,
    def: &PipelineDef,
) -> ApiResult<()> {
    if let Some(t) = def.source.target() {
        check_target(state, actor, project, t)
            .await
            .map_err(|mut e| {
                if let Some(d) = e.details.as_mut()
                    && d["field"] == "target"
                {
                    d["field"] = json!("source.target");
                }
                e
            })?;
    }
    let names: Vec<(String, String)> = def
        .source
        .secrets()
        .into_iter()
        .map(|(f, n)| (f, n.to_owned()))
        .collect();
    if names.is_empty() {
        return Ok(());
    }
    let (actor, project) = (actor.clone(), project.to_owned());
    state
        .db
        .read(move |c| {
            for (field, name) in &names {
                webhooks::check_secret_use_in(
                    c,
                    &actor,
                    Some(&project),
                    name,
                    field,
                    "a pipeline",
                )?;
            }
            Ok(())
        })
        .await
}

// ------------------------------------------------------------------ reads

async fn list(
    State(state): State<Shared>,
    actor: Actor,
    Path(name): Path<String>,
) -> ApiResult<Json<Vec<Pipeline>>> {
    policy::read_project(&actor, &name)?;
    let rows = state
        .db
        .read(move |c| {
            crate::store::projects::get(c, &name)?;
            pipelines::list(c, &name)
        })
        .await?;
    Ok(Json(rows.into_iter().map(|p| live(&state, p)).collect()))
}

async fn show(
    State(state): State<Shared>,
    actor: Actor,
    Path(id): Path<String>,
) -> ApiResult<Json<Pipeline>> {
    let p = load(&state, id).await?;
    policy::read_project(&actor, &p.project)?;
    Ok(Json(live(&state, p)))
}

#[derive(Debug, Default, Deserialize)]
#[serde(deny_unknown_fields)]
struct RunsQuery {
    limit: Option<usize>,
}

/// `GET /api/pipelines/{id}/runs`: newest first.
async fn runs(
    State(state): State<Shared>,
    actor: Actor,
    Path(id): Path<String>,
    ApiQuery(q): ApiQuery<RunsQuery>,
) -> ApiResult<Json<Vec<pipelines::Run>>> {
    let p = load(&state, id).await?;
    policy::read_project(&actor, &p.project)?;
    let limit = q
        .limit
        .unwrap_or(pipelines::MAX_MAX_RUNS as usize)
        .clamp(1, pipelines::MAX_MAX_RUNS as usize);
    let rows = state
        .db
        .read(move |c| pipelines::runs(c, &p.id, limit))
        .await?;
    Ok(Json(rows))
}

/// `GET /api/pipelines/{id}/runs/{run}`: one run with its steps. A
/// `github` run whose jobs were not read yet reads them now; when that
/// fails, `steps_error` says why.
async fn run_detail(
    State(state): State<Shared>,
    actor: Actor,
    Path((id, run)): Path<(String, String)>,
) -> ApiResult<Json<Value>> {
    let p = load(&state, id).await?;
    policy::read_project(&actor, &p.project)?;
    let (pid, rid) = (p.id.clone(), run.clone());
    let mut r = state
        .db
        .read(move |c| pipelines::run(c, &pid, &rid))
        .await?;
    let mut steps_error = None;
    if r.steps.is_none() {
        match engine::load_steps(&state, &p, &run).await {
            Ok(s) => r.steps = s,
            Err(e) => steps_error = Some(e.message),
        }
    }
    let mut v = serde_json::to_value(&r).map_err(|e| ApiError::internal(e.to_string()))?;
    v["pipeline_name"] = json!(p.def.name);
    v["project"] = json!(p.project);
    v["adapter"] = json!(p.def.source.kind());
    if let Some(e) = steps_error {
        v["steps_error"] = json!(e);
    }
    Ok(Json(v))
}

// ----------------------------------------------------------------- writes

async fn create(
    State(state): State<Shared>,
    actor: Actor,
    Path(name): Path<String>,
    ApiJson(body): ApiJson<Value>,
) -> ApiResult<(StatusCode, Json<Pipeline>)> {
    policy::admin_project(&actor, &name, "creating a pipeline")?;
    let def: PipelineDef = serde_json::from_value(body).map_err(bad_def)?;
    pipelines::validate(&def)?;
    let p = name.clone();
    state
        .db
        .read(move |c| crate::store::projects::get(c, &p).map(|_| ()))
        .await?;
    check_source(&state, &actor, &name, &def).await?;
    let label = actor.label();
    let p = state
        .db
        .write(actor.label(), move |t| {
            pipelines::create(t, &name, &def, &label, Utc::now())
        })
        .await?;
    Ok((StatusCode::CREATED, Json(live(&state, p))))
}

/// Merge a JSON patch into the fields: a key replaces the field, `null`
/// clears an optional field. `source` merges one level down, so
/// `{"source": {"branch": "main"}}` keeps the other source fields; a new
/// `source.kind` replaces the source.
fn merged(def: &PipelineDef, patch: Value) -> ApiResult<PipelineDef> {
    let Value::Object(p) = patch else {
        return Err(ApiError::bad_request("the body is a JSON object"));
    };
    let mut v = serde_json::to_value(def).map_err(|e| ApiError::internal(e.to_string()))?;
    let Value::Object(m) = &mut v else {
        return Err(ApiError::internal("a pipeline is not an object"));
    };
    for (k, x) in p {
        match (k.as_str(), x) {
            ("source", Value::Object(src)) => {
                let same_kind = src
                    .get("kind")
                    .is_none_or(|k| Some(k) == m.get("source").and_then(|s| s.get("kind")));
                let mut base = match m.get("source") {
                    Some(Value::Object(o)) if same_kind => o.clone(),
                    _ => serde_json::Map::new(),
                };
                for (sk, sx) in src {
                    if sx.is_null() {
                        base.remove(&sk);
                    } else {
                        base.insert(sk, sx);
                    }
                }
                m.insert("source".into(), Value::Object(base));
            }
            (_, Value::Null) => {
                m.remove(&k);
            }
            (_, x) => {
                m.insert(k, x);
            }
        }
    }
    serde_json::from_value(v).map_err(bad_def)
}

async fn update(
    State(state): State<Shared>,
    actor: Actor,
    Path(id): Path<String>,
    ApiJson(patch): ApiJson<Value>,
) -> ApiResult<Json<Pipeline>> {
    let p = load(&state, id).await?;
    policy::admin_project(&actor, &p.project, "changing a pipeline")?;
    let def = merged(&p.def, patch)?;
    pipelines::validate(&def)?;
    // A new source runs as the caller: the caller must be able to use its
    // target and secrets. So must an agent that turns on another actor's
    // pipeline.
    let takes_over = pipelines::takes_over(&p.def, &def)
        || (def.enabled && !p.def.enabled && actor.label() != p.run_as);
    if takes_over {
        check_source(&state, &actor, &p.project, &def).await?;
    }
    let label = actor.label();
    let id = p.id.clone();
    let p = state
        .db
        .write(actor.label(), move |t| {
            pipelines::update(t, &id, &def, &label, Utc::now())
        })
        .await?;
    Ok(Json(live(&state, p)))
}

async fn remove(
    State(state): State<Shared>,
    actor: Actor,
    Path(id): Path<String>,
) -> ApiResult<StatusCode> {
    let p = load(&state, id).await?;
    policy::admin_project(&actor, &p.project, "deleting a pipeline")?;
    state
        .db
        .write(actor.label(), move |t| pipelines::delete(t, &p.id))
        .await?;
    Ok(StatusCode::NO_CONTENT)
}

/// `POST /api/pipelines/{id}/refresh`: poll now, keep the runs, and
/// answer with the pipeline, what the poll changed, and the raw output of
/// a script. The poll acts as `run_as`, as a timed poll does; an agent
/// that is not the `run_as` must be able to use the target and the
/// secrets itself.
async fn refresh(
    State(state): State<Shared>,
    actor: Actor,
    Path(id): Path<String>,
) -> ApiResult<Json<Value>> {
    let p = load(&state, id).await?;
    policy::admin_project(&actor, &p.project, "refreshing a pipeline")?;
    if matches!(actor, Actor::Agent(_)) && actor.label() != p.run_as {
        check_source(&state, &actor, &p.project, &p.def).await?;
    }
    let Some(claim) = state.pipelines.claim(&p.id) else {
        return Err(ApiError::conflict(format!(
            "pipeline `{}` is polling now; wait for the poll to end",
            p.def.name
        )));
    };
    let (key, project, pid) = (p.key(), p.project.clone(), p.id.clone());
    state
        .db
        .write(actor.label(), move |t| {
            t.audit("pipeline.refresh", &key, Some(&project), json!({"id": pid}))
        })
        .await?;
    let (poll, output) = engine::run(&state, &p.id, claim).await?;
    let p = load(&state, p.id).await?;
    // The output can hold a secret value that the script printed.
    let output = serde_json::to_value(output).map_err(|e| ApiError::internal(e.to_string()))?;
    let output = crate::db::redact_json_values(
        &state.db.redactor().get(),
        &terrarium_core::redact::View::All,
        output,
    );
    Ok(Json(
        json!({"pipeline": live(&state, p), "poll": poll, "output": output}),
    ))
}

/// `POST /api/projects/{name}/pipelines/test`: poll a pipeline that is
/// not saved, once, as the caller, and answer with the runs (or the
/// error) and the raw output of a script. Nothing is kept. `name` may be
/// absent.
async fn test_poll(
    State(state): State<Shared>,
    actor: Actor,
    Path(name): Path<String>,
    ApiJson(body): ApiJson<Value>,
) -> ApiResult<Json<Value>> {
    policy::admin_project(&actor, &name, "testing a pipeline")?;
    let Value::Object(mut b) = body else {
        return Err(ApiError::bad_request("the body is a JSON object"));
    };
    if b.get("name")
        .and_then(Value::as_str)
        .is_none_or(|n| !crate::store::valid_name(n))
    {
        b.insert("name".into(), json!("test-poll"));
    }
    let def: PipelineDef = serde_json::from_value(Value::Object(b)).map_err(bad_def)?;
    pipelines::validate(&def)?;
    let p = name.clone();
    state
        .db
        .read(move |c| crate::store::projects::get(c, &p).map(|_| ()))
        .await?;
    check_source(&state, &actor, &name, &def).await?;
    let f = engine::fetch(&state, &actor, &name, &def, &Default::default()).await;
    let (runs, error) = match f.runs {
        Ok(r) => (r, None),
        Err(e) => (Vec::new(), Some(contract::cut(&e, 2000))),
    };
    let body = json!({
        "at": crate::store::schedules::stamp(f.at),
        "duration_ms": f.duration_ms,
        "runs": runs,
        "error": error,
        "output": f.output,
    });
    // The audit row, and the answer with no secret value that the script
    // printed.
    let (kind, target) = (def.source.kind(), def.source.target().map(str::to_owned));
    let body = state
        .db
        .write(actor.label(), move |t| {
            t.audit(
                "pipeline.test",
                &format!("{name}/test"),
                Some(&name),
                json!({"adapter": kind, "target": target, "error": body["error"]}),
            )?;
            Ok(t.redact_values(body))
        })
        .await?;
    Ok(Json(body))
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;

    #[test]
    fn patches_merge_the_source() {
        let def: PipelineDef = serde_json::from_value(json!({
            "name": "ci", "source": {"kind": "github", "repo": "o/r", "workflow": "ci.yml",
                                     "token_secret": "gh"},
            "max_runs": 30
        }))
        .unwrap();
        let d = merged(
            &def,
            json!({"source": {"branch": "main", "workflow": null}}),
        )
        .unwrap();
        let pipelines::Source::Github(g) = &d.source else {
            panic!()
        };
        assert_eq!(g.branch.as_deref(), Some("main"));
        assert_eq!(g.workflow, None);
        assert_eq!(g.token_secret.as_deref(), Some("gh"));
        assert_eq!(d.max_runs, 30);
        // A new kind replaces the source.
        let d = merged(
            &def,
            json!({"source": {"kind": "script", "target": "wsl", "script": "x"}}),
        )
        .unwrap();
        assert_eq!(d.source.kind(), "script");
        assert!(merged(&def, json!({"source": {"kind": "script"}})).is_err());
        assert!(merged(&def, json!({"colour": "red"})).is_err());
        assert!(merged(&def, json!([1])).is_err());
        let d = merged(&def, json!({"max_runs": null, "enabled": false})).unwrap();
        assert_eq!(d.max_runs, pipelines::DEFAULT_MAX_RUNS);
        assert!(!d.enabled);
    }
}
