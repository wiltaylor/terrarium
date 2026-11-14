//! Services C3 (TER-1266, design D4): the run of a `script` service
//! request on the service's target.
//!
//! A pass every [`TICK`] takes each `running` request of a script service
//! that has not started its run (`script_dir` is NULL; C2 moves a request
//! to `running` when its resources are granted) and runs it in its own
//! task:
//!
//! 1. **Stage.** A new directory on the target,
//!    `${TMPDIR:-/tmp}/terrarium-service-<id>-XXXXXX` (`mktemp -d`, mode
//!    0700), with `request.json` (the request JSON), `payload` (the request
//!    payload, when there is one, its byte count checked), and an empty
//!    `out/`.
//! 2. **Run.** The service's `command`, no shell, with the directory as
//!    its working directory and the paths in its environment ([`env`]),
//!    plus `env_secrets` (env name to secret name). The time limit is what
//!    is left of `max_run_time` from the start of the request.
//! 3. **Collect.** Exit 0: the optional `response.json` (at most
//!    [`RESPONSE_JSON_MAX`] bytes, JSON) is the response JSON, and the files
//!    under `out/` are the response files, copied as one `tar` stream into
//!    `<state>/services/payloads/<id>/response/` ([`response_dir`]). Only
//!    regular files and directories with plain paths are kept; at most
//!    [`MAX_FILES`] files, and the bytes within the room that
//!    `max_retained` leaves. Their size is `response_size`, which counts
//!    toward the payload caps.
//! 4. **End.** Exit 0: `done`. Any other end: `failed`, with the exit code
//!    and the redacted end of the log in `error`. A request that another
//!    writer ends while it runs (a cancel, C2's `max_run_time`) has its
//!    process killed. The directory on the target is removed in each case.
//!
//! Secrets go by name only: the values are set in the process environment
//! on the target and are never on a command line, in the audit, or in a
//! log. The log tail is redacted with the run's own values first, then
//! the daemon matcher. A project service opens its project's secret, and
//! a global secret only when the project holds a grant of it (PLAN
//! §16b.6a: a project_admin defines a project service). A global service
//! opens global secrets.
//!
//! A run is never started twice (Q4 A, no retry): `script_dir` is set
//! before the run starts, and a `running` request with a `script_dir` and
//! no live run in this daemon (it restarted) ends as `failed`.

use std::collections::{BTreeMap, HashSet};
use std::path::{Component, Path, PathBuf};
use std::sync::{Arc, LazyLock, Mutex};
use std::time::Duration;

use chrono::{DateTime, Utc};
use rusqlite::{OptionalExtension, params};
use serde::Serialize;
use serde_json::{Value, json};
use terrarium_core::redact::{Redactor, StreamRedactor, View};
use terrarium_core::service::{Kind, State};
use tokio::io::AsyncWriteExt;
use tokio::task::JoinHandle;
use tracing::{info, warn};
use zeroize::Zeroizing;

use crate::api::Shared;
use crate::error::{ApiError, ApiResult};
use crate::executor::{self, Control, End, Ev, RemoteCmd, Running, Stream};
use crate::store::services::{self as store, Outcome, RequestRow, ServiceRow};

/// How often the pass runs.
pub const TICK: Duration = Duration::from_secs(5);
/// The largest `response.json`.
pub const RESPONSE_JSON_MAX: usize = terrarium_core::service::REQUEST_JSON_MAX;
/// The most response files of one request (as `crate::service_result`
/// lists, C5).
pub const MAX_FILES: usize = 10_000;
/// The end of the log kept while the script runs, after redaction.
const LOG_KEEP: usize = 64 * 1024;
/// The end of the log in `error` of a failed run.
pub const LOG_TAIL: usize = 4 * 1024;
/// The time limit of each staging, collect, and clean-up step.
const STEP_TIMEOUT: u64 = 600;
/// How often a run checks that its request is still `running`.
const CHECK_EVERY: Duration = Duration::from_secs(2);
/// A chunk of the payload on its way to the target.
const CHUNK: usize = 256 * 1024;

/// The requests whose run is live in this daemon.
static LIVE: LazyLock<Mutex<HashSet<String>>> = LazyLock::new(|| Mutex::new(HashSet::new()));

fn live() -> std::sync::MutexGuard<'static, HashSet<String>> {
    LIVE.lock().unwrap_or_else(|e| e.into_inner())
}

/// Is the run of request `id` live in this daemon?
pub fn is_live(id: &str) -> bool {
    live().contains(id)
}

/// One pass at a time.
static TICK_LOCK: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());

pub fn spawn(state: Shared) -> JoinHandle<()> {
    tokio::spawn(async move {
        loop {
            tokio::time::sleep(TICK).await;
            if let Err(e) = tick(&state).await {
                warn!("service script pass failed: {}", e.message);
            }
        }
    })
}

