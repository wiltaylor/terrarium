//! `/api/projects/{name}/calendar/...` and `/api/calendar/...` (docs/api.md
//! §33): the calendar of each project, the global calendar, and the
//! combined view.
//!
//! Rights (`crate::policy::write_calendar`):
//!
//! - Every agent of a project (and the operator, and a sysadmin) reads the
//!   project calendar. Each agent reads the global calendar. A project
//!   view shows the global events too, marked `global`.
//! - A lead (project_admin of the project), a sysadmin, and the operator
//!   add, change, and remove each event of the project. A lead grants
//!   the calendar write right to a worker (`policy.calendar_write`): the
//!   worker then adds events, and changes and removes the ones it made.
//! - A sysadmin and the operator write the global calendar.

use axum::extract::{Path, State};
use axum::http::StatusCode;
use axum::routing::{get, post, put};
use axum::{Json, Router};
use chrono::{DateTime, Duration, NaiveDate, Utc};
use serde::Deserialize;
use serde_json::{Value, json};

use super::{ApiJson, ApiQuery, Shared};
use crate::actor::Actor;
use crate::error::{ApiError, ApiResult};
use crate::policy;
use crate::store::calendar::{self as cal, Event, Filter, Scope};
use crate::store::{agents, projects};

pub fn routes() -> Router<Shared> {
    Router::new()
        .route("/calendar", get(combined))
        .route("/calendar/global", get(global_view))
        .route("/calendar/global/events", post(create_global))
        .route(
            "/calendar/global/watches",
            get(global_watches).post(global_watch),
        )
        .route(
            "/calendar/global/watches/{watcher}",
            axum::routing::delete(global_unwatch),
        )
        .route(
            "/calendar/events/{id}",
            get(show).patch(update).delete(remove),
        )
        .route("/calendar/events/{id}/skip", post(skip))
        .route("/calendar/events/{id}/unskip", post(unskip))
        .route(
            "/calendar/events/{id}/assignees/{agent}",
            put(assign).delete(unassign),
        )
        .route("/calendar/events/{id}/notices", get(notices))
        .route("/projects/{name}/calendar", get(project_view))
        .route("/projects/{name}/calendar/events", post(create_project))
        .route(
            "/projects/{name}/calendar/watches",
            get(project_watches).post(project_watch),
        )
        .route(
            "/projects/{name}/calendar/watches/{watcher}",
            axum::routing::delete(project_unwatch),
        )
        .route("/projects/{name}/calendar/grants", get(grants))
        .route(
            "/projects/{name}/calendar/grants/{agent}",
            put(grant).delete(revoke),
        )
}

// ------------------------------------------------------------- the range

#[derive(Debug, Default, Deserialize)]
#[serde(deny_unknown_fields)]
struct RangeQuery {
    /// RFC 3339 or a date (UTC midnight). Default: the start of today.
    from: Option<String>,
    /// RFC 3339 or a date. Default: `from` + `days`.
    to: Option<String>,
    /// The days of the window when `to` is absent (default 31).
    days: Option<i64>,
    /// The combined view only: one project.
    project: Option<String>,
}

fn range_time(name: &str, v: &str) -> ApiResult<DateTime<Utc>> {
    let v = v.trim();
    if let Some(t) = cal::parse_utc(v) {
        return Ok(t);
    }
    if let Ok(d) = NaiveDate::parse_from_str(v, "%Y-%m-%d") {
        return Ok(d.and_time(chrono::NaiveTime::MIN).and_utc());
    }
    Err(ApiError::bad_request(format!(
        "bad `{name}` `{v}`: use an RFC 3339 time or a date (2026-11-10)"
    )))
}

fn range(q: &RangeQuery) -> ApiResult<(DateTime<Utc>, DateTime<Utc>)> {
    let from = match q.from.as_deref().filter(|s| !s.trim().is_empty()) {
        Some(v) => range_time("from", v)?,
        None => Utc::now()
            .date_naive()
            .and_time(chrono::NaiveTime::MIN)
            .and_utc(),
    };
    let to = match q.to.as_deref().filter(|s| !s.trim().is_empty()) {
        Some(v) => range_time("to", v)?,
        None => {
            let days = q.days.unwrap_or(31);
            if !(1..=cal::MAX_RANGE_DAYS).contains(&days) {
                return Err(ApiError::bad_request(format!(
                    "bad `days` {days}: use 1 to {}",
                    cal::MAX_RANGE_DAYS
                )));
            }
            from + Duration::days(days)
        }
    };
    if to <= from {
        return Err(ApiError::bad_request("`to` is not after `from`"));
    }
    if to - from > Duration::days(cal::MAX_RANGE_DAYS) {
        return Err(ApiError::bad_request(format!(
            "the window is longer than {} days",
            cal::MAX_RANGE_DAYS
        )));
    }
    Ok((from, to))
}

