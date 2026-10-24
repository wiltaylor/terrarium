//! `/api/targets`: target definitions, lifecycle verbs, and exec sessions
//! (PLAN §17, §19). The executor is `crate::executor`.
//!
//! Exec sessions work over plain request and answer, so they pass through
//! the agent RPC tunnel (docs/agent-rpc.md, "Streams over the tunnel"):
//!
//! - `POST .../exec` or `POST .../cp` opens a session: `{session_id}`.
//! - `POST .../sessions/{id}/input` sends stdin bytes (base64), `eof`,
//!   `resize`, `signal`, or the `finish` of a push.
//! - `GET .../sessions/{id}/output?since=&wait=` long-polls the output.
//! - `DELETE .../sessions/{id}` kills the run.
//! - `GET .../sessions/{id}/ws` is a WebSocket for an operator terminal.

use std::collections::BTreeMap;
use std::time::Duration;

use axum::extract::ws::WebSocket;
use axum::extract::{FromRequestParts, Path, Request, State, WebSocketUpgrade};
use axum::http::{StatusCode, header};
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use axum::{Json, Router};
use base64::Engine;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use terrarium_core::config::{TargetDef, TargetKind};
use terrarium_core::protocol::LifecycleVerb;
use terrarium_host::protocol::PtySize;

use super::{ApiJson, ApiQuery, Shared};
use crate::actor::Actor;
use crate::error::{ApiError, ApiResult, Code};
use crate::executor::{
    self, Control, CpBody, ExecSession, LifecycleOpts, MAX_INPUT, MAX_READ, MAX_WAIT_SECS,
    RemoteCmd,
};
use crate::policy;
use crate::store::targets::{self, TargetRow};
use crate::supervisor::{PtyAttachment, PtyInput};

pub fn routes() -> Router<Shared> {
    Router::new()
        .route("/targets", get(list).post(create))
        .route("/targets/{name}", get(show).patch(update).delete(remove))
        .route("/targets/{name}/up", post(up))
        .route("/targets/{name}/down", post(down))
        .route("/targets/{name}/snapshot", post(snapshot))
        .route("/targets/{name}/restore", post(restore))
        .route("/targets/{name}/status", post(status))
        .route("/targets/{name}/exec", post(exec))
        .route("/targets/{name}/cp", post(cp))
        .route(
            "/targets/{name}/sessions/{id}",
            get(session_show).delete(session_close),
        )
        .route("/targets/{name}/sessions/{id}/input", post(session_input))
        .route("/targets/{name}/sessions/{id}/output", get(session_output))
        .route("/targets/{name}/sessions/{id}/ws", get(session_ws))
}

/// A target with its state, as the API shows it.
#[derive(Debug, Clone, Serialize)]
pub struct TargetView {
    /// The target name. Targets have no other ID.
    pub id: String,
    #[serde(flatten)]
    pub def: TargetDef,
    /// `global` or `project`.
    pub scope: &'static str,
    /// `file`, `api`, or `fleet:<name>` (`docs/api.md` §18.1).
    pub source: String,
    /// The state that the last lifecycle verb saw.
    pub state: String,
    pub state_at: Option<String>,
    pub snapshots: Vec<String>,
    /// Whether the target's host agent is connected; `null` for `ssh`.
    pub host_online: Option<bool>,
    /// Whether the caller may exec on it (its policy lists it).
    pub usable: bool,
}

fn visible(actor: &Actor, def: &TargetDef) -> bool {
    match (actor, &def.project) {
        (Actor::Operator, _) | (_, None) => true,
        (Actor::Agent(a), Some(p)) => a.is_sysadmin() || a.project == *p,
    }
}