/// What one [`tick`] did.
#[derive(Debug, Default, Clone, PartialEq, Eq, Serialize)]
pub struct Tick {
    /// Requests whose run started, each in its own task.
    pub started: Vec<String>,
    /// Requests that ended as `failed`: their run was lost in a restart.
    pub lost: Vec<String>,
}

/// One pass: start the runs that are due, and end the lost ones.
pub async fn tick(state: &Shared) -> ApiResult<Tick> {
    let _one = TICK_LOCK.lock().await;
    let mut out = Tick::default();
    let rows = state
        .db
        .read(|c| {
            let mut stmt = c.prepare(
                "SELECT id, script_dir FROM service_requests WHERE state = 'running' ORDER BY id",
            )?;
            let rows = stmt
                .query_map([], |r| {
                    Ok((r.get::<_, String>(0)?, r.get::<_, Option<String>>(1)?))
                })?
                .collect::<Result<Vec<_>, _>>()?;
            let mut due = Vec::new();
            for (id, dir) in rows {
                let req = store::get_request(c, &id)?;
                // A gone service is C2's (it fails the request).
                let Some(svc) = store::find(c, &req.service)? else {
                    continue;
                };
                if svc.def.kind == Kind::Script {
                    due.push((req, svc, dir));
                }
            }
            Ok(due)
        })
        .await?;
    for (req, svc, dir) in rows {
        let id = req.id.clone();
        match dir {
            None => {
                if !live().insert(id.clone()) {
                    continue;
                }
                let claimed = {
                    let id = id.clone();
                    state
                        .db
                        .write("system", move |t| {
                            Ok(t.execute(
                                "UPDATE service_requests SET script_dir = ''
                                 WHERE id = ?1 AND state = 'running' AND script_dir IS NULL",
                                [&id],
                            )? == 1)
                        })
                        .await
                };
                match claimed {
                    Ok(true) => {
                        out.started.push(id.clone());
                        let st = state.clone();
                        tokio::spawn(async move {
                            run(&st, req, svc).await;
                            live().remove(&id);
                        });
                    }
                    Ok(false) => {
                        live().remove(&id);
                    }
                    Err(e) => {
                        live().remove(&id);
                        return Err(e);
                    }
                }
            }
            Some(dir) if !is_live(&id) => {
                let why = "the daemon restarted while the script ran; the run is not retried \
                           (send the request again)";
                if finish(state, &req, State::Failed, None, Some(why.to_owned()), None).await {
                    info!(request = %id, "service script run lost in a restart");
                    out.lost.push(id.clone());
                }
                if !dir.is_empty() {
                    let st = state.clone();
                    tokio::spawn(async move {
                        if let Ok(def) = target_def(&st, &svc).await {
                            remove_dir(&st, &def, &id, &dir).await;
                        }
                    });
                }
            }
            Some(_) => {}
        }
    }
    Ok(out)
}

/// The response files of request `id` (the directory that
/// `crate::service_result` of C5 lists).
pub fn response_dir(state_dir: &Path, id: &str) -> PathBuf {
    store::payload_dir(state_dir, id).join("response")
}

/// The environment of a run in `dir`, without the secrets.
pub fn env(svc: &str, id: &str, dir: &str, payload: bool) -> BTreeMap<String, String> {
    let mut e = BTreeMap::new();
    e.insert("TERRARIUM_SERVICE".into(), svc.to_owned());
    e.insert("TERRARIUM_SERVICE_REQUEST_ID".into(), id.to_owned());
    e.insert("TERRARIUM_SERVICE_DIR".into(), dir.to_owned());
    e.insert(
        "TERRARIUM_SERVICE_REQUEST".into(),
        format!("{dir}/request.json"),
    );
    if payload {
        e.insert("TERRARIUM_SERVICE_PAYLOAD".into(), format!("{dir}/payload"));
    }
    e.insert("TERRARIUM_SERVICE_OUTPUT".into(), format!("{dir}/out"));
    e.insert(
        "TERRARIUM_SERVICE_RESPONSE".into(),
        format!("{dir}/response.json"),
    );
    e
}

/// How a run ended, before it is written.
enum Fin {
    Done {
        response: Option<Value>,
    },
    Failed {
        exit_code: Option<i64>,
        error: String,
    },
    TimedOut(String),
    /// Another writer ended the request.
    Gone,
}

fn failed(error: impl Into<String>) -> Fin {
    Fin::Failed {
        exit_code: None,
        error: error.into(),
    }
}

