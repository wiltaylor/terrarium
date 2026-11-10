//! The pipeline polls (docs/api.md §31).
//!
//! The scheduler task (`crate::scheduler`) calls [`tick`] every
//! [`TICK`], with the monitor tick. A tick finds the due pipelines and
//! moves their next poll on in one write, then starts one poll for each.
//! A poll checks the rights of the pipeline's `run_as` actor again, opens
//! the secrets of the source as that actor, reads the runs through the
//! adapter, and keeps them (`crate::store::pipelines::record`).
//!
//! The adapters:
//!
//! - `github` ([`github`]): the GitHub Actions REST API, from the daemon.
//! - `script` ([`contract`]): a user script, run with `sh -c` on a target
//!   through the executor (as a monitor runs), that prints the runs as
//!   JSON. This is the way to any other system: Octopus Deploy, Jenkins,
//!   GitLab, Azure DevOps.
//!
//! One poll of a pipeline at a time (a tick that finds one running
//! counts a skip), and at most [`DEFAULT_PARALLEL`] polls at once in the
//! daemon ([`ENV_PARALLEL`]).

pub mod contract;
pub mod github;

use std::collections::{BTreeMap, HashMap};
use std::sync::Arc;
use std::time::{Duration, Instant};

use chrono::{DateTime, Utc};
use tokio::task::JoinHandle;
use tracing::{debug, warn};
use zeroize::Zeroizing;

use crate::actor::Actor;
use crate::api::Shared;
use crate::error::{ApiError, ApiResult};
use crate::executor::{self, End, RemoteCmd};
use crate::monitors::{Claim, Output};
use crate::policy;
use crate::store::pipelines::{
    self, GithubSource, Pipeline, PipelineDef, PollSummary, ScriptSource, Source,
};
use crate::webhooks;
use contract::{RunInfo, RunStatus, Step};

/// The period of the pipeline tick.
pub const TICK: Duration = Duration::from_secs(5);
/// The most polls at once in the daemon.
pub const DEFAULT_PARALLEL: usize = 4;
/// `TERRARIUM_PIPELINE_PARALLEL`: another value for [`DEFAULT_PARALLEL`].
pub const ENV_PARALLEL: &str = "TERRARIUM_PIPELINE_PARALLEL";
/// The bytes of stdout and of stderr that a script poll reads.
pub const OUTPUT_MAX: usize = 1024 * 1024;
/// The bytes of stdout and of stderr that a refresh answer shows.
pub const SHOWN_MAX: usize = 8 * 1024;
/// The time past `timeout_secs` that the daemon waits for the target to
/// end the command itself.
const GRACE: Duration = Duration::from_secs(5);

/// The run state of the polls: which pipelines poll now, and the slots.
/// The same mechanism as the monitors, with its own slots.
pub struct Runtime {
    inner: Arc<crate::monitors::Runtime>,
}

impl Default for Runtime {
    fn default() -> Self {
        let cap = std::env::var(ENV_PARALLEL)
            .ok()
            .and_then(|v| v.trim().parse().ok())
            .filter(|n| *n > 0)
            .unwrap_or(DEFAULT_PARALLEL);
        Self {
            inner: Arc::new(crate::monitors::Runtime::with_cap(cap)),
        }
    }
}

impl Runtime {
    /// Mark pipeline `id` as polling. `None` when a poll of it runs.
    pub fn claim(&self, id: &str) -> Option<Claim> {
        self.inner.claim(id)
    }

    /// A poll of pipeline `id` is in progress.
    pub fn is_running(&self, id: &str) -> bool {
        self.inner.is_running(id)
    }

    async fn slot(&self) -> Option<tokio::sync::OwnedSemaphorePermit> {
        self.inner.slot().await
    }
}

/// One poll: the runs or the error, the raw output of a script (none
/// for `github`, or when the script did not start), the start, and the
/// duration.
#[derive(Debug, Clone)]
pub struct Fetch {
    pub runs: Result<Vec<RunInfo>, String>,
    pub output: Option<Output>,
    pub at: DateTime<Utc>,
    pub duration_ms: i64,
}

