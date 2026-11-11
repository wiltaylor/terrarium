//! Waits, process runs, and watches (PLAN §12d, docs/api.md §6.7).
//!
//! - `GET|POST /api/agents/{id}/waits`: the waits of an agent (the waiter).
//! - `POST /api/agents/{id}/run`: a detached run on a target, with a
//!   `process` wait on it.
//! - `GET|DELETE /api/waits/{id}`, `GET /api/waits/{id}/log`.
//! - `GET|POST /api/agents/{id}/watches`: `{id}` is the watched agent.
//! - `GET /api/watches`, `DELETE /api/watches/{id}`.
//!
//! None of these calls blocks: a wait is registered, and the waiter ends
//! its turn. The end comes as mail.

use std::collections::BTreeMap;
use std::time::Duration;

use axum::extract::{Path, State};
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use axum::{Json, Router};
use serde::Deserialize;
use serde_json::{Value, json};

use super::{ApiJson, ApiQuery, Shared};
use crate::actor::Actor;
use crate::error::{ApiError, ApiResult, Code};
use crate::executor::{self, RemoteCmd};
use crate::policy;
use crate::store::agents::{self, AgentView};
use crate::store::waits::{self, NewWait, NewWatch, Until, Wait, WaitKind, WaitState, Watch};
use crate::waits as rt;

pub fn routes() -> Router<Shared> {
    Router::new()
        .route("/agents/{id}/waits", get(list).post(create))
        .route("/agents/{id}/run", post(run))
        .route("/agents/{id}/watches", get(watch_list).post(watch_create))
        .route("/waits/{id}", get(show).delete(cancel))
        .route("/waits/{id}/log", get(log))
        .route("/watches", get(watches_by))
        .route("/watches/{id}", axum::routing::delete(watch_remove))
}

/// The longest `reason` note of a wait.
const MAX_REASON: usize = 500;

/// Find an agent by key, with no read check: the caller checks its own
/// rule ([`policy::reach_agent`], [`policy::manage_waits`]).
async fn find(state: &Shared, actor: &Actor, key: String) -> ApiResult<AgentView> {
    find_in(state, key, actor.project().map(str::to_owned)).await
}

/// [`find`] with a bare name looked up in `context` first.
async fn find_in(state: &Shared, key: String, context: Option<String>) -> ApiResult<AgentView> {
    state
        .db
        .read(move |c| agents::find(c, &key, context.as_deref()))
        .await
}

async fn find_wait(state: &Shared, actor: &Actor, id: String) -> ApiResult<Wait> {
    let w = state.db.read(move |c| waits::get(c, &id)).await?;
    // A wait that the caller may not see is not found.
    policy::manage_waits(actor, &w.agent, &w.project, "reading a wait")
        .map_err(|_| ApiError::not_found(format!("no wait `{}`", w.id)))?;
    Ok(w)
}

/// A duration (`90m`, `2h`, `3600`) or an RFC 3339 time, as the time from
/// now. It must be in the future and at most `max` away.
pub(crate) fn parse_span(what: &str, s: &str, max: Duration) -> ApiResult<Duration> {
    let s = s.trim();
    let d = match chrono::DateTime::parse_from_rfc3339(s) {
        Ok(t) => (t.with_timezone(&chrono::Utc) - chrono::Utc::now())
            .to_std()
            .map_err(|_| ApiError::invalid(format!("`{what}` `{s}` is not in the future")))?,
        Err(_) => crate::settings::parse_duration(s).map_err(|e| {
            ApiError::invalid(format!(
                "bad `{what}` `{s}`: a duration such as 30m or 2h, or an RFC 3339 time ({e})"
            ))
        })?,
    };
    if d > max {
        return Err(ApiError::invalid(format!(
            "`{what}` `{s}` is longer than the limit of {} hours",
            max.as_secs() / 3600
        )));
    }
    Ok(d)
}

fn check_reason(reason: Option<String>) -> ApiResult<Option<String>> {
    let r = reason
        .map(|r| r.trim().to_owned())
        .filter(|r| !r.is_empty());
    if r.as_ref().is_some_and(|r| r.chars().count() > MAX_REASON) {
        return Err(ApiError::invalid(format!(
            "`reason` is longer than {MAX_REASON} characters"
        )));
    }
    Ok(r)
}