/// An event as the API shows it to `actor`: with `can_edit`.
fn shown(actor: &Actor, e: &Event) -> ApiResult<Value> {
    let mut v = serde_json::to_value(e).map_err(|x| ApiError::internal(x.to_string()))?;
    v["can_edit"] = json!(policy::change_event(actor, e.project.as_deref(), &e.created_by).is_ok());
    Ok(v)
}

/// `write` is the calendar that a new event of this view goes to, for
/// `can_write`: a project, the global one, or none (the combined view).
async fn view(
    state: &Shared,
    actor: &Actor,
    filter: Filter,
    write: Option<Option<&str>>,
    q: &RangeQuery,
) -> ApiResult<Json<Value>> {
    let (from, to) = range(q)?;
    let events = state.db.read(move |c| cal::list(c, &filter)).await?;
    let (occ, cut) = cal::expand(&events, from, to);
    // Only the events with an occurrence in the window.
    let shown_events = events
        .iter()
        .filter(|e| occ.iter().any(|o| o.event_id == e.id))
        .map(|e| shown(actor, e))
        .collect::<ApiResult<Vec<_>>>()?;
    let can_write = write.is_some_and(|p| policy::write_calendar(actor, p).is_ok());
    Ok(Json(json!({
        "from": crate::time_param::stored(from),
        "to": crate::time_param::stored(to),
        "occurrences": occ,
        "events": shown_events,
        "truncated": cut,
        "can_write": can_write,
    })))
}

/// `GET /api/projects/{name}/calendar`: the occurrences of the project's
/// events and of the global events in the window.
async fn project_view(
    State(state): State<Shared>,
    actor: Actor,
    Path(name): Path<String>,
    ApiQuery(q): ApiQuery<RangeQuery>,
) -> ApiResult<Json<Value>> {
    policy::read_calendar(&actor, Some(&name))?;
    if q.project.is_some() {
        return Err(ApiError::bad_request(
            "`project` is for GET /api/calendar; the path names the project",
        ));
    }
    let n = name.clone();
    state.db.read(move |c| projects::get(c, &n)).await?;
    let w = Some(Some(name.as_str()));
    view(&state, &actor, Filter::Projects(vec![name.clone()]), w, &q).await
}

/// `GET /api/calendar`: the combined view. The operator and a sysadmin
/// see each project and the global events (`project` narrows it to one
/// project); another agent sees its own project and the global events.
async fn combined(
    State(state): State<Shared>,
    actor: Actor,
    ApiQuery(q): ApiQuery<RangeQuery>,
) -> ApiResult<Json<Value>> {
    let filter = match (&q.project, &actor) {
        (Some(p), _) => {
            policy::read_calendar(&actor, Some(p))?;
            Filter::Projects(vec![p.clone()])
        }
        (None, Actor::Operator) => Filter::All,
        (None, Actor::Agent(a)) if a.reads_all() => Filter::All,
        (None, Actor::Agent(a)) => Filter::Projects(vec![a.project.clone()]),
    };
    let write = q.project.as_deref().map(Some);
    view(&state, &actor, filter, write, &q).await
}

/// `GET /api/calendar/global`: the global events only.
async fn global_view(
    State(state): State<Shared>,
    actor: Actor,
    ApiQuery(q): ApiQuery<RangeQuery>,
) -> ApiResult<Json<Value>> {
    view(&state, &actor, Filter::Global, Some(None), &q).await
}

// ------------------------------------------------------------ the events

async fn load(state: &Shared, id: String) -> ApiResult<Event> {
    state.db.read(move |c| cal::get(c, &id)).await
}