fn shown(b: &[u8]) -> String {
    let s = String::from_utf8_lossy(b);
    contract::cut(&s, SHOWN_MAX)
}

/// The network rules of the GitHub adapter: the webhooks' own.
fn rules(state: &Shared) -> github::NetRules {
    let (mode, own_ports, resolver) = state.webhooks.net_rules();
    github::NetRules {
        mode,
        own_ports,
        resolver,
    }
}

/// Open each secret of `source` as `actor`: the field name to the value.
async fn open_secrets(
    state: &Shared,
    actor: &Actor,
    project: &str,
    source: &Source,
) -> ApiResult<Vec<(String, Zeroizing<String>)>> {
    let names: Vec<(String, String)> = source
        .secrets()
        .into_iter()
        .map(|(f, n)| (f, n.to_owned()))
        .collect();
    if names.is_empty() {
        return Ok(Vec::new());
    }
    let (actor, project, dir) = (actor.clone(), project.to_owned(), state.state_dir.clone());
    state
        .db
        .read(move |c| {
            names
                .iter()
                .map(|(field, name)| {
                    webhooks::check_secret_use_in(
                        c,
                        &actor,
                        Some(&project),
                        name,
                        field,
                        "a pipeline",
                    )?;
                    let v = webhooks::open_value(c, &dir, Some(&project), name)?;
                    Ok((field.clone(), v))
                })
                .collect()
        })
        .await
}

/// The GitHub API of a source, with its token.
fn api<'a>(
    g: &'a GithubSource,
    token: Option<&'a str>,
    rules: &'a github::NetRules,
) -> github::Api<'a> {
    github::Api {
        base: g.api_url.as_deref().unwrap_or(github::DEFAULT_API),
        repo: &g.repo,
        token,
        rules,
    }
}

async fn fetch_github(
    state: &Shared,
    g: &GithubSource,
    max_runs: i64,
    token: Option<&str>,
    known: &HashMap<String, (RunStatus, bool)>,
) -> Result<Vec<RunInfo>, String> {
    let rules = rules(state);
    let api = api(g, token, &rules);
    let per_page = usize::try_from(max_runs).unwrap_or(20);
    let mut runs = api
        .runs(g.workflow.as_deref(), g.branch.as_deref(), per_page)
        .await?;
    // The jobs of the runs that changed, or that still run.
    let mut left = github::JOBS_PER_POLL;
    for r in &mut runs {
        if left == 0 {
            break;
        }
        let need = match known.get(&r.id) {
            None => true,
            Some((s, has)) => *s != r.status() || !r.status().is_final() || !has,
        };
        if !need {
            continue;
        }
        left -= 1;
        match api.jobs(&r.id).await {
            Ok(steps) => r.steps = Some(steps),
            Err(e) => debug!(run = %r.id, "the jobs of a GitHub run did not load: {e}"),
        }
    }
    Ok(runs)
}

