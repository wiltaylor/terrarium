//! `/api/projects/{name}/monitors`, `/api/monitors/...`, and
//! `/api/projects/{name}/dashboard` (docs/api.md §30).
//!
//! Members of a project (and its agents) read the monitors, the results,
//! and the dashboard. A monitor runs a script on a target, so a write
//! (create, change, delete, run now, test run, a layout save) needs
//! project_admin of the project, sysadmin, or the operator, and the
//! caller must be able to use the target: its policy or a lease (the
//! operator uses any target). The target is a global target or one of
//! the project.

use axum::extract::{Path, State};
use axum::http::StatusCode;
use axum::routing::{get, post};
use axum::{Json, Router};
use chrono::Utc;
use serde::Deserialize;
use serde_json::{Value, json};

use super::{ApiJson, ApiQuery, Shared};
use crate::actor::Actor;
use crate::error::{ApiError, ApiResult, Code};
use crate::executor;
use crate::monitors::layout::{self, Layout};
use crate::monitors::{self as engine};
use crate::policy;
use crate::store::monitors::{self, Monitor, MonitorDef};
use crate::store::schedules::field_error;

pub fn routes() -> Router<Shared> {
    Router::new()
        .route("/projects/{name}/monitors", get(list).post(create))
        .route("/projects/{name}/monitors/test", post(test_run))
        .route(
            "/projects/{name}/dashboard",
            get(dashboard).put(put_dashboard),
        )
        .route("/monitors/{id}", get(show).patch(update).delete(remove))
        .route("/monitors/{id}/run", post(run_now))
        .route("/monitors/{id}/results", get(results))
}

/// Fill `running` from the runtime.
fn live(state: &Shared, mut m: Monitor) -> Monitor {
    m.running = state.monitors.is_running(&m.id);
    m
}

/// The target must be usable by the caller (its policy or a lease), and
/// global or of the project.
pub(super) async fn check_target(
    state: &Shared,
    actor: &Actor,
    project: &str,
    target: &str,
) -> ApiResult<()> {
    let row = executor::usable(state, actor, target)
        .await
        .map_err(|e| match e.code {
            Code::NotFound => field_error("target", e.message),
            _ => e,
        })?;
    if let Some(p) = row.def.project.as_deref().filter(|p| *p != project) {
        return Err(field_error(
            "target",
            format!(
                "target `{target}` belongs to project `{p}`; use a global target or one of `{project}`"
            ),
        ));
    }
    if executor::no_exec(&row.def).is_err() {
        return Err(field_error(
            "target",
            format!("target `{target}` takes no commands"),
        ));
    }
    Ok(())
}

async fn load(state: &Shared, key: String) -> ApiResult<Monitor> {
    state.db.read(move |c| monitors::find(c, &key)).await
}

fn bad_def(e: serde_json::Error) -> ApiError {
    ApiError::invalid(format!("bad monitor: {e}"))
}

// ------------------------------------------------------------------ reads

async fn list(
    State(state): State<Shared>,
    actor: Actor,
    Path(name): Path<String>,
) -> ApiResult<Json<Vec<Monitor>>> {
    policy::read_project(&actor, &name)?;
    let rows = state
        .db
        .read(move |c| {
            crate::store::projects::get(c, &name)?;
            monitors::list(c, &name)
        })
        .await?;
    Ok(Json(rows.into_iter().map(|m| live(&state, m)).collect()))
}

async fn show(
    State(state): State<Shared>,
    actor: Actor,
    Path(id): Path<String>,
) -> ApiResult<Json<Monitor>> {
    let m = load(&state, id).await?;
    policy::read_project(&actor, &m.project)?;
    Ok(Json(live(&state, m)))
}

#[derive(Debug, Default, Deserialize)]
#[serde(deny_unknown_fields)]
struct ResultsQuery {
    since: Option<String>,
    until: Option<String>,
    limit: Option<usize>,
    /// The bucket of a downsample: seconds (`60`) or a duration (`5m`).
    bucket: Option<String>,
}