pub fn view(state: &Shared, actor: &Actor, r: TargetRow) -> TargetView {
    let host_online = match r.def.kind {
        TargetKind::Ssh => None,
        _ => Some(
            r.def
                .host
                .as_deref()
                .is_some_and(|h| state.executor.hosts.is_online(h)),
        ),
    };
    let usable = policy::use_target(actor, &r.def.name, r.def.project.as_deref()).is_ok();
    TargetView {
        id: r.def.name.clone(),
        scope: if r.def.project.is_some() {
            "project"
        } else {
            "global"
        },
        def: r.def,
        source: r.source,
        state: r.state,
        state_at: r.state_at,
        snapshots: r.snapshots,
        host_online,
        usable,
    }
}

/// The targets of a project and the global ones, for
/// `GET /api/projects/{name}/targets`.
pub async fn for_project(
    state: &Shared,
    actor: &Actor,
    project: &str,
) -> ApiResult<Vec<TargetView>> {
    let rows = state.db.read(targets::list).await?;
    Ok(rows
        .into_iter()
        .filter(|r| r.def.project.as_deref().is_none_or(|p| p == project))
        .map(|r| view(state, actor, r))
        .collect())
}

#[derive(Debug, Default, Deserialize)]
struct ListQuery {
    /// Only the targets the caller may exec on.
    #[serde(default)]
    usable: Option<bool>,
    #[serde(default)]
    project: Option<String>,
}

/// `GET /api/targets`: every target for the operator and sysadmins; for an
/// agent, the global targets and those of its project.
async fn list(
    State(state): State<Shared>,
    actor: Actor,
    ApiQuery(q): ApiQuery<ListQuery>,
) -> ApiResult<Json<Vec<TargetView>>> {
    let rows = state.db.read(targets::list).await?;
    Ok(Json(
        rows.into_iter()
            .filter(|r| visible(&actor, &r.def))
            .filter(|r| {
                q.project
                    .as_deref()
                    .is_none_or(|p| r.def.project.as_deref().is_none_or(|x| x == p))
            })
            .map(|r| view(&state, &actor, r))
            .filter(|v| !q.usable.unwrap_or(false) || v.usable)
            .collect(),
    ))
}

async fn load(state: &Shared, actor: &Actor, name: String) -> ApiResult<TargetRow> {
    let n = name.clone();
    let r = state.db.read(move |c| targets::get(c, &n)).await?;
    if !visible(actor, &r.def) {
        return Err(ApiError::not_found(format!("no target `{name}`")));
    }
    Ok(r)
}

async fn show(
    State(state): State<Shared>,
    actor: Actor,
    Path(name): Path<String>,
) -> ApiResult<Json<TargetView>> {
    let r = load(&state, &actor, name).await?;
    Ok(Json(view(&state, &actor, r)))
}

/// `POST /api/targets`: a TargetDef as JSON.
async fn create(
    State(state): State<Shared>,
    actor: Actor,
    ApiJson(def): ApiJson<TargetDef>,
) -> ApiResult<Response> {
    policy::define_target(&actor, def.project.as_deref())?;
    let r = state
        .db
        .write(actor.label(), move |t| {
            let r = targets::create(t, &def)?;
            t.audit(
                "target.create",
                &r.def.name,
                r.def.project.as_deref(),
                serde_json::to_value(&r.def)?,
            )?;
            Ok(r)
        })
        .await?;
    Ok((StatusCode::CREATED, Json(view(&state, &actor, r))).into_response())
}

/// `PATCH /api/targets/{name}`: a JSON merge patch of the definition.
async fn update(
    State(state): State<Shared>,
    actor: Actor,
    Path(name): Path<String>,
    ApiJson(patch): ApiJson<Value>,
) -> ApiResult<Json<TargetView>> {
    let current = load(&state, &actor, name.clone()).await?;
    policy::define_target(&actor, current.def.project.as_deref())?;
    if let Some(p) = patch.get("project")
        && p.as_str() != current.def.project.as_deref()
    {
        policy::define_target(&actor, p.as_str())?;
    }
    let r = state
        .db
        .write(actor.label(), move |t| {
            let r = targets::update(t, &name, &patch)?;
            t.audit("target.update", &name, r.def.project.as_deref(), patch)?;
            Ok(r)
        })
        .await?;
    Ok(Json(view(&state, &actor, r)))
}

