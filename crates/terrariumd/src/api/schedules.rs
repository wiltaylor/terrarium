//! `/api/schedules/...` (docs/api.md §19), and the run of one schedule:
//! the rights check of its `run_as` actor, then the action through the
//! normal paths (mail send, agent send and lifecycle, the executor, the
//! quota probe, the board).
//!
//! A trigger run ([`run_triggered`]) fills the `{{event.*}}` templates of
//! its action from the event, and makes its writes with the actor
//! `schedule:<id>` and `data.cause` on each event, so the trigger engine
//! sees which events a trigger made (the loop guard and the chain depth).
//! The rights stay those of `run_as`.
//!
//! A project- or system-owned schedule (`crate::owner`, docs/api.md §19.6)
//! has no actor behind it: each run checks the owner's rights (the
//! project's rules and grants, or the system's), then does the action
//! with the operator's paths. Its writes and its `target.exec` audit rows
//! have the actor `schedule:<id>`; its mail comes from `project:<name>`
//! (project-owned) or `system`.

use std::time::Duration;

use axum::extract::{Path, State};
use axum::http::StatusCode;
use axum::response::Response;
use axum::routing::{get, post};
use axum::{Json, Router};
use base64::Engine;
use chrono::Utc;
use serde::{Deserialize, Deserializer};
use serde_json::{Value, json};
use terrarium_core::schedule::{AgentVerb, CardOp, CatchUp, ScheduleAction, ScheduleSpec};
use terrarium_core::template::{self, EventView, Quote};
use terrarium_core::{AgentId, AgentStatus};

use super::{ApiJson, ApiQuery, Shared, paged};
use crate::actor::Actor;
use crate::error::{ApiError, ApiResult, Code};
use crate::events::StoredEvent;
use crate::executor::{self, End, OnBehalf, RemoteCmd, Stream};
use crate::owner::{self, Owner};
use crate::policy;
use crate::store::mail::Address;
use crate::store::projects::SOURCE_FILE;
use crate::store::schedules::{
    self, Begin, Decision, ListFilter, NewSchedule, Outcome, Reach, Run, Schedule, SchedulePatch,
    SpecGroup, field_error,
};
use crate::store::{Page, board};

pub fn routes() -> Router<Shared> {
    Router::new()
        .route("/schedules", get(list).post(create))
        .route("/schedules/{id}", get(show).patch(update).delete(remove))
        .route("/schedules/{id}/run", post(run_now))
        .route("/schedules/{id}/enable", post(enable))
        .route("/schedules/{id}/disable", post(disable))
        .route("/schedules/{id}/runs", get(runs))
        .route("/schedules/{id}/deliveries", get(deliveries))
        .route(
            "/schedules/{id}/deliveries/{delivery}/redeliver",
            post(redeliver),
        )
        .route("/schedules/{id}/test", post(test_run))
}

/// The bytes of stdout and stderr that an exec run keeps.
const TAIL: usize = 4000;
/// How long a turn action waits for an agent that it started.
const READY_WAIT: Duration = Duration::from_secs(120);

#[derive(Debug, Default, Clone, Deserialize)]
struct ListQuery {
    project: Option<String>,
    enabled: Option<bool>,
    /// `time` (cron, interval, at), `trigger` (on), or `action` (manual).
    kind: Option<String>,
}

async fn list(
    State(state): State<Shared>,
    actor: Actor,
    ApiQuery(q): ApiQuery<ListQuery>,
) -> ApiResult<Json<Vec<Schedule>>> {
    let global = policy::read_schedules(&actor, None).is_ok();
    let group = match q.kind.as_deref().filter(|k| !k.is_empty()) {
        None => None,
        Some("time") => Some(SpecGroup::Time),
        Some("trigger") => Some(SpecGroup::Trigger),
        Some("action") => Some(SpecGroup::Action),
        Some(other) => {
            return Err(ApiError::bad_request(format!(
                "bad `kind` `{other}`: use time, trigger, or action"
            )));
        }
    };
    let f = match q.project.filter(|p| !p.is_empty()) {
        Some(p) => {
            policy::read_schedules(&actor, Some(&p))?;
            ListFilter {
                project: Some(p),
                global,
                projects: None,
                enabled: q.enabled,
                group,
            }
        }
        // The operator and a sysadmin see all; an agent sees its project.
        None if global => ListFilter {
            project: None,
            global: true,
            projects: None,
            enabled: q.enabled,
            group,
        },
        None => ListFilter {
            project: actor.project().map(str::to_owned),
            global: false,
            projects: None,
            enabled: q.enabled,
            group,
        },
    };
    Ok(Json(state.db.read(move |c| schedules::list(c, &f)).await?))
}

/// Find the schedule of the path (a ULID, `project/name`, or a name) and
/// check that the actor may read it.
async fn load(state: &Shared, actor: &Actor, key: String) -> ApiResult<Schedule> {
    let context = actor.project().map(str::to_owned);
    let s = state
        .db
        .read(move |c| schedules::find(c, &key, context.as_deref()))
        .await?;
    policy::read_schedules(actor, s.project.as_deref())?;
    Ok(s)
}

fn file_managed(s: &Schedule) -> ApiError {
    ApiError::new(
        Code::FileManaged,
        format!(
            "schedule `{}` is declared in projects/{}.toml; edit the file, then POST /api/reload",
            s.key(),
            s.project.as_deref().unwrap_or_default()
        ),
    )
}

/// `Some(None)` for an explicit `null`, `None` when the key is absent.
fn explicit<'de, D, T>(d: D) -> Result<Option<Option<T>>, D::Error>
where
    D: Deserializer<'de>,
    T: Deserialize<'de>,
{
    Option::<T>::deserialize(d).map(Some)
}

fn parse_spec(v: Value) -> ApiResult<ScheduleSpec> {
    serde_json::from_value(v).map_err(|e| field_error("spec", e))
}

fn parse_action(v: Value) -> ApiResult<ScheduleAction> {
    serde_json::from_value(v).map_err(|e| field_error("action", e))
}

/// The body of `POST /api/schedules`.
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct NewBody {
    name: String,
    /// Absent: the caller's project (the operator: global). `null`: global.
    #[serde(default, deserialize_with = "explicit")]
    project: Option<Option<String>>,
    spec: Value,
    action: Value,
    #[serde(default)]
    enabled: Option<bool>,
    #[serde(default)]
    catch_up: Option<CatchUp>,
    #[serde(default)]
    max_concurrency: Option<u32>,
    /// `actor` (the default), `project`, or `system`.
    #[serde(default)]
    owner: Option<Owner>,
}

