//! `/api/schedules/...` (docs/api.md §19), and the run of one schedule:
//! the rights check of its `run_as` actor, then the action through the
//! normal paths (mail send, agent send and lifecycle, the executor, the
//! quota probe).

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
use terrarium_core::schedule::{AgentVerb, CatchUp, ScheduleAction, ScheduleSpec};
use terrarium_core::{AgentId, AgentStatus};

use super::{ApiJson, ApiQuery, Shared, paged};
use crate::actor::Actor;
use crate::error::{ApiError, ApiResult, Code};
use crate::executor::{self, End, RemoteCmd, Stream};
use crate::policy;
use crate::store::Page;
use crate::store::mail::Address;
use crate::store::projects::SOURCE_FILE;
use crate::store::schedules::{
    self, Begin, ListFilter, NewSchedule, Outcome, Reach, Run, Schedule, SchedulePatch, field_error,
};

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
}

async fn list(
    State(state): State<Shared>,
    actor: Actor,
    ApiQuery(q): ApiQuery<ListQuery>,
) -> ApiResult<Json<Vec<Schedule>>> {
    let global = policy::read_schedules(&actor, None).is_ok();
    let f = match q.project.filter(|p| !p.is_empty()) {
        Some(p) => {
            policy::read_schedules(&actor, Some(&p))?;
            ListFilter {
                project: Some(p),
                global,
                projects: None,
                enabled: q.enabled,
            }
        }
        // The operator and a sysadmin see all; an agent sees its project.
        None if global => ListFilter {
            project: None,
            global: true,
            projects: None,
            enabled: q.enabled,
        },
        None => ListFilter {
            project: actor.project().map(str::to_owned),
            global: false,
            projects: None,
            enabled: q.enabled,
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

/// The project and the action rights of a schedule write, for `actor`.
fn check_write(
    t: &rusqlite::Connection,
    actor: &Actor,
    project: Option<&str>,
    action: &ScheduleAction,
) -> ApiResult<Reach> {
    policy::schedule_scope(actor, project)?;
    let reach = schedules::resolve_reach(t, project, action)?;
    policy::manage_schedule(actor, project, &reach)?;
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
            check_write(t, &actor, new.project.as_deref(), &new.action)?;
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
            check_write(t, &actor, s.project.as_deref(), action)?;
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
        let (project, action) = (s.project.clone(), s.action.clone());
        let reach = state
            .db
            .read(move |c| check_write(c, &check, project.as_deref(), &action))
            .await?;
        caller_may_act(&state, &actor, &s, &reach).await?;
    }
    let v = state
        .db
        .write(actor.label(), move |t| {
            if enabled {
                check_write(t, &actor, s.project.as_deref(), &s.action)?;
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
    let (project, action) = (s.project.clone(), s.action.clone());
    let reach = state
        .db
        .read(move |c| check_write(c, &check, project.as_deref(), &action))
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
    let key = id.to_owned();
    let who = by.clone();
    let begin = state
        .db
        .write(by.clone(), move |t| {
            schedules::begin_run(t, &key, trigger, &who, Utc::now())
        })
        .await?;
    let (run, s) = match begin {
        Begin::Overlap(r) => return Ok(r),
        Begin::Started(r, s) => (r, s),
    };
    // The action runs in its own task: a panic in it fails the run, and
    // the run does not stay `running`.
    let st = state.clone();
    let outcome = match tokio::spawn(async move { perform(&st, &s).await }).await {
        Ok(o) => o,
        Err(e) => Outcome::failed("failed: the run task ended", e.to_string(), json!({})),
    };
    let run_id = run.id.clone();
    state
        .db
        .write(by, move |t| {
            schedules::finish_run(t, &run_id, &outcome, Utc::now())
        })
        .await
}

/// Check the rights of the `run_as` actor again, then do the action.
async fn perform(state: &Shared, s: &Schedule) -> Outcome {
    let (run_as, project, action) = (s.run_as.clone(), s.project.clone(), s.action.clone());
    let checked = state
        .db
        .read(move |c| {
            let Some(actor) = actor_of(c, &run_as)? else {
                return Ok(Err(format!(
                    "the schedule runs as `{run_as}`, which no longer exists"
                )));
            };
            let reach = match schedules::resolve_reach(c, project.as_deref(), &action) {
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
    match checked {
        Err(e) => Outcome::failed(format!("failed: {}", e.message), e.message, json!({})),
        Ok(Err(reason)) => Outcome::refused(reason),
        Ok(Ok((actor, reach))) => execute(state, s, actor, reach).await,
    }
}

fn failed(e: &ApiError, output: Value) -> Outcome {
    Outcome::failed(format!("failed: {}", e.message), e.message.clone(), output)
}

async fn execute(state: &Shared, s: &Schedule, actor: Actor, reach: Reach) -> Outcome {
    match (s.action.clone(), reach) {
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
            mail(state, actor, to, subject, body, thread).await
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
) -> Outcome {
    let result = state
        .db
        .write(actor.label(), move |t| {
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