#[derive(Debug, Default, Deserialize)]
struct ListQuery {
    #[serde(default)]
    all: Option<bool>,
    #[serde(default)]
    kind: Option<WaitKind>,
}

/// `GET /api/agents/{id}/waits`.
async fn list(
    State(state): State<Shared>,
    actor: Actor,
    Path(id): Path<String>,
    ApiQuery(q): ApiQuery<ListQuery>,
) -> ApiResult<Json<Vec<Wait>>> {
    let a = find(&state, &actor, id).await?;
    let key = a.agent.id.to_string();
    policy::manage_waits(&actor, &key, &a.agent.project, "listing waits")?;
    let all = q.all.unwrap_or(false);
    let rows = state
        .db
        .read(move |c| waits::list(c, &key, all, q.kind))
        .await?;
    Ok(Json(rows))
}

/// The body of `POST /api/agents/{id}/waits`.
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct WaitBody {
    kind: WaitKind,
    #[serde(default)]
    reservation: Option<String>,
    /// `agent`: the agent to wait on (ULID, `project/name`, or a name).
    #[serde(default)]
    agent: Option<String>,
    /// `agent`: done (default), idle, blocked, or any-change.
    #[serde(default)]
    until: Option<String>,
    /// `time`: a duration from now.
    #[serde(default, rename = "for")]
    for_: Option<String>,
    /// `time`: an RFC 3339 time.
    #[serde(default)]
    at: Option<String>,
    /// `process`: an exec session ID.
    #[serde(default)]
    session: Option<String>,
    /// `card`: the card and the column.
    #[serde(default)]
    card: Option<String>,
    #[serde(default)]
    column: Option<String>,
    /// `question`: the ID of one of the waiter's own questions.
    #[serde(default)]
    question: Option<String>,
    /// A duration or a time. Default: `TERRARIUM_WAIT_DEFAULT_TIMEOUT`
    /// (24 h) for `agent`, `process`, and `card`; none for `time`.
    #[serde(default)]
    timeout: Option<String>,
    /// The waiter's note: why it waits. In the wake mail.
    #[serde(default)]
    reason: Option<String>,
}

fn need<'a>(v: &'a Option<String>, field: &str, kind: &str) -> ApiResult<&'a str> {
    v.as_deref()
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .ok_or_else(|| ApiError::invalid(format!("a `{kind}` wait needs `{field}`")))
}

