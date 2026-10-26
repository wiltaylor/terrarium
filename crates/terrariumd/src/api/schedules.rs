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
use crate::executor::{self, End, RemoteCmd, Stream};
use crate::policy;
use crate::store::mail::Address;
use crate::store::projects::SOURCE_FILE;
use crate::store::schedules::{
    self, Begin, Decision, ListFilter, NewSchedule, Outcome, Reach, Run, Schedule, SchedulePatch,
    field_error,
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
}

/// The bytes of stdout and stderr that an exec run keeps.
const TAIL: usize = 4000;
/// How long a turn action waits for an agent that it started.
const READY_WAIT: Duration = Duration::from_secs(120);

#[derive(Debug, Default, Clone, Deserialize)]
struct ListQuery {
    project: Option<String>,
    enabled: Option<bool>,
    /// `time` (cron, interval, at) or `trigger` (on).
    kind: Option<String>,
}

async fn list(
    State(state): State<Shared>,
    actor: Actor,
    ApiQuery(q): ApiQuery<ListQuery>,
) -> ApiResult<Json<Vec<Schedule>>> {
    let global = policy::read_schedules(&actor, None).is_ok();
    let trigger = match q.kind.as_deref().filter(|k| !k.is_empty()) {
        None => None,
        Some("time") => Some(false),
        Some("trigger") => Some(true),
        Some(other) => {
            return Err(ApiError::bad_request(format!(
                "bad `kind` `{other}`: use time or trigger"
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
                trigger,
            }
        }
        // The operator and a sysadmin see all; an agent sees its project.
        None if global => ListFilter {
            project: None,
            global: true,
            projects: None,
            enabled: q.enabled,
            trigger,
        },
        None => ListFilter {
            project: actor.project().map(str::to_owned),
            global: false,
            projects: None,
            enabled: q.enabled,
            trigger,
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
}

/// The project and the action rights of a schedule write, for `actor`. A
/// trigger also checks its action kind and its escalation address.
fn check_write(
    t: &rusqlite::Connection,
    actor: &Actor,
    project: Option<&str>,
    spec: &ScheduleSpec,
    action: &ScheduleAction,
) -> ApiResult<Reach> {
    policy::schedule_scope(actor, project)?;
    schedules::check_trigger(t, project, spec, action)?;
    let reach = schedules::resolve_reach(t, project, action)?;
    policy::manage_schedule(actor, project, &reach)?;
    if let ScheduleSpec::On {
        escalate: Some(e), ..
    } = spec
    {
        let to = schedules::escalation_reach(t, project, e)?;
        policy::manage_schedule(actor, project, &to)?;
    }
    Ok(reach)
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
    };
    let s = state
        .db
        .write(actor.label(), move |t| {
            check_write(t, &actor, new.project.as_deref(), &new.spec, &new.action)?;
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
    let patch = SchedulePatch {
        name: b.name,
        spec: b.spec.map(parse_spec).transpose()?,
        action: b.action.map(parse_action).transpose()?,
        enabled: b.enabled,
        catch_up: b.catch_up,
        // The caller who changes the action owns it: the runs act as it,
        // so an edit cannot borrow the rights of the schedule's creator.
        run_as: Some(actor.label()),
        source: None,
    };
    let v = state
        .db
        .write(actor.label(), move |t| {
            let action = patch.action.as_ref().unwrap_or(&s.action);
            let spec = patch.spec.as_ref().unwrap_or(&s.spec);
            check_write(t, &actor, s.project.as_deref(), spec, action)?;
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
    if enabled {
        let check = actor.clone();
        let (project, spec, action) = (s.project.clone(), s.spec.clone(), s.action.clone());
        let reach = state
            .db
            .read(move |c| check_write(c, &check, project.as_deref(), &spec, &action))
            .await?;
        caller_may_act(&state, &actor, &s, &reach).await?;
    }
    let v = state
        .db
        .write(actor.label(), move |t| {
            if enabled {
                check_write(t, &actor, s.project.as_deref(), &s.spec, &s.action)?;
            } else {
                policy::schedule_scope(&actor, s.project.as_deref())?;
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
    let check = actor.clone();
    let (project, spec, action) = (s.project.clone(), s.spec.clone(), s.action.clone());
    let reach = state
        .db
        .read(move |c| check_write(c, &check, project.as_deref(), &spec, &action))
        .await?;
    caller_may_act(&state, &actor, &s, &reach).await?;
    Ok(Json(
        run_schedule(&state, &s.id, "manual", actor.label()).await?,
    ))
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

/// The actor of a `run_as` string, as the store knows it now: `None` when
/// the agent is gone.
fn actor_of(conn: &rusqlite::Connection, run_as: &str) -> ApiResult<Option<Actor>> {
    if run_as == "operator" {
        return Ok(Some(Actor::Operator));
    }
    match run_as.strip_prefix("agent:") {
        Some(id) => Ok(crate::store::agents::actor_for(conn, id)?.map(Actor::Agent)),
        None => Ok(None),
    }
}

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

/// Check the rights of the `run_as` actor again, then do the action.
async fn perform(state: &Shared, s: &Schedule, ctx: &RunCtx) -> Outcome {
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
    let (run_as, project) = (s.run_as.clone(), s.project.clone());
    let (checked_action, esc) = (action.clone(), escalate.clone());
    let checked = state
        .db
        .read(move |c| {
            let Some(actor) = actor_of(c, &run_as)? else {
                return Ok(Err(format!(
                    "the schedule runs as `{run_as}`, which no longer exists"
                )));
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
            if let Err(e) = policy::manage_schedule(&actor, project.as_deref(), &reach) {
                return Ok(Err(format!("{} (runs as {run_as})", e.message)));
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
            turn(
                state,
                actor,
                &id,
                &format!("{project}/{name}"),
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
        ) => exec(state, actor, &name, cmd, cwd, timeout_secs).await,
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
    let result = state
        .db
        .write(ctx.label(&actor), move |t| {
            t.cause = cause;
            let from = Address::from_actor(&actor);
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

/// Wait until a started agent is ready for a turn: `idle`, `working`, or
/// `blocked`. False when it stops or the wait ends.
async fn wait_ready(state: &Shared, agent: AgentId) -> bool {
    let end = tokio::time::Instant::now() + READY_WAIT;
    while tokio::time::Instant::now() < end {
        match state.supervisor.status(agent).await {
            Some(AgentStatus::Idle | AgentStatus::Working | AgentStatus::Blocked) => return true,
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
    let body = super::agents::SendBody { text };
    match super::agents::send(
        State(state.clone()),
        actor,
        Path(id.to_owned()),
        ApiJson(body),
    )
    .await
    {
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

async fn exec(
    state: &Shared,
    actor: Actor,
    target: &str,
    cmd: Vec<String>,
    cwd: Option<String>,
    timeout_secs: Option<u64>,
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
    let sess = match executor::start_exec(state, &actor, target, rc).await {
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
            run_as: "operator".into(),
            next_run_at: None,
            last_run: None,
            runs_count: 0,
            source: "api".into(),
            consecutive_skips: 0,
            disabled_reason: None,
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
