//! The update and the rollback (docs/api.md §34.5).
//!
//! An update moves the terrarium container to a release image:
//!
//! 1. `inspect`: find the terrarium container and its image.
//! 2. `resolve`: the tag (`latest`, `pre`, or a version) to a digest. The
//!    container runs that digest already: the run ends as `current`.
//! 3. `verify`: the signature or provenance of `<repo>@<digest>`
//!    ([`Verifier`]). A failed check stops the run (fail closed). The mode
//!    `off` skips it, and the step says so.
//! 4. `pull`: the image by digest. Before the drain, so that a slow or
//!    failed pull does not stop work.
//! 5. `drain`: `terrarium admin update drain` in the container: no new
//!    turns; the running turns get [`Config::drain_wait`] to end.
//! 6. `backup`: `terrariumd backup create` in the container (a full
//!    backup). Migrations only go forward, so the way back needs it.
//! 7. `stop`, then `remove`, then `start`: a new container with the same
//!    settings and the new image.
//! 8. `health`: the health check passes and `terrarium daemon status`
//!    answers, within [`Config::health_timeout`].
//!
//! A failure in 1 to 6 changes nothing: the drain ends (`resume`) and the
//! old container runs on (`failed`). A failure from 7 on goes back: the
//! updater removes the new container, restores the backup of step 6 with
//! the old image (offline, in a one-off container), and starts the old
//! image again (`rolled_back`, or `rollback_failed` when that fails too).
//!
//! A rollback is the same switch to the image before the last switch
//! (`previous.json`). It also restores the backup that the last switch
//! made, because the newer daemon can have migrated the database: the
//! data written since that switch is lost (it stays in the backup that the
//! rollback makes first).

use std::sync::{Arc, Mutex};
use std::time::Duration;

use chrono::Utc;

use crate::engine::{Container, Engine, Health, Registry, Res, Verifier};
use crate::protocol::{Image, Outcome, Request, Response, Run, RunKind, Status, Step, StepState};
use crate::store::{Previous, Store};

/// The settings of the updater.
#[derive(Debug, Clone)]
pub struct Config {
    /// The image repository of the releases.
    pub repository: String,
    /// How long the drain waits for the running turns.
    pub drain_wait: Duration,
    /// How long the new container has to become healthy.
    pub health_timeout: Duration,
    /// The time between two health reads.
    pub health_poll: Duration,
    /// The build of the updater, for the status.
    pub version: String,
}

/// The default image repository.
pub const DEFAULT_REPOSITORY: &str = "ghcr.io/wiltaylor/terrarium";
const HOT_SWAP_HOLD_LIMIT: Duration = Duration::from_secs(900);
const HOT_SWAP_RECOVERY_RESERVE: Duration = Duration::from_secs(60);

fn budget_left(deadline: tokio::time::Instant, reserve: Duration) -> Res<Duration> {
    deadline
        .saturating_duration_since(tokio::time::Instant::now())
        .checked_sub(reserve)
        .filter(|left| *left > Duration::from_millis(10))
        .ok_or_else(|| "hot swap would exceed the 15-minute runner hold budget".into())
}

fn bounded_command(argv: Vec<String>, deadline: tokio::time::Instant) -> Res<Vec<String>> {
    let left = budget_left(deadline, Duration::from_secs(5))?;
    let mut bounded = vec![
        "timeout".into(),
        "--kill-after=5".into(),
        format!("{:.3}s", left.as_secs_f64()),
    ];
    bounded.extend(argv);
    Ok(bounded)
}

impl Default for Config {
    fn default() -> Self {
        Self {
            repository: DEFAULT_REPOSITORY.into(),
            drain_wait: Duration::from_secs(600),
            health_timeout: Duration::from_secs(900),
            health_poll: Duration::from_secs(2),
            version: String::new(),
        }
    }
}