async fn run(state: &Shared, req: RequestRow, svc: ServiceRow) {
    let mut dir = None;
    let fin = match run_steps(state, &req, &svc, &mut dir).await {
        Ok(f) => f,
        Err(e) => failed(e.message),
    };
    if let Some((def, d)) = &dir {
        remove_dir(state, def, &req.id, d).await;
    }
    let (to, exit, error, response) = match fin {
        Fin::Done { response } => (State::Done, Some(0), None, response),
        Fin::Failed { exit_code, error } => (State::Failed, exit_code, Some(error), None),
        Fin::TimedOut(e) => (State::TimedOut, None, Some(e), None),
        Fin::Gone => {
            info!(request = %req.id, "service script run stopped: the request ended");
            return;
        }
    };
    if finish(state, &req, to, exit, error, response).await {
        info!(request = %req.id, service = %req.service, state = %to, "service script run ended");
    }
}

/// End request `req` as `to`, with its audit row. `false` when another
/// writer ended it first.
async fn finish(
    state: &Shared,
    req: &RequestRow,
    to: State,
    exit_code: Option<i64>,
    error: Option<String>,
    response: Option<Value>,
) -> bool {
    let (id, service, project) = (req.id.clone(), req.service.clone(), req.project.clone());
    let r = state
        .db
        .write("system", move |t| {
            let cur = store::get_request(t, &id)?;
            if cur.state != State::Running {
                return Ok(false);
            }
            store::transition(
                t,
                &id,
                State::Running,
                to,
                Outcome {
                    response,
                    exit_code,
                    error,
                    ..Outcome::default()
                },
            )?;
            t.audit(
                "service.run",
                &service,
                project.as_deref(),
                json!({"request": id, "kind": "script", "state": to, "exit_code": exit_code}),
            )?;
            Ok(true)
        })
        .await;
    match r {
        Ok(b) => b,
        Err(e) => {
            warn!(request = %req.id, "cannot end a service script run: {}", e.message);
            false
        }
    }
}

async fn target_def(state: &Shared, svc: &ServiceRow) -> ApiResult<terrarium_core::config::TargetDef> {
    let name = svc.def.target.clone();
    let what = format!("service `{}` on the target", svc.def.name);
    let row = state
        .db
        .read(move |c| {
            crate::store::jobs::check_not_release_target(c, &name, &what)?;
            crate::store::targets::get(c, &name)
        })
        .await?;
    let def = executor::resolved_def(state, &row, None, None).await?;
    executor::no_exec(&def)?;
    if executor::path_kind(state, &def) != terrarium_host::protocol::PathKind::Posix {
        return Err(ApiError::invalid(format!(
            "target `{}` is not a Unix target: a script service runs with sh",
            svc.def.target
        )));
    }
    Ok(def)
}

/// Open the `env_secrets` of `svc`: `(env, secret name, value)`.
async fn open_secrets(
    state: &Shared,
    svc: &ServiceRow,
) -> ApiResult<Vec<(String, String, Zeroizing<String>)>> {
    if svc.def.env_secrets.is_empty() {
        return Ok(Vec::new());
    }
    let pairs: Vec<(String, String)> = svc
        .def
        .env_secrets
        .iter()
        .map(|(e, s)| (e.clone(), s.clone()))
        .collect();
    let project = svc.def.project.clone();
    let dir = state.state_dir.clone();
    state
        .db
        .read(move |c| {
            pairs
                .into_iter()
                .map(|(env, secret)| {
                    if let Some(p) = &project {
                        crate::secrets::check_global_use(c, p, &secret, false)?;
                    }
                    let v = crate::webhooks::open_value(c, &dir, project.as_deref(), &secret)?;
                    Ok((env, secret, v))
                })
                .collect()
        })
        .await
}

fn sh(script: &str, args: &[&str], timeout: u64) -> RemoteCmd {
    RemoteCmd {
        argv: ["sh", "-c", script, "sh"]
            .iter()
            .copied()
            .chain(args.iter().copied())
            .map(str::to_owned)
            .collect(),
        timeout_secs: Some(timeout),
        operator: true,
        ..RemoteCmd::default()
    }
}

/// Is `d` the directory that the staging of request `id` made?
fn our_dir(d: &str, id: &str) -> bool {
    d.starts_with('/')
        && !d.contains(['\n', '\r', '\0'])
        && d.rsplit('/')
            .next()
            .is_some_and(|last| last.starts_with(&format!("terrarium-service-{id}-")))
        && Path::new(d)
            .components()
            .all(|c| matches!(c, Component::RootDir | Component::Normal(_)))
}

fn tail_text(b: &[u8], max: usize) -> String {
    let s = String::from_utf8_lossy(&b[b.len().saturating_sub(max)..]).into_owned();
    s.trim().to_owned()
}

/// Send `bytes` to the stdin of `run`, then its end, and collect it.
async fn feed(run: Running, bytes: &[u8], max: usize) -> executor::Collected {
    let _ = run.ctl.send(Control::Stdin(bytes.to_vec()));
    let _ = run.ctl.send(Control::Eof);
    run.collect(max).await
}