async fn remove(
    State(state): State<Shared>,
    actor: Actor,
    Path(name): Path<String>,
) -> ApiResult<StatusCode> {
    let current = load(&state, &actor, name.clone()).await?;
    policy::define_target(&actor, current.def.project.as_deref())?;
    state
        .db
        .write(actor.label(), move |t| {
            let r = targets::delete(t, &name)?;
            t.audit("target.delete", &name, r.def.project.as_deref(), json!({}))
        })
        .await?;
    Ok(StatusCode::NO_CONTENT)
}

/// Lifecycle verbs: an actor that may use the target, or one that may
/// define it.
async fn run_verb(
    state: Shared,
    actor: Actor,
    name: String,
    verb: LifecycleVerb,
    body: Option<LifecycleOpts>,
) -> ApiResult<Json<Value>> {
    let r = load(&state, &actor, name).await?;
    if policy::use_target(&actor, &r.def.name, r.def.project.as_deref()).is_err() {
        policy::define_target(&actor, r.def.project.as_deref()).map_err(|_| {
            ApiError::forbidden(format!(
                "target `{}` is not in your policy.targets, and you may not define it",
                r.def.name
            ))
        })?;
    }
    let opts = body.unwrap_or_default();
    let out = executor::lifecycle(&state, &r.def, verb, &opts).await?;
    let (target, project) = (r.def.name.clone(), r.def.project.clone());
    let detail = out.clone();
    let action = format!("target.{}", json!(verb).as_str().unwrap_or("verb"));
    state
        .db
        .write(actor.label(), move |t| {
            t.audit(&action, &target, project.as_deref(), detail)
        })
        .await?;
    Ok(Json(out))
}

/// An optional JSON body: none, or `{}` with options.
struct OptBody<T>(Option<T>);

impl<S: Send + Sync, T: serde::de::DeserializeOwned> axum::extract::FromRequest<S> for OptBody<T> {
    type Rejection = ApiError;

    async fn from_request(req: Request, state: &S) -> Result<Self, Self::Rejection> {
        let bytes = axum::body::Bytes::from_request(req, state)
            .await
            .map_err(|e| ApiError::bad_request(e.body_text()))?;
        if bytes.iter().all(u8::is_ascii_whitespace) {
            return Ok(Self(None));
        }
        serde_json::from_slice(&bytes)
            .map(|v| Self(Some(v)))
            .map_err(|e| ApiError::bad_request(format!("bad body: {e}")))
    }
}

macro_rules! verb_handler {
    ($name:ident, $verb:expr) => {
        async fn $name(
            State(state): State<Shared>,
            actor: Actor,
            Path(name): Path<String>,
            OptBody(body): OptBody<LifecycleOpts>,
        ) -> ApiResult<Json<Value>> {
            run_verb(state, actor, name, $verb, body).await
        }
    };
}

verb_handler!(up, LifecycleVerb::Up);
verb_handler!(down, LifecycleVerb::Down);
verb_handler!(snapshot, LifecycleVerb::Snapshot);
verb_handler!(restore, LifecycleVerb::Restore);
verb_handler!(status, LifecycleVerb::Status);

/// A PTY in an exec body: `true` (24x80) or `{rows, cols}`.
#[derive(Debug, Clone, Deserialize)]
#[serde(untagged)]
enum PtyArg {
    Flag(bool),
    Size(PtySize),
}

/// The body of `POST /api/targets/{name}/exec`.
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct ExecBody {
    /// The program and its arguments. Empty with `shell`.
    #[serde(default)]
    cmd: Vec<String>,
    #[serde(default)]
    cwd: Option<String>,
    #[serde(default)]
    env: BTreeMap<String, String>,
    #[serde(default)]
    pty: Option<PtyArg>,
    /// A login shell in a PTY, for `terrarium shell`.
    #[serde(default)]
    shell: bool,
    #[serde(default)]
    timeout_secs: Option<u64>,
}