/// The project and the action rights of a schedule write, for `actor`. A
/// trigger also checks its action kind and its escalation address.
fn check_write(
    t: &rusqlite::Connection,
    actor: &Actor,
    owner: Owner,
    project: Option<&str>,
    spec: &ScheduleSpec,
    action: &ScheduleAction,
) -> ApiResult<Reach> {
    policy::schedule_scope(actor, project)?;
    owner::may_manage(actor, owner, project, "managing a schedule")?;
    schedules::check_trigger(t, project, spec, action)?;
    let reach = schedules::resolve_reach(t, project, action)?;
    reach_ok(t, actor, owner, project, &reach)?;
    if let ScheduleAction::Webhook(a) = action {
        if owner.is_owned() {
            // The shape checks; the secrets with the owner's rights.
            crate::webhooks::check_write(t, &Actor::Operator, project, spec, a)?;
            crate::webhooks::check_owned_secrets(t, owner, project, a)?;
        } else {
            crate::webhooks::check_write(t, actor, project, spec, a)?;
        }
    }
    if let ScheduleSpec::On {
        escalate: Some(e), ..
    } = spec
    {
        let to = schedules::escalation_reach(t, project, e)?;
        reach_ok(t, actor, owner, project, &to)?;
    }
    Ok(reach)
}

/// May the action of a schedule of `owner` touch `reach`? An actor-owned
/// schedule: the actor's rules. Another: the owner's.
fn reach_ok(
    t: &rusqlite::Connection,
    actor: &Actor,
    owner: Owner,
    project: Option<&str>,
    reach: &Reach,
) -> ApiResult<()> {
    match owner {
        Owner::Actor => policy::manage_schedule(actor, project, reach),
        o => owner::schedule_reach(t, o, project, reach),
    }
}

async fn create(
    State(state): State<Shared>,
    actor: Actor,
    ApiJson(b): ApiJson<NewBody>,
) -> ApiResult<(StatusCode, Json<Schedule>)> {
    let project = match b.project {
        Some(p) => p.filter(|p| !p.is_empty()),
        None => actor.project().map(str::to_owned),
    };
    policy::schedule_scope(&actor, project.as_deref())?;
    schedules::check_name(&b.name)?;
    let spec = parse_spec(b.spec)?;
    schedules::validate_spec(&spec, Utc::now())?;
    let action = parse_action(b.action)?;
    if b.max_concurrency.is_some_and(|m| m != 1) {
        return Err(field_error(
            "max_concurrency",
            "only 1 is supported: one run at a time",
        ));
    }
    let new = NewSchedule {
        name: b.name,
        project,
        spec,
        action,
        enabled: b.enabled.unwrap_or(true),
        catch_up: b.catch_up.unwrap_or_default(),
        created_by: actor.label(),
        source: crate::store::projects::SOURCE_API.into(),
        owner: b.owner.unwrap_or_default(),
    };
    let s = state
        .db
        .write(actor.label(), move |t| {
            check_write(
                t,
                &actor,
                new.owner,
                new.project.as_deref(),
                &new.spec,
                &new.action,
            )?;
            schedules::create(t, &new, Utc::now())
        })
        .await?;
    Ok((StatusCode::CREATED, Json(s)))
}

async fn show(
    State(state): State<Shared>,
    actor: Actor,
    Path(id): Path<String>,
) -> ApiResult<Json<Schedule>> {
    Ok(Json(load(&state, &actor, id).await?))
}

/// The body of `PATCH /api/schedules/{id}`.
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct PatchBody {
    #[serde(default)]
    name: Option<String>,
    #[serde(default)]
    spec: Option<Value>,
    #[serde(default)]
    action: Option<Value>,
    #[serde(default)]
    enabled: Option<bool>,
    #[serde(default)]
    catch_up: Option<CatchUp>,
    /// A new owner. `actor`: the caller takes the schedule over (the runs
    /// act as it).
    #[serde(default)]
    owner: Option<Owner>,
}

async fn update(
    State(state): State<Shared>,
    actor: Actor,
    Path(id): Path<String>,
    ApiJson(b): ApiJson<PatchBody>,
) -> ApiResult<Json<Schedule>> {
    let s = load(&state, &actor, id).await?;
    if s.source == SOURCE_FILE {
        return Err(file_managed(&s));
    }
    policy::schedule_scope(&actor, s.project.as_deref())?;
    owner::may_manage(&actor, s.owner, s.project.as_deref(), "changing a schedule")?;
    let owner = b.owner.unwrap_or(s.owner);
    let patch = SchedulePatch {
        name: b.name,
        spec: b.spec.map(parse_spec).transpose()?,
        action: b.action.map(parse_action).transpose()?,
        enabled: b.enabled,
        catch_up: b.catch_up,
        // The caller who changes the action owns it: the runs act as it,
        // so an edit cannot borrow the rights of the schedule's creator.
        // So does a caller that gives `owner: actor` (a take-over).
        run_as: Some(actor.label()),
        source: None,
        owner: b.owner,
        take_over: b.owner == Some(Owner::Actor),
    };
    let v = state
        .db
        .write(actor.label(), move |t| {
            let action = patch.action.as_ref().unwrap_or(&s.action);
            let spec = patch.spec.as_ref().unwrap_or(&s.spec);
            check_write(t, &actor, owner, s.project.as_deref(), spec, action)?;
            schedules::update(t, &s.id, &patch, Utc::now())
        })
        .await?;
    Ok(Json(v))
}

async fn remove(
    State(state): State<Shared>,
    actor: Actor,
    Path(id): Path<String>,
) -> ApiResult<StatusCode> {
    let s = load(&state, &actor, id).await?;
    if s.source == SOURCE_FILE {
        return Err(file_managed(&s));
    }
    policy::schedule_scope(&actor, s.project.as_deref())?;
    owner::may_manage(&actor, s.owner, s.project.as_deref(), "deleting a schedule")?;
    state
        .db
        .write(actor.label(), move |t| schedules::delete(t, &s.id))
        .await?;
    Ok(StatusCode::NO_CONTENT)
}

async fn set_enabled(
    state: Shared,
    actor: Actor,
    id: String,
    enabled: bool,
) -> ApiResult<Json<Schedule>> {
    let s = load(&state, &actor, id).await?;
    if s.source == SOURCE_FILE {
        return Err(file_managed(&s));
    }
    owner::may_manage(
        &actor,
        s.owner,
        s.project.as_deref(),
        if enabled {
            "enabling a schedule"
        } else {
            "disabling a schedule"
        },
    )?;
    let mut gone = false;
    if enabled {
        let check = actor.clone();
        let (project, spec, action, owner, run_as) = (
            s.project.clone(),
            s.spec.clone(),
            s.action.clone(),
            s.owner,
            s.run_as.clone(),
        );
        let (reach, missing) = state
            .db
            .read(move |c| {
                let reach = check_write(c, &check, owner, project.as_deref(), &spec, &action)?;
                let missing = owner == Owner::Actor && actor_of(c, &run_as)?.is_none();
                Ok((reach, missing))
            })
            .await?;
        // The `run_as` agent is gone: the caller takes the schedule over.
        gone = missing;
        if !gone {
            caller_may_act(&state, &actor, &s, &reach).await?;
        }
    }
    let v = state
        .db
        .write(actor.label(), move |t| {
            if enabled {
                check_write(t, &actor, s.owner, s.project.as_deref(), &s.spec, &s.action)?;
            } else {
                policy::schedule_scope(&actor, s.project.as_deref())?;
            }
            if gone {
                let patch = SchedulePatch {
                    enabled: Some(true),
                    run_as: Some(actor.label()),
                    take_over: true,
                    ..Default::default()
                };
                return schedules::update(t, &s.id, &patch, Utc::now());
            }
            schedules::set_enabled(t, &s.id, enabled, None, Utc::now())
        })
        .await?;
    Ok(Json(v))
}