/// The image tag of a request target: `latest` and `pre` as they are, a
/// version without its `v`. `None` for a target that is no tag.
pub fn tag_of(target: &str) -> Option<String> {
    let t = target.trim();
    let t = t
        .strip_prefix('v')
        .filter(|r| r.starts_with(|c: char| c.is_ascii_digit()))
        .unwrap_or(t);
    let ok = !t.is_empty()
        && t.len() <= 128
        && t.chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '-' | '_'))
        && t.starts_with(|c: char| c.is_ascii_alphanumeric());
    ok.then(|| t.to_owned())
}

/// The command that drains the daemon (the CLI in the container talks to
/// the daemon's own Unix socket, as the operator).
pub fn drain_argv(wait: Duration) -> Vec<String> {
    vec![
        "terrarium".into(),
        "admin".into(),
        "update".into(),
        "drain".into(),
        "--wait".into(),
        wait.as_secs().to_string(),
    ]
}

/// The command that ends a drain.
pub fn resume_argv() -> Vec<String> {
    ["terrarium", "admin", "update", "resume"]
        .map(String::from)
        .to_vec()
}

/// The command that makes the backup.
pub fn backup_argv() -> Vec<String> {
    ["terrariumd", "backup", "create", "--json"]
        .map(String::from)
        .to_vec()
}

/// The command that restores `path` (the daemon is stopped).
pub fn restore_argv(path: &str) -> Vec<String> {
    vec![
        "terrariumd".into(),
        "backup".into(),
        "restore".into(),
        path.into(),
        "--json".into(),
    ]
}

/// The command that checks the daemon after the start.
pub fn status_argv() -> Vec<String> {
    ["terrarium", "daemon", "status"].map(String::from).to_vec()
}

pub fn hot_argv(command: &str, args: &[&str]) -> Vec<String> {
    ["python3", "/usr/local/lib/terrarium-hot-swap.py", command]
        .into_iter()
        .chain(args.iter().copied())
        .map(str::to_owned)
        .collect()
}

/// The updater: one run at a time.
pub struct Updater {
    engine: Arc<dyn Engine>,
    registry: Arc<dyn Registry>,
    verifier: Arc<dyn Verifier>,
    store: Store,
    cfg: Config,
    /// The run in progress, with its steps so far.
    running: Mutex<Option<Run>>,
    /// Held while a run runs.
    busy: Arc<tokio::sync::Mutex<()>>,
}

fn now() -> String {
    Utc::now().to_rfc3339_opts(chrono::SecondsFormat::Millis, true)
}

/// A failure of a step: the name and the reason.
struct Fail(String);

impl Updater {
    pub fn new(
        engine: Arc<dyn Engine>,
        registry: Arc<dyn Registry>,
        verifier: Arc<dyn Verifier>,
        store: Store,
        cfg: Config,
    ) -> Self {
        Self {
            engine,
            registry,
            verifier,
            store,
            cfg,
            running: Mutex::new(None),
            busy: Arc::new(tokio::sync::Mutex::new(())),
        }
    }