async fn fetch_script(
    state: &Shared,
    actor: &Actor,
    project: &str,
    s: &ScriptSource,
    env: BTreeMap<String, String>,
    name: &str,
) -> (Result<Vec<RunInfo>, String>, Option<Output>) {
    let row = match executor::usable(state, actor, &s.target).await {
        Ok(r) => r,
        Err(e) => return (Err(e.message), None),
    };
    if let Some(p) = row.def.project.as_deref().filter(|p| *p != project) {
        return (
            Err(format!(
                "target `{}` belongs to project `{p}`; a pipeline uses global targets and those of its project",
                s.target
            )),
            None,
        );
    }
    if let Some(why) = crate::monitors::down(state, &row) {
        return (Err(format!("target down: {why}")), None);
    }
    let tdef = match executor::resolved_def(state, &row, Some(actor), None).await {
        Ok(d) => d,
        Err(e) => return (Err(e.message), None),
    };
    let mut env = env;
    env.insert("TERRARIUM_PROJECT".into(), project.to_owned());
    env.insert("TERRARIUM_PIPELINE".into(), name.to_owned());
    let cmd = RemoteCmd {
        argv: vec!["sh".into(), "-c".into(), s.script.clone()],
        env,
        timeout_secs: Some(s.timeout_secs),
        holder: Some(terrarium_host::native::service_holder(project, "pipelines")),
        ..Default::default()
    };
    let run = match executor::start_cmd(state, &tdef, &cmd) {
        Ok(r) => r,
        Err(e) => return (Err(e.message), None),
    };
    let limit = Duration::from_secs(s.timeout_secs) + GRACE;
    let timed_out = format!("timed out after {} s", s.timeout_secs);
    let Ok(c) = tokio::time::timeout(limit, run.collect(OUTPUT_MAX)).await else {
        return (Err(timed_out), None);
    };
    let exit_code = match &c.end {
        Some(End::Exit { code, .. }) => *code,
        _ => None,
    };
    let output = Output {
        exit_code,
        stdout: shown(&c.stdout),
        stderr: shown(&c.stderr),
        end: c.end.clone(),
    };
    let runs = match &c.end {
        Some(End::Exit {
            reason, code: None, ..
        }) if reason == "timeout" => Err(timed_out),
        Some(End::Exit { code: Some(0), .. }) => {
            contract::parse_output(&String::from_utf8_lossy(&c.stdout))
        }
        Some(End::Exit { code, .. }) => {
            let err = String::from_utf8_lossy(&c.stderr);
            let line = err.lines().rev().find(|l| !l.trim().is_empty());
            Err(format!(
                "the script exited with code {}{}",
                code.map_or_else(|| "none".to_owned(), |c| c.to_string()),
                line.map(|l| format!(": {}", contract::cut(l.trim(), 300)))
                    .unwrap_or_default()
            ))
        }
        Some(End::Error { code, message }) => Err(format!("{code}: {message}")),
        Some(End::Cp { .. }) | None => Err("the command ended without an exit".into()),
    };
    (runs, Some(output))
}

/// Poll `def` once as `actor`, for `project`. `known` is what the store
/// has (the GitHub adapter reads the jobs of the runs that changed).
/// Never fails: a refused right, a missing secret, a target that is
/// down, and an adapter error are each the error of the fetch.
pub async fn fetch(
    state: &Shared,
    actor: &Actor,
    project: &str,
    def: &PipelineDef,
    known: &HashMap<String, (RunStatus, bool)>,
) -> Fetch {
    let at = Utc::now();
    let started = Instant::now();
    let done = |runs: Result<Vec<RunInfo>, String>, output: Option<Output>| Fetch {
        runs,
        output,
        at,
        duration_ms: i64::try_from(started.elapsed().as_millis()).unwrap_or(i64::MAX),
    };
    if let Err(e) = policy::admin_project(actor, project, "polling a pipeline") {
        return done(
            Err(format!("{} (polls as {})", e.message, actor.label())),
            None,
        );
    }
    let secrets = match open_secrets(state, actor, project, &def.source).await {
        Ok(s) => s,
        Err(e) => return done(Err(e.message), None),
    };
    let Some(_slot) = state.pipelines.slot().await else {
        return done(Err("the pipeline runtime is closed".into()), None);
    };
    match &def.source {
        Source::Github(g) => {
            let token = secrets.first().map(|(_, v)| v.as_str());
            let runs = fetch_github(state, g, def.max_runs, token, known).await;
            done(runs, None)
        }
        Source::Script(s) => {
            let env: BTreeMap<String, String> = secrets
                .iter()
                .filter_map(|(field, v)| {
                    field
                        .strip_prefix("source.env_secrets.")
                        .map(|k| (k.to_owned(), v.as_str().to_owned()))
                })
                .collect();
            let (runs, output) = fetch_script(state, actor, project, s, env, &def.name).await;
            done(runs, output)
        }
    }
}

fn actor_of(conn: &rusqlite::Connection, run_as: &str) -> ApiResult<Option<Actor>> {
    webhooks::actor_of(conn, run_as)
}