/// `POST /api/agents/{id}/waits`.
async fn create(
    State(state): State<Shared>,
    actor: Actor,
    Path(id): Path<String>,
    ApiJson(b): ApiJson<WaitBody>,
) -> ApiResult<Response> {
    let waiter = find(&state, &actor, id).await?;
    let waiter_id = waiter.agent.id.to_string();
    policy::own_waits(&actor, &waiter_id, "registering a wait")?;
    let tuning = state.waits.tuning();
    let reason = check_reason(b.reason.clone())?;
    let timeout = match b.timeout.as_deref() {
        Some(t) => Some(parse_span("timeout", t, tuning.max_timeout)?),
        None if b.kind == WaitKind::Time => None,
        None => Some(tuning.default_timeout),
    };
    let mut poll_session = None;
    let mut read_session = None;
    let mut new = match b.kind {
        WaitKind::Reservation => {
            let id = need(&b.reservation, "reservation", "reservation")?.to_owned();
            let r = state
                .db
                .read(move |c| crate::store::reservations::get(c, &id))
                .await?;
            policy::read_project(&actor, &r.project)?;
            NewWait {
                agent: waiter_id.clone(),
                kind: WaitKind::Reservation,
                spec: json!({"reservation":r.id}),
                summary: format!("reservation {} on {}", r.id, r.host),
                target_agent: None,
                reason,
                timeout_at: None,
                due_at: None,
            }
        }
        WaitKind::Question => {
            // The check that the question is open runs in the write.
            let qid = need(&b.question, "question", "question")?.to_owned();
            let timeout_at = timeout.map(rt::from_now);
            let w = state
                .db
                .write(actor.label(), move |t| {
                    super::questions::create_wait(t, &waiter_id, &qid, timeout_at, reason)
                })
                .await?;
            return Ok((StatusCode::CREATED, Json(w)).into_response());
        }
        WaitKind::Agent => {
            let key = need(&b.agent, "agent", "agent")?.to_owned();
            // A bare name is a peer of the waiter first.
            let target = find_in(&state, key, Some(waiter.agent.project.clone())).await?;
            if let Actor::Agent(me) = &actor {
                policy::reach_agent(me, &target.agent.name, &target.agent.project)?;
            }
            let until = match b.until.as_deref() {
                Some(u) => Until::parse(u)?,
                None => Until::Done,
            };
            let name = format!("{}/{}", target.agent.project, target.agent.name);
            let tid = target.agent.id.to_string();
            NewWait {
                agent: waiter_id.clone(),
                kind: WaitKind::Agent,
                spec: json!({"agent": tid, "agent_name": name, "until": until.as_str(),
                             "from_status": target.agent.status}),
                summary: format!("agent {name} ({})", until.as_str().replace('_', "-")),
                target_agent: Some(tid),
                reason,
                timeout_at: None,
                due_at: None,
            }
        }
        WaitKind::Time => {
            let d = match (&b.for_, &b.at) {
                (Some(f), None) => parse_span("for", f, tuning.max_timeout)?,
                (None, Some(a)) => parse_span("at", a, tuning.max_timeout)?,
                _ => {
                    return Err(ApiError::invalid(
                        "a `time` wait needs one of `for` (a duration) or `at` (a time)",
                    ));
                }
            };
            let due = rt::from_now(d);
            NewWait {
                agent: waiter_id.clone(),
                kind: WaitKind::Time,
                spec: json!({"due_at": due}),
                summary: format!("time {due}"),
                target_agent: None,
                reason,
                timeout_at: None,
                due_at: Some(due),
            }
        }
        WaitKind::Process => {
            let sid = need(&b.session, "session", "process")?.to_owned();
            let sess = state
                .executor
                .find(&sid)
                .ok_or_else(|| ApiError::not_found(format!("no exec session `{sid}`")))?;
            let mine = match &actor {
                Actor::Operator => {
                    sess.owner.is_none() || sess.owner.as_deref() == Some(&waiter_id)
                }
                Actor::Agent(a) => sess.owner.as_deref() == Some(a.id.as_str()),
            };
            if !mine {
                return Err(ApiError::not_found(format!("no exec session `{sid}`")));
            }
            // A detached tool run (`terrarium tool run --detach`): nobody else
            // reads the session, so the daemon reads it to the end, as for
            // `terrarium run`: the result gets the (redacted) output tails, and
            // the reaper of unread sessions leaves it alone.
            if sess.kind == "tool" {
                read_session = Some(sess.clone());
            } else {
                poll_session = Some(sid.clone());
            }
            NewWait {
                agent: waiter_id.clone(),
                kind: WaitKind::Process,
                spec: json!({"target": sess.target, "session": sid, "run": false}),
                // A detached tool run (`terrarium tool run --detach`).
                summary: if sess.kind == "tool" {
                    format!("tool run {} on {}", short(&sid), sess.target)
                } else {
                    format!("process {} on {}", short(&sid), sess.target)
                },
                target_agent: None,
                reason,
                timeout_at: None,
                due_at: None,
            }
        }
        WaitKind::Card => {
            let card = need(&b.card, "card", "card")?.to_owned();
            let column = need(&b.column, "column", "card")?.to_owned();
            let (c, columns, resolved) = {
                let (key, want) = (card.clone(), column.clone());
                state
                    .db
                    .read(move |c| {
                        let card = crate::store::board::get_card(c, &key)?;
                        let board = crate::store::board::get_board(c, &card.project)?;
                        let cols: Vec<String> =
                            board.columns.iter().map(|k| k.name.clone()).collect();
                        // A role alias (`working`, `blocked`) names the
                        // column with that role.
                        let resolved =
                            crate::store::board::resolve_column(c, &card.project, &want).ok();
                        Ok((card, cols, resolved))
                    })
                    .await?
            };
            policy::read_project(&actor, &c.project)?;
            let Some(column) = resolved else {
                return Err(ApiError::invalid(format!(
                    "the board of `{}` has no column `{column}`; it has {}",
                    c.project,
                    columns.join(", ")
                )));
            };
            NewWait {
                agent: waiter_id.clone(),
                kind: WaitKind::Card,
                spec: json!({"card": c.id, "column": column}),
                summary: format!("card {} in {column}", c.id),
                target_agent: None,
                reason,
                timeout_at: None,
                due_at: None,
            }
        }
    };
    new.timeout_at = timeout.map(rt::from_now);
    let w = state
        .db
        .write(actor.label(), move |t| waits::create(t, new))
        .await?;
    if let Some(sid) = poll_session {
        rt::spawn_session_poller(&state, w.id.clone(), sid);
    }
    if let Some(sess) = read_session {
        rt::spawn_run_reader(&state, w.id.clone(), waiter_id.clone(), sess);
    }
    Ok((StatusCode::CREATED, Json(w)).into_response())
}

