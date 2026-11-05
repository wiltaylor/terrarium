//! The run of a tool proxy (GitHub #21, the wiki page `design/tool-proxies`
//! D4 and D6, TER-30): `POST /api/tools/{name}/run`.
//!
//! The caller is an agent with a grant on the tool (its own or its
//! project's), or the operator for a test. The daemon checks the agent's
//! arguments ([`terrarium_core::tool::check_args`]), takes a token of the
//! rate of the (agent, tool) pair, and builds the command itself: the
//! tool's argv with the agent's arguments appended as more entries, the
//! tool's `cwd`, `timeout`, and `env_secrets`, no shell, and no pty. The
//! agent sets none of these. The run is an exec session of the kind `tool`
//! ([`crate::executor::start_tool_exec`]): the output is capped at
//! `max_output` and redacted as a stream, with the run's own secret values
//! first. The caller reads it as any exec session it owns.
//!
//! Each run writes one `tool.run` audit row at its end and two `tool.run`
//! events (started, ended) on the agent's stream. The output is not in
//! the audit.

use std::collections::{BTreeMap, HashMap};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use serde_json::json;
use terrarium_core::tool::{self as core_tool, ToolDef};
use zeroize::Zeroizing;

use crate::actor::Actor;
use crate::api::Shared;
use crate::error::{ApiError, ApiResult, Code};
use crate::events::EventDraft;
use crate::executor::{self, ExecSession, RemoteCmd, ToolEnd, ToolRun};
use crate::store::tools::{self as store, ToolRow};

/// The rate buckets of the (agent, tool) pairs, and the stdin rule of the
/// live tool sessions.
#[derive(Default)]
pub struct Rates {
    buckets: Mutex<HashMap<(String, String), Bucket>>,
    /// Session ID to `stdin` of its tool, while the run is live.
    stdin: Mutex<HashMap<String, bool>>,
}

/// A token bucket: `n` runs at once, then `n` each `per`.
struct Bucket {
    tokens: f64,
    last: Instant,
}

impl Rates {
    /// Does tool session `id` take stdin? `None`: not a live tool run.
    pub fn takes_stdin(&self, id: &str) -> Option<bool> {
        self.stdin
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .get(id)
            .copied()
    }

    fn set_stdin(&self, id: &str, on: Option<bool>) {
        let mut m = self.stdin.lock().unwrap_or_else(|e| e.into_inner());
        match on {
            Some(v) => {
                m.insert(id.to_owned(), v);
            }
            None => {
                m.remove(id);
            }
        }
    }

    /// Take one run of `tool` for `caller` at the rate `(n, per)`. `Err`
    /// with the wait until the next token.
    fn take(&self, caller: &str, tool: &str, (n, per): (u32, Duration)) -> Result<(), Duration> {
        let mut b = self.buckets.lock().unwrap_or_else(|e| e.into_inner());
        let now = Instant::now();
        let cap = f64::from(n);
        let rate = cap / per.as_secs_f64();
        let e = b
            .entry((caller.to_owned(), tool.to_owned()))
            .or_insert(Bucket {
                tokens: cap,
                last: now,
            });
        e.tokens = (e.tokens + now.duration_since(e.last).as_secs_f64() * rate).min(cap);
        e.last = now;
        if e.tokens >= 1.0 {
            e.tokens -= 1.0;
            Ok(())
        } else {
            Err(Duration::from_secs_f64((1.0 - e.tokens) / rate))
        }
    }
}

/// The started run: the session, and where it runs.
pub struct Started {
    pub session: Arc<ExecSession>,
    pub tool: String,
    pub target: String,
}