    fn running(&self) -> std::sync::MutexGuard<'_, Option<Run>> {
        self.running
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }

    /// The answer of a status request.
    pub async fn status(&self) -> Status {
        let (current, current_error) = match self.engine.target().await {
            Ok(c) => (Some(c.image), None),
            Err(e) => (None, Some(e)),
        };
        Status {
            updater_version: self.cfg.version.clone(),
            repository: self.cfg.repository.clone(),
            verify: self.verifier.mode(),
            current,
            current_error,
            previous: self.store.previous().map(|p| p.image),
            running: self.running().clone(),
            history: self.store.history(),
        }
    }

    /// Answer one request. An update or a rollback runs in the background.
    pub async fn handle(self: &Arc<Self>, req: Request) -> Response {
        let (kind, target, actor) = match req {
            Request::Status => return Response::Status(Box::new(self.status().await)),
            Request::Update { target, actor } => (RunKind::Update, target, actor),
            Request::Rollback { actor } => (RunKind::Rollback, "previous".to_owned(), actor),
        };
        match self.begin(kind, &target, &actor) {
            Ok((run, guard)) => {
                let me = self.clone();
                let id = run.id.clone();
                tokio::spawn(async move {
                    let _guard = guard;
                    me.execute(run).await;
                });
                Response::Accepted { run: id }
            }
            Err(r) => r,
        }
    }

    /// Check the request and take the run lock.
    fn begin(
        &self,
        kind: RunKind,
        target: &str,
        actor: &str,
    ) -> Result<(Run, tokio::sync::OwnedMutexGuard<()>), Response> {
        if kind == RunKind::Update && tag_of(target).is_none() {
            return Err(Response::Error {
                message: format!("`{target}` is not a version, `latest`, or `pre`"),
                busy: false,
            });
        }
        let Ok(guard) = self.busy.clone().try_lock_owned() else {
            return Err(Response::Error {
                message: "an update or a rollback runs now; wait for its end".into(),
                busy: true,
            });
        };
        let run = Run {
            id: ulid::Ulid::new().to_string(),
            kind,
            actor: if actor.is_empty() {
                "unknown".into()
            } else {
                actor.to_owned()
            },
            target: target.to_owned(),
            from: None,
            to: None,
            backup: None,
            outcome: Outcome::Running,
            error: None,
            steps: Vec::new(),
            started_at: now(),
            ended_at: None,
        };
        *self.running() = Some(run.clone());
        Ok((run, guard))
    }

    /// Run an update or a rollback to its end, in this task (the tests;
    /// [`Updater::handle`] spawns it). Returns the record.
    pub async fn run_now(self: &Arc<Self>, kind: RunKind, target: &str, actor: &str) -> Run {
        match self.begin(kind, target, actor) {
            Ok((run, _guard)) => self.execute(run).await,
            Err(r) => Run {
                id: String::new(),
                kind,
                actor: actor.into(),
                target: target.into(),
                from: None,
                to: None,
                backup: None,
                outcome: Outcome::Failed,
                error: Some(match r {
                    Response::Error { message, .. } => message,
                    _ => "the request was refused".into(),
                }),
                steps: Vec::new(),
                started_at: now(),
                ended_at: Some(now()),
            },
        }
    }

    async fn execute(&self, run: Run) -> Run {
        let id = run.id.clone();
        tracing::info!(run = %id, kind = ?run.kind, target = %run.target, actor = %run.actor, "run starts");
        let outcome = self.switch(run.kind, &run.target).await;
        let mut done = self.running().take().unwrap_or(run);
        match outcome {
            Ok(o) => done.outcome = o,
            Err((o, Fail(e))) => {
                done.outcome = o;
                done.error = Some(e);
            }
        }
        done.ended_at = Some(now());
        tracing::info!(run = %id, outcome = done.outcome.as_str(), error = ?done.error, "run ends");
        if let Err(e) = self.store.push(&done) {
            tracing::warn!("cannot write the history: {e}");
        }
        done
    }

    /// Change the record of the run in progress.
    fn with_run(&self, f: impl FnOnce(&mut Run)) {
        if let Some(r) = self.running().as_mut() {
            f(r);
        }
    }

    fn begin_step(&self, name: &str) -> usize {
        let mut idx = 0;
        self.with_run(|r| {
            r.steps.push(Step {
                name: name.into(),
                state: StepState::Running,
                detail: String::new(),
                at: now(),
            });
            idx = r.steps.len() - 1;
        });
        tracing::info!(step = name, "step");
        idx
    }

    fn end_step(&self, idx: usize, state: StepState, detail: impl Into<String>) {
        let detail = detail.into();
        self.with_run(|r| {
            if let Some(s) = r.steps.get_mut(idx) {
                s.state = state;
                s.detail = detail.clone();
            }
        });
        if state == StepState::Failed {
            tracing::warn!(detail = %detail, "step failed");
        }
    }

    /// Run `fut` as the step `name`: `ok` makes the detail of a success.
    async fn step<T>(
        &self,
        name: &str,
        fut: impl std::future::Future<Output = Res<T>>,
        ok: impl FnOnce(&T) -> String,
    ) -> Result<T, Fail> {
        let idx = self.begin_step(name);
        match fut.await {
            Ok(v) => {
                self.end_step(idx, StepState::Ok, ok(&v));
                Ok(v)
            }
            Err(e) => {
                self.end_step(idx, StepState::Failed, e.clone());
                Err(Fail(format!("{name}: {e}")))
            }
        }
    }

    fn skip(&self, name: &str, detail: &str) {
        let idx = self.begin_step(name);
        self.end_step(idx, StepState::Skipped, detail);
    }

    /// Run a command in the container; a non-zero exit is a failure.
    async fn exec_ok(&self, id: &str, argv: Vec<String>) -> Res<String> {
        let out = self.engine.exec(id, argv).await?;
        if out.code == 0 {
            Ok(out.stdout)
        } else {
            Err(out.reason())
        }
    }

    /// End the drain of the old daemon after an early failure.
    async fn resume(&self, cur: &Container) {
        let r = self.exec_ok(&cur.id, resume_argv()).await;
        let idx = self.begin_step("resume");
        match r {
            Ok(_) => self.end_step(idx, StepState::Ok, "the daemon takes new turns again"),
            Err(e) => self.end_step(idx, StepState::Failed, e),
        }
    }

    async fn switch(&self, kind: RunKind, target: &str) -> Result<Outcome, (Outcome, Fail)> {
        let failed = |f: Fail| (Outcome::Failed, f);
        let cur = self
            .step("inspect", self.engine.target(), |c| {
                format!("container {} runs {}", c.name, c.image.reference)
            })
            .await
            .map_err(failed)?;
        self.with_run(|r| r.from = Some(cur.image.clone()));

        // The image to switch to, and the backup to restore first.
        let (to, restore) = match kind {
            RunKind::Update => {
                let tag = tag_of(target).unwrap_or_default();
                let repo = self.cfg.repository.clone();
                let digest = self
                    .step("resolve", self.registry.resolve(&repo, &tag), |d| {
                        format!("{repo}:{tag} is {d}")
                    })
                    .await
                    .map_err(failed)?;
                let reference = format!("{repo}@{digest}");
                if cur.image.digest.as_deref() == Some(digest.as_str())
                    || cur.image.reference == reference
                {
                    self.with_run(|r| r.to = Some(cur.image.clone()));
                    self.skip("pull", "the container runs this image already");
                    return Ok(Outcome::Current);
                }
                if self.verifier.mode() == "off" {
                    self.skip(
                        "verify",
                        "TERRARIUM_UPDATE_VERIFY=off: the signature is not checked",
                    );
                } else {
                    self.step("verify", self.verifier.verify(&reference), Clone::clone)
                        .await
                        .map_err(failed)?;
                }
                let to = self
                    .step("pull", self.engine.pull(&reference), |i| {
                        format!(
                            "pulled {} ({})",
                            i.reference,
                            i.version.as_deref().unwrap_or("no version label")
                        )
                    })
                    .await
                    .map_err(failed)?;
                (to, None)
            }
            RunKind::Rollback => {
                let Some(prev) = self.store.previous() else {
                    return Err(failed(Fail(
                        "no switch to roll back: the updater has no previous image".into(),
                    )));
                };
                let Some(backup) = prev.backup.clone() else {
                    return Err(failed(Fail(
                        "the last switch made no backup; a rollback cannot restore the data".into(),
                    )));
                };
                let to = self.local_or_pull(&prev).await.map_err(failed)?;
                self.skip("verify", "the image ran here before");
                (to, Some(backup))
            }
        };
        self.with_run(|r| r.to = Some(to.clone()));

        let prepared = self
            .step("environment", self.engine.prepare(&cur), |c| {
                let keys = c.spec["changed_env_keys"].as_array();
                format!(
                    "resolved deployment environment; changed keys (values hidden): {}",
                    keys.map(|a| a
                        .iter()
                        .filter_map(|v| v.as_str())
                        .collect::<Vec<_>>()
                        .join(", "))
                        .unwrap_or_default()
                )
            })
            .await
            .map_err(failed)?;

        // Explicit rollback always replaces the container: retained runners
        // must never replay against a journal rewound after admitted writes.
        if kind == RunKind::Update
            && prepared.spec["changed_env_keys"]
                .as_array()
                .is_none_or(|a| a.is_empty())
            && self
                .step(
                    "hot-swap-eligibility",
                    self.engine.can_hot_swap(&cur, &to),
                    |yes| format!("runtime base and runner epoch compatible: {yes}"),
                )
                .await
                .map_err(failed)?
        {
            return self.hot_swap(&cur, &to).await;
        }

        // Drain and back up. A failure here changes nothing.
        if let Err(f) = self
            .step(
                "drain",
                self.exec_ok(&cur.id, drain_argv(self.cfg.drain_wait)),
                |out| last_line(out, "no new turns; the running turns ended or waited"),
            )
            .await
        {
            self.resume(&cur).await;
            return Err(failed(f));
        }
        let backup = match self
            .step(
                "backup",
                async {
                    let out = self.exec_ok(&cur.id, backup_argv()).await?;
                    backup_path(&out)
                },
                |p| format!("full backup {p}"),
            )
            .await
        {
            Ok(p) => p,
            Err(f) => {
                self.resume(&cur).await;
                return Err(failed(f));
            }
        };
        self.with_run(|r| r.backup = Some(backup.clone()));

        // From here the container changes: a failure goes back.
        match self.replace(&prepared, &to, restore.as_deref()).await {
            Ok(()) => {
                let prev = Previous {
                    image: cur.image.clone(),
                    backup: Some(backup),
                };
                if let Err(e) = self.store.set_previous(&prev) {
                    tracing::warn!("cannot write previous.json: {e}");
                }
                if let Err(e) = self.store.set_image(&to.reference) {
                    tracing::warn!("cannot write the image file: {e}");
                }
                Ok(Outcome::Succeeded)
            }
            Err(f) => match self.recover(&cur, &backup).await {
                Ok(()) => Err((Outcome::RolledBack, f)),
                Err(Fail(e)) => Err((
                    Outcome::RollbackFailed,
                    Fail(format!("{}; the way back failed too: {e}", f.0)),
                )),
            },
        }
    }

    async fn hot_swap(&self, cur: &Container, to: &Image) -> Result<Outcome, (Outcome, Fail)> {
        let failed = |f: Fail| (Outcome::Failed, f);
        let old = self
            .exec_ok(&cur.id, hot_argv("probe", &[]))
            .await
            .map_err(|e| failed(Fail(e)))?;
        let identity = serde_json::to_string(to).map_err(|e| failed(Fail(e.to_string())))?;
        let release = self
            .step(
                "stage",
                async {
                    let out = self
                        .engine
                        .run_once(cur, to, hot_argv("stage", &[&identity]))
                        .await?;
                    if out.code != 0 {
                        return Err(out.reason());
                    }
                    Ok(out.stdout.trim().to_owned())
                },
                Clone::clone,
            )
            .await
            .map_err(failed)?;
        self.exec_ok(&cur.id, hot_argv("begin", &[&release]))
            .await
            .map_err(|e| failed(Fail(e)))?;
        // Staging is outside the hold window. Reserve one minute for recovery
        // rather than adding a fresh 15-minute health wait after a long drain.
        let hold_deadline = tokio::time::Instant::now() + HOT_SWAP_HOLD_LIMIT;
        let switch_deadline = hold_deadline - HOT_SWAP_RECOVERY_RESERVE;
        if let Err(f) = self
            .step(
                "pause-daemon",
                self.exec_ok(&cur.id, hot_argv("pause", &["660"])),
                Clone::clone,
            )
            .await
        {
            let recovery = async {
                let left = budget_left(hold_deadline, Duration::ZERO)?
                    .as_secs_f64()
                    .to_string();
                self.exec_ok(&cur.id, hot_argv("cancel-pause", &[&left]))
                    .await?;
                self.wait_hot_healthy_until(&cur.id, hold_deadline).await
            }
            .await;
            return Err(match recovery {
                Ok(_) => failed(f),
                Err(e) => (
                    Outcome::RollbackFailed,
                    Fail(format!("{}; cancel pause: {e}", f.0)),
                ),
            });
        }

        // No daemon is running here. Hold/replay runners keep their processes,
        // while the snapshot includes every completed pre-upgrade write.
        let backup = self
            .step(
                "backup",
                async {
                    let mut argv = backup_argv();
                    argv[0] = "/opt/terrarium/bundled/bin/terrariumd".into();
                    let out = self
                        .engine
                        .run_once(cur, &cur.image, bounded_command(argv, switch_deadline)?)
                        .await?;
                    if out.code != 0 {
                        return Err(out.reason());
                    }
                    let path = backup_path(&out.stdout)?;
                    let snapshot = self
                        .engine
                        .run_once(
                            cur,
                            &cur.image,
                            bounded_command(hot_argv("snapshot", &[&path]), switch_deadline)?,
                        )
                        .await?;
                    if snapshot.code != 0 {
                        return Err(snapshot.reason());
                    }
                    Ok(path)
                },
                Clone::clone,
            )
            .await;
        let backup = match backup {
            Ok(path) => path,
            Err(f) => {
                // No new code or migrations have run. Resume the original release.
                let recovery = async {
                    let left = budget_left(hold_deadline, Duration::ZERO)?
                        .as_secs_f64()
                        .to_string();
                    self.exec_ok(&cur.id, hot_argv("start", &[old.trim(), &left]))
                        .await?;
                    self.wait_hot_healthy_until(&cur.id, hold_deadline).await?;
                    self.commit_with_retry(&cur.id, &[]).await
                }
                .await;
                return Err((
                    if recovery.is_ok() {
                        Outcome::Failed
                    } else {
                        Outcome::RollbackFailed
                    },
                    f,
                ));
            }
        };
        self.with_run(|r| r.backup = Some(backup.clone()));
        let attempt = async {
            let left = budget_left(switch_deadline, Duration::ZERO).map_err(Fail)?;
            let control_timeout = left.as_secs_f64().to_string();
            self.step(
                "start-daemon",
                self.exec_ok(&cur.id, hot_argv("start", &[&release, &control_timeout])),
                Clone::clone,
            )
            .await?;
            self.step(
                "health",
                self.wait_hot_healthy_until(&cur.id, switch_deadline),
                Clone::clone,
            )
            .await?;
            self.store
                .set_previous(&Previous {
                    image: cur.image.clone(),
                    backup: Some(backup.clone()),
                })
                .map_err(|e| Fail(e.to_string()))?;
            self.store
                .set_image(&to.reference)
                .map_err(|e| Fail(e.to_string()))?;
            Ok::<_, Fail>(())
        }
        .await;
        match attempt {
            Ok(()) => {
                // Commit may have removed probation even if Docker loses the
                // response. Never rewind the journal after attempting admission.
                self.step(
                    "commit",
                    self.commit_with_retry(&cur.id, &[&backup, old.trim()]),
                    Clone::clone,
                )
                .await
                .map_err(failed)?;
                Ok(Outcome::Succeeded)
            }
            Err(f) => {
                let recovery = async {
                    let left = budget_left(hold_deadline, Duration::ZERO)?
                        .as_secs_f64()
                        .to_string();
                    self.exec_ok(&cur.id, hot_argv("pause", &[&left])).await?;
                    // Keep the live runners' open files and logs intact.
                    let out = self
                        .engine
                        .run_once(
                            cur,
                            &cur.image,
                            bounded_command(
                                hot_argv("restore-database", &[&backup]),
                                hold_deadline,
                            )?,
                        )
                        .await?;
                    if out.code != 0 {
                        return Err(out.reason());
                    }
                    let left = budget_left(hold_deadline, Duration::ZERO)?
                        .as_secs_f64()
                        .to_string();
                    self.exec_ok(&cur.id, hot_argv("start", &[old.trim(), &left]))
                        .await?;
                    self.wait_hot_healthy_until(&cur.id, hold_deadline).await?;
                    self.store
                        .set_image(&cur.image.reference)
                        .map_err(|e| e.to_string())?;
                    self.commit_with_retry(&cur.id, &[&backup]).await?;
                    Ok::<_, String>(())
                }
                .await;
                match recovery {
                    Ok(()) => Err((Outcome::RolledBack, f)),
                    Err(e) => Err((
                        Outcome::RollbackFailed,
                        Fail(format!("{}; recovery: {e}; runners remain held", f.0)),
                    )),
                }
            }
        }
    }

    async fn wait_hot_healthy_until(
        &self,
        id: &str,
        hold_deadline: tokio::time::Instant,
    ) -> Res<String> {
        let deadline = (tokio::time::Instant::now() + self.cfg.health_timeout).min(hold_deadline);
        // Docker health can still describe the old daemon. Require a new live
        // status answer too, and allow the normal start-period failures.
        loop {
            if matches!(self.engine.health(id).await?, Health::Healthy)
                && self.exec_ok(id, status_argv()).await.is_ok()
            {
                return Ok("new daemon healthy; RPC admission still held".into());
            }
            if tokio::time::Instant::now() >= deadline {
                return Err("hot-swap health timeout".into());
            }
            tokio::time::sleep(self.cfg.health_poll).await;
        }
    }

    async fn commit_with_retry(&self, id: &str, args: &[&str]) -> Res<String> {
        let mut last = String::new();
        for attempt in 0..3 {
            match self.exec_ok(id, hot_argv("commit", args)).await {
                Ok(out) => return Ok(out),
                Err(error) => last = error,
            }
            if attempt < 2 {
                tokio::time::sleep(self.cfg.health_poll.min(Duration::from_secs(1))).await;
            }
        }
        Err(format!(
            "commit failed after 3 attempts: {last}; do not restore the database; retry the idempotent commit command (docker/README.md)"
        ))
    }

    /// The image of a rollback: here, else pulled by its digest.
    async fn local_or_pull(&self, prev: &Previous) -> Result<Image, Fail> {
        let want = &prev.image;
        let idx = self.begin_step("pull");
        match self.engine.image(&want.id).await {
            Ok(Some(mut i)) => {
                i.reference = want.reference.clone();
                i.digest = i.digest.or_else(|| want.digest.clone());
                self.end_step(idx, StepState::Ok, format!("{} is here", want.reference));
                return Ok(i);
            }
            Ok(None) => {}
            Err(e) => {
                self.end_step(idx, StepState::Failed, e.clone());
                return Err(Fail(format!("pull: {e}")));
            }
        }
        let Some(digest) = &want.digest else {
            let e = format!(
                "the image {} is gone and has no registry digest to pull it again",
                want.reference
            );
            self.end_step(idx, StepState::Failed, e.clone());
            return Err(Fail(format!("pull: {e}")));
        };
        let repo = want
            .reference
            .split_once('@')
            .map_or(self.cfg.repository.as_str(), |(r, _)| r);
        let reference = format!("{repo}@{digest}");
        match self.engine.pull(&reference).await {
            Ok(i) => {
                self.end_step(idx, StepState::Ok, format!("pulled {reference}"));
                Ok(i)
            }
            Err(e) => {
                self.end_step(idx, StepState::Failed, e.clone());
                Err(Fail(format!("pull: {e}")))
            }
        }
    }

    /// Stop `cur`, restore `restore` with the image `to` when given, and
    /// start `to` with the settings of `cur`; then wait for its health.
    async fn replace(
        &self,
        cur: &Container,
        to: &Image,
        restore: Option<&str>,
    ) -> Result<(), Fail> {
        self.step("stop", self.engine.stop(&cur.id), |()| {
            format!("stopped {}", cur.name)
        })
        .await?;
        if let Some(path) = restore {
            self.step(
                "restore",
                async {
                    let out = self.engine.run_once(cur, to, restore_argv(path)).await?;
                    if out.code == 0 {
                        Ok(())
                    } else {
                        Err(out.reason())
                    }
                },
                |()| format!("restored {path}"),
            )
            .await?;
        }
        self.step("remove", self.engine.remove(&cur.id), |()| {
            format!("removed the old container {}", short(&cur.id))
        })
        .await?;
        let new = self
            .step("start", self.engine.create(cur, to), |c| {
                format!("started {} ({}) on {}", c.name, short(&c.id), to.reference)
            })
            .await?;
        self.step("health", self.wait_healthy(&new.id), Clone::clone)
            .await?;
        Ok(())
    }

    /// Wait for the health check, then for `terrarium daemon status`.
    async fn wait_healthy(&self, id: &str) -> Res<String> {
        let started = tokio::time::Instant::now();
        loop {
            match self.engine.health(id).await? {
                Health::Healthy => break,
                Health::Starting => {}
                Health::Unhealthy(why) => return Err(format!("unhealthy: {why}")),
                Health::Exited(why) => return Err(format!("the container stopped: {why}")),
            }
            if started.elapsed() >= self.cfg.health_timeout {
                return Err(format!(
                    "not healthy after {}s",
                    self.cfg.health_timeout.as_secs()
                ));
            }
            tokio::time::sleep(self.cfg.health_poll).await;
        }
        self.exec_ok(id, status_argv())
            .await
            .map_err(|e| format!("terrarium daemon status: {e}"))?;
        Ok(format!(
            "healthy after {}s; the daemon answers",
            started.elapsed().as_secs()
        ))
    }

    /// Go back to `cur`: remove what runs under its name, restore
    /// `backup` with the old image, and start the old image.
    async fn recover(&self, cur: &Container, backup: &str) -> Result<(), Fail> {
        let idx = self.begin_step("rollback");
        self.end_step(
            idx,
            StepState::Ok,
            format!("going back to {}", cur.image.reference),
        );
        // Whatever runs now under the name: the new container, or the old
        // one when its stop failed. The restore needs the daemon stopped.
        if let Ok(now) = self.engine.target().await {
            self.step("remove", self.engine.remove(&now.id), |()| {
                format!("removed {}", short(&now.id))
            })
            .await?;
        }
        self.step(
            "restore",
            async {
                let out = self
                    .engine
                    .run_once(cur, &cur.image, restore_argv(backup))
                    .await?;
                if out.code == 0 {
                    Ok(())
                } else {
                    Err(out.reason())
                }
            },
            |()| format!("restored {backup}"),
        )
        .await?;
        let old = self
            .step("start", self.engine.create(cur, &cur.image), |c| {
                format!("started {} on {}", short(&c.id), cur.image.reference)
            })
            .await?;
        self.step("health", self.wait_healthy(&old.id), Clone::clone)
            .await?;
        Ok(())
    }
}

fn short(id: &str) -> &str {
    let id = id.strip_prefix("sha256:").unwrap_or(id);
    id.get(..12).unwrap_or(id)
}

fn last_line(out: &str, default: &str) -> String {
    out.lines()
        .rev()
        .find(|l| !l.trim().is_empty())
        .map_or_else(|| default.to_owned(), |l| l.trim().to_owned())
}

/// The path of the backup from `terrariumd backup create --json`.
pub fn backup_path(out: &str) -> Res<String> {
    let v: serde_json::Value = serde_json::from_str(out.trim())
        .map_err(|e| format!("cannot read the answer of the backup: {e}"))?;
    v.get("path")
        .and_then(|p| p.as_str())
        .filter(|p| !p.is_empty())
        .map(str::to_owned)
        .ok_or_else(|| "the answer of the backup has no path".into())
}

#[cfg(test)]
#[path = "machine_tests.rs"]
mod tests;