fn short(id: &str) -> String {
    id.chars()
        .rev()
        .take(6)
        .collect::<Vec<_>>()
        .into_iter()
        .rev()
        .collect()
}

/// The body of `POST /api/agents/{id}/run`.
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct RunBody {
    target: String,
    cmd: Vec<String>,
    /// A short name for the run, for example `build`.
    #[serde(default)]
    name: Option<String>,
    #[serde(default)]
    cwd: Option<String>,
    /// The env of the command. A value `@secret:<name>[.<field>]` is a
    /// secret that the daemon resolves at the start, from a `run` grant
    /// that names the target or a `read` grant (TER-142,
    /// `crate::secrets::run_env`).
    #[serde(default)]
    env: BTreeMap<String, String>,
    /// The run stops at this time (a duration or a time). Default
    /// `TERRARIUM_WAIT_DEFAULT_TIMEOUT`.
    #[serde(default)]
    timeout: Option<String>,
    #[serde(default)]
    reason: Option<String>,
    /// A `lab_from_workspace` target: the holder (docs/api.md §17.4).
    #[serde(default)]
    agent: Option<String>,
}

/// How long a run's wait waits past the run's own timeout, so that the
/// target's end (with the output) comes first.
const RUN_GRACE: Duration = Duration::from_secs(60);