async fn enable(
    State(state): State<Shared>,
    actor: Actor,
    Path(id): Path<String>,
) -> ApiResult<Json<Schedule>> {
    set_enabled(state, actor, id, true).await
}

async fn disable(
    State(state): State<Shared>,
    actor: Actor,
    Path(id): Path<String>,
) -> ApiResult<Json<Schedule>> {
    set_enabled(state, actor, id, false).await
}

/// An agent that runs or enables a schedule of another actor must be able
/// to do the action itself: use the target of an exec (its policy or a
/// lease), and mail the recipient under its own mail rules. Else a run-now
/// would borrow the rights of the `run_as` actor.
async fn caller_may_act(
    state: &Shared,
    actor: &Actor,
    s: &Schedule,
    reach: &Reach,
) -> ApiResult<()> {
    if s.owner.is_owned() {
        // The owner's rights act, and the caller manages the owner's items.
        return owner::may_manage(actor, s.owner, s.project.as_deref(), "running a schedule");
    }
    let Actor::Agent(a) = actor else {
        return Ok(());
    };
    if s.run_as == actor.label() {
        return Ok(());
    }
    match reach {
        Reach::Target { name, .. } => executor::usable(state, actor, name).await.map(|_| ()),
        Reach::Mail(to) => policy::mail_to(a, to),
        _ => Ok(()),
    }
}

/// `POST /api/schedules/{id}/run`: run now and answer with the run when
/// it ends. The run acts as the schedule's `run_as`, as a timed run does.
async fn run_now(
    State(state): State<Shared>,
    actor: Actor,
    Path(id): Path<String>,
) -> ApiResult<Json<Run>> {
    let s = load(&state, &actor, id).await?;
    // A disabled project action does not run: its button is off. A
    // disabled time schedule still runs now, as before.
    if s.spec.is_manual() && !s.enabled {
        return Err(ApiError::conflict(format!(
            "action `{}` is disabled; enable it first (terrarium schedule enable {})",
            s.key(),
            s.key()
        )));
    }
    let check = actor.clone();
    let (project, spec, action, owner) =
        (s.project.clone(), s.spec.clone(), s.action.clone(), s.owner);
    let reach = state
        .db
        .read(move |c| check_write(c, &check, owner, project.as_deref(), &spec, &action))
        .await?;
    caller_may_act(&state, &actor, &s, &reach).await?;
    Ok(Json(
        run_schedule(&state, &s.id, "manual", actor.label()).await?,
    ))
}

/// `GET /api/schedules/{id}/deliveries`: the webhook deliveries of a
/// schedule, newest first (docs/api.md §19.5).
async fn deliveries(
    State(state): State<Shared>,
    actor: Actor,
    Path(id): Path<String>,
    ApiQuery(page): ApiQuery<Page>,
) -> ApiResult<Response> {
    let s = load(&state, &actor, id).await?;
    let rows = state
        .db
        .read(move |c| crate::store::webhooks::list(c, &s.id, &page))
        .await?;
    Ok(paged(rows))
}

/// The rights of a redeliver: those of a run-now, on a webhook action.
async fn may_send(state: &Shared, actor: &Actor, s: &Schedule) -> ApiResult<()> {
    if crate::webhooks::action_of(s).is_none() {
        return Err(ApiError::invalid(format!(
            "schedule `{}` runs a {} action, not a webhook",
            s.key(),
            s.action.kind()
        )));
    }
    may_run(state, actor, s).await
}

/// The rights of a run-now or a test run, for any action.
async fn may_run(state: &Shared, actor: &Actor, s: &Schedule) -> ApiResult<()> {
    let check = actor.clone();
    let (project, spec, action, owner) =
        (s.project.clone(), s.spec.clone(), s.action.clone(), s.owner);
    let reach = state
        .db
        .read(move |c| check_write(c, &check, owner, project.as_deref(), &spec, &action))
        .await?;
    caller_may_act(state, actor, s, &reach).await
}

/// `POST /api/schedules/{id}/deliveries/{delivery}/redeliver`: queue a
/// copy of a delivery, due now. `202` with the new delivery.
async fn redeliver(
    State(state): State<Shared>,
    actor: Actor,
    Path((id, delivery)): Path<(String, String)>,
) -> ApiResult<(StatusCode, Json<crate::store::webhooks::Delivery>)> {
    let s = load(&state, &actor, id).await?;
    may_send(&state, &actor, &s).await?;
    let d = state
        .db
        .write(actor.label(), move |t| {
            let d = crate::store::webhooks::get(t, &delivery)?;
            if d.schedule != s.id {
                return Err(ApiError::not_found(format!(
                    "no webhook delivery `{delivery}` of schedule `{}`",
                    s.key()
                )));
            }
            crate::store::webhooks::redeliver(t, &delivery, Utc::now())
        })
        .await?;
    state.webhooks.wake();
    Ok((StatusCode::ACCEPTED, Json(d)))
}

/// The mark of a test run at the start of a mail subject, a turn text,
/// and a card title.
const TEST_TAG: &str = "[test]";

/// The body of `POST /api/schedules/{id}/test`. All of it is optional.
#[derive(Debug, Default, Deserialize)]
#[serde(deny_unknown_fields)]
struct TestBody {
    /// The sample event. Absent: a sample of the trigger's first kind.
    #[serde(default)]
    event: Option<SampleEvent>,
}

/// A sample event for a test run. Each absent field comes from the sample
/// of its kind. Other keys (`seq`, `id`, `at` of an event from the log)
/// are ignored, so an event from `GET /api/events` works as it is.
#[derive(Debug, Default, Deserialize)]
struct SampleEvent {
    #[serde(default)]
    kind: Option<String>,
    #[serde(default)]
    summary: Option<String>,
    #[serde(default)]
    project: Option<String>,
    #[serde(default)]
    agent: Option<String>,
    #[serde(default)]
    actor: Option<String>,
    #[serde(default)]
    data: Option<Value>,
}