fn step_error(what: &str, c: &executor::Collected) -> ApiError {
    let end = c.end();
    let why = match &end {
        End::Error { message, .. } => message.clone(),
        End::Exit { code, signal, .. } => match (code, signal) {
            (Some(c), _) => format!("exit {c}"),
            (_, Some(s)) => format!("signal {s}"),
            _ => "no exit code".into(),
        },
        End::Cp { .. } => "a copy end".into(),
    };
    let err = c.stderr_tail();
    ApiError::internal(if err.is_empty() {
        format!("{what} failed ({why})")
    } else {
        format!("{what} failed ({why}): {err}")
    })
}

async fn run_steps(
    state: &Shared,
    req: &RequestRow,
    svc: &ServiceRow,
    staged: &mut Option<(terrarium_core::config::TargetDef, String)>,
) -> ApiResult<Fin> {
    let started = req
        .started_at
        .as_deref()
        .and_then(|s| DateTime::parse_from_rfc3339(s).ok())
        .map_or_else(Utc::now, |d| d.with_timezone(&Utc));
    let limit = chrono::Duration::from_std(svc.def.max_run_time())
        .unwrap_or_else(|_| chrono::Duration::hours(1));
    let deadline = started + limit;
    let left = |now: DateTime<Utc>| u64::try_from((deadline - now).num_seconds()).unwrap_or(0);
    if left(Utc::now()) == 0 {
        return Ok(Fin::TimedOut(
            "the run passed the service's max run time before it started".into(),
        ));
    }
    let def = match target_def(state, svc).await {
        Ok(d) => d,
        Err(e) => return Ok(failed(e.message)),
    };
    let secrets = match open_secrets(state, svc).await {
        Ok(s) => s,
        Err(e) => return Ok(failed(e.message)),
    };

    // 1. Stage.
    let request = serde_json::to_vec(&req.request)?;
    let mk = executor::start_cmd(
        state,
        &def,
        &sh(
            "set -e; umask 077; d=$(mktemp -d \"${TMPDIR:-/tmp}/terrarium-service-$1-XXXXXX\"); \
             mkdir \"$d/out\"; cat > \"$d/request.json\"; printf '%s' \"$d\"",
            &[req.id.as_str()],
            STEP_TIMEOUT,
        ),
    )?;
    let c = feed(mk, &request, 4096).await;
    let d = String::from_utf8_lossy(&c.stdout).trim().to_owned();
    if !c.end().success() {
        return Ok(failed(step_error("making the run directory", &c).message));
    }
    if !our_dir(&d, &req.id) {
        return Ok(failed(format!(
            "making the run directory gave a bad path `{}`",
            d.escape_debug()
        )));
    }
    *staged = Some((def.clone(), d.clone()));
    {
        let (id, d) = (req.id.clone(), d.clone());
        state
            .db
            .write("system", move |t| {
                t.execute(
                    "UPDATE service_requests SET script_dir = ?2 WHERE id = ?1",
                    params![id, d],
                )?;
                Ok(())
            })
            .await?;
    }
    let has_payload = req.payload_size > 0;
    if has_payload {
        let file = store::payload_file(&state.state_dir, &req.id);
        match push_payload(state, &def, &d, &file, req.payload_size).await {
            Ok(()) => {}
            Err(e) => return Ok(failed(e.message)),
        }
    }

    // 2. Run.
    let mut env = env(&svc.def.name, &req.id, &d, has_payload);
    for (k, _, v) in &secrets {
        env.insert(k.clone(), v.to_string());
    }
    let cmd = RemoteCmd {
        argv: svc.def.command.clone(),
        cwd: Some(d.clone()),
        env,
        timeout_secs: Some(left(Utc::now()).max(1)),
        operator: true,
        ..RemoteCmd::default()
    };
    let own: Vec<(String, Zeroizing<String>)> = secrets
        .into_iter()
        .map(|(_, name, v)| (format!("service/{name}"), v))
        .collect();
    let running = match executor::start_cmd(state, &def, &cmd) {
        Ok(r) => r,
        Err(e) => return Ok(failed(format!("cannot start the script: {}", e.message))),
    };
    drop(cmd);
    let (end, log) = match watch(state, &req.id, running, &own).await? {
        Watched::Ended(end, log) => (end, log),
        Watched::Gone => return Ok(Fin::Gone),
    };
    drop(own);
    let code = match &end {
        End::Exit { code, .. } => code.map(i64::from),
        _ => None,
    };
    if Utc::now() >= deadline || matches!(&end, End::Exit { reason, .. } if reason == "timeout") {
        return Ok(Fin::TimedOut(with_log(
            "the run passed the service's max run time".into(),
            &log,
        )));
    }
    if code != Some(0) {
        let why = match &end {
            End::Exit {
                code: Some(c), ..
            } => format!("the script exited with code {c}"),
            End::Exit {
                signal: Some(s), ..
            } => format!("the script ended by signal {s}"),
            End::Error { message, .. } => format!("the script did not run: {message}"),
            _ => "the script ended with no exit code".into(),
        };
        return Ok(Fin::Failed {
            exit_code: code,
            error: with_log(why, &log),
        });
    }

    // 3. Collect.
    let response = match read_response_json(state, &def, &d).await {
        Ok(v) => v,
        Err(e) => {
            return Ok(Fin::Failed {
                exit_code: Some(0),
                error: e.message,
            });
        }
    };
    if let Err(e) = collect_output(state, req, svc, &def, &d).await {
        return Ok(Fin::Failed {
            exit_code: Some(0),
            error: e.message,
        });
    }
    Ok(Fin::Done { response })
}