/// `POST /api/agents/{id}/run`: start a detached command on a target as
/// the agent, with a `process` wait on it. The command runs with the
/// agent's own target rights (its policy and leases). Each secret
/// reference in `env` is resolved from the agent's grants as they are
/// now, goes into the env of the remote process only, is redacted from
/// the output first, and writes a `secret.run` audit row with no value
/// (TER-142).
async fn run(
    State(state): State<Shared>,
    actor: Actor,
    Path(id): Path<String>,
    ApiJson(b): ApiJson<RunBody>,
) -> ApiResult<Response> {
    let waiter = find(&state, &actor, id).await?;
    let waiter_id = waiter.agent.id.to_string();
    policy::own_waits(&actor, &waiter_id, "starting a run")?;
    if b.cmd.is_empty() {
        return Err(ApiError::invalid(
            "`cmd` is empty: give the program and its arguments",
        ));
    }
    let name = b.name.as_deref().map(str::trim).filter(|n| !n.is_empty());
    if name.is_some_and(|n| n.chars().count() > 40) {
        return Err(ApiError::invalid("`name` is longer than 40 characters"));
    }
    let tuning = state.waits.tuning();
    let reason = check_reason(b.reason.clone())?;
    let limit = match b.timeout.as_deref() {
        Some(t) => parse_span("timeout", t, tuning.max_timeout)?,
        None => tuning.default_timeout,
    };
    let key = waiter_id.clone();
    let (open, agent_actor) = state
        .db
        .read(move |c| Ok((waits::open_count(c, &key)?, agents::actor_for(c, &key)?)))
        .await?;
    if open >= waits::MAX_OPEN_WAITS {
        return Err(ApiError::new(
            Code::RateLimited,
            format!(
                "{} has {open} open waits; an agent may have {}. Cancel one first",
                waiter.agent.name,
                waits::MAX_OPEN_WAITS
            ),
        ));
    }
    let agent_actor = Actor::Agent(
        agent_actor.ok_or_else(|| ApiError::not_found(format!("no agent `{waiter_id}`")))?,
    );
    let mut env = b.env.clone();
    let refs = crate::secrets::run_env::parse_refs(&env)?;
    let injected = if refs.is_empty() {
        Vec::new()
    } else {
        let key = crate::secrets::key(&state.state_dir)?;
        let proxy_on = state.git_proxy.is_on();
        let (id, project, target) = (
            waiter_id.clone(),
            waiter.agent.project.clone(),
            b.target.clone(),
        );
        state
            .db
            .read(move |c| {
                let use_only = crate::git_proxy::use_only_secrets(c, proxy_on)?;
                crate::secrets::run_env::resolve(c, &key, &id, &project, &target, &refs, &use_only)
            })
            .await?
    };
    for i in &injected {
        env.insert(i.env.clone(), i.value.to_string());
    }
    let command = b.cmd.join(" ");
    let cmd = RemoteCmd {
        argv: b.cmd.clone(),
        cwd: b.cwd.clone(),
        env,
        pty: None,
        timeout_secs: Some(limit.as_secs().max(1)),
        shell: false,
        agent: b.agent.clone(),

        // `start_exec` sets these from the caller.
        holder: None,
        operator: false,
    };
    let own: Vec<(String, zeroize::Zeroizing<String>)> = injected
        .iter()
        .map(|i| (i.label(), i.value.clone()))
        .collect();
    let sess = executor::start_run_exec(&state, &agent_actor, &b.target, cmd, own).await?;
    let label = name.map(str::to_owned).unwrap_or_else(|| {
        b.cmd
            .first()
            .map(|p| p.rsplit('/').next().unwrap_or(p).to_owned())
            .unwrap_or_default()
    });
    let mut command_short: String = command.chars().take(200).collect();
    if command.chars().count() > 200 {
        command_short.push('…');
    }
    let new = NewWait {
        agent: waiter_id.clone(),
        kind: WaitKind::Process,
        spec: json!({"target": b.target, "session": sess.id, "name": name,
                     "command": command_short, "run": true}),
        summary: format!("process {label} on {}", b.target),
        target_agent: None,
        reason,
        timeout_at: Some(rt::from_now(limit + RUN_GRACE)),
        due_at: None,
    };
    // The audit rows of the secrets: no value.
    let audits: Vec<(String, Option<String>, Value)> = injected
        .iter()
        .map(|i| {
            (
                format!("{}/{}", i.scope, i.name),
                crate::secrets::scope_project(&i.scope).map(str::to_owned),
                i.audit_detail(&waiter_id, &b.target, &sess.id, ""),
            )
        })
        .collect();
    drop(injected);
    let created = state
        .db
        .write(actor.label(), move |t| {
            let w = waits::create(t, new)?;
            for (object, project, mut detail) in audits {
                detail["run"] = json!(w.id);
                t.audit("secret.run", &object, project.as_deref(), detail)?;
            }
            Ok(w)
        })
        .await;
    let w = match created {
        Ok(w) => w,
        Err(e) => {
            sess.kill();
            return Err(e);
        }
    };
    rt::spawn_run_reader(&state, w.id.clone(), waiter_id, sess.clone());
    Ok((
        StatusCode::CREATED,
        Json(json!({"wait": w, "session_id": sess.id, "target": sess.target})),
    )
        .into_response())
}

/// `GET /api/waits/{id}`.
async fn show(
    State(state): State<Shared>,
    actor: Actor,
    Path(id): Path<String>,
) -> ApiResult<Json<Wait>> {
    Ok(Json(find_wait(&state, &actor, id).await?))
}

#[derive(Debug, Default, Deserialize)]
struct CancelQuery {
    #[serde(default)]
    kill: Option<bool>,
}