/// The answer of `POST /api/schedules/{id}/test`.
#[derive(Debug, serde::Serialize)]
struct TestAnswer {
    /// Always `true`: nothing of a test counts as a run.
    test: bool,
    /// The action kind: `mail`, `turn`, `webhook`, ...
    action: &'static str,
    /// With a given event (a trigger only): the event went through the
    /// scope, the kinds, and each `match` rule. `false`: nothing ran, and
    /// `reason` says why. `null`: no check (the default sample, or a time
    /// schedule).
    matched: Option<bool>,
    #[serde(skip_serializing_if = "Option::is_none")]
    reason: Option<String>,
    /// The sample event (a trigger only).
    #[serde(skip_serializing_if = "Option::is_none")]
    event: Option<terrarium_core::webhook::EventDoc>,
    /// The end of the action: `ok`, `failed`, or `skipped` (the rights).
    /// Absent when nothing ran.
    #[serde(skip_serializing_if = "Option::is_none")]
    status: Option<&'static str>,
    #[serde(skip_serializing_if = "Option::is_none")]
    summary: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    output: Option<Value>,
    #[serde(skip_serializing_if = "Option::is_none")]
    error: Option<String>,
    /// A webhook: the delivery and the request (no secret value).
    #[serde(skip_serializing_if = "Option::is_none")]
    delivery: Option<crate::store::webhooks::Delivery>,
    #[serde(skip_serializing_if = "Option::is_none")]
    request: Option<crate::webhooks::Preview>,
}

/// The sample event of a test: the given fields over the sample of the
/// kind (the given kind, else the trigger's first kind).
fn sample_event(
    s: &Schedule,
    given: Option<SampleEvent>,
    now: chrono::DateTime<Utc>,
) -> ApiResult<terrarium_core::webhook::EventDoc> {
    let given = given.unwrap_or_default();
    let mut e = match given.kind.as_deref().map(str::trim) {
        Some("") => {
            return Err(crate::store::schedules::field_error(
                "event.kind",
                "use an event kind, for example monitor.status",
            ));
        }
        Some(k) => terrarium_core::webhook::sample_event(
            k,
            s.project.as_deref(),
            &crate::store::schedules::stamp(now),
        ),
        None => crate::webhooks::sample_for(&s.spec, s.project.as_deref(), now),
    };
    if let Some(v) = given.summary {
        e.summary = v;
    }
    if let Some(v) = given.project {
        e.project = Some(v).filter(|p| !p.is_empty());
    }
    if let Some(v) = given.agent {
        e.agent = Some(v);
    }
    if let Some(v) = given.actor {
        e.actor = Some(v);
    }
    if let Some(v) = given.data {
        if !v.is_object() {
            return Err(crate::store::schedules::field_error(
                "event.data",
                "use a JSON object",
            ));
        }
        e.data = v;
    }
    Ok(e)
}

/// Why a trigger would not run for the sample event: the scope, the
/// kinds, or the first `match` rule that does not hold. `None`: it
/// matches. The loop guard, the dedupe, the escalation, and the rate
/// limit do not apply to a test.
fn test_mismatch(s: &Schedule, e: &StoredEvent) -> Option<String> {
    let ScheduleSpec::On { kinds, matches, .. } = &s.spec else {
        return None;
    };
    if let Some(p) = &s.project
        && e.project.as_deref() != Some(p.as_str())
    {
        return Some(format!(
            "the trigger sees only the events of project {p}; the event's project is {}",
            e.project.as_deref().unwrap_or("none")
        ));
    }
    if !kinds
        .iter()
        .any(|k| terrarium_core::schedule::glob(k, &e.kind))
    {
        return Some(format!(
            "the kind `{}` is not one of the trigger's kinds ({})",
            e.kind,
            kinds.join(", ")
        ));
    }
    let view = schedules::event_view(e);
    matches.iter().find(|m| !m.holds(&view)).map(|m| {
        let value = match &m.value {
            Value::String(v) => v.clone(),
            other => other.to_string(),
        };
        format!(
            "the match rule `{} {} {value}` does not hold for the event",
            m.path,
            m.op.as_str()
        )
    })
}

/// The event of the log that a test run renders its templates from.
fn stored_of(e: &terrarium_core::webhook::EventDoc) -> StoredEvent {
    StoredEvent {
        seq: 0,
        id: String::new(),
        at: e.at.clone(),
        kind: e.kind.clone(),
        actor: e.actor.clone(),
        agent: e.agent.clone(),
        agent_id: None,
        project: e.project.clone(),
        card: None,
        session: None,
        link: Default::default(),
        summary: e.summary.clone(),
        data: e.data.clone(),
    }
}

/// `POST /api/schedules/{id}/test` (docs/api.md §19.7): run the action once
/// as a test, for a sample event. With an event in the body, a trigger
/// checks it against its scope, kinds, and `match` rules first; no match
/// runs nothing. A test
/// writes no run, changes no counter (no dedupe, no escalation, no rate
/// limit, no failure count), and writes the audit row `schedule.test`.
/// A mail subject and a turn text start with `[test]`; a card action uses
/// the key `test:<key>`; a webhook delivery has `test: true`.
async fn test_run(
    State(state): State<Shared>,
    actor: Actor,
    Path(id): Path<String>,
    body: axum::body::Bytes,
) -> ApiResult<Json<TestAnswer>> {
    let b: TestBody = if body.iter().all(u8::is_ascii_whitespace) {
        TestBody::default()
    } else {
        serde_json::from_slice(&body).map_err(|e| ApiError::bad_request(e.to_string()))?
    };
    let s = load(&state, &actor, id).await?;
    may_run(&state, &actor, &s).await?;
    let is_trigger = s.spec.is_trigger();
    // The match decides only for an event that the caller gives: the
    // default sample is generic, and a test of it runs the action.
    let check = is_trigger && b.event.is_some();
    let doc = sample_event(&s, b.event, Utc::now())?;
    let stored = stored_of(&doc);
    let mut answer = TestAnswer {
        test: true,
        action: s.action.kind(),
        matched: None,
        reason: None,
        event: is_trigger.then(|| doc.clone()),
        status: None,
        summary: None,
        output: None,
        error: None,
        delivery: None,
        request: None,
    };
    let mismatch = if check {
        test_mismatch(&s, &stored)
    } else {
        None
    };
    if check {
        answer.matched = Some(mismatch.is_none());
    }
    if let Some(why) = mismatch {
        answer.reason = Some(why);
    } else if crate::webhooks::action_of(&s).is_some() {
        let r = crate::webhooks::test_send(&state, &s, &actor, &doc).await?;
        answer.status = Some(if r.delivery.status == "ok" {
            "ok"
        } else {
            "failed"
        });
        answer.summary = Some(format!(
            "webhook delivery {}: {}",
            r.delivery.id, r.delivery.status
        ));
        answer.delivery = Some(r.delivery);
        answer.request = Some(r.request);
    } else {
        let label = schedules::run_label(&s.id);
        let ctx = RunCtx {
            label: Some(label),
            cause: Some(json!({"schedule": s.id, "event": null, "depth": 1, "test": true})),
            event: is_trigger.then_some(stored),
            escalate: None,
            test: true,
            // `perform` sets `from` and `holder` for a project- or
            // system-owned schedule.
            ..Default::default()
        };
        let (st, s2) = (state.clone(), s.clone());
        let o = match tokio::spawn(async move { perform(&st, &s2, &ctx).await }).await {
            Ok(o) => o,
            Err(e) => Outcome::failed("failed: the test task ended", e.to_string(), json!({})),
        };
        answer.status = Some(o.status);
        answer.summary = Some(o.summary);
        answer.output = Some(o.output);
        answer.error = o.error;
    }
    let audit = json!({
        "action": answer.action,
        "matched": answer.matched,
        "reason": answer.reason,
        "event_kind": is_trigger.then(|| doc.kind.clone()),
        "status": answer.status,
        "summary": answer.summary,
    });
    let (sid, project) = (s.id.clone(), s.project.clone());
    state
        .db
        .write(actor.label(), move |t| {
            t.audit("schedule.test", &sid, project.as_deref(), audit)
        })
        .await?;
    Ok(Json(answer))
}