fn with_log(why: String, log: &[u8]) -> String {
    let t = tail_text(log, LOG_TAIL);
    if t.is_empty() {
        why
    } else {
        format!("{why}; the end of its log:\n{t}")
    }
}

/// Stream payload `file` (`size` bytes) to `<d>/payload`.
async fn push_payload(
    state: &Shared,
    def: &terrarium_core::config::TargetDef,
    d: &str,
    file: &Path,
    size: u64,
) -> ApiResult<()> {
    use tokio::io::AsyncReadExt;
    let mut f = tokio::fs::File::open(file)
        .await
        .map_err(|e| ApiError::internal(format!("cannot read the request payload: {e}")))?;
    let run = executor::start_cmd(
        state,
        def,
        &sh(
            "cat > \"$1/payload\" && wc -c < \"$1/payload\"",
            &[d],
            STEP_TIMEOUT,
        ),
    )?;
    let mut buf = vec![0u8; CHUNK];
    loop {
        let n = f
            .read(&mut buf)
            .await
            .map_err(|e| ApiError::internal(format!("cannot read the request payload: {e}")))?;
        if n == 0 {
            break;
        }
        if run.ctl.send(Control::Stdin(buf[..n].to_vec())).is_err() {
            break;
        }
        tokio::task::yield_now().await;
    }
    let _ = run.ctl.send(Control::Eof);
    let c = run.collect(4096).await;
    if !c.end().success() {
        return Err(step_error("copying the payload to the target", &c));
    }
    let got = String::from_utf8_lossy(&c.stdout).trim().parse::<u64>().ok();
    if got != Some(size) {
        return Err(ApiError::internal(format!(
            "copying the payload to the target wrote {} bytes of {size}",
            got.map_or_else(|| "an unknown count of".into(), |n| n.to_string())
        )));
    }
    Ok(())
}

enum Watched {
    Ended(End, Vec<u8>),
    /// Another writer ended the request; the process was killed.
    Gone,
}

/// One output stream's redaction: the run's own values, then the daemon
/// matcher.
struct Redact {
    own: Option<StreamRedactor>,
    daemon: StreamRedactor,
}

impl Redact {
    fn new(own: Option<&Arc<Redactor>>, daemon: &Arc<Redactor>) -> Self {
        Self {
            own: own.map(|o| StreamRedactor::new(o.clone(), View::All)),
            daemon: StreamRedactor::new(daemon.clone(), View::Hidden),
        }
    }

    fn push(&mut self, b: &[u8]) -> Vec<u8> {
        match &mut self.own {
            Some(o) => {
                let mid = o.push(b);
                self.daemon.push(&mid)
            }
            None => self.daemon.push(b),
        }
    }

    fn finish(&mut self) -> Vec<u8> {
        let mut out = match &mut self.own {
            Some(o) => {
                let mid = o.finish();
                self.daemon.push(&mid)
            }
            None => Vec::new(),
        };
        out.extend(self.daemon.finish());
        out
    }
}

fn keep(log: &mut Vec<u8>, b: &[u8]) {
    log.extend_from_slice(b);
    if log.len() > LOG_KEEP {
        let cut = log.len() - LOG_KEEP;
        log.drain(..cut);
    }
}

