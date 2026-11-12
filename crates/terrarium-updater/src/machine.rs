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
//! 5. Supervised recreate: online `backup` before either drain. Legacy
//!    sources retain the full backup after drain (recorded in history).
//! 6. `drain`: `terrarium admin update drain` in the container: no new
//!    turns; the running turns get [`Config::drain_wait`] to end.
//!    Supervised sources then pause and take an exact SQLite snapshot for
//!    automatic rollback. The ZIP is the manual restore point.
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
    /// How long each drain waits for running turns or jobs.
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
const HOT_SWAP_STOP_RESERVE: Duration = Duration::from_secs(5);
// Helper termination belongs to recovery, never to the final daemon stop.
const HELPER_TERMINATION_RESERVE: Duration = Duration::from_secs(5);

async fn recovery_before_deadline<T>(
    deadline: tokio::time::Instant,
    work: impl std::future::Future<Output = Res<T>>,
) -> Res<T> {
    tokio::time::timeout_at(deadline, work).await.map_err(|_| {
        "recovery exceeded the remaining runner hold budget; completion uncertain".to_owned()
    })?
}

fn budget_left(deadline: tokio::time::Instant, reserve: Duration) -> Res<Duration> {
    deadline
        .saturating_duration_since(tokio::time::Instant::now())
        .checked_sub(reserve)
        .filter(|left| *left > Duration::from_millis(10))
        .ok_or_else(|| "hot swap would exceed the 15-minute runner hold budget".into())
}