#[derive(Debug, Serialize)]
struct Opened {
    session_id: String,
    target: String,
    kind: &'static str,
}

fn opened(s: &ExecSession) -> Response {
    (
        StatusCode::CREATED,
        Json(Opened {
            session_id: s.id.clone(),
            target: s.target.clone(),
            kind: s.kind,
        }),
    )
        .into_response()
}

async fn exec(
    State(state): State<Shared>,
    actor: Actor,
    Path(name): Path<String>,
    ApiJson(b): ApiJson<ExecBody>,
) -> ApiResult<Response> {
    let pty = match b.pty {
        None | Some(PtyArg::Flag(false)) => None,
        Some(PtyArg::Flag(true)) => Some(PtySize::default()),
        Some(PtyArg::Size(s)) => Some(s),
    };
    let cmd = RemoteCmd {
        argv: b.cmd,
        cwd: b.cwd,
        env: b.env,
        pty: if b.shell {
            pty.or(Some(PtySize::default()))
        } else {
            pty
        },
        timeout_secs: b.timeout_secs,
        shell: b.shell,
    };
    let s = executor::start_exec(&state, &actor, &name, cmd).await?;
    Ok(opened(&s))
}

async fn cp(
    State(state): State<Shared>,
    actor: Actor,
    Path(name): Path<String>,
    ApiJson(b): ApiJson<CpBody>,
) -> ApiResult<Response> {
    let s = executor::start_cp(&state, &actor, &name, b).await?;
    Ok(opened(&s))
}

async fn session_show(
    State(state): State<Shared>,
    actor: Actor,
    Path((name, id)): Path<(String, String)>,
) -> ApiResult<Json<Value>> {
    Ok(Json(state.executor.session(&actor, &name, &id)?.view()))
}