/// Read the run to its end; the redacted end of its output is kept. Kill
/// it when its request is no longer `running`.
async fn watch(
    state: &Shared,
    id: &str,
    mut run: Running,
    own: &[(String, Zeroizing<String>)],
) -> ApiResult<Watched> {
    let own = executor::own_redactor(own);
    let daemon = state.db.redactor().get();
    let mut out = Redact::new(own.as_ref(), &daemon);
    let mut err = Redact::new(own.as_ref(), &daemon);
    let mut log = Vec::new();
    let _ = run.ctl.send(Control::Eof);
    let mut check = tokio::time::interval(CHECK_EVERY);
    check.tick().await;
    let mut killed = false;
    let end = loop {
        tokio::select! {
            ev = run.events.recv() => match ev {
                None => break End::Error {
                    code: "lost".into(),
                    message: "the run ended without an end".into(),
                },
                Some(Ev::Data(s, b)) => {
                    let r = if s == Stream::Stderr { &mut err } else { &mut out };
                    let safe = r.push(&b);
                    keep(&mut log, &safe);
                }
                Some(Ev::End(e)) => break e,
                Some(_) => {}
            },
            _ = check.tick(), if !killed => {
                let i = id.to_owned();
                let st = state.db.read(move |c| store::get_request(c, &i)).await?.state;
                if st != State::Running {
                    let _ = run.ctl.send(Control::Kill);
                    killed = true;
                }
            }
        }
    };
    for r in [&mut out, &mut err] {
        let rest = r.finish();
        keep(&mut log, &rest);
    }
    if killed {
        return Ok(Watched::Gone);
    }
    Ok(Watched::Ended(end, log))
}

/// The optional `<d>/response.json` as JSON.
async fn read_response_json(
    state: &Shared,
    def: &terrarium_core::config::TargetDef,
    d: &str,
) -> ApiResult<Option<Value>> {
    let run = executor::start_cmd(
        state,
        def,
        &sh(
            "f=\"$1/response.json\"; if [ -e \"$f\" ]; then [ -f \"$f\" ] && [ ! -L \"$f\" ] || \
             { echo 'response.json is not a regular file' >&2; exit 3; }; cat -- \"$f\"; \
             else printf none; fi",
            &[d],
            STEP_TIMEOUT,
        ),
    )?;
    let c = feed(run, &[], RESPONSE_JSON_MAX + 1).await;
    if !c.end().success() {
        return Err(step_error("reading response.json", &c));
    }
    if c.stdout == b"none" {
        return Ok(None);
    }
    if c.stdout.len() > RESPONSE_JSON_MAX {
        return Err(ApiError::invalid(format!(
            "response.json is over {RESPONSE_JSON_MAX} bytes"
        )));
    }
    serde_json::from_slice(&c.stdout)
        .map(Some)
        .map_err(|e| ApiError::invalid(format!("response.json is not JSON: {e}")))
}

/// The room for the response files of `req`: what `max_retained` of the
/// service and the global cap leave.
async fn room(state: &Shared, svc: &ServiceRow) -> ApiResult<u64> {
    let name = svc.def.name.clone();
    let (mine, all) = state
        .db
        .read(move |c| {
            Ok((
                store::retained_bytes(c, Some(&name))?,
                store::retained_bytes(c, None)?,
            ))
        })
        .await?;
    Ok(svc
        .def
        .max_retained_bytes()
        .saturating_sub(mine)
        .min(terrarium_core::service::GLOBAL_PAYLOAD_CAP.saturating_sub(all)))
}

/// Copy the files of `<d>/out` into the response directory of `req`.
async fn collect_output(
    state: &Shared,
    req: &RequestRow,
    svc: &ServiceRow,
    def: &terrarium_core::config::TargetDef,
    d: &str,
) -> ApiResult<()> {
    let room = room(state, svc).await?;
    let base = store::payload_dir(&state.state_dir, &req.id);
    tokio::fs::create_dir_all(&base)
        .await
        .map_err(|e| ApiError::internal(format!("cannot make the response directory: {e}")))?;
    let tar_path = base.join("response.tar.part");
    let part = base.join("response.part");
    let r = collect_into(state, def, d, &tar_path, &part, room).await;
    let _ = tokio::fs::remove_file(&tar_path).await;
    let total = match r {
        Ok(n) => n,
        Err(e) => {
            let _ = tokio::fs::remove_dir_all(&part).await;
            return Err(e);
        }
    };
    let dst = response_dir(&state.state_dir, &req.id);
    let _ = tokio::fs::remove_dir_all(&dst).await;
    tokio::fs::rename(&part, &dst)
        .await
        .map_err(|e| ApiError::internal(format!("cannot keep the response files: {e}")))?;
    let id = req.id.clone();
    state
        .db
        .write("system", move |t| {
            t.execute(
                "UPDATE service_requests SET response_size = ?2 WHERE id = ?1",
                params![id, i64::try_from(total).unwrap_or(i64::MAX)],
            )?;
            Ok(())
        })
        .await?;
    Ok(())
}