/// `DELETE /api/waits/{id}[?kill=true]`: cancel an open wait. `kill` also
/// ends the run of a `process` wait; without it, the run goes on. A kill
/// ends the process group and session of the run on the target (`TERM`,
/// then `KILL` after a grace time), and the answer has the report as
/// `kill`: `{pid, term, kill, left}` (TER-99).
async fn cancel(
    State(state): State<Shared>,
    actor: Actor,
    Path(id): Path<String>,
    ApiQuery(q): ApiQuery<CancelQuery>,
) -> ApiResult<Json<Value>> {
    let w = find_wait(&state, &actor, id).await?;
    policy::manage_waits(&actor, &w.agent, &w.project, "cancelling a wait")?;
    if w.state != WaitState::Open {
        return Err(ApiError::conflict(format!(
            "wait {} is {} already",
            w.id,
            w.state.as_str()
        )));
    }
    let by_waiter = actor.agent().is_some_and(|a| a.id == w.agent);
    let (wid, by) = (w.id.clone(), actor.label());
    let done = state
        .db
        .write(actor.label(), move |t| {
            waits::finish(
                t,
                &wid,
                WaitState::Cancelled,
                json!({"outcome": "cancelled", "by": by}),
                !by_waiter,
                0,
            )
        })
        .await?;
    let report = if q.kill.unwrap_or(false) && w.kind == WaitKind::Process {
        rt::kill_run_report(&state, &w).await
    } else {
        None
    };
    let id = w.id.clone();
    match done {
        Some(_) => {
            let wait = state.db.read(move |c| waits::get(c, &id)).await?;
            let mut v = serde_json::to_value(wait)
                .map_err(|e| ApiError::internal(format!("cannot encode the wait: {e}")))?;
            if let (Some(r), Value::Object(m)) = (report, &mut v) {
                m.insert("kill".into(), r);
            }
            Ok(Json(v))
        }
        None => Err(ApiError::conflict(format!("wait {} ended meanwhile", w.id))),
    }
}

#[derive(Debug, Default, Deserialize)]
struct LogQuery {
    #[serde(default)]
    max: Option<usize>,
}

/// `GET /api/waits/{id}/log`: the tail of the output of a `process` wait.
/// The text is untrusted.
async fn log(
    State(state): State<Shared>,
    actor: Actor,
    Path(id): Path<String>,
    ApiQuery(q): ApiQuery<LogQuery>,
) -> ApiResult<Json<rt::RunLog>> {
    let w = find_wait(&state, &actor, id).await?;
    if w.kind != WaitKind::Process {
        return Err(ApiError::invalid(format!(
            "wait {} is a `{}` wait; only a `process` wait has a log",
            w.id,
            w.kind.as_str()
        )));
    }
    Ok(Json(rt::run_log(&state, &w, q.max.unwrap_or(rt::LOG_TAIL))))
}

// ----------------------------------------------------------------- watches

/// `GET /api/agents/{id}/watches`: the watches on the agent and by it.
async fn watch_list(
    State(state): State<Shared>,
    actor: Actor,
    Path(id): Path<String>,
) -> ApiResult<Json<Vec<Watch>>> {
    let a = find(&state, &actor, id).await?;
    let key = a.agent.id.to_string();
    policy::manage_waits(&actor, &key, &a.agent.project, "listing watches")?;
    Ok(Json(
        state.db.read(move |c| waits::watches_of(c, &key)).await?,
    ))
}

/// The body of `POST /api/agents/{id}/watches`.
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct WatchBody {
    /// The statuses: idle, waiting, working (or busy), blocked, error,
    /// stopped, starting, restarting.
    on: Vec<String>,
    #[serde(default)]
    once: bool,
    /// Coalesce flaps (default true).
    #[serde(default)]
    debounce: Option<bool>,
    /// The operator only: `operator` (the default) or an agent that gets
    /// the mail.
    #[serde(default)]
    watcher: Option<String>,
}