fn bucket_secs(v: &str) -> ApiResult<i64> {
    let v = v.trim();
    let secs = match v.parse::<i64>() {
        Ok(n) => n,
        Err(_) => crate::store::schedules::parse_every(v).map_err(|e| {
            ApiError::bad_request(format!(
                "bad `bucket` `{v}`: {e}; use seconds or 30s, 5m, 1h"
            ))
        })?,
    };
    if !(1..=7 * 86_400).contains(&secs) {
        return Err(ApiError::bad_request(format!(
            "bad `bucket` `{v}`: use 1 s to 7 d"
        )));
    }
    Ok(secs)
}

/// `GET /api/monitors/{id}/results`: oldest first. With `bucket`, one
/// point for each bucket: the mean, min, and max of the values, the
/// worst status, and the count.
async fn results(
    State(state): State<Shared>,
    actor: Actor,
    Path(id): Path<String>,
    ApiQuery(q): ApiQuery<ResultsQuery>,
) -> ApiResult<Json<Value>> {
    let m = load(&state, id).await?;
    policy::read_project(&actor, &m.project)?;
    let since = crate::time_param::opt_time_param("since", q.since.as_deref())?;
    let until = crate::time_param::opt_time_param("until", q.until.as_deref())?;
    let limit = q
        .limit
        .unwrap_or(monitors::DEFAULT_RESULTS)
        .clamp(1, monitors::MAX_RESULTS);
    let bucket = q.bucket.as_deref().map(bucket_secs).transpose()?;
    let fetch = if bucket.is_some() {
        monitors::MAX_RESULTS
    } else {
        limit
    };
    let rows = state
        .db
        .read(move |c| monitors::results(c, &m.id, since.as_deref(), until.as_deref(), fetch))
        .await?;
    let out = match bucket {
        None => serde_json::to_value(rows),
        Some(secs) => {
            let pts: Vec<(i64, _, _)> = rows
                .iter()
                .filter_map(|r| {
                    chrono::DateTime::parse_from_rfc3339(&r.at)
                        .ok()
                        .map(|t| (t.timestamp_millis(), r.status, r.value))
                })
                .collect();
            let mut points = layout::downsample(&pts, secs * 1000);
            if points.len() > limit {
                points.drain(..points.len() - limit);
            }
            serde_json::to_value(points)
        }
    }
    .map_err(|e| ApiError::internal(e.to_string()))?;
    Ok(Json(out))
}

// ----------------------------------------------------------------- writes

async fn create(
    State(state): State<Shared>,
    actor: Actor,
    Path(name): Path<String>,
    ApiJson(body): ApiJson<Value>,
) -> ApiResult<(StatusCode, Json<Monitor>)> {
    policy::admin_project(&actor, &name, "creating a monitor")?;
    let def: MonitorDef = serde_json::from_value(body).map_err(bad_def)?;
    monitors::validate(&def)?;
    check_target(&state, &actor, &name, &def.target).await?;
    let label = actor.label();
    let m = state
        .db
        .write(actor.label(), move |t| {
            monitors::create(t, &name, &def, &label, Utc::now())
        })
        .await?;
    Ok((StatusCode::CREATED, Json(live(&state, m))))
}

/// Merge a JSON patch into the fields: a key replaces the field, `null`
/// clears an optional field.
fn merged(def: &MonitorDef, patch: Value) -> ApiResult<MonitorDef> {
    let Value::Object(p) = patch else {
        return Err(ApiError::bad_request("the body is a JSON object"));
    };
    let mut v = serde_json::to_value(def).map_err(|e| ApiError::internal(e.to_string()))?;
    let Value::Object(m) = &mut v else {
        return Err(ApiError::internal("a monitor is not an object"));
    };
    for (k, x) in p {
        if x.is_null() {
            m.remove(&k);
        } else {
            m.insert(k, x);
        }
    }
    serde_json::from_value(v).map_err(bad_def)
}