async fn runs(
    State(state): State<Shared>,
    actor: Actor,
    Path(id): Path<String>,
    ApiQuery(page): ApiQuery<Page>,
) -> ApiResult<Response> {
    let s = load(&state, &actor, id).await?;
    let rows = state
        .db
        .read(move |c| schedules::list_runs(c, &s.id, &page))
        .await?;
    Ok(paged(rows))
}

// ---------------------------------------------------------------- the run

use crate::webhooks::actor_of;

/// The context of one run: the event of a trigger run, and the label and
/// cause of its writes.
#[derive(Debug, Clone, Default)]
struct RunCtx {
    /// The actor of the run's writes. `None`: the `run_as` label.
    label: Option<String>,
    /// `data.cause` of the run's events.
    cause: Option<Value>,
    /// The event of a trigger run.
    event: Option<StoredEvent>,
    /// The escalation of a trigger run: the matches in a row.
    escalate: Option<i64>,
    /// The sender of the run's mail. `None`: the actor.
    from: Option<Address>,
    /// The resource budget of an exec (`<project>/schedules`). `None`: the
    /// actor's.
    holder: Option<String>,
    /// A test run (`POST /api/schedules/{id}/test`): a mail subject and a
    /// turn text start with `[test]`, and a card action uses the key
    /// `test:<key>`.
    test: bool,
}

impl RunCtx {
    fn label(&self, actor: &Actor) -> String {
        self.label.clone().unwrap_or_else(|| actor.label())
    }
}

/// Run schedule `id` once. `trigger` is `cron`, `manual`, or `catch_up`;
/// `by` labels the run's events and audit row (the `run_as` for a timed
/// run, the caller for a run-now). A run while another run of the
/// schedule is still running is `skipped`. Returns the run when it ends.
pub async fn run_schedule(
    state: &Shared,
    id: &str,
    trigger: &'static str,
    by: String,
) -> ApiResult<Run> {
    run_with(state, id, trigger, by.clone(), by, RunCtx::default()).await
}

/// Run trigger `id` for `event` (the trigger engine, `crate::scheduler`):
/// its action with the templates filled from the event, or, for
/// [`Decision::Escalate`], one mail to the escalation address. The run
/// has the trigger `event` and the event's `seq`; its writes have the
/// actor `schedule:<id>` and `data.cause`.
pub async fn run_triggered(
    state: &Shared,
    id: &str,
    event: StoredEvent,
    decision: Decision,
) -> ApiResult<Run> {
    let label = schedules::run_label(id);
    let by = event.actor.clone().unwrap_or_else(|| "system".into());
    let ctx = RunCtx {
        label: Some(label.clone()),
        cause: Some(schedules::cause(id, &event)),
        escalate: match decision {
            Decision::Escalate(n) => Some(n),
            _ => None,
        },
        event: Some(event),
        ..Default::default()
    };
    run_with(state, id, "event", label, by, ctx).await
}

async fn run_with(
    state: &Shared,
    id: &str,
    trigger: &'static str,
    label: String,
    by: String,
    mut ctx: RunCtx,
) -> ApiResult<Run> {
    let key = id.to_owned();
    let seq = ctx.event.as_ref().map(|e| e.seq);
    let cause = ctx.cause.clone();
    let begin = state
        .db
        .write(label.clone(), move |t| {
            t.cause = cause;
            schedules::begin_run(t, &key, trigger, &by, seq, Utc::now())
        })
        .await?;
    let (run, s) = match begin {
        Begin::Overlap(r) => return Ok(r),
        Begin::Started(r, s) => (r, s),
    };
    if let Some(Value::Object(c)) = &mut ctx.cause {
        c.insert("run".into(), json!(run.id));
    }
    let cause = ctx.cause.clone();
    // The action runs in its own task: a panic in it fails the run, and
    // the run does not stay `running`.
    let st = state.clone();
    let outcome = match tokio::spawn(async move { perform(&st, &s, &ctx).await }).await {
        Ok(o) => o,
        Err(e) => Outcome::failed("failed: the run task ended", e.to_string(), json!({})),
    };
    let run_id = run.id.clone();
    state
        .db
        .write(label, move |t| {
            t.cause = cause;
            schedules::finish_run(t, &run_id, &outcome, Utc::now())
        })
        .await
}

/// The action of a trigger with its templates filled from the event. Text
/// that an agent reads takes each value as quoted data, and a mail body or
/// a card text ends with the event as a fenced block; an exec argv
/// element and a key take the value as it is. A time schedule's action
/// stays as it is.
fn render_action(s: &Schedule, event: Option<&EventView<'_>>) -> ScheduleAction {
    if !s.spec.is_trigger() {
        return s.action.clone();
    }
    let data = |t: &str| template::render(t, event, Quote::Data);
    let with_block = |text: String| match event {
        Some(e) if text.trim().is_empty() => template::event_block(e),
        Some(e) => format!("{text}\n\n{}", template::event_block(e)),
        None => text,
    };
    match &s.action {
        ScheduleAction::Mail {
            to,
            subject,
            body,
            thread,
        } => ScheduleAction::Mail {
            to: to.clone(),
            subject: data(subject),
            body: with_block(data(body)),
            thread: thread.clone(),
        },
        ScheduleAction::Exec {
            target,
            cmd,
            cwd,
            timeout_secs,
        } => ScheduleAction::Exec {
            target: target.clone(),
            cmd: cmd
                .iter()
                .map(|c| template::render(c, event, Quote::Plain))
                .collect(),
            cwd: cwd.clone(),
            timeout_secs: *timeout_secs,
        },
        ScheduleAction::Card {
            op,
            key,
            title,
            body,
            column,
        } => ScheduleAction::Card {
            op: *op,
            key: template::render(key, event, Quote::Plain),
            title: title.as_deref().map(data),
            body: Some(with_block(body.as_deref().map(data).unwrap_or_default())),
            column: column.clone(),
        },
        other => other.clone(),
    }
}