/// `POST /api/agents/{id}/watches`: watch the agent `{id}`.
async fn watch_create(
    State(state): State<Shared>,
    actor: Actor,
    Path(id): Path<String>,
    ApiJson(b): ApiJson<WatchBody>,
) -> ApiResult<Response> {
    let watched = find(&state, &actor, id).await?;
    let watcher = match (&actor, b.watcher.as_deref().map(str::trim)) {
        (Actor::Agent(me), None) => me.id.clone(),
        (Actor::Agent(me), Some(w)) if w == me.id || w == me.name => me.id.clone(),
        (Actor::Agent(_), Some(_)) => {
            return Err(ApiError::forbidden(
                "an agent watches for itself only; leave out `watcher`",
            ));
        }
        (Actor::Operator, None | Some("operator")) => "operator".to_owned(),
        (Actor::Operator, Some(key)) => find(&state, &actor, key.to_owned())
            .await?
            .agent
            .id
            .to_string(),
    };
    if let Actor::Agent(me) = &actor {
        policy::reach_agent(me, &watched.agent.name, &watched.agent.project)?;
    }
    let on =
        b.on.iter()
            .flat_map(|s| s.split(','))
            .filter(|s| !s.trim().is_empty())
            .map(waits::watch_status)
            .collect::<ApiResult<Vec<_>>>()?;
    let new = NewWatch {
        watcher,
        agent: watched.agent.id.to_string(),
        on,
        once: b.once,
        debounce: b.debounce.unwrap_or(true),
    };
    if matches!(actor, Actor::Operator) {
        policy::operator_only(&actor, "creating a watch for another caller")?;
    }
    let w = state
        .db
        .write(actor.label(), move |t| waits::create_watch(t, new))
        .await?;
    Ok((StatusCode::CREATED, Json(w)).into_response())
}

#[derive(Debug, Default, Deserialize)]
struct WatchesQuery {
    #[serde(default)]
    watcher: Option<String>,
}

/// `GET /api/watches?watcher=`: the watches of a watcher. An agent lists
/// its own by default; the operator lists `operator` by default.
async fn watches_by(
    State(state): State<Shared>,
    actor: Actor,
    ApiQuery(q): ApiQuery<WatchesQuery>,
) -> ApiResult<Json<Vec<Watch>>> {
    let key = match (&actor, q.watcher.as_deref().map(str::trim)) {
        (Actor::Agent(me), None) => me.id.clone(),
        (Actor::Operator, None | Some("operator")) => "operator".to_owned(),
        (Actor::Agent(_), Some("operator")) => {
            return Err(ApiError::forbidden(
                "the operator's watches are for the operator only",
            ));
        }
        (_, Some(k)) => {
            let a = find(&state, &actor, k.to_owned()).await?;
            let id = a.agent.id.to_string();
            policy::manage_waits(&actor, &id, &a.agent.project, "listing watches")?;
            id
        }
    };
    if key == "operator" {
        policy::operator_only(&actor, "listing operator watches")?;
    }
    Ok(Json(
        state.db.read(move |c| waits::watches_by(c, &key)).await?,
    ))
}

/// `DELETE /api/watches/{id}`: the watcher, a project_admin of the
/// watcher's project, or the operator.
async fn watch_remove(
    State(state): State<Shared>,
    actor: Actor,
    Path(id): Path<String>,
) -> ApiResult<StatusCode> {
    let w = state.db.read(move |c| waits::get_watch(c, &id)).await?;
    if w.watcher == "operator" {
        policy::operator_only(&actor, "removing an operator watch")?;
    }
    let allowed = match (&actor, w.watcher.as_str()) {
        (Actor::Operator, _) => true,
        (Actor::Agent(_), "operator") => false,
        (Actor::Agent(a), watcher) => {
            let project = w.watcher_name.split('/').next().unwrap_or_default();
            a.id == watcher || a.is_admin_of(project)
        }
    };
    if !allowed {
        return Err(ApiError::not_found(format!("no watch `{}`", w.id)));
    }
    let wid = w.id.clone();
    state
        .db
        .write(actor.label(), move |t| waits::delete_watch(t, &wid))
        .await?;
    Ok(StatusCode::NO_CONTENT)
}

/// The open waits of an agent, for `GET /api/agents/{id}`.
pub async fn open_waits(state: &Shared, agent: String) -> ApiResult<Vec<Value>> {
    let rows = state.db.read(move |c| waits::open_of(c, &agent)).await?;
    Ok(rows
        .into_iter()
        .filter_map(|w| serde_json::to_value(w).ok())
        .collect())
}