async fn create(state: &Shared, actor: &Actor, scope: Scope, body: Value) -> ApiResult<Event> {
    policy::write_calendar(actor, scope.project())?;
    let input = cal::create_input(body)?;
    let label = actor.label();
    state
        .db
        .write(actor.label(), move |t| {
            cal::create(t, &scope, &input, &label)
        })
        .await
}

async fn create_project(
    State(state): State<Shared>,
    actor: Actor,
    Path(name): Path<String>,
    ApiJson(body): ApiJson<Value>,
) -> ApiResult<(StatusCode, Json<Event>)> {
    let e = create(&state, &actor, Scope::Project(name), body).await?;
    Ok((StatusCode::CREATED, Json(e)))
}

async fn create_global(
    State(state): State<Shared>,
    actor: Actor,
    ApiJson(body): ApiJson<Value>,
) -> ApiResult<(StatusCode, Json<Event>)> {
    let e = create(&state, &actor, Scope::Global, body).await?;
    Ok((StatusCode::CREATED, Json(e)))
}

async fn show(
    State(state): State<Shared>,
    actor: Actor,
    Path(id): Path<String>,
) -> ApiResult<Json<Value>> {
    let e = load(&state, id).await?;
    policy::read_calendar(&actor, e.project.as_deref())?;
    Ok(Json(shown(&actor, &e)?))
}

async fn notices(
    State(state): State<Shared>,
    actor: Actor,
    Path(id): Path<String>,
) -> ApiResult<Json<Value>> {
    let e = load(&state, id).await?;
    policy::read_calendar(&actor, e.project.as_deref())?;
    let rows = state.db.read(move |c| cal::notices(c, &e.id)).await?;
    Ok(Json(json!(rows)))
}