/// Run tool `name` for `actor` with the agent arguments `argv`.
pub async fn run(state: &Shared, actor: &Actor, name: &str, argv: Vec<String>) -> ApiResult<Started> {
    let key = name.to_owned();
    let agent = actor.agent().map(|a| (a.id.clone(), a.project.clone()));
    let (row, granted) = state
        .db
        .read(move |c| {
            let row = store::find(c, &key)?;
            let granted = match &agent {
                Some((id, project)) => store::granted_to(c, id, project)?
                    .iter()
                    .any(|t| t.def.name == key),
                None => false,
            };
            Ok((row, granted))
        })
        .await?;
    // An agent with no grant learns nothing of the tool: the same `404` as
    // a tool that does not exist.
    let row: ToolRow = match (row, actor) {
        (Some(r), Actor::Operator) => r,
        (Some(r), Actor::Agent(_)) if granted => r,
        _ => {
            return Err(ApiError::not_found(format!(
                "no tool `{name}` that you may run; `terrarium tool ls` lists yours"
            )));
        }
    };
    let def = row.def;
    core_tool::check_args(&def.args, &argv).map_err(ApiError::invalid)?;
    let caller = actor.label();
    let rate = match &def.rate {
        Some(r) => core_tool::parse_rate(r).map_err(ApiError::invalid)?,
        None => core_tool::DEFAULT_RATE,
    };
    if let Err(wait) = state.tool_rates.take(&caller, &def.name, rate) {
        return Err(ApiError::new(
            Code::RateLimited,
            format!(
                "tool `{}` runs at most {} times per {}s for you; try again in {}s",
                def.name,
                rate.0,
                rate.1.as_secs(),
                wait.as_secs().max(1)
            ),
        )
        .with_details(json!({"retry_after": wait.as_secs().max(1)})));
    }
    let secrets = open_secrets(state, &def).await?;
    let env: BTreeMap<String, String> = secrets
        .iter()
        .map(|(env, _, v)| (env.clone(), v.to_string()))
        .collect();
    let cmd = RemoteCmd {
        argv: def.command.iter().cloned().chain(argv.iter().cloned()).collect(),
        cwd: def.cwd.clone(),
        env,
        pty: None,
        timeout_secs: timeout_secs(&def)?,
        shell: false,
        agent: None,
        holder: None,
    };
    let max_output = match &def.max_output {
        Some(m) => terrarium_core::skills::parse_size(m)
            .ok_or_else(|| ApiError::invalid(format!("bad max_output `{m}`")))?,
        None => core_tool::DEFAULT_OUTPUT_MAX,
    };
    let view = match actor {
        Actor::Agent(a) => terrarium_core::redact::View::Scope(a.project.clone()),
        Actor::Operator => terrarium_core::redact::View::All,
    };
    let started_at = Instant::now();
    let st = state.clone();
    let (tool, target) = (def.name.clone(), def.target.clone());
    let audit_argv = argv.clone();
    let (agent_id, agent_name) = match actor {
        Actor::Agent(a) => (Some(a.id.clone()), Some(format!("{}/{}", a.project, a.name))),
        Actor::Operator => (None, None),
    };
    let project = def.project.clone().or_else(|| actor.agent().map(|a| a.project.clone()));
    let label = caller.clone();
    let sid_cell: Arc<Mutex<Option<String>>> = Arc::default();
    let ended = {
        let sid_cell = sid_cell.clone();
        let (tool, target, project) = (tool.clone(), target.clone(), project.clone());
        let (agent_id, agent_name) = (agent_id.clone(), agent_name.clone());
        Box::new(move |e: ToolEnd| {
            if let Some(id) = sid_cell.lock().unwrap_or_else(|e| e.into_inner()).take() {
                st.tool_rates.set_stdin(&id, None);
            }
            tokio::spawn(async move {
                record_end(
                    &st, label, tool, target, project, agent_id, agent_name, audit_argv,
                    started_at, e,
                )
                .await;
            });
        })
    };
    let session = executor::start_tool_exec(
        state,
        actor,
        &def.target,
        cmd,
        ToolRun {
            stdin: def.stdin,
            max_output,
            secrets: secrets
                .into_iter()
                .map(|(_, secret, v)| (format!("tool/{secret}"), v))
                .collect(),
            view,
            on_end: ended,
        },
    )
    .await?;
    state.tool_rates.set_stdin(&session.id, Some(def.stdin));
    *sid_cell.lock().unwrap_or_else(|e| e.into_inner()) = Some(session.id.clone());
    if session.is_done() {
        state.tool_rates.set_stdin(&session.id, None);
    }
    let (sid, t, tg, p) = (session.id.clone(), tool.clone(), target.clone(), project.clone());
    let (aid, aname) = (agent_id, agent_name);
    let argv_n = argv.len();
    state
        .db
        .write(caller, move |t_| {
            let mut ev = EventDraft::new(
                "tool.run",
                format!("tool {t} started on {tg}"),
                json!({"change": "started", "tool": t, "target": tg, "session": sid,
                       "args": argv_n}),
            );
            if let (Some(id), Some(n)) = (aid, aname) {
                ev = ev.agent(id, n);
            }
            if let Some(p) = p {
                ev = ev.project(p);
            }
            t_.emit(ev)?;
            Ok(())
        })
        .await?;
    Ok(Started {
        session,
        tool,
        target,
    })
}