/// Check the rights of the owner again (the `run_as` actor, or the
/// project's or the system's), then do the action.
async fn perform(state: &Shared, s: &Schedule, ctx: &RunCtx) -> Outcome {
    // A project- or system-owned schedule acts with the operator's paths
    // after the owner's check below; its writes show the schedule.
    let owned_ctx;
    let ctx = if s.owner.is_owned() {
        let mut c = ctx.clone();
        c.label.get_or_insert_with(|| schedules::run_label(&s.id));
        c.from = Some(match (&s.owner, &s.project) {
            (Owner::Project, Some(p)) => Address::Project(p.clone()),
            _ => Address::System,
        });
        c.holder = Some(format!(
            "{}/schedules",
            s.project.as_deref().unwrap_or("global")
        ));
        owned_ctx = c;
        &owned_ctx
    } else {
        ctx
    };
    let view = ctx.event.as_ref().map(schedules::event_view);
    let action = render_action(s, view.as_ref());
    let escalate = match (&s.spec, ctx.escalate) {
        (
            ScheduleSpec::On {
                escalate: Some(e), ..
            },
            Some(n),
        ) => Some((e.clone(), n)),
        _ => None,
    };
    let (run_as, project, owner) = (s.run_as.clone(), s.project.clone(), s.owner);
    let (checked_action, esc) = (action.clone(), escalate.clone());
    let checked = state
        .db
        .read(move |c| {
            let actor = match owner {
                Owner::Actor => match actor_of(c, &run_as)? {
                    Some(a) => a,
                    None => {
                        return Ok(Err(format!(
                            "the schedule runs as `{run_as}`, which no longer exists; take it \
                             over with an edit (--owner actor or --owner project)"
                        )));
                    }
                },
                _ => Actor::Operator,
            };
            let reach = match &esc {
                Some((e, _)) => schedules::escalation_reach(c, project.as_deref(), e),
                None => schedules::resolve_reach(c, project.as_deref(), &checked_action),
            };
            let reach = match reach {
                Ok(r) => r,
                Err(e) if matches!(e.code, Code::Invalid | Code::NotFound) => {
                    return Ok(Err(e.message));
                }
                Err(e) => return Err(e),
            };
            match owner {
                Owner::Actor => {
                    if let Err(e) = policy::manage_schedule(&actor, project.as_deref(), &reach) {
                        return Ok(Err(format!("{} (runs as {run_as})", e.message)));
                    }
                }
                o => {
                    if let Err(e) = owner::schedule_reach(c, o, project.as_deref(), &reach) {
                        return Ok(Err(format!("{} (owner {})", e.message, o.as_str())));
                    }
                }
            }
            Ok(Ok((actor, reach)))
        })
        .await;
    match (checked, escalate) {
        (Err(e), _) => Outcome::failed(format!("failed: {}", e.message), e.message, json!({})),
        (Ok(Err(reason)), _) => Outcome::refused(reason),
        (Ok(Ok((actor, Reach::Mail(to)))), Some((e, n))) => {
            let key = template::render(&e.key, view.as_ref(), Quote::Data);
            let resets = if e.reset_on.is_empty() {
                "an enable of the trigger".to_owned()
            } else {
                format!("an event of {}", e.reset_on.join(", "))
            };
            let body = format!(
                "Trigger {} matched {n} events in a row for the key {key}. It does not run \
                 its action for this key again until {resets} with the same key comes.\n\n{}",
                s.key(),
                view.as_ref().map(template::event_block).unwrap_or_default()
            );
            let subject = format!("Trigger {} escalated", s.key());
            let mut o = mail(state, actor, to, subject, body, None, ctx).await;
            if o.status == "ok" {
                o.summary = format!("escalated after {n} in a row: {}", o.summary);
            }
            o
        }
        (Ok(Ok((actor, reach))), _) => execute(state, s, action, actor, reach, ctx).await,
    }
}

fn failed(e: &ApiError, output: Value) -> Outcome {
    Outcome::failed(format!("failed: {}", e.message), e.message.clone(), output)
}

async fn execute(
    state: &Shared,
    s: &Schedule,
    action: ScheduleAction,
    actor: Actor,
    reach: Reach,
    ctx: &RunCtx,
) -> Outcome {
    match (action, reach) {
        (
            ScheduleAction::Mail {
                subject,
                body,
                thread,
                ..
            },
            Reach::Mail(to),
        ) => {
            let subject = if subject.trim().is_empty() {
                format!("schedule {}", s.name)
            } else {
                subject
            };
            let (subject, body) = if ctx.test {
                (
                    format!("{TEST_TAG} {subject}"),
                    format!(
                        "This is a test run of {} (`terrarium schedule test`).\n\n{body}",
                        s.key()
                    ),
                )
            } else {
                (subject, body)
            };
            mail(state, actor, to, subject, body, thread, ctx).await
        }
        (
            ScheduleAction::Turn {
                text,
                start_if_stopped,
                ..
            },
            Reach::Agent { id, name, project },
        ) => {
            let text = if ctx.test {
                format!("{TEST_TAG} {text}")
            } else {
                text
            };
            turn(
                state,
                actor,
                &id,
                &format!("{project}/{name}"),
                &s.name,
                text,
                start_if_stopped,
            )
            .await
        }
        (ScheduleAction::Agent { verb, .. }, Reach::Agent { id, name, project }) => {
            agent_verb(state, actor, &id, &format!("{project}/{name}"), verb).await
        }
        (
            ScheduleAction::Exec {
                cmd,
                cwd,
                timeout_secs,
                ..
            },
            Reach::Target { name, .. },
        ) => exec(state, s, actor, &name, cmd, cwd, timeout_secs, ctx).await,
        (ScheduleAction::Probe { .. }, Reach::Subscription { name, .. }) => {
            probe(state, &name).await
        }
        (
            ScheduleAction::Card {
                op,
                key,
                title,
                body,
                column,
            },
            Reach::Card { project },
        ) => {
            let title = title
                .filter(|t| !t.trim().is_empty())
                .unwrap_or_else(|| format!("{}: {}", s.name, template::quote(&key)));
            // A test card has its own key, so a real event never comments
            // on it, and a test close finds no real card.
            let (key, title) = if ctx.test {
                (format!("test:{key}"), format!("{TEST_TAG} {title}"))
            } else {
                (key, title)
            };
            let card = CardWork {
                project,
                op,
                key,
                title,
                body: body.unwrap_or_default(),
                column,
            };
            card_action(state, actor, card, ctx).await
        }
        (ScheduleAction::Webhook(a), Reach::Webhook { .. }) => {
            crate::webhooks::queue(state, s, &a, ctx.event.as_ref(), ctx.cause.clone()).await
        }
        _ => Outcome::failed(
            "failed: the action and its target do not match",
            "internal: the action and its reach do not match",
            json!({}),
        ),
    }
}