async fn update(
    State(state): State<Shared>,
    actor: Actor,
    Path(id): Path<String>,
    ApiJson(patch): ApiJson<Value>,
) -> ApiResult<Json<Monitor>> {
    let m = load(&state, id).await?;
    policy::admin_project(&actor, &m.project, "changing a monitor")?;
    let def = merged(&m.def, patch)?;
    monitors::validate(&def)?;
    // A new script or target runs as the caller: the caller must be able
    // to use the target. So must an agent that turns on another actor's
    // monitor.
    let takes_over = def.script != m.def.script
        || def.target != m.def.target
        || (def.enabled && !m.def.enabled && actor.label() != m.run_as);
    if takes_over {
        check_target(&state, &actor, &m.project, &def.target).await?;
    }
    let label = actor.label();
    let id = m.id.clone();
    let m = state
        .db
        .write(actor.label(), move |t| {
            monitors::update(t, &id, &def, &label, Utc::now())
        })
        .await?;
    Ok(Json(live(&state, m)))
}

async fn remove(
    State(state): State<Shared>,
    actor: Actor,
    Path(id): Path<String>,
) -> ApiResult<StatusCode> {
    let m = load(&state, id).await?;
    policy::admin_project(&actor, &m.project, "deleting a monitor")?;
    state
        .db
        .write(actor.label(), move |t| monitors::delete(t, &m.id))
        .await?;
    Ok(StatusCode::NO_CONTENT)
}

/// `POST /api/monitors/{id}/run`: run now, keep the result, and answer
/// with it and the raw output. The run acts as the monitor's `run_as`, as
/// a timed run does; an agent that is not the `run_as` must be able to
/// use the target itself.
async fn run_now(
    State(state): State<Shared>,
    actor: Actor,
    Path(id): Path<String>,
) -> ApiResult<Json<Value>> {
    let m = load(&state, id).await?;
    policy::admin_project(&actor, &m.project, "running a monitor")?;
    if matches!(actor, Actor::Agent(_)) && actor.label() != m.run_as {
        check_target(&state, &actor, &m.project, &m.def.target).await?;
    }
    let Some(claim) = state.monitors.claim(&m.id) else {
        return Err(ApiError::conflict(format!(
            "monitor `{}` is running now; wait for the run to end",
            m.def.name
        )));
    };
    let (key, project, mid) = (m.key(), m.project.clone(), m.id.clone());
    state
        .db
        .write(actor.label(), move |t| {
            t.audit("monitor.run", &key, Some(&project), json!({"id": mid}))
        })
        .await?;
    let (result, output) = engine::run(&state, &m.id, claim).await?;
    Ok(Json(json!({"result": result, "output": output})))
}

/// `POST /api/projects/{name}/monitors/test`: run a monitor that is not
/// saved, once, as the caller, and answer with the parsed result and the
/// raw output. Nothing is kept. `name` and `schedule` may be absent.
async fn test_run(
    State(state): State<Shared>,
    actor: Actor,
    Path(name): Path<String>,
    ApiJson(body): ApiJson<Value>,
) -> ApiResult<Json<Value>> {
    policy::admin_project(&actor, &name, "running a monitor")?;
    let Value::Object(mut b) = body else {
        return Err(ApiError::bad_request("the body is a JSON object"));
    };
    if b.get("name")
        .and_then(Value::as_str)
        .is_none_or(|n| !crate::store::valid_name(n))
    {
        b.insert("name".into(), json!("test-run"));
    }
    b.entry("schedule")
        .or_insert_with(|| json!({"kind": "interval", "every": "1m"}));
    let def: MonitorDef = serde_json::from_value(Value::Object(b)).map_err(bad_def)?;
    monitors::validate(&def)?;
    let p = name.clone();
    state
        .db
        .read(move |c| crate::store::projects::get(c, &p).map(|_| ()))
        .await?;
    check_target(&state, &actor, &name, &def.target).await?;
    let (target, script) = (
        def.target.clone(),
        crate::monitors::parse::cut_chars(&def.script, 200),
    );
    let p = name.clone();
    state
        .db
        .write(actor.label(), move |t| {
            t.audit("monitor.test", &target, Some(&p), json!({"script": script}))
        })
        .await?;
    let e = engine::execute(&state, &actor, &name, &def).await;
    let parsed = &e.parsed;
    Ok(Json(json!({
        "result": {
            "at": crate::store::schedules::stamp(e.at),
            "status": parsed.status,
            "value": parsed.value,
            "text": parsed.text,
            "json": parsed.json,
            "error": parsed.error,
            "duration_ms": e.duration_ms,
        },
        "output": e.output,
    })))
}