async fn switch_before_deadline<T>(
    deadline: tokio::time::Instant,
    work: impl std::future::Future<Output = Result<T, Fail>>,
) -> Result<T, Fail> {
    tokio::time::timeout_at(deadline, work)
        .await
        .unwrap_or_else(|_| {
            Err(Fail(
                "hot swap pause exceeded the runner hold safety deadline; resuming old daemon"
                    .into(),
            ))
        })
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

/// Drain only job admission, using the daemon's authenticated CLI socket.
pub fn drain_jobs_argv(wait: Duration) -> Vec<String> {
    let mut argv = drain_argv(wait);
    argv[3] = "drain-jobs".into();
    argv.push("--json".into());
    argv
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
    front: tokio::sync::watch::Sender<Option<Run>>,
    cfg: Config,
    /// The run in progress, with its steps so far.
    running: Mutex<Option<Run>>,
    /// Held while a run runs.
    busy: Arc<tokio::sync::Mutex<()>>,
    #[cfg(test)]
    hot_budget: (Duration, Duration, Duration),
}

#[derive(Default)]
struct Replacement {
    deadline: Option<tokio::time::Instant>,
    admission_attempted: bool,
    helper: Option<String>,
}

fn now() -> String {
    Utc::now().to_rfc3339_opts(chrono::SecondsFormat::Millis, true)
}

type FrontWriter = Arc<dyn Fn(&Run) -> std::io::Result<()> + Send + Sync>;

fn front_publisher(writer: FrontWriter) -> tokio::sync::watch::Sender<Option<Run>> {
    let (sender, mut receiver) = tokio::sync::watch::channel::<Option<Run>>(None);
    tokio::spawn(async move {
        while receiver.changed().await.is_ok() {
            let Some(run) = receiver.borrow_and_update().clone() else {
                continue;
            };
            let writer = writer.clone();
            // One writer at a time preserves order, even while newer snapshots
            // replace the pending slot. A stalled write never blocks updates.
            match tokio::task::spawn_blocking(move || writer(&run)).await {
                Ok(Ok(())) => (),
                result => tracing::warn!(?result, "cannot export maintenance progress"),
            }
        }
    });
    sender
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
        let export_store = store.clone();
        let front = front_publisher(Arc::new(move |run| export_store.export_front(run)));
        Self {
            engine,
            registry,
            verifier,
            store,
            front,
            cfg,
            #[cfg(test)]
            hot_budget: (
                HOT_SWAP_HOLD_LIMIT,
                HOT_SWAP_RECOVERY_RESERVE,
                HOT_SWAP_STOP_RESERVE,
            ),
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
            Request::RetentionRepair { confirm } => return self.retention_repair(confirm).await,
            Request::RetentionInit {
                confirm,
                no_rollback_reference,
            } => return self.retention_init(confirm, no_rollback_reference).await,
            Request::Retention { release } => return self.retention(release.as_deref()).await,
            Request::Status => return Response::Status(Box::new(self.status().await)),
            Request::Update { target, actor } => (RunKind::Update, target, actor),
            Request::Rollback { actor } => (RunKind::Rollback, "previous".to_owned(), actor),
        };
        if let Err(message) = self.reconcile_retention().await {
            return Response::Error {
                message,
                busy: true,
            };
        }
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
        if let Err(e) = self.store.retention_begin(&run) {
            return Err(Response::Error {
                message: format!("cannot protect update backups: {e}"),
                busy: false,
            });
        }
        *self.running() = Some(run.clone());
        self.export_front(&run);
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
        self.export_front(&done);
        tracing::info!(run = %id, outcome = done.outcome.as_str(), error = ?done.error, "run ends");
        if let Err(e) = self.store.push(&done) {
            tracing::warn!("cannot write the history: {e}");
        } else if let Err(e) = self.store.retention_finish(&done) {
            tracing::warn!("cannot release update retention hold: {e}");
        }
        done
    }

    async fn retention_repair(&self, confirm: bool) -> Response {
        let Ok(_guard) = self.busy.clone().try_lock_owned() else {
            return Response::Error {
                message: "update or rollback in progress".into(),
                busy: true,
            };
        };
        let result = async {
            let cur = self.engine.target().await?;
            let helpers = self.engine.running_retention_helpers(&cur.id).await?;
            if !helpers.is_empty() {
                return Err(format!(
                    "retention helper still running: {}",
                    helpers.join(", ")
                ));
            }
            let out = self
                .exec_ok(&cur.id, hot_argv("retention-inventory", &[]))
                .await?;
            let inventory: std::collections::BTreeMap<String, String> =
                serde_json::from_str(&out).map_err(|e| format!("bad retention inventory: {e}"))?;
            let protected = self
                .store
                .retention_repair(false, &inventory, Vec::new())
                .map_err(|e| e.to_string())?;
            let arg = serde_json::to_string(&protected).map_err(|e| e.to_string())?;
            let out = self
                .exec_ok(&cur.id, hot_argv("preview-backups", &[&arg]))
                .await?;
            let mut detail: serde_json::Value =
                serde_json::from_str(&out).map_err(|e| format!("bad retention preview: {e}"))?;
            let names = detail["preview"]
                .as_array()
                .ok_or("retention preview omitted file list")?
                .iter()
                .map(|entry| {
                    entry["name"]
                        .as_str()
                        .map(str::to_owned)
                        .ok_or("retention preview omitted name")
                })
                .collect::<Result<Vec<_>, _>>()?;
            if confirm {
                // Reinspect after the preview, before replacing lost identities.
                if !self
                    .engine
                    .running_retention_helpers(&cur.id)
                    .await?
                    .is_empty()
                {
                    return Err("retention helper still running".into());
                }
                // Revoke a created-but-not-started helper too. A delayed start
                // must not race repaired update admission.
                self.exec_ok(&cur.id, hot_argv("retention-repair-revoke", &[]))
                    .await?;
                self.store
                    .retention_repair(true, &inventory, names)
                    .map_err(|e| e.to_string())?;
            }
            detail["holds"] = serde_json::to_value(inventory).map_err(|e| e.to_string())?;
            detail["initialized"] = confirm.into();
            Ok(detail)
        }
        .await;
        match result {
            Ok(detail) => Response::Retained { detail },
            Err(message) => Response::Error {
                message,
                busy: false,
            },
        }
    }

    async fn retention_init(&self, confirm: bool, no_rollback: bool) -> Response {
        let Ok(_guard) = self.busy.clone().try_lock_owned() else {
            return Response::Error {
                message: "update or rollback in progress".into(),
                busy: true,
            };
        };
        let result = async {
            let protected = self
                .store
                .retention_init(false, no_rollback)
                .map_err(|e| e.to_string())?;
            let cur = self.engine.target().await?;
            let protected = serde_json::to_string(&protected).map_err(|e| e.to_string())?;
            let out = self
                .exec_ok(&cur.id, hot_argv("preview-backups", &[&protected]))
                .await?;
            let mut detail: serde_json::Value =
                serde_json::from_str(&out).map_err(|e| format!("bad retention preview: {e}"))?;
            if confirm {
                let names = detail["preview"]
                    .as_array()
                    .ok_or("retention preview omitted file list")?
                    .iter()
                    .map(|entry| {
                        entry["name"]
                            .as_str()
                            .map(str::to_owned)
                            .ok_or("retention preview omitted name")
                    })
                    .collect::<Result<Vec<_>, _>>()?;
                self.store
                    .retention_confirm(no_rollback, names)
                    .map_err(|e| e.to_string())?;
            }
            detail["initialized"] = confirm.into();
            detail["confirmation"] =
                "Run with --confirm to initialize retention and enable bounded deletion passes"
                    .into();
            Ok(detail)
        }
        .await;
        match result {
            Ok(detail) => Response::Retained { detail },
            Err(message) => Response::Error {
                message,
                busy: false,
            },
        }
    }

    async fn retention_control(
        &self,
        container: &str,
        action: &str,
        value: serde_json::Value,
    ) -> Result<serde_json::Value, String> {
        let arg = serde_json::to_string(&value).map_err(|e| e.to_string())?;
        let out = self
            .exec_ok(container, hot_argv("retention-control", &[action, &arg]))
            .await?;
        serde_json::from_str(&out).map_err(|e| format!("bad retention control response: {e}"))
    }

    /// Startup and update admission use the same durable reconciliation path.
    pub async fn reconcile_retention(&self) -> Result<(), String> {
        let _guard = self
            .busy
            .clone()
            .try_lock_owned()
            .map_err(|_| "update or retention in progress".to_owned())?;
        self.reconcile_retention_locked().await
    }

    async fn reconcile_retention_locked(&self) -> Result<(), String> {
        let Some(running) = self.store.retention_running().map_err(|e| e.to_string())? else {
            return Ok(());
        };
        // First revoke the token durably. A delayed exec start or prepare can
        // never restore it. The control command kills only its exact pidfd.
        self.retention_control(
            &running.container,
            "cancel",
            serde_json::json!({"token":running.token}),
        )
        .await?;
        for _ in 0..20 {
            let info = self.engine.exec_inspect(&running.exec_id).await?;
            if !info.running {
                return self
                    .store
                    .set_retention_running(None)
                    .map_err(|e| e.to_string());
            }
            tokio::time::sleep(Duration::from_millis(25)).await;
        }
        Err("retention helper still running: update admission remains blocked".into())
    }

    async fn retention_execute(
        &self,
        container: &str,
        _protected: Vec<String>,
    ) -> Result<serde_json::Value, String> {
        let token = ulid::Ulid::new().to_string();
        let exec_id = self
            .engine
            .exec_create(container, hot_argv("retain-backups", &[&token]))
            .await?;
        let mut running = crate::store::RetentionRunning {
            container: container.into(),
            exec_id,
            token: token.clone(),
            pid: None,
            worker_pid: None,
            worker_start: None,
        };
        self.store
            .set_retention_running(Some(running.clone()))
            .map_err(|e| e.to_string())?;
        let execution = async {
            self.retention_control(
                container,
                "prepare",
                self.store
                    .retention_authorization(&running)
                    .map_err(|e| e.to_string())?,
            )
            .await?;
            self.engine.exec_start(&running.exec_id).await?;
            let mut renewed = tokio::time::Instant::now();
            loop {
                if renewed.elapsed() >= Duration::from_secs(2) {
                    let authorization = self
                        .store
                        .retention_authorization(&running)
                        .map_err(|e| e.to_string())?;
                    self.retention_control(container, "renew", authorization)
                        .await?;
                    renewed = tokio::time::Instant::now();
                }
                let info = self.engine.exec_inspect(&running.exec_id).await?;
                if info.pid != 0 {
                    running.pid = Some(info.pid);
                }
                let identity = self
                    .retention_control(container, "status", serde_json::json!({"token":token}))
                    .await?;
                running.worker_pid = identity["pid"].as_u64();
                running.worker_start = identity["start"].as_str().map(str::to_owned);
                self.store
                    .set_retention_running(Some(running.clone()))
                    .map_err(|e| e.to_string())?;
                if !info.running {
                    if info.code != 0 {
                        return Err(format!("retention helper exit {}", info.code));
                    }
                    return self
                        .retention_control(container, "receipt", serde_json::json!({"token":token}))
                        .await;
                }
                tokio::time::sleep(Duration::from_millis(100)).await;
            }
        };
        let result = tokio::time::timeout(Duration::from_secs(900), execution)
            .await
            .unwrap_or_else(|_| Err("retention helper timed out".into()));
        if result.is_err() && self.store.retention_running().is_err() {
            // Safety state itself may have disappeared. Revoke using the exact
            // identity already held in memory, even if rereading it now fails.
            self.retention_control(container, "cancel", serde_json::json!({"token":token}))
                .await?;
        }
        // Even success revokes the token before clearing the durable hold.
        // On any failure to revoke/inspect/stop, the interlock stays on disk.
        self.reconcile_retention_locked().await?;
        if result
            .as_ref()
            .is_ok_and(|detail| detail["stopped"] != true)
        {
            self.store
                .retention_pass_finished()
                .map_err(|e| e.to_string())?;
        }
        result
    }

    async fn retention(&self, release: Option<&str>) -> Response {
        let Ok(_guard) = self.busy.clone().try_lock_owned() else {
            return Response::Error {
                message: "update or rollback in progress: retention deferred".into(),
                busy: true,
            };
        };
        let result = async {
            self.reconcile_retention_locked().await?;
            let protected = self
                .store
                .retention_protected(release)
                .map_err(|e| e.to_string())?;
            if let Some(id) = release {
                return Ok(serde_json::json!({"released": id, "removed": []}));
            }
            let cur = self.engine.target().await?;
            // A read-only preview avoids generating permanent revocation
            // tombstones every minute when there is nothing eligible to prune.
            let arg = serde_json::to_string(&protected).map_err(|e| e.to_string())?;
            let preview = self
                .exec_ok(&cur.id, hot_argv("preview-backups", &[&arg]))
                .await?;
            let preview: serde_json::Value = serde_json::from_str(&preview)
                .map_err(|e| format!("bad retention preview: {e}"))?;
            if preview["preview"]
                .as_array()
                .ok_or("retention preview omitted file list")?
                .is_empty()
            {
                return Ok(serde_json::json!({"removed":[]}));
            }
            self.retention_execute(&cur.id, protected).await
        }
        .await;
        match result {
            Ok(detail) => Response::Retained { detail },
            Err(message) => Response::Error {
                message,
                busy: false,
            },
        }
    }

    /// Change the record of the run in progress.
    fn with_run(&self, f: impl FnOnce(&mut Run)) {
        if let Some(r) = self.running().as_mut() {
            f(r);
            self.export_front(r);
        }
    }

    fn export_front(&self, run: &Run) {
        // Memory-only, latest-wins publication. No filesystem work or await
        // on the updater's safety-critical path or under its running mutex.
        self.front.send_replace(Some(run.clone()));
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

    /// End both drains after health or an early failure.
    async fn resume(&self, cur: &Container) {
        let _ = self.resume_checked(cur, false).await;
    }

    async fn resume_checked(
        &self,
        cur: &Container,
        require_confirmation: bool,
    ) -> Result<(), Fail> {
        let idx = self.begin_step("resume");
        match tokio::time::timeout(Duration::from_secs(30), async {
            loop {
                // Old daemons support this same command. A rollback may
                // expose the API before recovery has settled. Retry only
                // this idempotent operation, within the existing reserve.
                let resumed = matches!(
                    tokio::time::timeout(
                        Duration::from_secs(5),
                        self.exec_ok(&cur.id, resume_argv())
                    )
                    .await,
                    Ok(Ok(_))
                );
                if resumed && !require_confirmation {
                    return;
                }
                let argv = ["terrarium", "admin", "update", "status", "--json"]
                    .map(String::from)
                    .to_vec();
                if let Ok(out) = self.exec_ok(&cur.id, argv).await
                    && (resumed || !require_confirmation)
                    && let Ok(v) = serde_json::from_str::<serde_json::Value>(&out)
                    && v["drain"]["on"] == false
                    && v["drain"].get("jobs_drain").is_some_and(|v| v.is_null())
                {
                    return;
                }
                tokio::time::sleep(self.cfg.health_poll).await;
            }
        })
        .await
        {
            Ok(()) => {
                self.end_step(
                    idx,
                    StepState::Ok,
                    "the daemon takes new turns and jobs again",
                );
                Ok(())
            }
            _ => {
                let warning = "cannot resume turns and jobs (values hidden)";
                tracing::warn!("{warning}");
                self.end_step(idx, StepState::Failed, warning);
                Err(Fail(warning.into()))
            }
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
        let (to, restore, recorded) = match kind {
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
                (to, None, None)
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
                // Legacy records have no saved spec. Use the inspected
                // container, never resolve a potentially broken deployment.
                let spec = prev.spec.clone().unwrap_or_else(|| cur.spec.clone());
                let to = self.local_or_pull(&prev).await.map_err(failed)?;
                self.skip("verify", "the image ran here before");
                (to, Some(backup), Some(spec))
            }
        };
        self.with_run(|r| r.to = Some(to.clone()));

        let (prepared, _) = self
            .step(
                "environment",
                async {
                    if let Some(spec) = recorded {
                        let mut saved = cur.clone();
                        saved.spec = spec;
                        Ok((saved, String::new()))
                    } else {
                        let prepared = self.engine.prepare(&cur, &to).await?;
                        let out = self.engine.run_once(
                            &prepared,
                            &to,
                            vec!["/opt/terrarium/bundled/bin/terrariumd".into(), "required-templates-check".into()],
                        ).await?;
                        if out.code != 0 {
                            return Err(format!(
                                "required daemon templates pre-check (exit {}): {}{}",
                                out.code, out.stdout.trim(), out.stderr.trim()
                            ));
                        }
                        Ok((prepared, out.stdout.trim().to_owned()))
                    }
                },
                |(c, templates)| {
                    if kind == RunKind::Rollback {
                        return "using recorded container environment (values hidden)".into();
                    }
                    let keys = c.spec["changed_env_keys"].as_array();
                    format!(
                        "{templates}; resolved deployment environment; changed keys (values hidden): {}",
                        keys.map(|a| a
                            .iter()
                            .filter_map(|v| v.as_str())
                            .collect::<Vec<_>>()
                            .join(", "))
                            .unwrap_or_default()
                    )
                },
            )
            .await
            .map_err(failed)?;

        // Explicit rollback always replaces the container: retained runners
        // must never replay against a journal rewound after admitted writes.
        let hot = kind == RunKind::Update
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
                .map_err(failed)?;

        let supervised = !hot
            && self
                .engine
                .supervised_snapshot(&cur)
                .await
                .map_err(|e| failed(Fail(e)))?;
        let online_backup = if supervised {
            let backup = self.online_backup(&cur).await.map_err(failed)?;
            self.with_run(|r| r.backup = Some(backup.clone()));
            Some(backup)
        } else {
            None
        };

        #[cfg(not(test))]
        let (hold, reserve, stop) = (
            HOT_SWAP_HOLD_LIMIT,
            HOT_SWAP_RECOVERY_RESERVE,
            HOT_SWAP_STOP_RESERVE,
        );
        #[cfg(test)]
        let (hold, reserve, stop) = self.hot_budget;
        // One clock starts with admission drain, not after the snapshot.
        let deadlines = supervised.then(|| {
            let end = tokio::time::Instant::now() + hold;
            (end, end - reserve, end - stop)
        });

        // Older daemons may not support jobs-only drain. Never prevent an
        // update for that reason, and never record their error values.
        let idx = self.begin_step("drain-jobs");
        let jobs = tokio::time::timeout(
            self.cfg.drain_wait + Duration::from_secs(5),
            self.exec_ok(&cur.id, drain_jobs_argv(self.cfg.drain_wait)),
        );
        let jobs = if let Some((_, switch, _)) = deadlines {
            tokio::time::timeout_at(switch, jobs)
                .await
                .unwrap_or_else(Err)
        } else {
            jobs.await
        };
        match jobs {
            Ok(Ok(out)) => {
                let detail = serde_json::from_str::<serde_json::Value>(&out)
                    .ok()
                    .and_then(|v| v["jobs"].as_array().cloned())
                    .map(|jobs| format!("job admission drained; still running: {jobs:?}"))
                    .unwrap_or_else(|| "job admission drained (values hidden)".into());
                tracing::info!("{detail}");
                self.end_step(idx, StepState::Ok, detail);
            }
            _ => {
                let warning = "jobs drain failed or timed out (values hidden); continuing update";
                tracing::warn!("{warning}");
                self.end_step(idx, StepState::Failed, warning);
            }
        }

        if hot {
            let result = self.hot_swap(&cur, &to).await;
            // Healthy recovery and early failures must release job admission
            // too. Do not resume a daemon whose recovery failed.
            if !matches!(
                &result,
                Err((Outcome::RollbackFailed | Outcome::RecoveryFailed, _))
            ) {
                // Recovery leaves this final reserve for ending jobs drain.
                let _ = tokio::time::timeout(HOT_SWAP_STOP_RESERVE, self.resume(&cur)).await;
            }
            return result;
        }

        // Drain and back up. A failure here changes nothing.
        let drain = self.step(
            "drain",
            self.exec_ok(&cur.id, drain_argv(self.cfg.drain_wait)),
            |out| last_line(out, "no new turns; the running turns ended or waited"),
        );
        let drained = if let Some((_, switch, _)) = deadlines {
            tokio::time::timeout_at(switch, drain)
                .await
                .unwrap_or_else(|_| {
                    Err(Fail(
                        "recreate drain exceeded the hold safety deadline".into(),
                    ))
                })
        } else {
            drain.await
        };
        if let Err(f) = drained {
            if let Some((end, _, recovery)) = deadlines {
                return Err(self
                    .recreate_early_failure(
                        &cur,
                        online_backup.as_deref().unwrap_or_default(),
                        end,
                        recovery,
                        false,
                        f,
                    )
                    .await);
            }
            self.resume(&cur).await;
            return Err(failed(f));
        }
        let backup = if let Some(backup) = online_backup {
            backup
        } else {
            match self
                .step(
                    "backup",
                    async {
                        let out = self.exec_ok(&cur.id, backup_argv()).await?;
                        backup_path(&out)
                    },
                    |p| format!("full backup {p}; legacy recreate: backup after drain"),
                )
                .await
            {
                Ok(p) => p,
                Err(f) => {
                    self.resume(&cur).await;
                    return Err(failed(f));
                }
            }
        };
        self.with_run(|r| r.backup = Some(backup.clone()));

        if let Some((end, switch, recovery)) = deadlines {
            self.recreate_snapshot(&cur, &backup, end, switch, recovery)
                .await?;
        }

        // From here the container changes: a failure goes back.
        let mut replacement = Replacement {
            deadline: deadlines.map(|(_, switch, _)| switch),
            ..Default::default()
        };
        let replace = self.replace(&prepared, &to, restore.as_deref(), &mut replacement);
        let replaced = if let Some((_, switch, _)) = deadlines {
            tokio::time::timeout_at(switch, replace)
                .await
                .unwrap_or_else(|_| {
                    Err(Fail(
                        "recreate replacement exceeded the hold safety deadline".into(),
                    ))
                })
        } else {
            replace.await
        };
        if let Some((end, _, recovery)) = deadlines
            && let Err(e) = self.end_helper(&mut replacement.helper, recovery).await
        {
            return Err(self.stop_recreate(&cur, &backup, end, e).await);
        }
        match replaced {
            Ok(()) => {
                if let Some((_, switch, _)) = deadlines {
                    self.finish_recreate(&cur, &backup, switch).await;
                }
                let prev = Previous {
                    image: cur.image.clone(),
                    spec: Some(cur.spec.clone()),
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
            Err(f) => {
                if replacement.admission_attempted
                    && let Some((end, _, _)) = deadlines
                {
                    // Resume may have admitted writes. Never rewind that journal.
                    return Err(self.stop_recreate(&cur, &backup, end, f.0).await);
                }
                match self
                    .recover(
                        &cur,
                        &backup,
                        deadlines.map(|(end, _, recovery)| (end, recovery)),
                    )
                    .await
                {
                    Ok(()) => {
                        if let Some((_, _, recovery)) = deadlines {
                            self.finish_recreate(&cur, &backup, recovery).await;
                        }
                        Err((Outcome::RolledBack, f))
                    }
                    Err(Fail(e)) => Err((
                        if supervised {
                            Outcome::RecoveryFailed
                        } else {
                            Outcome::RollbackFailed
                        },
                        Fail(format!("{}; the way back failed too: {e}", f.0)),
                    )),
                }
            }
        }
    }

    async fn online_backup(&self, cur: &Container) -> Result<String, Fail> {
        let backup_started = Utc::now();
        self.step(
            "backup",
            async {
                let out = self.exec_ok(&cur.id, backup_argv()).await?;
                backup_path(&out)
            },
            |p| format!("full backup {p}; began at {backup_started}; writes after that time may not be included; manual pre-update restore point"),
        ).await
    }

    async fn tracked_once(
        &self,
        cur: &Container,
        image: &Image,
        argv: Vec<String>,
        deadline: tokio::time::Instant,
        helper: &mut Option<String>,
    ) -> Res<crate::engine::ExecOut> {
        budget_left(deadline, Duration::ZERO)?;
        let id = self
            .engine
            .prepare_once(cur, image, bounded_command(argv, deadline)?)
            .await?;
        // This assignment precedes start. Cancellation cannot lose a running
        // helper's ID, and callers must verify its termination before restart.
        *helper = Some(id.clone());
        let idx = self.begin_step("offline-helper");
        self.end_step(
            idx,
            StepState::Ok,
            format!("recorded helper {id} before start; termination must be verified"),
        );
        self.engine.run_prepared_once(&id).await
    }

    async fn end_helper(
        &self,
        helper: &mut Option<String>,
        deadline: tokio::time::Instant,
    ) -> Res<()> {
        if let Some(id) = helper.as_deref() {
            tokio::time::timeout_at(deadline, self.engine.remove_once(id))
                .await
                .map_err(|_| "restore helper termination deadline elapsed".to_owned())??;
            *helper = None;
        }
        Ok(())
    }

    async fn finish_recreate(&self, cur: &Container, backup: &str, deadline: tokio::time::Instant) {
        // Use the source helper even when the destination is a legacy image.
        if let Err(e) = self
            .step(
                "snapshot-cleanup",
                async {
                    let out = tokio::time::timeout_at(
                        deadline,
                        self.engine.run_once(
                            cur,
                            &cur.image,
                            bounded_command(hot_argv("commit", &[backup]), deadline)?,
                        ),
                    )
                    .await
                    .map_err(|_| "snapshot cleanup deadline elapsed".to_owned())??;
                    if out.code != 0 {
                        return Err(out.reason());
                    }
                    Ok(())
                },
                |()| "recreate transaction committed; snapshot released".into(),
            )
            .await
        {
            tracing::warn!("snapshot cleanup failed, retained for recovery: {}", e.0);
        }
    }

    async fn recreate_snapshot(
        &self,
        cur: &Container,
        backup: &str,
        deadline: tokio::time::Instant,
        switch_deadline: tokio::time::Instant,
        recovery_deadline: tokio::time::Instant,
    ) -> Result<(), (Outcome, Fail)> {
        let mut helper = None;
        let work = self
            .step(
                "snapshot-database",
                async {
                    tokio::time::timeout_at(switch_deadline, async {
                        let old = self.exec_ok(&cur.id, hot_argv("probe", &[])).await?;
                        // begin clears a prior completed transaction's admission flag.
                        self.exec_ok(&cur.id, hot_argv("begin", &[old.trim()]))
                            .await?;
                        self.exec_ok(&cur.id, hot_argv("pause", &["960"])).await?;
                        let out = self
                            .tracked_once(
                                cur,
                                &cur.image,
                                hot_argv("snapshot", &[backup]),
                                switch_deadline,
                                &mut helper,
                            )
                            .await?;
                        self.end_helper(&mut helper, switch_deadline).await?;
                        if out.code != 0 {
                            return Err(out.reason());
                        }
                        // End the supervisor hold before container replacement. Once
                        // stopped there are no retained runners or automatic restart.
                        self.step("stop", self.engine.stop(&cur.id), |()| {
                            "stopped the paused source container".into()
                        })
                        .await
                        .map_err(|f| f.0)?;
                        Ok(())
                    })
                    .await
                    .map_err(|_| "recreate snapshot exceeded the hold safety deadline".to_owned())?
                },
                |()| "exact paused database snapshot for automatic recreate rollback".into(),
            )
            .await;
        if let Err(e) = self.end_helper(&mut helper, recovery_deadline).await {
            return Err(self.failed_hot_recovery(cur, backup, deadline, e).await);
        }
        if let Err(f) = work {
            return Err(self
                .recreate_early_failure(cur, backup, deadline, recovery_deadline, true, f)
                .await);
        }
        Ok(())
    }

    async fn recreate_early_failure(
        &self,
        cur: &Container,
        backup: &str,
        deadline: tokio::time::Instant,
        recovery_deadline: tokio::time::Instant,
        paused: bool,
        f: Fail,
    ) -> (Outcome, Fail) {
        let recovery = recovery_before_deadline(recovery_deadline, async {
            if paused {
                let left = budget_left(recovery_deadline, Duration::ZERO)?
                    .as_secs_f64()
                    .to_string();
                self.exec_ok(&cur.id, hot_argv("cancel-pause", &[&left]))
                    .await?;
            }
            self.wait_hot_healthy_until(&cur.id, recovery_deadline, None)
                .await?;
            self.resume_checked(cur, true).await.map_err(|f| f.0)?;
            Ok(())
        })
        .await;
        match recovery {
            Ok(_) => (Outcome::Failed, f),
            Err(e) => {
                self.failed_hot_recovery(
                    cur,
                    backup,
                    deadline,
                    format!("{}; recreate snapshot recovery: {e}", f.0),
                )
                .await
            }
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
        // VACUUM INTO in the daemon's backup CLI is safe while it is live.
        // The ZIP is the manual pre-update restore point, not the exact
        // automatic rollback point (the paused database snapshot below).
        let backup = self.online_backup(cur).await.map_err(failed)?;
        self.with_run(|r| r.backup = Some(backup.clone()));
        self.exec_ok(&cur.id, hot_argv("begin", &[&release]))
            .await
            .map_err(|e| failed(Fail(e)))?;
        // Staging is outside the hold window. Reserve one minute for recovery
        // rather than adding a fresh 15-minute health wait after a long drain.
        #[cfg(not(test))]
        let (hold_limit, recovery_reserve, stop_reserve) = (
            HOT_SWAP_HOLD_LIMIT,
            HOT_SWAP_RECOVERY_RESERVE,
            HOT_SWAP_STOP_RESERVE,
        );
        #[cfg(test)]
        let (hold_limit, recovery_reserve, stop_reserve) = self.hot_budget;
        let hold_deadline = tokio::time::Instant::now() + hold_limit;
        let switch_deadline = hold_deadline - recovery_reserve;
        let recovery_deadline = hold_deadline - stop_reserve;
        if let Err(f) = self
            .step(
                "pause-daemon",
                async {
                    tokio::time::timeout_at(
                        switch_deadline,
                        self.exec_ok(&cur.id, hot_argv("pause", &["660"])),
                    )
                    .await
                    .map_err(|_| {
                        "hot swap pause exceeded the runner hold safety deadline".to_owned()
                    })?
                },
                Clone::clone,
            )
            .await
        {
            let recovery = recovery_before_deadline(recovery_deadline, async {
                let left = budget_left(recovery_deadline, Duration::ZERO)?
                    .as_secs_f64()
                    .to_string();
                self.exec_ok(&cur.id, hot_argv("cancel-pause", &[&left]))
                    .await?;
                self.wait_hot_healthy_until(&cur.id, recovery_deadline, None)
                    .await
            })
            .await;
            return Err(match recovery {
                Ok(_) => failed(f),
                Err(e) => {
                    self.failed_hot_recovery(
                        cur,
                        &backup,
                        hold_deadline,
                        format!("{}; cancel pause: {e}", f.0),
                    )
                    .await
                }
            });
        }

        // No daemon is running here. Hold/replay runners keep their processes,
        // while the snapshot includes every completed pre-upgrade write.
        let mut helper = None;
        let snapshot = self
            .step(
                "snapshot-database",
                async {
                    let snapshot = tokio::time::timeout_at(
                        switch_deadline,
                        self.tracked_once(
                            cur,
                            &cur.image,
                            hot_argv("snapshot", &[&backup]),
                            switch_deadline,
                            &mut helper,
                        ),
                    )
                    .await
                    .map_err(|_| {
                        "hot swap database snapshot exceeded the runner hold safety deadline"
                            .to_owned()
                    })?;
                    let snapshot = snapshot?;
                    if snapshot.code != 0 {
                        return Err(snapshot.reason());
                    }
                    Ok("exact paused database snapshot for automatic rollback".to_owned())
                },
                Clone::clone,
            )
            .await;
        if let Err(e) = self.end_helper(&mut helper, recovery_deadline).await {
            return Err(self
                .failed_hot_recovery(cur, &backup, hold_deadline, e)
                .await);
        }
        match snapshot {
            Ok(_) => (),
            Err(f) => {
                // No new code or migrations have run. Resume the original release.
                let recovery = recovery_before_deadline(recovery_deadline, async {
                    let left = budget_left(recovery_deadline, Duration::ZERO)?
                        .as_secs_f64()
                        .to_string();
                    self.exec_ok(&cur.id, hot_argv("start", &[old.trim(), &left]))
                        .await?;
                    self.wait_hot_healthy_until(&cur.id, recovery_deadline, None)
                        .await?;
                    self.commit_with_retry(&cur.id, &[]).await
                })
                .await;
                return Err(match recovery {
                    Ok(_) => (Outcome::Failed, f),
                    Err(e) => {
                        self.failed_hot_recovery(
                            cur,
                            &backup,
                            hold_deadline,
                            format!("{}; snapshot failure recovery: {e}", f.0),
                        )
                        .await
                    }
                });
            }
        };
        let attempt = async {
            let left = budget_left(switch_deadline, Duration::ZERO).map_err(Fail)?;
            let control_timeout = left.as_secs_f64().to_string();
            let now = Utc::now();
            let startup_since = format!("{}.{:09}", now.timestamp(), now.timestamp_subsec_nanos());
            self.step(
                "start-daemon",
                self.exec_ok(&cur.id, hot_argv("start", &[&release, &control_timeout])),
                Clone::clone,
            )
            .await?;
            self.step(
                "health",
                self.wait_hot_healthy_until(&cur.id, switch_deadline, Some(&startup_since)),
                Clone::clone,
            )
            .await?;
            self.store
                .set_previous(&Previous {
                    image: cur.image.clone(),
                    spec: Some(cur.spec.clone()),
                    backup: Some(backup.clone()),
                })
                .map_err(|e| Fail(e.to_string()))?;
            self.store
                .set_image(&to.reference)
                .map_err(|e| Fail(e.to_string()))?;
            Ok::<_, Fail>(())
        };
        let attempt = switch_before_deadline(switch_deadline, attempt).await;
        match attempt {
            Ok(()) => {
                // Commit may have removed probation even if Docker loses the
                // response. Never rewind the journal after attempting admission.
                self.step(
                    "commit",
                    async {
                        tokio::time::timeout_at(
                            switch_deadline,
                            self.commit_with_retry(&cur.id, &[&backup, old.trim()]),
                        )
                        .await
                        .map_err(|_| "hot swap admission exceeded the runner hold safety deadline; admission was attempted, automatic rollback is unsafe".to_owned())?
                    },
                    Clone::clone,
                )
                .await
                .map_err(failed)?;
                // Admission may already have effects. Failure here is visible,
                // but must never enter automatic database rollback.
                self.step(
                    "activation",
                    async {
                        let deadline = tokio::time::Instant::now()
                            + self.cfg.health_timeout.min(Duration::from_secs(30));
                        loop {
                            match self.exec_ok(&cur.id, hot_argv("ready", &[])).await {
                                Ok(out) => break Ok(out),
                                Err(error) if tokio::time::Instant::now() >= deadline => {
                                    break Err(error);
                                }
                                Err(_) => tokio::time::sleep(self.cfg.health_poll).await,
                            }
                        }
                    },
                    Clone::clone,
                )
                .await
                .map_err(failed)?;
                Ok(Outcome::Succeeded)
            }
            Err(f) => {
                // A deadline can cancel an engine call inside a step before
                // step() gets its answer. Do not leave that step "running".
                self.with_run(|r| {
                    for step in &mut r.steps {
                        if step.state == StepState::Running {
                            step.state = StepState::Failed;
                            step.detail = f.0.clone();
                        }
                    }
                });
                let work_deadline = recovery_deadline - HELPER_TERMINATION_RESERVE;
                let recovery = recovery_before_deadline(work_deadline, async {
                    let left = budget_left(work_deadline, Duration::ZERO)?
                        .as_secs_f64()
                        .to_string();
                    self.exec_ok(&cur.id, hot_argv("pause", &[&left])).await?;
                    // Keep the live runners' open files and logs intact.
                    let out = self
                        .tracked_once(
                            cur,
                            &cur.image,
                            hot_argv("restore-database", &[&backup]),
                            work_deadline,
                            &mut helper,
                        )
                        .await?;
                    self.end_helper(&mut helper, work_deadline).await?;
                    if out.code != 0 {
                        return Err(out.reason());
                    }
                    let left = budget_left(work_deadline, Duration::ZERO)?
                        .as_secs_f64()
                        .to_string();
                    self.exec_ok(&cur.id, hot_argv("start", &[old.trim(), &left]))
                        .await?;
                    self.wait_hot_healthy_until(&cur.id, work_deadline, None)
                        .await?;
                    self.store
                        .set_image(&cur.image.reference)
                        .map_err(|e| e.to_string())?;
                    self.commit_with_retry(&cur.id, &[&backup]).await?;
                    Ok::<_, String>(())
                })
                .await;
                if let Err(e) = self.end_helper(&mut helper, recovery_deadline).await {
                    return Err(self
                        .failed_hot_recovery(cur, &backup, hold_deadline, e)
                        .await);
                }
                match recovery {
                    Ok(()) => Err((Outcome::RolledBack, f)),
                    Err(e) => Err(self
                        .failed_hot_recovery(
                            cur,
                            &backup,
                            hold_deadline,
                            format!("{}; recovery: {e}", f.0),
                        )
                        .await),
                }
            }
        }
    }

    async fn failed_hot_recovery(
        &self,
        cur: &Container,
        backup: &str,
        deadline: tokio::time::Instant,
        reason: String,
    ) -> (Outcome, Fail) {
        // A cancelled/failed restore may still have changed pages. Never
        // start or admit a daemon, or attempt a second restore, on uncertainty.
        let stopped =
            recovery_before_deadline(deadline, self.engine.stop_for_recovery(&cur.id)).await;
        let stop = if stopped.is_ok() {
            "container stopped"
        } else {
            "container stop unconfirmed; operator must stop it"
        };
        (
            Outcome::RecoveryFailed,
            Fail(format!(
                "{reason}; recovery_failed: {stop}; do not restart on an unverified database; manual restore required: paused snapshot {backup}.hot-swap.sqlite (if created and retained), pre-update ZIP {backup}; verify any outstanding restore helper has stopped"
            )),
        )
    }

    async fn wait_hot_healthy_until(
        &self,
        id: &str,
        hold_deadline: tokio::time::Instant,
        startup_since: Option<&str>,
    ) -> Res<String> {
        let deadline = (tokio::time::Instant::now() + self.cfg.health_timeout).min(hold_deadline);
        // Docker health can still describe the old daemon. Require a new live
        // status answer too, and allow the normal start-period failures.
        let mut read_startup_log = startup_since;
        loop {
            if let Some(since) = read_startup_log {
                match self.engine.daemon_failure_since(id, since).await {
                    Ok(Some(reason)) => return Err(reason),
                    Ok(None) => {}
                    Err(_) => {
                        let warning = "cannot read daemon startup log (values hidden); continuing health checks";
                        tracing::warn!("{warning}");
                        let idx = self.begin_step("startup-log-warning");
                        self.end_step(idx, StepState::Failed, warning);
                        read_startup_log = None;
                    }
                }
            }
            let health = self.engine.health(id).await?;
            if matches!(health, Health::Exited(_)) {
                return Err("daemon container exited during startup (values hidden)".into());
            }
            if matches!(health, Health::Healthy) && self.exec_ok(id, status_argv()).await.is_ok() {
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
        replacement: &mut Replacement,
    ) -> Result<(), Fail> {
        if replacement.deadline.is_none() {
            self.step("stop", self.engine.stop(&cur.id), |()| {
                format!("stopped {}", cur.name)
            })
            .await?;
        }
        if let Some(path) = restore {
            self.step(
                "restore",
                async {
                    let out = if let Some(deadline) = replacement.deadline {
                        self.tracked_once(
                            cur,
                            to,
                            restore_argv(path),
                            deadline,
                            &mut replacement.helper,
                        )
                        .await?
                    } else {
                        self.engine.run_once(cur, to, restore_argv(path)).await?
                    };
                    if let Some(deadline) = replacement.deadline {
                        self.end_helper(&mut replacement.helper, deadline).await?;
                    }
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
        replacement.admission_attempted = true;
        if replacement.deadline.is_some() {
            self.resume_checked(&new, true).await?;
        } else {
            self.resume(&new).await;
        }
        Ok(())
    }

    /// Wait for both container health and daemon readiness within one deadline.
    async fn wait_healthy(&self, id: &str) -> Res<String> {
        let started = tokio::time::Instant::now();
        let deadline = started + self.cfg.health_timeout;
        loop {
            let mut status_error = None;
            match self.engine.health(id).await? {
                // Docker also reports Healthy when no healthcheck is configured.
                // The process may be running before the daemon socket is ready.
                Health::Healthy => {
                    match tokio::time::timeout_at(deadline, self.exec_ok(id, status_argv())).await {
                        Ok(Ok(_)) => {
                            return Ok(format!(
                                "healthy after {}s; the daemon answers",
                                started.elapsed().as_secs()
                            ));
                        }
                        Ok(Err(e)) => status_error = Some(e),
                        Err(_) => status_error = Some("readiness deadline elapsed".into()),
                    }
                }
                Health::Starting => {}
                Health::Unhealthy(why) => return Err(format!("unhealthy: {why}")),
                Health::Exited(why) => return Err(format!("the container stopped: {why}")),
            }
            if started.elapsed() >= self.cfg.health_timeout {
                let why = status_error
                    .map(|e| format!("; terrarium daemon status: {e}"))
                    .unwrap_or_default();
                return Err(format!(
                    "not healthy after {}s{why}",
                    self.cfg.health_timeout.as_secs()
                ));
            }
            tokio::time::sleep_until(
                (tokio::time::Instant::now() + self.cfg.health_poll).min(deadline),
            )
            .await;
        }
    }

    /// Go back to `cur`: remove what runs under its name, restore
    /// `backup` with the old image, and start the old image.
    async fn recover(
        &self,
        cur: &Container,
        backup: &str,
        deadlines: Option<(tokio::time::Instant, tokio::time::Instant)>,
    ) -> Result<(), Fail> {
        let Some((deadline, recovery_deadline)) = deadlines else {
            return self.recover_inner(cur, backup, None, &mut None).await;
        };
        let mut helper = None;
        let work_deadline = recovery_deadline - HELPER_TERMINATION_RESERVE;
        let recovered = tokio::time::timeout_at(
            work_deadline,
            self.recover_inner(cur, backup, Some(work_deadline), &mut helper),
        )
        .await;
        if let Err(e) = self.end_helper(&mut helper, recovery_deadline).await {
            return Err(self.stop_recreate(cur, backup, deadline, e).await.1);
        }
        match recovered {
            Ok(Ok(())) => Ok(()),
            failure => {
                let reason = match failure {
                    Ok(Err(Fail(e))) => e,
                    _ => "recreate recovery deadline elapsed".into(),
                };
                Err(self.stop_recreate(cur, backup, deadline, reason).await.1)
            }
        }
    }

    async fn stop_recreate(
        &self,
        cur: &Container,
        backup: &str,
        deadline: tokio::time::Instant,
        reason: String,
    ) -> (Outcome, Fail) {
        // The source may have been removed. Resolve the current target so an
        // uncertain destination or recovered daemon is stopped, not just c0.
        let stop = tokio::time::timeout_at(deadline, async {
            let now = self.engine.target().await;
            let id = now.as_ref().map_or(cur.id.as_str(), |c| c.id.as_str());
            self.engine.stop_for_recovery(id).await
        })
        .await;
        (
            Outcome::RecoveryFailed,
            Fail(format!(
                "{reason}; recreate recovery failed; stop confirmed: {}; manual restore required: {backup}.hot-swap.sqlite; verify any outstanding helper has stopped",
                matches!(stop, Ok(Ok(())))
            )),
        )
    }

    async fn recover_inner(
        &self,
        cur: &Container,
        backup: &str,
        snapshot_deadline: Option<tokio::time::Instant>,
        helper: &mut Option<String>,
    ) -> Result<(), Fail> {
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
                let out = if let Some(deadline) = snapshot_deadline {
                    let out = self
                        .tracked_once(
                            cur,
                            &cur.image,
                            hot_argv("restore-database", &[backup]),
                            deadline,
                            helper,
                        )
                        .await?;
                    self.end_helper(helper, deadline).await?;
                    out
                } else {
                    self.engine
                        .run_once(cur, &cur.image, restore_argv(backup))
                        .await?
                };
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
        if snapshot_deadline.is_some() {
            self.resume_checked(&old, true).await?;
        } else {
            self.resume(&old).await;
        }
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