async fn mail(
    state: &Shared,
    actor: Actor,
    to: Address,
    subject: String,
    body: String,
    thread: Option<String>,
    ctx: &RunCtx,
) -> Outcome {
    let cause = ctx.cause.clone();
    let from = ctx
        .from
        .clone()
        .unwrap_or_else(|| Address::from_actor(&actor));
    let result = state
        .db
        .write(ctx.label(&actor), move |t| {
            t.cause = cause;
            super::mail::send_checked(t, &actor, &from, &to, &subject, &body, thread.as_deref())
        })
        .await;
    match result {
        Ok(m) => {
            let mut summary = format!("mail {} to {}", m.id, m.to_name);
            if m.held {
                summary.push_str(" (held by the loop guard: no pointer)");
            }
            Outcome::ok(
                summary,
                json!({"message_id": m.id, "thread_id": m.thread_id, "to": m.to_name,
                       "held": m.held, "held_reason": m.held_reason}),
            )
        }
        Err(e) => failed(&e, json!({})),
    }
}

/// The rendered fields of a `card` action.
struct CardWork {
    project: String,
    op: CardOp,
    key: String,
    title: String,
    body: String,
    column: Option<String>,
}

/// A `card` action, as `run_as`: `open` makes a card with the label
/// `trigger:<key>`, or comments on the open card with that label; `close`
/// moves the open card to `done` (or `column`), with the body as the
/// comment. No open card to close is `ok`.
async fn card_action(state: &Shared, actor: Actor, w: CardWork, ctx: &RunCtx) -> Outcome {
    let cause = ctx.cause.clone();
    let result = state
        .db
        .write(ctx.label(&actor), move |t| {
            t.cause = cause;
            let label = board::trigger_label(&w.key);
            let open = board::find_open_by_label(t, &w.project, &label)?;
            let body = w.body.trim().to_owned();
            match (w.op, open) {
                (CardOp::Open, Some(id)) => {
                    let text = if body.is_empty() {
                        format!("The trigger matched again: {}", template::quote(&w.key))
                    } else {
                        body
                    };
                    board::add_comment(t, &actor, &id, &text)?;
                    Ok((
                        format!("comment on {id}"),
                        json!({"card": id, "op": "comment"}),
                    ))
                }
                (CardOp::Open, None) => {
                    let new = board::NewCard {
                        title: Some(w.title),
                        body: Some(body),
                        column: w.column,
                        labels: vec![label],
                        ..Default::default()
                    };
                    let c = board::create_card(t, &actor, &w.project, new)?;
                    Ok((
                        format!("opened {}", c.id),
                        json!({"card": c.id, "op": "open"}),
                    ))
                }
                (CardOp::Close, Some(id)) => {
                    // find_open_by_label skips held cards. Check again, so
                    // a schedule that runs as the operator never moves a
                    // card that the GitHub author gate holds (TER-85).
                    if board::is_held(t, &id)? {
                        return Err(ApiError::forbidden(format!(
                            "{id} is held by the GitHub author gate; a schedule does not move it"
                        )));
                    }
                    let to = w.column.as_deref().unwrap_or(board::DONE);
                    let comment = Some(body.as_str()).filter(|b| !b.is_empty());
                    let c = board::move_card(t, &actor, &id, to, None, comment)?;
                    Ok((
                        format!("closed {} ({})", c.id, c.column),
                        json!({"card": c.id, "op": "close", "column": c.column}),
                    ))
                }
                (CardOp::Close, None) => Ok((
                    format!("no open card for {}", template::quote(&w.key)),
                    json!({"card": null, "op": "close"}),
                )),
            }
        })
        .await;
    match result {
        Ok((summary, output)) => Outcome::ok(summary, output),
        Err(e) => failed(&e, json!({})),
    }
}

/// Wait until a started agent is ready for a turn: `idle`, `waiting`,
/// `working`, or `blocked`. False when it stops or the wait ends.
async fn wait_ready(state: &Shared, agent: AgentId) -> bool {
    let end = tokio::time::Instant::now() + READY_WAIT;
    while tokio::time::Instant::now() < end {
        match state.supervisor.status(agent).await {
            Some(
                AgentStatus::Idle
                | AgentStatus::Waiting
                | AgentStatus::Working
                | AgentStatus::Blocked,
            ) => return true,
            Some(AgentStatus::Error | AgentStatus::Stopped) => return false,
            _ => tokio::time::sleep(Duration::from_millis(50)).await,
        }
    }
    false
}

async fn turn(
    state: &Shared,
    actor: Actor,
    id: &str,
    who: &str,
    schedule: &str,
    text: String,
    start_if_stopped: bool,
) -> Outcome {
    let Ok(agent) = id.parse::<AgentId>() else {
        return Outcome::failed(
            "failed: bad agent ID",
            format!("bad agent ID `{id}`"),
            json!({}),
        );
    };
    let before = state.supervisor.status(agent).await;
    let live = before.is_some_and(AgentStatus::is_live);
    let mut started = false;
    if !live {
        if !start_if_stopped {
            return Outcome::skipped("agent not running");
        }
        if let Err(e) =
            super::agents::start(State(state.clone()), actor.clone(), Path(id.to_owned())).await
        {
            return failed(&e, json!({"agent": who, "started": false}));
        }
        started = true;
        if !wait_ready(state, agent).await {
            return Outcome::failed(
                format!("failed: {who} did not become ready"),
                format!("agent {who} did not become ready for a turn"),
                json!({"agent": who, "started": true}),
            );
        }
    }
    let body = super::agents::SendBody { text, raw: false };
    // The turn starts with `[System:schedule <name>]` (PLAN §8.5).
    let source = terrarium_core::turn_tag::TurnSource::System {
        kind: terrarium_core::turn_tag::SystemKind::Schedule,
        detail: Some(schedule.to_owned()),
    };
    match super::agents::send_as(state, actor, id.to_owned(), body, &source).await {
        Ok(_) => Outcome::ok(
            if started {
                format!("started {who} and sent the turn")
            } else {
                format!("turn sent to {who}")
            },
            json!({"agent": who, "started": started, "delivery": "accepted",
                   "status_before": before}),
        ),
        Err(e) => failed(&e, json!({"agent": who, "started": started})),
    }
}

async fn agent_verb(state: &Shared, actor: Actor, id: &str, who: &str, verb: AgentVerb) -> Outcome {
    let (st, path) = (State(state.clone()), Path(id.to_owned()));
    let result = match verb {
        AgentVerb::Start => super::agents::start(st, actor, path).await,
        AgentVerb::Stop => super::agents::stop(st, actor, path).await,
        AgentVerb::Restart => super::agents::restart(st, actor, path).await,
    };
    match result {
        Ok((_, Json(v))) => {
            // The supervisor knows the new status before the store does.
            let status = state
                .supervisor
                .status(v.agent.id)
                .await
                .unwrap_or(v.agent.status);
            Outcome::ok(
                format!("{} {who}: {status}", verb.as_str()),
                json!({"agent": who, "verb": verb, "status": status}),
            )
        }
        Err(e) => failed(&e, json!({"agent": who, "verb": verb})),
    }
}

/// The last `TAIL` bytes of `buf`, as text.
fn tail(buf: &[u8]) -> String {
    let start = buf.len().saturating_sub(TAIL);
    String::from_utf8_lossy(&buf[start..]).into_owned()
}