// -------------------------------------------------------------- dashboard

/// `GET /api/projects/{name}/dashboard`: the layout, and every monitor of
/// the project with its last result. A block whose monitor is gone, or
/// whose kind no longer fits, stays out.
async fn dashboard(
    State(state): State<Shared>,
    actor: Actor,
    Path(name): Path<String>,
) -> ApiResult<Json<Value>> {
    policy::read_project(&actor, &name)?;
    let p = name.clone();
    let (layout, meta, rows) = state
        .db
        .read(move |c| {
            crate::store::projects::get(c, &p)?;
            let (l, meta) = monitors::layout_of(c, &p)?;
            Ok((l, meta, monitors::list(c, &p)?))
        })
        .await?;
    let kinds = rows.iter().map(|m| (m.id.clone(), m.def.kind)).collect();
    let blocks: Vec<_> = layout
        .blocks
        .into_iter()
        .filter(|b| {
            layout::check(
                &Layout {
                    blocks: vec![b.clone()],
                },
                &kinds,
            )
            .is_ok()
        })
        .collect();
    let rows: Vec<Monitor> = rows.into_iter().map(|m| live(&state, m)).collect();
    let (updated_at, updated_by) = meta.unzip();
    Ok(Json(json!({
        "project": name,
        "blocks": blocks,
        "updated_at": updated_at,
        "updated_by": updated_by,
        "monitors": rows,
    })))
}

async fn put_dashboard(
    State(state): State<Shared>,
    actor: Actor,
    Path(name): Path<String>,
    ApiJson(body): ApiJson<Value>,
) -> ApiResult<Json<Value>> {
    policy::admin_project(&actor, &name, "editing the dashboard")?;
    let l: Layout =
        serde_json::from_value(body).map_err(|e| ApiError::invalid(format!("bad layout: {e}")))?;
    let label = actor.label();
    let p = name.clone();
    state
        .db
        .write(actor.label(), move |t| {
            monitors::put_layout(t, &p, &l, &label)
        })
        .await?;
    dashboard(State(state), actor, Path(name)).await
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;

    #[test]
    fn buckets() {
        assert_eq!(bucket_secs("60").unwrap(), 60);
        assert_eq!(bucket_secs("5m").unwrap(), 300);
        assert!(bucket_secs("0").is_err());
        assert!(bucket_secs("8d").is_err());
        assert!(bucket_secs("soon").is_err());
    }

    #[test]
    fn patches_merge_and_clear() {
        let def: MonitorDef = serde_json::from_value(json!({
            "name": "disk", "target": "box", "script": "df", "kind": "number",
            "schedule": {"kind": "interval", "every": "1m"}, "unit": "%", "warn": 80, "crit": 90
        }))
        .unwrap();
        let d = merged(&def, json!({"warn": null, "crit": 95, "enabled": false})).unwrap();
        assert_eq!(d.warn, None);
        assert_eq!(d.crit, Some(95.0));
        assert!(!d.enabled);
        assert_eq!(d.unit.as_deref(), Some("%"));
        assert!(merged(&def, json!({"colour": "red"})).is_err());
        assert!(merged(&def, json!([1])).is_err());
    }
}