async fn session_close(
    State(state): State<Shared>,
    actor: Actor,
    Path((name, id)): Path<(String, String)>,
) -> ApiResult<StatusCode> {
    state.executor.close(&actor, &name, &id)?;
    Ok(StatusCode::NO_CONTENT)
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct Resize {
    rows: u16,
    cols: u16,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct Finish {
    bytes: u64,
    sha256: String,
}

/// The body of `POST .../sessions/{id}/input`. Each present field is one
/// control, in this order: data, resize, signal, eof, finish.
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct InputBody {
    /// Bytes for stdin (or the file of a push), base64.
    #[serde(default)]
    data: Option<String>,
    #[serde(default)]
    eof: bool,
    #[serde(default)]
    resize: Option<Resize>,
    #[serde(default)]
    signal: Option<String>,
    /// The end of a push: the byte count and the SHA-256 of the file.
    #[serde(default)]
    finish: Option<Finish>,
}

async fn session_input(
    State(state): State<Shared>,
    actor: Actor,
    Path((name, id)): Path<(String, String)>,
    ApiJson(b): ApiJson<InputBody>,
) -> ApiResult<Json<Value>> {
    let s = state.executor.session(&actor, &name, &id)?;
    let mut controls = Vec::new();
    if let Some(d) = &b.data {
        let bytes = base64::engine::general_purpose::STANDARD
            .decode(d)
            .map_err(|e| ApiError::bad_request(format!("`data` is not base64: {e}")))?;
        if bytes.len() > MAX_INPUT {
            return Err(ApiError::new(
                Code::TooLarge,
                format!(
                    "one input carries at most {MAX_INPUT} bytes; send {} in chunks",
                    bytes.len()
                ),
            ));
        }
        controls.push(Control::Stdin(bytes));
    }
    if let Some(r) = b.resize {
        controls.push(Control::Resize {
            rows: r.rows,
            cols: r.cols,
        });
    }
    if let Some(sig) = b.signal {
        controls.push(Control::Signal(sig));
    }
    if b.eof {
        controls.push(Control::Eof);
    }
    if let Some(f) = b.finish {
        controls.push(Control::Finish {
            bytes: f.bytes,
            sha256: f.sha256,
        });
    }
    for c in controls {
        if !s.control(c) {
            return Err(ApiError::conflict(
                "the session has ended; read its output for the end",
            ));
        }
    }
    Ok(Json(json!({"session_id": s.id, "accepted": true})))
}

#[derive(Debug, Deserialize)]
struct OutputQuery {
    #[serde(default)]
    since: u64,
    /// Seconds to wait for output; at most 60.
    #[serde(default)]
    wait: Option<u64>,
    /// The most bytes to return; at most 1 MiB.
    #[serde(default)]
    max: Option<usize>,
}

async fn session_output(
    State(state): State<Shared>,
    actor: Actor,
    Path((name, id)): Path<(String, String)>,
    ApiQuery(q): ApiQuery<OutputQuery>,
) -> ApiResult<Json<executor::Output>> {
    let s = state.executor.session(&actor, &name, &id)?;
    let wait = Duration::from_secs(q.wait.unwrap_or(0).min(MAX_WAIT_SECS));
    Ok(Json(
        s.read(q.since, wait, q.max.unwrap_or(MAX_READ)).await?,
    ))
}

fn is_websocket(req: &Request) -> bool {
    req.headers()
        .get(header::UPGRADE)
        .and_then(|v| v.to_str().ok())
        .is_some_and(|v| v.eq_ignore_ascii_case("websocket"))
}

/// `GET .../sessions/{id}/ws`: the session as a terminal WebSocket, with
/// the frames of the agent PTY (docs/api.md §6.5): binary or `input`
/// frames in, `resize` frames, binary output.
async fn session_ws(
    State(state): State<Shared>,
    actor: Actor,
    Path((name, id)): Path<(String, String)>,
    req: Request,
) -> ApiResult<Response> {
    let s = state.executor.session(&actor, &name, &id)?;
    if !is_websocket(&req) {
        return Err(ApiError::bad_request(
            "this endpoint is a WebSocket; send an upgrade request",
        ));
    }
    let (mut parts, _) = req.into_parts();
    let ws = WebSocketUpgrade::from_request_parts(&mut parts, &())
        .await
        .map_err(|e| ApiError::bad_request(e.body_text()))?;
    Ok(ws.on_upgrade(move |socket| ws_bridge(socket, s)))
}

async fn ws_bridge(socket: WebSocket, s: std::sync::Arc<ExecSession>) {
    let (out_tx, out_rx) = tokio::sync::mpsc::channel::<Vec<u8>>(16);
    let (in_tx, mut in_rx) = tokio::sync::mpsc::channel::<PtyInput>(16);
    let reader = {
        let s = s.clone();
        tokio::spawn(async move {
            let mut since = 0u64;
            loop {
                let Ok(o) = s.read(since, Duration::from_secs(30), MAX_READ).await else {
                    return;
                };
                since = o.next;
                for c in &o.chunks {
                    if let Ok(b) = base64::engine::general_purpose::STANDARD.decode(&c.data)
                        && out_tx.send(b).await.is_err()
                    {
                        return;
                    }
                }
                if o.done {
                    return;
                }
            }
        })
    };
    let writer = {
        let s = s.clone();
        tokio::spawn(async move {
            while let Some(i) = in_rx.recv().await {
                let c = match i {
                    PtyInput::Bytes(b) => Control::Stdin(b),
                    PtyInput::Resize { rows, cols } => Control::Resize { rows, cols },
                };
                if !s.control(c) {
                    return;
                }
            }
        })
    };
    super::streams::pty_session(
        socket,
        PtyAttachment {
            snapshot: Vec::new(),
            output: out_rx,
            input: in_tx,
        },
    )
    .await;
    reader.abort();
    writer.abort();
}