async fn collect_into(
    state: &Shared,
    def: &terrarium_core::config::TargetDef,
    d: &str,
    tar_path: &Path,
    part: &Path,
    room: u64,
) -> ApiResult<u64> {
    let io = |e: std::io::Error| ApiError::internal(format!("cannot store the response files: {e}"));
    // The tar stream: the file bytes, plus up to 1 KiB of header and
    // padding for each entry, plus the end blocks.
    let cap = room
        .saturating_add(u64::try_from(MAX_FILES * 2 + 16).unwrap_or(u64::MAX).saturating_mul(1024))
        .saturating_add(20 * 1024);
    let mut run = executor::start_cmd(
        state,
        def,
        &sh("cd -- \"$1/out\" && tar -cf - .", &[d], STEP_TIMEOUT),
    )?;
    let _ = run.ctl.send(Control::Eof);
    let mut f = tokio::fs::File::create(tar_path).await.map_err(io)?;
    let mut n = 0u64;
    let mut stderr = Vec::new();
    let end = loop {
        match run.events.recv().await {
            None => {
                break End::Error {
                    code: "lost".into(),
                    message: "the copy ended without an end".into(),
                };
            }
            Some(Ev::Data(Stream::Stderr, b)) => {
                stderr.extend_from_slice(&b[..b.len().min(4096usize.saturating_sub(stderr.len()))]);
            }
            Some(Ev::Data(_, b)) => {
                n += b.len() as u64;
                if n > cap {
                    let _ = run.ctl.send(Control::Kill);
                    return Err(ApiError::invalid(format!(
                        "the output files are over the {room} bytes that the service's \
                         max_retained leaves"
                    )));
                }
                f.write_all(&b).await.map_err(io)?;
            }
            Some(Ev::End(e)) => break e,
            Some(_) => {}
        }
    };
    f.flush().await.map_err(io)?;
    drop(f);
    if !end.success() {
        let c = executor::Collected {
            stderr,
            end: Some(end),
            ..Default::default()
        };
        return Err(step_error("copying the output files", &c));
    }
    let (t, p) = (tar_path.to_path_buf(), part.to_path_buf());
    tokio::task::spawn_blocking(move || unpack(&t, &p, room))
        .await
        .map_err(|e| ApiError::internal(format!("the unpack task failed: {e}")))?
}

/// Unpack tar file `tar` into the new directory `dst`: regular files and
/// directories with plain paths, at most [`MAX_FILES`] files of at most
/// `room` bytes together. Other entries (links, devices) are left out.
pub fn unpack(tar: &Path, dst: &Path, room: u64) -> ApiResult<u64> {
    use std::io::Read as _;
    let io = |e: std::io::Error| ApiError::internal(format!("cannot unpack the response files: {e}"));
    let _ = std::fs::remove_dir_all(dst);
    std::fs::create_dir_all(dst).map_err(io)?;
    let f = std::fs::File::open(tar).map_err(io)?;
    let mut a = tar::Archive::new(f);
    let (mut total, mut files) = (0u64, 0usize);
    for e in a.entries().map_err(io)? {
        let mut e = e.map_err(io)?;
        let path = e.path().map_err(io)?.into_owned();
        let mut rel = PathBuf::new();
        for c in path.components() {
            match c {
                Component::CurDir => {}
                Component::Normal(p) => rel.push(p),
                _ => {
                    return Err(ApiError::invalid(format!(
                        "the output has a bad path `{}`",
                        path.display()
                    )));
                }
            }
        }
        let kind = e.header().entry_type();
        if rel.as_os_str().is_empty() {
            continue;
        }
        let at = dst.join(&rel);
        if kind.is_dir() {
            std::fs::create_dir_all(&at).map_err(io)?;
        } else if kind.is_file() {
            files += 1;
            if files > MAX_FILES {
                return Err(ApiError::invalid(format!(
                    "the output has more than {MAX_FILES} files"
                )));
            }
            let size = e.size();
            total = total.saturating_add(size);
            if total > room {
                return Err(ApiError::invalid(format!(
                    "the output files are over the {room} bytes that the service's max_retained \
                     leaves"
                )));
            }
            if let Some(parent) = at.parent() {
                std::fs::create_dir_all(parent).map_err(io)?;
            }
            let mut w = std::fs::File::create(&at).map_err(io)?;
            let copied = std::io::copy(&mut (&mut e).take(size), &mut w).map_err(io)?;
            if copied != size {
                return Err(ApiError::internal("the output tar stream is cut"));
            }
        }
    }
    Ok(total)
}

/// Remove run directory `d` of request `id` on the target.
async fn remove_dir(state: &Shared, def: &terrarium_core::config::TargetDef, id: &str, d: &str) {
    if !our_dir(d, id) {
        return;
    }
    let r = match executor::start_cmd(state, def, &sh("rm -rf -- \"$1\"", &[d], STEP_TIMEOUT)) {
        Ok(run) => feed(run, &[], 4096).await,
        Err(e) => {
            warn!(request = %id, "cannot remove the run directory of a service script: {}", e.message);
            return;
        }
    };
    if !r.end().success() {
        warn!(request = %id, "cannot remove the run directory of a service script: {}", r.stderr_tail());
    }
}