/// Poll pipeline `id` once and keep the result. `claim` is its mark: the
/// caller took it with [`Runtime::claim`].
pub async fn run(
    state: &Shared,
    id: &str,
    claim: Claim,
) -> ApiResult<(PollSummary, Option<Output>)> {
    let key = id.to_owned();
    let (p, actor, known) = state
        .db
        .read(move |c| {
            let p = pipelines::get(c, &key)?;
            let actor = actor_of(c, &p.run_as)?;
            let known = pipelines::known(c, &key)?;
            Ok((p, actor, known))
        })
        .await?;
    let f = match actor {
        Some(a) => fetch(state, &a, &p.project, &p.def, &known).await,
        None => Fetch {
            runs: Err(format!(
                "the pipeline polls as `{}`, which no longer exists; save the pipeline again to poll as you",
                p.run_as
            )),
            output: None,
            at: Utc::now(),
            duration_ms: 0,
        },
    };
    let key = id.to_owned();
    let (runs, at) = (f.runs, f.at);
    let r = state
        .db
        .write("system", move |t| pipelines::record(t, &key, runs, at))
        .await;
    drop(claim);
    Ok((r?, f.output))
}

/// One started poll.
pub type Started = JoinHandle<ApiResult<(PollSummary, Option<Output>)>>;

/// Start the polls of the pipelines due at `now`. A pipeline whose poll
/// still runs gets a skip. Returns the started polls.
pub async fn tick(state: &Shared, now: DateTime<Utc>) -> Vec<Started> {
    let due = state
        .db
        .write("system", move |t| {
            let due = pipelines::due(t, now)?;
            for p in &due {
                pipelines::advance(t, &p.id, now)?;
            }
            Ok(due)
        })
        .await;
    let due = match due {
        Ok(d) => d,
        Err(e) => {
            warn!("the pipeline tick cannot read the due pipelines: {e}");
            return Vec::new();
        }
    };
    let mut started = Vec::new();
    let mut skipped = Vec::new();
    for p in due {
        match state.pipelines.claim(&p.id) {
            None => skipped.push(p.id),
            Some(claim) => {
                let st = state.clone();
                started.push(tokio::spawn(async move {
                    let r = run(&st, &p.id, claim).await;
                    if let Err(e) = &r {
                        warn!(pipeline = %p.id, "a pipeline poll failed to record: {e}");
                    }
                    r
                }));
            }
        }
    }
    if !skipped.is_empty() {
        let r = state
            .db
            .write("system", move |t| {
                for id in &skipped {
                    pipelines::add_skip(t, id)?;
                }
                Ok(())
            })
            .await;
        if let Err(e) = r {
            warn!("the pipeline tick cannot count the skips: {e}");
        }
    }
    started
}

/// The steps of run `run_id` of `p`, read now when the store has none and
/// the adapter can (`github`: the jobs of the run). `Ok(None)`: no steps.
pub async fn load_steps(
    state: &Shared,
    p: &Pipeline,
    run_id: &str,
) -> ApiResult<Option<Vec<Step>>> {
    let Source::Github(g) = &p.def.source else {
        return Ok(None);
    };
    let run_as = p.run_as.clone();
    let actor = state
        .db
        .read(move |c| actor_of(c, &run_as))
        .await?
        .ok_or_else(|| {
            ApiError::conflict(format!(
                "the pipeline polls as `{}`, which no longer exists",
                p.run_as
            ))
        })?;
    policy::admin_project(&actor, &p.project, "polling a pipeline")?;
    let secrets = open_secrets(state, &actor, &p.project, &p.def.source).await?;
    let token = secrets.first().map(|(_, v)| v.as_str());
    let rules = rules(state);
    let steps = api(g, token, &rules)
        .jobs(run_id)
        .await
        .map_err(|e| ApiError::new(crate::error::Code::Unavailable, e))?;
    let (id, rid, s2) = (p.id.clone(), run_id.to_owned(), steps.clone());
    state
        .db
        .write("system", move |t| pipelines::set_steps(t, &id, &rid, &s2))
        .await?;
    Ok(Some(steps))
}