/// A write on an existing event: read it, check the rights, then run `f`
/// in one write.
async fn change<F>(state: &Shared, actor: &Actor, id: String, f: F) -> ApiResult<Event>
where
    F: FnOnce(&mut crate::db::Tx<'_>, &Event, &str) -> ApiResult<Event> + Send + 'static,
{
    let e = load(state, id).await?;
    policy::read_calendar(actor, e.project.as_deref())?;
    policy::change_event(actor, e.project.as_deref(), &e.created_by)?;
    let label = actor.label();
    state
        .db
        .write(actor.label(), move |t| {
            // The event as the write sees it.
            let now = cal::get(t, &e.id)?;
            f(t, &now, &label)
        })
        .await
}

async fn update(
    State(state): State<Shared>,
    actor: Actor,
    Path(id): Path<String>,
    ApiJson(patch): ApiJson<Value>,
) -> ApiResult<Json<Event>> {
    let e = change(&state, &actor, id, move |t, e, label| {
        let input = cal::merged_input(e, patch)?;
        cal::update(t, &e.id, &input, label, Utc::now())
    })
    .await?;
    Ok(Json(e))
}

async fn remove(
    State(state): State<Shared>,
    actor: Actor,
    Path(id): Path<String>,
) -> ApiResult<StatusCode> {
    change(&state, &actor, id, |t, e, label| {
        cal::delete(t, &e.id, label, Utc::now())
    })
    .await?;
    Ok(StatusCode::NO_CONTENT)
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct OccurrenceBody {
    /// The start (RFC 3339), or the local date when one occurrence starts
    /// on it.
    occurrence: String,
}

async fn set_skip(
    state: &Shared,
    actor: &Actor,
    id: String,
    b: OccurrenceBody,
    on: bool,
) -> ApiResult<Event> {
    change(state, actor, id, move |t, e, label| {
        if e.recurrence.is_none() {
            return Err(crate::store::schedules::field_error(
                "occurrence",
                "the event does not repeat; delete it instead",
            ));
        }
        let at = cal::occurrence_of(e, &b.occurrence)?;
        cal::set_skip(t, &e.id, at, on, label, Utc::now())
    })
    .await
}

async fn skip(
    State(state): State<Shared>,
    actor: Actor,
    Path(id): Path<String>,
    ApiJson(b): ApiJson<OccurrenceBody>,
) -> ApiResult<Json<Event>> {
    Ok(Json(set_skip(&state, &actor, id, b, true).await?))
}

async fn unskip(
    State(state): State<Shared>,
    actor: Actor,
    Path(id): Path<String>,
    ApiJson(b): ApiJson<OccurrenceBody>,
) -> ApiResult<Json<Event>> {
    Ok(Json(set_skip(&state, &actor, id, b, false).await?))
}

async fn assign(
    State(state): State<Shared>,
    actor: Actor,
    Path((id, agent)): Path<(String, String)>,
) -> ApiResult<Json<Event>> {
    let e = change(&state, &actor, id, move |t, e, label| {
        cal::set_assignee(t, &e.id, &agent, true, label, Utc::now())
    })
    .await?;
    Ok(Json(e))
}

async fn unassign(
    State(state): State<Shared>,
    actor: Actor,
    Path((id, agent)): Path<(String, String)>,
) -> ApiResult<Json<Event>> {
    let e = change(&state, &actor, id, move |t, e, label| {
        cal::set_assignee(t, &e.id, &agent, false, label, Utc::now())
    })
    .await?;
    Ok(Json(e))
}

// ----------------------------------------------------------- the watches

#[derive(Debug, Default, Deserialize)]
#[serde(deny_unknown_fields)]
struct WatchBody {
    /// `operator`, an agent (ID, `project/name`, or a name), or absent
    /// for the caller.
    #[serde(default)]
    watcher: Option<String>,
}

/// The watcher key (`operator` or an agent ULID) for `who` on `scope`,
/// after the rights check: an actor watches for itself with the read
/// right; the operator Inbox is for the operator; another agent needs a
/// lead of the project (project_admin) for a project calendar, or a
/// sysadmin for the global one, and must be able to read the calendar.
async fn watcher_key(
    state: &Shared,
    actor: &Actor,
    scope: &Scope,
    who: Option<String>,
) -> ApiResult<String> {
    policy::read_calendar(actor, scope.project())?;
    let who = who
        .map(|w| w.trim().to_owned())
        .filter(|w| !w.is_empty() && w != "me");
    let Some(who) = who else {
        return Ok(match actor {
            Actor::Operator => "operator".into(),
            Actor::Agent(a) => a.id.clone(),
        });
    };
    if who == "operator" {
        policy::operator_only(actor, "watching a calendar for the operator Inbox")?;
        return Ok(who);
    }
    let sc = scope.clone();
    let ctx = actor.project().map(str::to_owned);
    let a = state
        .db
        .read(move |c| agents::find(c, &who, ctx.as_deref().or(sc.project())))
        .await?;
    let id = a.agent.id.to_string();
    if actor.agent().is_some_and(|me| me.id == id) {
        return Ok(id);
    }
    match scope.project() {
        Some(p) => {
            policy::admin_project(actor, p, "making another agent watch the calendar")?;
            let sysadmin = a.agent.roles.contains(&terrarium_core::Role::Sysadmin);
            if a.agent.project != p && !sysadmin {
                return Err(ApiError::invalid(format!(
                    "agent `{}/{}` cannot read the calendar of `{p}`",
                    a.agent.project, a.agent.name
                )));
            }
        }
        None => policy::sysadmin(actor, "making another agent watch the global calendar")?,
    }
    Ok(id)
}

/// An empty body watches for the caller.
fn watch_body(body: &[u8]) -> ApiResult<WatchBody> {
    if body.iter().all(u8::is_ascii_whitespace) {
        return Ok(WatchBody::default());
    }
    serde_json::from_slice(body).map_err(|e| ApiError::bad_request(format!("bad body: {e}")))
}

async fn list_watches(state: &Shared, actor: &Actor, scope: Scope) -> ApiResult<Json<Value>> {
    policy::read_calendar(actor, scope.project())?;
    if let Some(p) = scope.project().map(str::to_owned) {
        state.db.read(move |c| projects::get(c, &p)).await?;
    }
    let rows = state.db.read(move |c| cal::watchers(c, &scope)).await?;
    Ok(Json(json!(rows)))
}

async fn watch(
    state: &Shared,
    actor: &Actor,
    scope: Scope,
    b: WatchBody,
    on: bool,
) -> ApiResult<Json<Value>> {
    let key = watcher_key(state, actor, &scope, b.watcher).await?;
    let label = actor.label();
    let (k, sc) = (key.clone(), scope.clone());
    let changed = state
        .db
        .write(actor.label(), move |t| {
            cal::set_watch(t, &sc, &k, on, &label)
        })
        .await?;
    Ok(Json(json!({
        "calendar": scope.label(),
        "watcher": key,
        "watching": on,
        "changed": changed,
    })))
}

async fn project_watches(
    State(state): State<Shared>,
    actor: Actor,
    Path(name): Path<String>,
) -> ApiResult<Json<Value>> {
    list_watches(&state, &actor, Scope::Project(name)).await
}

async fn global_watches(State(state): State<Shared>, actor: Actor) -> ApiResult<Json<Value>> {
    list_watches(&state, &actor, Scope::Global).await
}

async fn project_watch(
    State(state): State<Shared>,
    actor: Actor,
    Path(name): Path<String>,
    body: axum::body::Bytes,
) -> ApiResult<Json<Value>> {
    let b = watch_body(&body)?;
    watch(&state, &actor, Scope::Project(name), b, true).await
}

async fn global_watch(
    State(state): State<Shared>,
    actor: Actor,
    body: axum::body::Bytes,
) -> ApiResult<Json<Value>> {
    let b = watch_body(&body)?;
    watch(&state, &actor, Scope::Global, b, true).await
}

async fn project_unwatch(
    State(state): State<Shared>,
    actor: Actor,
    Path((name, watcher)): Path<(String, String)>,
) -> ApiResult<Json<Value>> {
    let b = WatchBody {
        watcher: Some(watcher),
    };
    watch(&state, &actor, Scope::Project(name), b, false).await
}

async fn global_unwatch(
    State(state): State<Shared>,
    actor: Actor,
    Path(watcher): Path<String>,
) -> ApiResult<Json<Value>> {
    let b = WatchBody {
        watcher: Some(watcher),
    };
    watch(&state, &actor, Scope::Global, b, false).await
}

// ------------------------------------------------------------ the grants

async fn grants(
    State(state): State<Shared>,
    actor: Actor,
    Path(name): Path<String>,
) -> ApiResult<Json<Value>> {
    policy::read_calendar(&actor, Some(&name))?;
    let rows = state
        .db
        .read(move |c| {
            projects::get(c, &name)?;
            cal::grants(c, &name)
        })
        .await?;
    Ok(Json(json!(rows)))
}

/// Set `policy.calendar_write` of an agent of the project: the grant is
/// a field of the agent's policy, so `agent edit --policy` and an agent
/// file set it too. A lead (project_admin), a sysadmin, or the operator.
async fn set_grant(
    state: &Shared,
    actor: &Actor,
    project: String,
    agent: String,
    on: bool,
) -> ApiResult<Json<Value>> {
    policy::admin_project(actor, &project, "granting the calendar write right")?;
    let p = project.clone();
    let a = state
        .db
        .read(move |c| agents::find(c, &agent, Some(&p)))
        .await?;
    if a.agent.project != project {
        return Err(ApiError::invalid(format!(
            "agent `{}/{}` is not in project `{project}`",
            a.agent.project, a.agent.name
        )));
    }
    if a.source == projects::SOURCE_FILE {
        return Err(super::agents::file_managed(&a));
    }
    let id = a.agent.id.to_string();
    let name = a.agent.name.clone();
    let changed = state
        .db
        .write(actor.label(), move |t| {
            let mut pol = agents::get(t, &id)?.agent.policy;
            if pol.calendar_write == on {
                return Ok(false);
            }
            pol.calendar_write = on;
            let patch = agents::AgentPatch {
                policy: Some(pol),
                ..Default::default()
            };
            agents::write_def(t, &id, &patch)?;
            t.audit(
                if on {
                    "calendar.grant"
                } else {
                    "calendar.revoke"
                },
                &id,
                Some(&project),
                json!({"agent": id, "calendar_write": on}),
            )?;
            Ok(true)
        })
        .await?;
    Ok(Json(json!({
        "agent": a.agent.id.to_string(),
        "name": name,
        "project": a.agent.project,
        "calendar_write": on,
        "changed": changed,
    })))
}

async fn grant(
    State(state): State<Shared>,
    actor: Actor,
    Path((name, agent)): Path<(String, String)>,
) -> ApiResult<Json<Value>> {
    set_grant(&state, &actor, name, agent, true).await
}

async fn revoke(
    State(state): State<Shared>,
    actor: Actor,
    Path((name, agent)): Path<(String, String)>,
) -> ApiResult<Json<Value>> {
    set_grant(&state, &actor, name, agent, false).await
}