/// The `script_dir` of request `id` (tests).
#[doc(hidden)]
pub async fn script_dir(state: &Shared, id: &str) -> ApiResult<Option<String>> {
    let id = id.to_owned();
    state
        .db
        .read(move |c| {
            Ok(c.query_row(
                "SELECT script_dir FROM service_requests WHERE id = ?1",
                [&id],
                |r| r.get::<_, Option<String>>(0),
            )
            .optional()?
            .flatten())
        })
        .await
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;

    #[test]
    fn the_run_dir_rule() {
        let id = "01M7ABC";
        assert!(our_dir("/tmp/terrarium-service-01M7ABC-x1Y2z3", id));
        assert!(our_dir("/var/tmp/a/terrarium-service-01M7ABC-q", id));
        for bad in [
            "",
            "tmp/terrarium-service-01M7ABC-x",
            "/tmp/terrarium-service-OTHER-x",
            "/tmp/terrarium-service-01M7ABC-x/..",
            "/tmp/../terrarium-service-01M7ABC-x",
            "/tmp/terrarium-service-01M7ABC-x\n/etc",
            "/",
        ] {
            assert!(!our_dir(bad, id), "{bad:?}");
        }
    }

    #[test]
    fn the_env_has_the_paths() {
        let e = env("video", "R1", "/tmp/d", true);
        assert_eq!(e["TERRARIUM_SERVICE_REQUEST"], "/tmp/d/request.json");
        assert_eq!(e["TERRARIUM_SERVICE_PAYLOAD"], "/tmp/d/payload");
        assert_eq!(e["TERRARIUM_SERVICE_OUTPUT"], "/tmp/d/out");
        assert_eq!(e["TERRARIUM_SERVICE_RESPONSE"], "/tmp/d/response.json");
        assert_eq!(e["TERRARIUM_SERVICE_REQUEST_ID"], "R1");
        assert!(!env("video", "R1", "/tmp/d", false).contains_key("TERRARIUM_SERVICE_PAYLOAD"));
    }

    fn tar_of(entries: &[(&str, tar::EntryType, &[u8])]) -> tempfile::NamedTempFile {
        let f = tempfile::NamedTempFile::new().unwrap();
        let mut b = tar::Builder::new(f.reopen().unwrap());
        for (path, kind, data) in entries {
            let mut h = tar::Header::new_gnu();
            h.set_entry_type(*kind);
            h.set_size(data.len() as u64);
            h.set_mode(0o644);
            if kind.is_symlink() {
                h.set_link_name("/etc/passwd").unwrap();
            }
            // `set_path` refuses `..`; write the name bytes as they are.
            let name = h.as_old_mut().name.as_mut();
            name[..path.len()].copy_from_slice(path.as_bytes());
            h.set_cksum();
            b.append(&h, *data).unwrap();
        }
        b.finish().unwrap();
        f
    }

    #[test]
    fn unpack_keeps_files_and_dirs_only() {
        use tar::EntryType as T;
        let t = tar_of(&[
            ("./", T::Directory, b""),
            ("./a.txt", T::Regular, b"hello"),
            ("./sub/", T::Directory, b""),
            ("./sub/b.bin", T::Regular, b"xyz"),
            ("./link", T::Symlink, b""),
        ]);
        let dir = tempfile::tempdir().unwrap();
        let dst = dir.path().join("response.part");
        assert_eq!(unpack(t.path(), &dst, 100).unwrap(), 8);
        assert_eq!(std::fs::read(dst.join("a.txt")).unwrap(), b"hello");
        assert_eq!(std::fs::read(dst.join("sub/b.bin")).unwrap(), b"xyz");
        assert!(std::fs::symlink_metadata(dst.join("link")).is_err());
    }

    #[test]
    fn unpack_refuses_escapes_and_size() {
        use tar::EntryType as T;
        let dir = tempfile::tempdir().unwrap();
        let t = tar_of(&[("../evil", T::Regular, b"x")]);
        let e = unpack(t.path(), &dir.path().join("p1"), 100).unwrap_err();
        assert!(e.message.contains("bad path"), "{}", e.message);
        assert!(!dir.path().join("evil").exists());
        let t = tar_of(&[("/abs", T::Regular, b"x")]);
        assert!(unpack(t.path(), &dir.path().join("p2"), 100).is_err());
        let t = tar_of(&[("./a", T::Regular, b"12345"), ("./b", T::Regular, b"6")]);
        let e = unpack(t.path(), &dir.path().join("p3"), 5).unwrap_err();
        assert!(e.message.contains("over the 5 bytes"), "{}", e.message);
    }
}