/// The `timeout` of a tool in seconds, if it has one.
fn timeout_secs(def: &ToolDef) -> ApiResult<Option<u64>> {
    def.timeout
        .as_deref()
        .map(|t| {
            terrarium_core::eta::parse_duration(t)
                .map(|d| u64::try_from(d.num_seconds()).unwrap_or(0).max(1))
                .map_err(|e| ApiError::invalid(format!("timeout: {e}")))
        })
        .transpose()
}

/// Open the `env_secrets` of `def`: `(env, secret name, value)`. The scope
/// is the tool's: a project tool reads its project's secret, else the
/// global one.
async fn open_secrets(
    state: &Shared,
    def: &ToolDef,
) -> ApiResult<Vec<(String, String, Zeroizing<String>)>> {
    if def.env_secrets.is_empty() {
        return Ok(Vec::new());
    }
    let pairs: Vec<(String, String)> = def
        .env_secrets
        .iter()
        .map(|(e, s)| (e.clone(), s.clone()))
        .collect();
    let project = def.project.clone();
    let dir = state.state_dir.clone();
    state
        .db
        .read(move |c| {
            pairs
                .into_iter()
                .map(|(env, secret)| {
                    let v = crate::webhooks::open_value(c, &dir, project.as_deref(), &secret)?;
                    Ok((env, secret, v))
                })
                .collect()
        })
        .await
}

/// The `tool.run` audit row and the `ended` event of a run.
#[allow(clippy::too_many_arguments)]
async fn record_end(
    state: &Shared,
    label: String,
    tool: String,
    target: String,
    project: Option<String>,
    agent_id: Option<String>,
    agent_name: Option<String>,
    argv: Vec<String>,
    started_at: Instant,
    e: ToolEnd,
) {
    let exit = match &e.end {
        executor::End::Exit { code, .. } => json!(code),
        _ => serde_json::Value::Null,
    };
    let duration_ms = u64::try_from(started_at.elapsed().as_millis()).unwrap_or(u64::MAX);
    let r = state
        .db
        .write(label, move |t| {
            let argv: Vec<String> = argv.into_iter().map(|a| t.redact_string(a)).collect();
            let detail = json!({"tool": tool, "target": target, "agent": agent_name,
                "argv": argv, "exit": exit, "end": e.end, "duration_ms": duration_ms,
                "bytes": e.bytes, "truncated": e.truncated});
            t.audit("tool.run", &tool, project.as_deref(), detail.clone())?;
            let mut ev = EventDraft::new(
                "tool.run",
                format!("tool {tool} ended on {target}"),
                json!({"change": "ended", "tool": tool, "target": target, "exit": exit,
                       "duration_ms": duration_ms, "bytes": e.bytes,
                       "truncated": e.truncated}),
            );
            if let (Some(id), Some(n)) = (agent_id, agent_name) {
                ev = ev.agent(id, n);
            }
            if let Some(p) = project {
                ev = ev.project(p);
            }
            t.emit(ev)?;
            Ok(())
        })
        .await;
    if let Err(err) = r {
        tracing::warn!("cannot record the end of a tool run: {err}");
    }
}