fn keep_tail(buf: &mut Vec<u8>, more: &[u8]) {
    buf.extend_from_slice(more);
    if buf.len() > 2 * TAIL {
        let cut = buf.len() - TAIL;
        buf.drain(..cut);
    }
}

#[allow(clippy::too_many_arguments)]
async fn exec(
    state: &Shared,
    s: &Schedule,
    actor: Actor,
    target: &str,
    cmd: Vec<String>,
    cwd: Option<String>,
    timeout_secs: Option<u64>,
    ctx: &RunCtx,
) -> Outcome {
    let limit = timeout_secs
        .unwrap_or(schedules::DEFAULT_EXEC_TIMEOUT)
        .min(schedules::MAX_EXEC_TIMEOUT);
    let shown = cmd.join(" ");
    let rc = RemoteCmd {
        argv: cmd,
        cwd,
        timeout_secs: Some(limit),
        ..Default::default()
    };
    let mut detail = serde_json::Map::new();
    detail.insert("schedule".into(), json!(s.id));
    detail.insert("owner".into(), json!(s.owner.as_str()));
    if !s.owner.is_owned() {
        detail.insert("run_as".into(), json!(s.run_as));
    }
    let on = OnBehalf {
        label: ctx.label.clone(),
        holder: ctx.holder.clone(),
        detail,
    };
    let sess = match executor::start_exec_as(state, &actor, target, rc, Some(on)).await {
        Ok(s) => s,
        Err(e) => return failed(&e, json!({"target": target, "cmd": shown})),
    };
    let deadline = tokio::time::Instant::now() + Duration::from_secs(limit + 15);
    let (mut out, mut err) = (Vec::new(), Vec::new());
    let mut since = 0u64;
    let end: Option<End> = loop {
        if tokio::time::Instant::now() >= deadline {
            sess.kill();
            break None;
        }
        match sess.read(since, Duration::from_secs(2), 1 << 20).await {
            Err(_) => break None,
            Ok(o) => {
                for c in &o.chunks {
                    let bytes = base64::engine::general_purpose::STANDARD
                        .decode(&c.data)
                        .unwrap_or_default();
                    match c.stream {
                        Stream::Stderr => keep_tail(&mut err, &bytes),
                        _ => keep_tail(&mut out, &bytes),
                    }
                }
                since = o.next;
                if o.done {
                    break o.end;
                }
            }
        }
    };
    let _ = state.executor.close(&actor, target, &sess.id);
    let code = match &end {
        Some(End::Exit { code, .. }) => *code,
        _ => None,
    };
    let output = json!({"target": target, "cmd": shown, "exit_code": code, "end": end,
                        "stdout": tail(&out), "stderr": tail(&err)});
    match end {
        Some(End::Exit { code: Some(0), .. }) => {
            Outcome::ok(format!("exec on {target}: exit 0"), output)
        }
        Some(End::Exit {
            code,
            signal,
            reason,
        }) => {
            let what = match (code, signal) {
                (Some(c), _) => format!("exit code {c}"),
                (None, Some(s)) => format!("signal {s}"),
                _ => reason,
            };
            Outcome::failed(format!("exec on {target}: {what}"), what, output)
        }
        Some(End::Error { code, message }) => Outcome::failed(
            format!("exec on {target}: {message}"),
            format!("{code}: {message}"),
            output,
        ),
        Some(End::Cp { .. }) | None => Outcome::failed(
            format!("exec on {target}: timed out after {limit} s"),
            format!("timed out after {limit} s"),
            output,
        ),
    }
}

async fn probe(state: &Shared, name: &str) -> Outcome {
    match crate::probes::run_subscription(state, name).await {
        Ok(o) => {
            let windows: Vec<String> = o
                .windows
                .iter()
                .map(|w| format!("{} {:.0}%", w.window, w.used_pct))
                .collect();
            let output = serde_json::to_value(&o).unwrap_or(Value::Null);
            if o.probe_status == "failed" {
                let e = o.error.clone().unwrap_or_else(|| "the probe failed".into());
                Outcome::failed(format!("probe {name}: {e}"), e, output)
            } else {
                Outcome::ok(
                    format!("probe {name}: {} ({})", o.probe_status, windows.join(", ")),
                    output,
                )
            }
        }
        Err(e) => failed(&e, json!({"subscription": name})),
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::*;

    fn trigger(action: ScheduleAction) -> Schedule {
        Schedule {
            id: "01MA0V7N6Q1Y3K2P4R5S6T7U8V".into(),
            name: "ci".into(),
            project: Some("terra".into()),
            enabled: true,
            spec: serde_json::from_value(json!({"kind": "on", "kinds": ["ci.*"]})).unwrap(),
            action,
            catch_up: CatchUp::Skip,
            max_concurrency: 1,
            created_by: "operator".into(),
            owner: Owner::Actor,
            run_as: "operator".into(),
            next_run_at: None,
            last_run: None,
            runs_count: 0,
            source: "api".into(),
            consecutive_skips: 0,
            disabled_reason: None,
            webhook_failures: 0,
            created_at: String::new(),
            updated_at: String::new(),
            anchor_at: String::new(),
        }
    }

    #[test]
    fn an_exec_argv_takes_each_value_as_one_argument() {
        let data = json!({"branch": "main; rm -rf / #", "repo": "wil/terrarium"});
        let e = EventView {
            seq: 5,
            kind: "ci.run.failed",
            summary: "CI failed",
            project: Some("terra"),
            actor: None,
            data: &data,
        };
        let s = trigger(ScheduleAction::Exec {
            target: "box".into(),
            cmd: vec![
                "./fix.sh".into(),
                "--branch={{event.data.branch}}".into(),
                "{{event.data.repo}}".into(),
                "{{event.data.missing}}".into(),
            ],
            cwd: None,
            timeout_secs: None,
        });
        let ScheduleAction::Exec { cmd, .. } = render_action(&s, Some(&e)) else {
            panic!("not exec")
        };
        assert_eq!(
            cmd,
            ["./fix.sh", "--branch=main; rm -rf / #", "wil/terrarium", ""]
        );
        // A time schedule keeps its text as it is.
        let mut t = s.clone();
        t.spec = ScheduleSpec::Interval { every: "1h".into() };
        assert_eq!(render_action(&t, Some(&e)), t.action);
        // A mail body quotes the values and ends with the event block.
        let m = trigger(ScheduleAction::Mail {
            to: "worker".into(),
            subject: "{{event.kind}}".into(),
            body: "on {{event.data.branch}}".into(),
            thread: None,
        });
        let ScheduleAction::Mail { subject, body, .. } = render_action(&m, Some(&e)) else {
            panic!("not mail")
        };
        assert_eq!(subject, "`ci.run.failed`");
        assert!(
            body.starts_with("on `main; rm -rf / #`\n\nThe event"),
            "{body}"
        );
    }
}
