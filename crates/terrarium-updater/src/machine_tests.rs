//! The update and rollback state machine with a fake engine: no Docker.

#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::collections::{HashMap, HashSet};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use super::*;
use crate::engine::{BoxFut, ExecOut};

const OLD: &str = "terrarium:dev";
const REPO: &str = "ghcr.io/example/terrarium";
const DIGEST: &str = "sha256:2222";
const BACKUP: &str = "/state/backups/terrarium-backup-1.zip";

#[derive(Default)]
struct State {
    resume_status: Option<String>,
    resume_hang: bool,
    jobs_output: Option<String>,
    jobs_hang: bool,
    fail_once: HashSet<String>,
    hot: bool,
    supervised: bool,
    database: u32,
    snapshot_database: u32,
    startup_crash: bool,
    pending_exec: Option<String>,
    pending_once: Option<String>,
    pending_old_health: bool,
    pending_stop: bool,
    pending_engine: Option<&'static str>,
    jobs_delay: Duration,
    helpers: HashMap<String, (Container, Image, Vec<String>)>,
    helper_next: u32,
    staged: Option<Image>,
    /// The containers by name.
    containers: HashMap<String, Container>,
    images: HashMap<String, Image>,
    /// What the updater did, in order.
    calls: Vec<String>,
    /// The operations that fail: `pull`, `drain`, `backup`, `stop`,
    /// `restore`, `create:<ref>`, `health:<ref>`, `status:<ref>`.
    fail: HashSet<String>,
    /// Operations that fail this many times before succeeding.
    transient_fail: HashMap<String, usize>,
    next: u32,
}

#[derive(Default, Clone)]
struct Fake(Arc<Mutex<State>>);

impl Fake {
    fn with_old() -> Self {
        let f = Self::default();
        {
            let mut s = f.0.lock().unwrap();
            let img = old_image();
            s.images.insert(img.id.clone(), img.clone());
            s.containers.insert(
                "terrarium".into(),
                Container {
                    id: "c0".into(),
                    name: "terrarium".into(),
                    image: img,
                    spec: serde_json::json!({"Name": "/terrarium"}),
                },
            );
        }
        f
    }

    fn fail(&self, what: &str) {
        self.0.lock().unwrap().fail.insert(what.into());
    }

    fn calls(&self) -> Vec<String> {
        self.0.lock().unwrap().calls.clone()
    }

    fn running_image(&self) -> Option<String> {
        self.0
            .lock()
            .unwrap()
            .containers
            .get("terrarium")
            .map(|c| c.image.reference.clone())
    }

    fn call(&self, c: String) -> bool {
        let mut s = self.0.lock().unwrap();
        let key = c.split_once(' ').map_or(c.as_str(), |(k, _)| k).to_owned();
        s.calls.push(c);
        if let Some(remaining) = s.transient_fail.get_mut(&key)
            && *remaining > 0
        {
            *remaining -= 1;
            return true;
        }
        s.fail.contains(&key) || s.fail_once.remove(&key)
    }

    fn image_of(&self, id: &str) -> Option<String> {
        let s = self.0.lock().unwrap();
        s.containers
            .values()
            .find(|c| c.id == id)
            .map(|c| c.image.reference.clone())
    }
}

fn old_image() -> Image {
    Image {
        reference: OLD.into(),
        id: "sha256:1111".into(),
        digest: None,
        version: Some("0.1.0".into()),
    }
}

impl Engine for Fake {
    fn supervised_snapshot<'a>(&'a self, _cur: &'a Container) -> BoxFut<'a, Res<bool>> {
        Box::pin(async { Ok(self.0.lock().unwrap().supervised) })
    }
    fn can_hot_swap<'a>(&'a self, _cur: &'a Container, _to: &'a Image) -> BoxFut<'a, Res<bool>> {
        Box::pin(async { Ok(self.0.lock().unwrap().hot) })
    }
    fn prepare<'a>(&'a self, spec: &'a Container, _image: &'a Image) -> BoxFut<'a, Res<Container>> {
        Box::pin(async move {
            if self.0.lock().unwrap().fail.contains("environment") {
                return Err("Compose environment resolution failed (values hidden)".into());
            }
            let mut prepared = spec.clone();
            prepared.spec["prepared"] = serde_json::json!(true);
            Ok(prepared)
        })
    }
    fn target(&self) -> BoxFut<'_, Res<Container>> {
        Box::pin(async move {
            self.0
                .lock()
                .unwrap()
                .containers
                .get("terrarium")
                .cloned()
                .ok_or_else(|| "no terrarium container".to_owned())
        })
    }

    fn image<'a>(&'a self, reference: &'a str) -> BoxFut<'a, Res<Option<Image>>> {
        Box::pin(async move { Ok(self.0.lock().unwrap().images.get(reference).cloned()) })
    }

    fn pull<'a>(&'a self, reference: &'a str) -> BoxFut<'a, Res<Image>> {
        Box::pin(async move {
            if self.call(format!("pull {reference}")) {
                return Err("pull refused".into());
            }
            let digest = reference.split_once('@').map(|(_, d)| d.to_owned());
            let img = Image {
                reference: reference.into(),
                id: format!("sha256:id-{}", digest.clone().unwrap_or_default()),
                digest,
                version: Some("0.2.0".into()),
            };
            self.0
                .lock()
                .unwrap()
                .images
                .insert(img.id.clone(), img.clone());
            Ok(img)
        })
    }

    fn exec<'a>(&'a self, id: &'a str, argv: Vec<String>) -> BoxFut<'a, Res<ExecOut>> {
        Box::pin(async move {
            if argv[0] == "python3" {
                let cmd = &argv[2];
                let failed = self.call(format!("hot:{cmd}"));
                let pending = {
                    let s = self.0.lock().unwrap();
                    let key = format!("hot:{cmd}");
                    let count = s.calls.iter().filter(|c| *c == &key).count();
                    s.pending_exec.as_deref() == Some(format!("hot:{cmd}:{count}").as_str())
                };
                if pending {
                    self.call("pending-exec".into());
                    std::future::pending::<()>().await;
                }
                if cmd == "start" && !failed {
                    let mut s = self.0.lock().unwrap();
                    let image = if argv[3] == "b".repeat(64) {
                        s.staged.clone().unwrap()
                    } else {
                        old_image()
                    };
                    s.containers.get_mut("terrarium").unwrap().image = image;
                }
                return Ok(ExecOut {
                    code: i64::from(failed),
                    stdout: if cmd == "probe" {
                        "a".repeat(64)
                    } else {
                        String::new()
                    },
                    stderr: "control failed".into(),
                });
            }
            let what = match (argv[0].as_str(), argv.get(3).map(String::as_str)) {
                ("terrarium", Some("status")) if argv.get(2).is_some_and(|s| s == "update") => {
                    "update-status".to_owned()
                }
                ("terrarium", Some("drain")) => "drain".to_owned(),
                ("terrarium", Some("drain-jobs")) => "drain-jobs".to_owned(),
                ("terrarium", Some("resume")) => "resume".to_owned(),
                ("terrariumd", _) => "backup".to_owned(),
                _ => format!("status:{}", self.image_of(id).unwrap_or_default()),
            };
            let failed = self.call(format!("{what} {id}"));
            if what == "drain" && self.0.lock().unwrap().supervised {
                // A completed write after the online ZIP, before quiescence.
                self.0.lock().unwrap().database = 2;
            }
            if what == "resume" && self.0.lock().unwrap().resume_hang {
                std::future::pending::<()>().await;
            }
            if what == "drain-jobs" {
                let delay = self.0.lock().unwrap().jobs_delay;
                if !delay.is_zero() {
                    tokio::time::sleep(delay).await;
                }
                let hang = self.0.lock().unwrap().jobs_hang;
                if hang {
                    std::future::pending::<()>().await;
                }
            }
            let stdout = if what == "update-status" {
                let s = self.0.lock().unwrap();
                s.resume_status.clone().unwrap_or_else(|| if s.supervised {
                    r#"{"drain":{"on":false,"jobs_drain":null}}"#.into()
                } else { String::new() })
            } else if what == "backup" {
                format!(r#"{{"name": "terrarium-backup-1.zip", "path": "{BACKUP}"}}"#)
            } else if what == "drain-jobs" {
                self.0
                    .lock()
                    .unwrap()
                    .jobs_output
                    .clone()
                    .unwrap_or_else(|| r#"{"jobs":[]}"#.into())
            } else {
                String::new()
            };
            Ok(ExecOut {
                code: i64::from(failed),
                stdout,
                stderr: if failed {
                    format!("{what} broke")
                } else {
                    String::new()
                },
            })
        })
    }

    fn stop<'a>(&'a self, id: &'a str) -> BoxFut<'a, Res<()>> {
        Box::pin(async move {
            if self.call(format!("stop {id}")) {
                return Err("stop refused".into());
            }
            Ok(())
        })
    }

    fn remove<'a>(&'a self, id: &'a str) -> BoxFut<'a, Res<()>> {
        Box::pin(async move {
            self.call(format!("remove {id}"));
            if self.0.lock().unwrap().pending_engine == Some("remove") {
                self.call("pending-remove".into());
                std::future::pending::<()>().await;
            }
            self.0.lock().unwrap().containers.retain(|_, c| c.id != id);
            Ok(())
        })
    }

    fn stop_for_recovery<'a>(&'a self, id: &'a str) -> BoxFut<'a, Res<()>> {
        Box::pin(async move {
            self.call(format!("stop-for-recovery {id}"));
            let pending = self.0.lock().unwrap().pending_stop;
            if pending {
                self.call("pending-stop".into());
                std::future::pending::<()>().await;
            }
            Ok(())
        })
    }

    fn create<'a>(&'a self, spec: &'a Container, image: &'a Image) -> BoxFut<'a, Res<Container>> {
        Box::pin(async move {
            if self.call(format!("create:{} new", image.reference)) {
                return Err("create refused".into());
            }
            if self.0.lock().unwrap().pending_engine == Some("create") {
                self.call("pending-create".into());
                std::future::pending::<()>().await;
            }
            let mut s = self.0.lock().unwrap();
            s.next += 1;
            let c = Container {
                id: format!("c{}", s.next),
                name: spec.name.clone(),
                image: image.clone(),
                spec: spec.spec.clone(),
            };
            s.containers.insert(c.name.clone(), c.clone());
            Ok(c)
        })
    }

    fn run_once<'a>(
        &'a self,
        _spec: &'a Container,
        image: &'a Image,
        argv: Vec<String>,
    ) -> BoxFut<'a, Res<ExecOut>> {
        Box::pin(async move {
            let argv = if argv.first().is_some_and(|s| s == "timeout") {
                argv[3..].to_vec()
            } else {
                argv
            };
            if argv.get(1).is_some_and(|s| s == "required-templates-check") {
                let failed = self.0.lock().unwrap().fail.contains("required-templates");
                return Ok(ExecOut {
                    code: i64::from(
                        self.0
                            .lock()
                            .unwrap()
                            .fail
                            .contains("required-templates-crash"),
                    ),
                    stdout: if failed {
                        "warning: required daemon template global:request is missing".into()
                    } else {
                        "required daemon templates: all present".into()
                    },
                    ..Default::default()
                });
            }
            if argv[0] == "python3" {
                let command = &argv[2];
                let failed = self.call(format!("hot:{command}"));
                let pending = self.0.lock().unwrap().pending_once.as_deref() == Some(command);
                if pending {
                    self.call("pending-once".into());
                    std::future::pending::<()>().await;
                }
                if command == "stage" {
                    self.0.lock().unwrap().staged = Some(image.clone());
                }
                if !failed {
                    let mut s = self.0.lock().unwrap();
                    if command == "snapshot" {
                        s.snapshot_database = s.database;
                    }
                    if command == "restore-database" {
                        s.database = s.snapshot_database;
                    }
                }
                return Ok(ExecOut {
                    code: i64::from(failed),
                    stdout: if command == "stage" {
                        "b".repeat(64)
                    } else {
                        String::new()
                    },
                    ..Default::default()
                });
            }
            if argv[0] == "/opt/terrarium/bundled/bin/terrariumd" && argv[2] == "create" {
                let failed = self.call("offline-backup".into());
                return Ok(ExecOut {
                    code: i64::from(failed),
                    stdout: format!(r#"{{"path":"{BACKUP}"}}"#),
                    ..Default::default()
                });
            }
            let failed = self.call(format!("restore {} {}", image.reference, argv[3]));
            Ok(ExecOut {
                code: i64::from(failed),
                stdout: String::new(),
                stderr: if failed {
                    "restore broke".into()
                } else {
                    String::new()
                },
            })
        })
    }

    fn prepare_once<'a>(&'a self, spec: &'a Container, image: &'a Image, argv: Vec<String>) -> BoxFut<'a, Res<String>> {
        Box::pin(async move {
            let mut s = self.0.lock().unwrap();
            s.helper_next += 1;
            let id = format!("helper{}", s.helper_next);
            s.calls.push(format!("helper-created {id}"));
            s.helpers.insert(id.clone(), (spec.clone(), image.clone(), argv));
            Ok(id)
        })
    }

    fn run_prepared_once<'a>(&'a self, id: &'a str) -> BoxFut<'a, Res<ExecOut>> {
        Box::pin(async move {
            self.call(format!("helper-start {id}"));
            let (spec, image, argv) = self.0.lock().unwrap().helpers.get(id).unwrap().clone();
            self.run_once(&spec, &image, argv).await
        })
    }

    fn remove_once<'a>(&'a self, id: &'a str) -> BoxFut<'a, Res<()>> {
        Box::pin(async move {
            if self.call(format!("helper-remove {id}")) {
                return Err("helper termination unverified".into());
            }
            self.0.lock().unwrap().helpers.remove(id);
            self.call(format!("helper-gone {id}"));
            Ok(())
        })
    }

    fn health<'a>(&'a self, id: &'a str) -> BoxFut<'a, Res<Health>> {
        Box::pin(async move {
            let pending = self.0.lock().unwrap().pending_old_health;
            if pending && self.image_of(id).as_deref() == Some(OLD) {
                self.call("pending-old-health".into());
                std::future::pending::<()>().await;
            }
            let image = self.image_of(id).unwrap_or_default();
            let s = self.0.lock().unwrap();
            if s.fail.contains(&format!("health:{image}")) {
                return Ok(Health::Unhealthy("the check failed 3 times".into()));
            }
            Ok(Health::Healthy)
        })
    }

    fn daemon_failure_since<'a>(
        &'a self,
        id: &'a str,
        since: &'a str,
    ) -> BoxFut<'a, Res<Option<String>>> {
        Box::pin(async move {
            let (seconds, nanos) = since.split_once('.').unwrap();
            assert!(seconds.parse::<i64>().is_ok());
            assert_eq!(nanos.len(), 9);
            assert!(nanos.parse::<u32>().unwrap() < 1_000_000_000);
            if self.0.lock().unwrap().fail.contains("startup-log") {
                return Err("transport error with private values".into());
            }
            let crashes = self.0.lock().unwrap().startup_crash;
            if crashes && self.image_of(id).as_deref() == Some(new_ref().as_str()) {
                self.call("daemon-exit-1".into());
                return Ok(Some("new daemon exited during startup; last error: FOREIGN KEY constraint failed (values hidden)".into()));
            }
            Ok(None)
        })
    }
}

struct FakeRegistry;

impl Registry for FakeRegistry {
    fn resolve<'a>(&'a self, repository: &'a str, tag: &'a str) -> BoxFut<'a, Res<String>> {
        Box::pin(async move {
            assert_eq!(repository, REPO);
            match tag {
                "latest" | "0.2.0" => Ok(DIGEST.into()),
                "0.1.0" => Ok("sha256:1111digest".into()),
                other => Err(format!("no tag {other}")),
            }
        })
    }
}

struct FakeVerify {
    mode: &'static str,
    ok: bool,
    seen: Mutex<Vec<String>>,
}

impl Verifier for FakeVerify {
    fn mode(&self) -> String {
        self.mode.into()
    }

    fn verify<'a>(&'a self, reference: &'a str) -> BoxFut<'a, Res<String>> {
        Box::pin(async move {
            self.seen.lock().unwrap().push(reference.into());
            if self.ok {
                Ok("signed by the release workflow".into())
            } else {
                Err("no signature".into())
            }
        })
    }
}

struct Rig {
    fake: Fake,
    verify: Arc<FakeVerify>,
    updater: Arc<Updater>,
    store: Store,
    _dir: tempfile::TempDir,
}

fn rig_with(mode: &'static str, ok: bool) -> Rig {
    let fake = Fake::with_old();
    let verify = Arc::new(FakeVerify {
        mode,
        ok,
        seen: Mutex::default(),
    });
    let dir = tempfile::tempdir().unwrap();
    let store = Store::new(dir.path(), 10);
    let cfg = Config {
        repository: REPO.into(),
        drain_wait: Duration::from_secs(30),
        health_timeout: Duration::from_millis(50),
        health_poll: Duration::from_millis(1),
        version: "0.1.0+test".into(),
    };
    let updater = Arc::new(Updater::new(
        Arc::new(fake.clone()),
        Arc::new(FakeRegistry),
        verify.clone(),
        store.clone(),
        cfg,
    ));
    Rig {
        fake,
        verify,
        updater,
        store,
        _dir: dir,
    }
}

fn rig() -> Rig {
    rig_with("gh", true)
}

async fn wait_front(
    store: &Store,
    matches: impl Fn(&serde_json::Value) -> bool,
) -> serde_json::Value {
    tokio::time::timeout(Duration::from_secs(3), async {
        loop {
            if let Ok(bytes) = std::fs::read(store.dir().join("front/current-run.json"))
                && let Ok(value) = serde_json::from_slice::<serde_json::Value>(&bytes)
                && matches(&value)
            {
                return value;
            }
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
    })
    .await
    .unwrap()
}

#[tokio::test]
async fn front_export_tracks_steps_and_survives_a_mid_step_stop() {
    let r = rig();
    let (_, guard) = r
        .updater
        .begin(RunKind::Update, "latest", "private-actor")
        .unwrap();
    let path = r.store.dir().join("front/current-run.json");
    let read =
        || serde_json::from_slice::<serde_json::Value>(&std::fs::read(&path).unwrap()).unwrap();
    wait_front(&r.store, |v| v["outcome"] == "running").await;
    let idx = r.updater.begin_step("snapshot-database");
    wait_front(&r.store, |v| v["steps"][idx]["state"] == "running").await;
    assert_eq!(read()["steps"][idx]["state"], "running");
    // A new reader after an updater crash sees a complete last transition.
    let reopened = Store::new(r.store.dir(), 10);
    let value: serde_json::Value = serde_json::from_slice(
        &std::fs::read(reopened.dir().join("front/current-run.json")).unwrap(),
    )
    .unwrap();
    assert_eq!(value["steps"][idx]["name"], "snapshot-database");
    r.updater
        .end_step(idx, StepState::Failed, "private-command-output");
    wait_front(&r.store, |v| v["steps"][idx]["state"] == "failed").await;
    assert_eq!(read()["steps"][idx]["state"], "failed");
    assert!(!read().to_string().contains("private-"));
    drop(guard);
}

#[tokio::test]
async fn front_export_retains_the_completed_run() {
    let r = rig();
    let run = r
        .updater
        .run_now(RunKind::Update, "latest", "operator")
        .await;
    wait_front(&r.store, |v| v["ended_at"].is_string()).await;
    let value: serde_json::Value = serde_json::from_slice(
        &std::fs::read(r.store.dir().join("front/current-run.json")).unwrap(),
    )
    .unwrap();
    assert_eq!(value["outcome"], run.outcome.as_str());
    assert_eq!(value["ended_at"], run.ended_at.unwrap());
    assert_eq!(value["steps"].as_array().unwrap().len(), run.steps.len());
}

#[tokio::test]
async fn stalled_front_writer_cannot_delay_hold_deadline_recovery() {
    struct Release(Arc<(Mutex<bool>, std::sync::Condvar)>);
    impl Drop for Release {
        fn drop(&mut self) {
            *self.0.0.lock().unwrap() = true;
            self.0.1.notify_all();
        }
    }
    let mut r = rig();
    let gate = Arc::new((Mutex::new(false), std::sync::Condvar::new()));
    let release = Release(gate.clone());
    let entered = Arc::new(tokio::sync::Notify::new());
    let signal = entered.clone();
    let store = r.store.clone();
    let updater = Arc::get_mut(&mut r.updater).unwrap();
    updater.front = front_publisher(Arc::new(move |run| {
        signal.notify_one();
        let (lock, wake) = &*gate;
        let mut released = lock.lock().unwrap();
        while !*released {
            released = wake.wait(released).unwrap();
        }
        store.export_front(run)
    }));
    updater.hot_budget = (
        Duration::from_secs(12),
        Duration::from_secs(6),
        Duration::from_secs(1),
    );
    updater.cfg.health_timeout = Duration::from_secs(240);
    {
        let mut state = r.fake.0.lock().unwrap();
        state.hot = true;
        state.startup_crash = true;
        state.pending_once = Some("restore-database".into());
    }
    let started = tokio::time::Instant::now();
    let updater = r.updater.clone();
    let work =
        tokio::spawn(async move { updater.run_now(RunKind::Update, "latest", "operator").await });
    tokio::time::timeout(Duration::from_secs(3), entered.notified())
        .await
        .unwrap();
    let run = tokio::time::timeout(Duration::from_secs(13), work)
        .await
        .expect("stalled projection must not delay recovery")
        .unwrap();
    assert!(started.elapsed() < Duration::from_millis(12500));
    assert_eq!(run.outcome, Outcome::RecoveryFailed);
    assert!(
        r.fake
            .calls()
            .iter()
            .any(|call| call == "stop-for-recovery c0")
    );
    // No timeout or automatic release exists in the writer. It stays blocked
    // until after the safety-critical update/recovery has finished.
    assert!(!*release.0.0.lock().unwrap());
    drop(release);
    let value = wait_front(&r.store, |v| v["ended_at"].is_string()).await;
    assert_eq!(value["outcome"], "recovery_failed");
    assert_eq!(value["steps"].as_array().unwrap().len(), run.steps.len());
}

#[cfg(feature = "bin")]
#[tokio::test]
#[ignore = "requires owned tiny containers on terrarium-docker-scratch"]
async fn real_docker_startup_health() {
    let healthy = std::env::var("TER984_HEALTHY_CONTAINER").unwrap();
    let crashed = std::env::var("TER984_CRASHED_CONTAINER").unwrap();
    assert!(healthy.starts_with("ter-984-"));
    assert!(crashed.starts_with("ter-984-"));
    let mut r = rig();
    let updater = Arc::get_mut(&mut r.updater).unwrap();
    updater.engine = Arc::new(crate::docker::Docker {
        socket: "/var/run/docker.sock".into(),
        target: None,
    });
    updater.cfg.health_timeout = Duration::from_secs(5);
    let deadline = tokio::time::Instant::now() + Duration::from_secs(10);
    // The fixtures are started after the epoch. This exercises dockerd's
    // fractional UNIX timestamp parser as well as the real health/exec path.
    updater
        .wait_hot_healthy_until(&healthy, deadline, Some("0.000000000"))
        .await
        .unwrap();
    let reason = updater
        .wait_hot_healthy_until(&crashed, deadline, Some("0.000000000"))
        .await
        .unwrap_err();
    assert!(reason.contains("FOREIGN KEY constraint failed"), "{reason}");
    assert!(!reason.contains("private-fixture-value"));
}

#[tokio::test]
async fn resume_retries_an_old_daemon_that_is_not_ready() {
    let r = rig();
    r.fake.0.lock().unwrap().fail_once.insert("resume".into());
    let (_run, _guard) = r.updater.begin(RunKind::Update, "test", "test").unwrap();
    let cur = r.fake.target().await.unwrap();
    r.updater.resume(&cur).await;
    let calls = r.fake.calls();
    assert_eq!(calls.iter().filter(|c| *c == "resume c0").count(), 2);
    assert_eq!(
        r.updater
            .running()
            .as_ref()
            .unwrap()
            .steps
            .last()
            .unwrap()
            .state,
        StepState::Ok
    );
}

#[tokio::test]
async fn resume_accepts_verified_open_drains_after_a_failed_or_slow_wake() {
    for hang in [false, true] {
        let r = rig();
        let (_run, _guard) = r.updater.begin(RunKind::Update, "test", "test").unwrap();
        r.fake.fail("resume");
        {
            let mut s = r.fake.0.lock().unwrap();
            s.resume_hang = hang;
            s.resume_status = Some(r#"{"drain":{"on":false,"jobs_drain":null}}"#.into());
        }
        let cur = r.fake.target().await.unwrap();
        tokio::time::timeout(Duration::from_secs(10), r.updater.resume(&cur))
            .await
            .unwrap();
        assert!(r.fake.calls().iter().any(|c| c == "update-status c0"));
        assert_eq!(
            r.updater
                .running()
                .as_ref()
                .unwrap()
                .steps
                .last()
                .unwrap()
                .state,
            StepState::Ok
        );
    }
}

#[tokio::test]
async fn missing_jobs_endpoint_warns_without_exposing_values_and_continues() {
    let r = rig();
    r.fake.fail("drain-jobs");
    let run = r
        .updater
        .run_now(RunKind::Update, "latest", "operator")
        .await;
    assert_eq!(run.outcome, Outcome::Succeeded);
    let step = run.steps.iter().find(|s| s.name == "drain-jobs").unwrap();
    assert_eq!(step.state, StepState::Failed);
    assert!(step.detail.contains("values hidden"));
    assert!(!step.detail.contains("broke"));
    let calls = r.fake.calls();
    assert!(
        calls.iter().position(|c| c == "drain-jobs c0").unwrap()
            < calls.iter().position(|c| c == "stop c0").unwrap()
    );
    assert_eq!(calls.last().unwrap(), "resume c1");
}

#[tokio::test]
async fn jobs_wait_expiry_records_remaining_jobs_and_continues() {
    let r = rig();
    r.fake.0.lock().unwrap().jobs_output = Some(r#"{"timed_out":true,"jobs":["job-123"]}"#.into());
    let run = r
        .updater
        .run_now(RunKind::Update, "latest", "operator")
        .await;
    assert_eq!(run.outcome, Outcome::Succeeded);
    assert!(
        run.steps
            .iter()
            .find(|s| s.name == "drain-jobs")
            .unwrap()
            .detail
            .contains("job-123")
    );
}

#[tokio::test]
async fn hung_jobs_drain_has_an_outer_deadline() {
    let mut r = rig();
    Arc::get_mut(&mut r.updater).unwrap().cfg.drain_wait = Duration::ZERO;
    r.fake.0.lock().unwrap().jobs_hang = true;
    let run = tokio::time::timeout(
        Duration::from_secs(7),
        r.updater.run_now(RunKind::Update, "latest", "operator"),
    )
    .await
    .unwrap();
    assert_eq!(run.outcome, Outcome::Succeeded);
    assert!(steps(&run).contains(&"!drain-jobs".into()));
}

#[tokio::test]
async fn hot_swap_keeps_container_and_commits_only_after_health() {
    let r = rig();
    r.fake.0.lock().unwrap().hot = true;
    let run = r
        .updater
        .run_now(RunKind::Update, "latest", "operator")
        .await;
    assert_eq!(run.outcome, Outcome::Succeeded, "{run:#?}");
    let calls = r.fake.calls();
    assert!(
        !calls
            .iter()
            .any(|c| c.starts_with("drain ") || c.starts_with("remove") || c.starts_with("stop"))
    );
    assert_eq!(calls.last().unwrap(), "resume c0");
    assert!(
        calls.iter().position(|c| c == "hot:ready").unwrap()
            < calls.iter().position(|c| c == "resume c0").unwrap()
    );
    assert!(
        calls.iter().position(|c| c == "backup c0").unwrap()
            < calls.iter().position(|c| c == "hot:pause").unwrap()
    );
    assert!(
        calls.iter().position(|c| c == "hot:pause").unwrap()
            < calls.iter().position(|c| c == "hot:snapshot").unwrap()
    );
    assert!(!calls.iter().any(|c| c == "offline-backup"));
    let backup = run.steps.iter().find(|s| s.name == "backup").unwrap();
    assert!(backup.detail.contains("began at"));
    assert!(
        backup
            .detail
            .contains("writes after that time may not be included")
    );
    // The hot swap records the original engine spec: a rollback replaces
    // the container with it instead of reusing the hot-swapped one.
    assert!(r.store.previous().is_some_and(|p| p.spec.is_some()));
    let rollback = r
        .updater
        .run_now(RunKind::Rollback, "previous", "operator")
        .await;
    assert_eq!(rollback.outcome, Outcome::Succeeded, "{rollback:#?}");
    assert!(r.fake.calls().iter().any(|c| c == "stop c0"));
}

#[tokio::test]
async fn hot_swap_failed_health_restores_before_old_release_admission() {
    let r = rig();
    r.fake.0.lock().unwrap().hot = true;
    r.fake.fail(&format!("health:{}", new_ref()));
    let run = r
        .updater
        .run_now(RunKind::Update, "latest", "operator")
        .await;
    assert_eq!(run.outcome, Outcome::RolledBack, "{run:#?}");
    assert_eq!(r.fake.running_image().as_deref(), Some(OLD));
    let calls = r.fake.calls();
    assert_eq!(calls.iter().filter(|c| *c == "hot:pause").count(), 2);
    assert_eq!(calls.iter().filter(|c| *c == "hot:commit").count(), 1);
    assert!(
        calls
            .iter()
            .position(|c| c == "hot:restore-database")
            .unwrap()
            < calls.iter().position(|c| c == "hot:commit").unwrap()
    );
}

#[tokio::test]
async fn unreadable_startup_log_does_not_replace_health_checks() {
    for healthy in [true, false] {
        let r = rig();
        r.fake.0.lock().unwrap().hot = true;
        r.fake.fail("startup-log");
        if !healthy {
            r.fake.fail(&format!("health:{}", new_ref()));
        }
        let run = r
            .updater
            .run_now(RunKind::Update, "latest", "operator")
            .await;
        assert_eq!(
            run.outcome,
            if healthy {
                Outcome::Succeeded
            } else {
                Outcome::RolledBack
            },
            "{run:#?}"
        );
        let warnings: Vec<_> = run
            .steps
            .iter()
            .filter(|s| s.name == "startup-log-warning")
            .collect();
        assert_eq!(warnings.len(), 1);
        assert_eq!(
            warnings[0].detail,
            "cannot read daemon startup log (values hidden); continuing health checks"
        );
    }
}

#[tokio::test]
async fn hot_swap_daemon_exit_rolls_back_without_waiting_for_health_timeout() {
    let mut r = rig();
    Arc::get_mut(&mut r.updater).unwrap().cfg.health_timeout = Duration::from_secs(240);
    {
        let mut state = r.fake.0.lock().unwrap();
        state.hot = true;
        state.startup_crash = true;
    }
    // Docker health deliberately remains healthy: it still describes the
    // supervisor/old process, not the just-crashed daemon child.
    let run = tokio::time::timeout(
        Duration::from_secs(3),
        r.updater.run_now(RunKind::Update, "latest", "operator"),
    )
    .await
    .expect("crash rollback must not wait four minutes");
    assert_eq!(run.outcome, Outcome::RolledBack, "{run:#?}");
    assert_eq!(r.fake.running_image().as_deref(), Some(OLD));
    let health = run.steps.iter().find(|s| s.name == "health").unwrap();
    assert_eq!(health.state, StepState::Failed);
    assert!(
        health
            .detail
            .contains("FOREIGN KEY constraint failed (values hidden)")
    );
    let calls = r.fake.calls();
    assert!(
        calls.iter().position(|c| c == "daemon-exit-1").unwrap()
            < calls
                .iter()
                .position(|c| c == "hot:restore-database")
                .unwrap()
    );
}

#[tokio::test]
async fn every_pending_hot_recovery_stage_ends_inside_the_original_hold_budget() {
    let mut cases = tokio::task::JoinSet::new();
    for case in [
        "cancel",
        "cancel-health",
        "snapshot-start",
        "snapshot-health",
        "snapshot-commit",
        "pause",
        "restore",
        "start",
        "health",
        "commit",
        "stop",
    ] {
        cases.spawn(async move {
            let mut r = rig();
            let updater = Arc::get_mut(&mut r.updater).unwrap();
            // Keep >5 s for the real helper kill grace, but make all cases
            // concurrent and short. Production uses 900/60/5 seconds.
            updater.hot_budget = (
                Duration::from_secs(12),
                Duration::from_secs(6),
                Duration::from_secs(1),
            );
            updater.cfg.health_timeout = Duration::from_secs(240);
            {
                let mut s = r.fake.0.lock().unwrap();
                s.hot = true;
                if case.starts_with("cancel") {
                    s.fail.insert("hot:pause".into());
                } else if case.starts_with("snapshot") {
                    s.fail.insert("hot:snapshot".into());
                } else {
                    s.startup_crash = true;
                }
                s.pending_exec = match case {
                    "cancel" => Some("hot:cancel-pause:1".into()),
                    "snapshot-start" => Some("hot:start:1".into()),
                    "snapshot-commit" | "commit" => Some("hot:commit:1".into()),
                    "pause" => Some("hot:pause:2".into()),
                    "start" => Some("hot:start:2".into()),
                    _ => None,
                };
                s.pending_old_health =
                    matches!(case, "cancel-health" | "snapshot-health" | "health");
                if case == "restore" {
                    s.pending_once = Some("restore-database".into());
                }
                if case == "stop" {
                    s.fail.insert("hot:restore-database".into());
                    s.pending_stop = true;
                }
            }
            let started = tokio::time::Instant::now();
            let run = tokio::time::timeout(
                Duration::from_secs(13),
                r.updater.run_now(RunKind::Update, "latest", "operator"),
            )
            .await
            .expect("pending recovery must not hold the daemon indefinitely");
            assert!(
                started.elapsed() < Duration::from_millis(12500),
                "{case}: {:?}",
                started.elapsed()
            );
            assert_eq!(run.outcome, Outcome::RecoveryFailed, "{case}: {run:#?}");
            assert_eq!(r.store.history()[0].outcome, Outcome::RecoveryFailed);
            assert!(run.outcome.is_failure());
            let error = run.error.unwrap();
            assert!(
                error.contains(&format!("{BACKUP}.hot-swap.sqlite")),
                "{case}: {error}"
            );
            let calls = r.fake.calls();
            assert!(
                calls.iter().any(|c| c.starts_with("pending-")),
                "{case}: {calls:?}"
            );
            assert!(
                calls.iter().any(|c| c == "stop-for-recovery c0"),
                "{case}: {calls:?}"
            );
            assert!(
                !calls.iter().any(|c| c.starts_with("resume ")),
                "{case}: {calls:?}"
            );
            if case == "restore" || case == "stop" {
                assert_eq!(
                    calls.iter().filter(|c| *c == "hot:start").count(),
                    1,
                    "never start old daemon after an uncertain restore"
                );
            }
            if case == "stop" {
                assert!(error.contains("stop unconfirmed"));
            }
        });
    }
    while let Some(result) = cases.join_next().await {
        result.unwrap();
    }
}

#[tokio::test]
async fn failed_restore_stops_the_container_and_never_starts_or_admits_the_old_daemon() {
    let r = rig();
    {
        let mut s = r.fake.0.lock().unwrap();
        s.hot = true;
        s.startup_crash = true;
        s.fail.insert("hot:restore-database".into());
    }
    let run = r
        .updater
        .run_now(RunKind::Update, "latest", "operator")
        .await;
    assert_eq!(run.outcome, Outcome::RecoveryFailed, "{run:#?}");
    let calls = r.fake.calls();
    assert_eq!(calls.iter().filter(|c| *c == "hot:start").count(), 1);
    assert!(
        !calls
            .iter()
            .any(|c| c == "hot:commit" || c.starts_with("resume "))
    );
    assert_eq!(calls.last().unwrap(), "stop-for-recovery c0");
}

#[tokio::test]
async fn hot_swap_backup_failure_never_pauses() {
    let r = rig();
    r.fake.0.lock().unwrap().hot = true;
    r.fake.fail("backup");
    let run = r
        .updater
        .run_now(RunKind::Update, "latest", "operator")
        .await;
    assert_eq!(run.outcome, Outcome::Failed);
    assert!(!r.fake.calls().iter().any(|c| c == "hot:pause"));
}

#[tokio::test]
async fn hot_swap_snapshot_failure_resumes_old_release_without_restore() {
    let r = rig();
    r.fake.0.lock().unwrap().hot = true;
    r.fake.fail("hot:snapshot");
    let run = r
        .updater
        .run_now(RunKind::Update, "latest", "operator")
        .await;
    assert_eq!(run.outcome, Outcome::Failed, "{run:#?}");
    assert!(r.fake.calls().iter().any(|c| c == "hot:start"));
    assert!(!r.fake.calls().iter().any(|c| c == "hot:restore-database"));
    assert_eq!(r.fake.running_image().as_deref(), Some(OLD));
}

#[tokio::test]
async fn ambiguous_commit_never_rewinds_the_journal() {
    let r = rig();
    r.fake.0.lock().unwrap().hot = true;
    r.fake.fail("hot:commit");
    let run = r
        .updater
        .run_now(RunKind::Update, "latest", "operator")
        .await;
    assert_eq!(run.outcome, Outcome::Failed);
    assert!(!r.fake.calls().iter().any(|c| c == "hot:restore-database"));
    assert_eq!(
        r.fake.calls().iter().filter(|c| *c == "hot:commit").count(),
        3
    );
}

#[tokio::test]
async fn lost_commit_response_is_retried_without_rewinding() {
    let r = rig();
    r.fake.0.lock().unwrap().hot = true;
    r.fake
        .0
        .lock()
        .unwrap()
        .fail_once
        .insert("hot:commit".into());
    let result = r
        .updater
        .run_now(RunKind::Update, "latest", "operator")
        .await;
    assert_eq!(result.outcome, Outcome::Succeeded);
    assert_eq!(
        r.fake.calls().iter().filter(|c| *c == "hot:commit").count(),
        2
    );
    assert!(!r.fake.calls().iter().any(|c| c == "hot:restore-database"));
}

#[tokio::test]
async fn failed_post_commit_activation_reports_failure_without_rewind() {
    let r = rig();
    r.fake.0.lock().unwrap().hot = true;
    r.fake.fail("hot:ready");
    let result = r
        .updater
        .run_now(RunKind::Update, "latest", "operator")
        .await;
    assert_eq!(result.outcome, Outcome::Failed);
    assert!(r.fake.calls().iter().any(|c| c == "hot:commit"));
    assert!(!r.fake.calls().iter().any(|c| c == "hot:restore-database"));
}

#[tokio::test]
async fn failed_pause_is_cancelled_without_switching_or_restoring() {
    let r = rig();
    r.fake.0.lock().unwrap().hot = true;
    r.fake.fail("hot:pause");
    let result = r
        .updater
        .run_now(RunKind::Update, "latest", "operator")
        .await;
    assert_eq!(result.outcome, Outcome::Failed);
    assert!(r.fake.calls().iter().any(|c| c == "hot:cancel-pause"));
    assert!(
        !r.fake
            .calls()
            .iter()
            .any(|c| c == "hot:start" || c == "hot:restore-database")
    );
}

#[test]
fn total_hold_budget_reserves_recovery_and_bounds_offline_work() {
    let now = tokio::time::Instant::now();
    assert!(budget_left(now + Duration::from_secs(50), HOT_SWAP_RECOVERY_RESERVE).is_err());
    assert!(bounded_command(vec!["backup".into()], now + Duration::from_secs(4)).is_err());
    let command = bounded_command(vec!["backup".into()], now + Duration::from_secs(100)).unwrap();
    assert_eq!(&command[..2], ["timeout", "--kill-after=5"]);
    assert_eq!(command[3], "backup");
    assert!(command[2].trim_end_matches('s').parse::<f64>().unwrap() <= 95.0);
}

#[tokio::test]
async fn hung_switch_fails_at_safety_deadline_with_recovery_time_left() {
    let hold = tokio::time::Instant::now() + HOT_SWAP_RECOVERY_RESERVE + Duration::from_millis(20);
    let deadline = hold - HOT_SWAP_RECOVERY_RESERVE;
    let result = switch_before_deadline::<()>(deadline, std::future::pending()).await;
    assert!(
        result
            .unwrap_err()
            .0
            .contains("runner hold safety deadline")
    );
    assert!(tokio::time::Instant::now() < hold);
}

fn steps(run: &Run) -> Vec<String> {
    run.steps
        .iter()
        .map(|s| {
            let st = match s.state {
                StepState::Ok => "",
                StepState::Failed => "!",
                StepState::Skipped => "-",
                StepState::Running => "?",
            };
            format!("{st}{}", s.name)
        })
        .collect()
}

fn new_ref() -> String {
    format!("{REPO}@{DIGEST}")
}

#[tokio::test]
async fn supervised_recreate_backs_up_before_both_drains_and_snapshots_before_replacement() {
    let r = rig();
    r.fake.0.lock().unwrap().supervised = true;
    let run = r
        .updater
        .run_now(RunKind::Update, "latest", "operator")
        .await;
    assert_eq!(run.outcome, Outcome::Succeeded, "{run:#?}");
    let calls = r.fake.calls();
    let expected = [
        "backup c0",
        "drain-jobs c0",
        "drain c0",
        "hot:probe",
        "hot:begin",
        "hot:pause",
        "hot:snapshot",
        "stop c0",
        "remove c0",
    ];
    let positions: Vec<_> = expected
        .iter()
        .map(|name| calls.iter().position(|s| s == name).unwrap())
        .collect();
    assert!(positions.windows(2).all(|p| p[0] < p[1]), "{calls:?}");
    assert!(calls.iter().any(|c| c == "hot:commit"));
    assert_eq!(r.fake.0.lock().unwrap().snapshot_database, 2);
}

#[tokio::test]
async fn supervised_recreate_rollback_keeps_the_post_zip_write() {
    let r = rig();
    r.fake.0.lock().unwrap().supervised = true;
    r.fake.fail(&format!("health:{}", new_ref()));
    let run = r
        .updater
        .run_now(RunKind::Update, "latest", "operator")
        .await;
    assert_eq!(run.outcome, Outcome::RolledBack, "{run:#?}");
    let calls = r.fake.calls();
    assert!(
        calls.iter().any(|c| c == "hot:restore-database"),
        "{calls:?}"
    );
    assert!(!calls.iter().any(|c| c.starts_with("restore ")));
    assert_eq!(r.fake.0.lock().unwrap().database, 2);
    assert_eq!(r.fake.running_image().as_deref(), Some(OLD));
}

#[tokio::test]
async fn supervised_recreate_snapshot_failure_resumes_without_replacing() {
    let r = rig();
    r.fake.0.lock().unwrap().supervised = true;
    r.fake.fail("hot:snapshot");
    let run = r
        .updater
        .run_now(RunKind::Update, "latest", "operator")
        .await;
    assert_eq!(run.outcome, Outcome::Failed, "{run:#?}");
    let calls = r.fake.calls();
    assert!(calls.iter().any(|c| c == "hot:cancel-pause"));
    assert!(calls.iter().any(|c| c == "resume c0"));
    assert!(!calls.iter().any(|c| c.starts_with("remove ")));
}

#[tokio::test]
async fn supervised_recreate_backup_failure_never_drains() {
    let r = rig();
    r.fake.0.lock().unwrap().supervised = true;
    r.fake.fail("backup");
    let run = r
        .updater
        .run_now(RunKind::Update, "latest", "operator")
        .await;
    assert_eq!(run.outcome, Outcome::Failed);
    assert!(
        !r.fake
            .calls()
            .iter()
            .any(|c| c.starts_with("drain") || c == "hot:pause")
    );
}

#[tokio::test]
async fn supervised_recreate_uncertain_restore_never_starts_old_daemon_or_cleans_snapshot() {
    let r = rig();
    r.fake.0.lock().unwrap().supervised = true;
    r.fake.fail(&format!("health:{}", new_ref()));
    r.fake.fail("hot:restore-database");
    let run = r
        .updater
        .run_now(RunKind::Update, "latest", "operator")
        .await;
    assert_eq!(run.outcome, Outcome::RecoveryFailed, "{run:#?}");
    let calls = r.fake.calls();
    assert!(
        !calls
            .iter()
            .any(|c| c == &format!("create:{OLD} new") || c == "hot:commit")
    );
    assert!(r.fake.running_image().is_none());
}

#[tokio::test]
async fn supervised_recreate_pending_snapshot_recovers_within_original_hold_budget() {
    let mut r = rig();
    Arc::get_mut(&mut r.updater).unwrap().hot_budget = (
        Duration::from_secs(12),
        Duration::from_secs(6),
        Duration::from_secs(1),
    );
    {
        let mut s = r.fake.0.lock().unwrap();
        s.supervised = true;
        s.pending_once = Some("snapshot".into());
    }
    let run = tokio::time::timeout(
        Duration::from_secs(13),
        r.updater.run_now(RunKind::Update, "latest", "operator"),
    )
    .await
    .unwrap();
    assert_eq!(run.outcome, Outcome::Failed, "{run:#?}");
    let calls = r.fake.calls();
    assert!(calls.iter().any(|c| c == "pending-once"));
    assert!(calls.iter().any(|c| c == "hot:cancel-pause"));
    assert!(calls.iter().any(|c| c == "resume c0"));
    assert!(!calls.iter().any(|c| c.starts_with("remove ")));
}

#[tokio::test]
async fn supervised_recreate_pending_replacement_stages_share_the_drain_deadline() {
    let mut cases = tokio::task::JoinSet::new();
    for stage in ["remove", "create", "resume"] {
        cases.spawn(async move {
            let mut r = rig();
            Arc::get_mut(&mut r.updater).unwrap().hot_budget = (
                Duration::from_secs(20),
                Duration::from_secs(7),
                Duration::from_secs(1),
            );
            {
                let mut s = r.fake.0.lock().unwrap();
                s.supervised = true;
                // Consume budget before snapshot. A clock reset there would
                // add these seconds back and miss this test's total bound.
                s.jobs_delay = Duration::from_secs(3);
                s.pending_engine = if stage == "resume" { None } else { Some(stage) };
                s.resume_hang = stage == "resume";
            }
            let run = tokio::time::timeout(
                Duration::from_secs(21),
                r.updater.run_now(RunKind::Update, "latest", "operator"),
            )
            .await
            .expect("replacement and recovery must share the original drain deadline");
            assert_eq!(run.outcome, Outcome::RecoveryFailed, "{stage}: {run:#?}");
            let calls = r.fake.calls();
            assert!(
                calls.iter().any(|c| c.starts_with("stop-for-recovery ")),
                "{stage}: {calls:?}"
            );
            assert!(!calls.iter().any(|c| c == "hot:commit"), "{stage}");
            if stage == "resume" {
                assert!(calls.iter().any(|c| c == "resume c1"));
                assert!(!calls.iter().any(|c| c == "hot:restore-database"));
                assert_eq!(calls.last().unwrap(), "stop-for-recovery c1");
            } else {
                assert!(calls.iter().any(|c| c == &format!("pending-{stage}")));
            }
        });
    }
    while let Some(result) = cases.join_next().await {
        result.unwrap();
    }
}

#[tokio::test]
async fn an_update_drains_backs_up_and_switches_the_image() {
    let r = rig();
    let run = r
        .updater
        .run_now(RunKind::Update, "latest", "operator")
        .await;
    assert_eq!(run.outcome, Outcome::Succeeded, "{run:#?}");
    assert_eq!(
        steps(&run),
        [
            "inspect",
            "resolve",
            "verify",
            "pull",
            "environment",
            "hot-swap-eligibility",
            "drain-jobs",
            "drain",
            "backup",
            "stop",
            "remove",
            "start",
            "health",
            "resume"
        ]
    );
    assert_eq!(r.verify.seen.lock().unwrap().clone(), [new_ref()]);
    assert_eq!(
        r.fake.calls(),
        [
            format!("pull {}", new_ref()),
            "drain-jobs c0".into(),
            "drain c0".into(),
            "backup c0".into(),
            "stop c0".into(),
            "remove c0".into(),
            format!("create:{} new", new_ref()),
            format!("status:{} c1", new_ref()),
            "resume c1".into(),
        ]
    );
    assert_eq!(r.fake.running_image(), Some(new_ref()));
    assert_eq!(run.from.unwrap().reference, OLD);
    assert_eq!(run.to.unwrap().version.as_deref(), Some("0.2.0"));
    assert_eq!(run.backup.as_deref(), Some(BACKUP));
    assert!(
        run.steps
            .iter()
            .any(|s| s.detail.contains("legacy recreate: backup after drain"))
    );
    // The way back and the image for the deploy wrapper.
    let prev = r.store.previous().unwrap();
    assert_eq!(prev.image.reference, OLD);
    assert_eq!(prev.backup.as_deref(), Some(BACKUP));
    assert_eq!(r.store.image(), Some(new_ref()));
    assert_eq!(r.store.history()[0].id, run.id);
}

#[tokio::test]
async fn the_same_digest_is_current_and_changes_nothing() {
    let r = rig();
    assert_eq!(
        r.updater
            .run_now(RunKind::Update, "0.2.0", "operator")
            .await
            .outcome,
        Outcome::Succeeded
    );
    let before = r.fake.calls().len();
    let run = r
        .updater
        .run_now(RunKind::Update, "v0.2.0", "operator")
        .await;
    assert_eq!(run.outcome, Outcome::Current, "{run:#?}");
    assert_eq!(r.fake.calls().len(), before, "no drain, no pull");
}

#[tokio::test]
async fn unresolved_environment_stops_before_drain_backup_or_swap() {
    let r = rig();
    r.fake.fail("environment");
    let run = r
        .updater
        .run_now(RunKind::Update, "latest", "operator")
        .await;
    assert_eq!(run.outcome, Outcome::Failed);
    assert_eq!(
        steps(&run),
        ["inspect", "resolve", "verify", "pull", "!environment"]
    );
    assert_eq!(r.fake.running_image().as_deref(), Some(OLD));
    assert!(r.fake.calls().iter().all(|c| c.starts_with("pull")));
}

#[tokio::test]
async fn missing_required_template_warns_and_update_proceeds() {
    let r = rig();
    r.fake.fail("required-templates");
    let run = r
        .updater
        .run_now(RunKind::Update, "latest", "operator")
        .await;
    assert_eq!(run.outcome, Outcome::Succeeded);
    let step = run.steps.iter().find(|s| s.name == "environment").unwrap();
    assert!(
        step.detail
            .contains("warning: required daemon template global:request is missing")
    );
    assert!(steps(&run).contains(&"drain".to_string()));
}

#[tokio::test]
async fn crashed_template_check_stops_before_drain() {
    let r = rig();
    r.fake.fail("required-templates-crash");
    let run = r
        .updater
        .run_now(RunKind::Update, "latest", "operator")
        .await;
    assert_eq!(run.outcome, Outcome::Failed);
    assert!(!steps(&run).contains(&"drain".to_string()));
}

#[tokio::test]
async fn a_failed_signature_check_stops_before_the_pull() {
    let r = rig_with("gh", false);
    let run = r
        .updater
        .run_now(RunKind::Update, "latest", "operator")
        .await;
    assert_eq!(run.outcome, Outcome::Failed);
    assert_eq!(steps(&run), ["inspect", "resolve", "!verify"]);
    assert!(run.error.unwrap().contains("verify: no signature"));
    assert!(r.fake.calls().is_empty(), "no pull, no drain");
    assert_eq!(r.fake.running_image().as_deref(), Some(OLD));
}

#[tokio::test]
async fn verification_off_is_a_skipped_step() {
    let r = rig_with("off", false);
    let run = r
        .updater
        .run_now(RunKind::Update, "latest", "operator")
        .await;
    assert_eq!(run.outcome, Outcome::Succeeded);
    assert_eq!(steps(&run)[2], "-verify");
    assert!(r.verify.seen.lock().unwrap().is_empty());
}

#[tokio::test]
async fn a_bad_tag_or_a_failed_pull_does_not_drain() {
    let r = rig();
    let run = r
        .updater
        .run_now(RunKind::Update, "9.9.9", "operator")
        .await;
    assert_eq!(run.outcome, Outcome::Failed);
    assert_eq!(steps(&run), ["inspect", "!resolve"]);
    r.fake.fail("pull");
    let run = r
        .updater
        .run_now(RunKind::Update, "latest", "operator")
        .await;
    assert_eq!(run.outcome, Outcome::Failed);
    assert!(!r.fake.calls().iter().any(|c| c.starts_with("drain")));
    // A target that is no tag is refused before a run.
    let run = r
        .updater
        .run_now(RunKind::Update, "x; rm -rf /", "operator")
        .await;
    assert!(run.error.unwrap().contains("not a version"));
}

#[tokio::test]
async fn a_failed_drain_or_backup_resumes_the_old_daemon() {
    let r = rig();
    r.fake.fail("backup");
    let run = r
        .updater
        .run_now(RunKind::Update, "latest", "operator")
        .await;
    assert_eq!(run.outcome, Outcome::Failed);
    assert_eq!(
        steps(&run),
        [
            "inspect",
            "resolve",
            "verify",
            "pull",
            "environment",
            "hot-swap-eligibility",
            "drain-jobs",
            "drain",
            "!backup",
            "resume"
        ]
    );
    assert!(run.error.unwrap().contains("backup broke"));
    assert!(!r.fake.calls().iter().any(|c| c.starts_with("stop")));
    assert_eq!(r.fake.running_image().as_deref(), Some(OLD));
    assert_eq!(r.store.previous(), None);
    assert_eq!(r.store.image(), None);

    let r = rig();
    r.fake.fail("drain");
    let run = r
        .updater
        .run_now(RunKind::Update, "latest", "operator")
        .await;
    assert_eq!(steps(&run).last().unwrap(), "resume");
    assert!(!r.fake.calls().iter().any(|c| c.starts_with("backup")));
}

#[tokio::test]
async fn a_running_container_retries_status_until_the_daemon_is_ready() {
    let r = rig();
    let status = format!("status:{}", new_ref());
    r.fake
        .0
        .lock()
        .unwrap()
        .transient_fail
        .insert(status.clone(), 2);
    let run = r
        .updater
        .run_now(RunKind::Update, "latest", "operator")
        .await;
    assert_eq!(run.outcome, Outcome::Succeeded, "{run:#?}");
    assert_eq!(
        r.fake
            .calls()
            .iter()
            .filter(|c| c.starts_with(&status))
            .count(),
        3
    );
    assert!(!steps(&run).contains(&"rollback".into()));
}

#[tokio::test]
async fn a_running_container_with_an_unreachable_daemon_times_out_and_rolls_back() {
    let r = rig();
    let status = format!("status:{}", new_ref());
    r.fake.fail(&status);
    let started = tokio::time::Instant::now();
    let run = r
        .updater
        .run_now(RunKind::Update, "latest", "operator")
        .await;
    assert!(started.elapsed() >= r.updater.cfg.health_timeout);
    assert_eq!(run.outcome, Outcome::RolledBack, "{run:#?}");
    assert!(
        run.error
            .as_deref()
            .unwrap()
            .contains("terrarium daemon status")
    );
    assert!(
        r.fake
            .calls()
            .iter()
            .filter(|c| c.starts_with(&status))
            .count()
            > 1
    );
    assert_eq!(r.fake.running_image().as_deref(), Some(OLD));
}

#[tokio::test]
async fn an_unhealthy_new_image_goes_back_to_the_old_image_and_data() {
    let r = rig();
    // First self-update: there is no recorded previous spec yet.
    assert_eq!(r.store.previous(), None);
    let original = r.fake.0.lock().unwrap().containers["terrarium"]
        .spec
        .clone();
    r.fake.fail(&format!("health:{}", new_ref()));
    let run = r
        .updater
        .run_now(RunKind::Update, "latest", "operator")
        .await;
    assert_eq!(run.outcome, Outcome::RolledBack, "{run:#?}");
    assert_eq!(
        r.fake.0.lock().unwrap().containers["terrarium"].spec,
        original
    );
    assert!(
        r.fake.0.lock().unwrap().containers["terrarium"]
            .spec
            .get("prepared")
            .is_none()
    );
    assert!(run.error.as_deref().unwrap().contains("health: unhealthy"));
    assert_eq!(
        steps(&run),
        [
            "inspect",
            "resolve",
            "verify",
            "pull",
            "environment",
            "hot-swap-eligibility",
            "drain-jobs",
            "drain",
            "backup",
            "stop",
            "remove",
            "start",
            "!health",
            "rollback",
            "remove",
            "restore",
            "start",
            "health",
            "resume"
        ]
    );
    let calls = r.fake.calls();
    let tail: Vec<&str> = calls
        .iter()
        .rev()
        .take(5)
        .rev()
        .map(String::as_str)
        .collect();
    assert_eq!(
        tail,
        [
            "remove c1",
            &format!("restore {OLD} {BACKUP}"),
            &format!("create:{OLD} new"),
            &format!("status:{OLD} c2"),
            "resume c2",
        ]
    );
    assert_eq!(r.fake.running_image().as_deref(), Some(OLD));
    // A failed update leaves no way "back" to the old image.
    assert_eq!(r.store.previous(), None);
    assert_eq!(r.store.image(), None);
}

#[tokio::test]
async fn a_failed_way_back_is_rollback_failed() {
    let r = rig();
    r.fake.fail(&format!("create:{}", new_ref()));
    r.fake.fail("restore");
    let run = r
        .updater
        .run_now(RunKind::Update, "latest", "operator")
        .await;
    assert_eq!(run.outcome, Outcome::RollbackFailed);
    let e = run.error.unwrap();
    assert!(e.contains("start: create refused"), "{e}");
    assert!(
        e.contains("the way back failed too: restore: exit 1: restore broke"),
        "{e}"
    );
}

#[tokio::test]
async fn a_rollback_restores_the_backup_of_the_last_switch() {
    let r = rig();
    assert_eq!(
        r.updater
            .run_now(RunKind::Update, "latest", "operator")
            .await
            .outcome,
        Outcome::Succeeded
    );
    assert_eq!(
        r.fake.0.lock().unwrap().containers["terrarium"].spec["prepared"],
        true
    );
    // Model invalid Compose interpolation after the successful update.
    r.fake.fail("environment");
    let run = r
        .updater
        .run_now(RunKind::Rollback, "previous", "agent:01SYS")
        .await;
    assert_eq!(run.outcome, Outcome::Succeeded, "{run:#?}");
    assert_eq!(
        steps(&run),
        [
            "inspect",
            "pull",
            "-verify",
            "environment",
            "drain-jobs",
            "drain",
            "backup",
            "stop",
            "restore",
            "remove",
            "start",
            "health",
            "resume"
        ]
    );
    assert!(
        r.fake.calls().contains(&format!("restore {OLD} {BACKUP}")),
        "the old image restores the backup of the update"
    );
    assert_eq!(r.fake.running_image().as_deref(), Some(OLD));
    assert!(
        r.fake.0.lock().unwrap().containers["terrarium"]
            .spec
            .get("prepared")
            .is_none()
    );
    // A rollback of the rollback goes forward again.
    let prev = r.store.previous().unwrap();
    assert_eq!(prev.image.reference, new_ref());
    assert_eq!(r.store.image().as_deref(), Some(OLD));
    assert_eq!(run.actor, "agent:01SYS");
}

#[tokio::test]
async fn legacy_rollback_uses_the_inspected_spec_without_resolving_environment() {
    let r = rig();
    r.updater
        .run_now(RunKind::Update, "latest", "operator")
        .await;
    let mut previous = r.store.previous().unwrap();
    previous.spec = None;
    r.store.set_previous(&previous).unwrap();
    let inspected = r.fake.0.lock().unwrap().containers["terrarium"]
        .spec
        .clone();
    r.fake.fail("environment");
    let run = r
        .updater
        .run_now(RunKind::Rollback, "previous", "operator")
        .await;
    assert_eq!(run.outcome, Outcome::Succeeded, "{run:#?}");
    assert_eq!(
        r.fake.0.lock().unwrap().containers["terrarium"].spec,
        inspected
    );
    assert_eq!(r.fake.running_image().as_deref(), Some(OLD));
}

#[tokio::test]
async fn a_rollback_needs_a_previous_switch() {
    let r = rig();
    let run = r
        .updater
        .run_now(RunKind::Rollback, "previous", "operator")
        .await;
    assert_eq!(run.outcome, Outcome::Failed);
    assert!(run.error.unwrap().contains("no switch to roll back"));
    assert!(r.fake.calls().is_empty());
}

#[tokio::test]
async fn one_run_at_a_time() {
    let r = rig();
    let (_run, _guard) = r
        .updater
        .begin(RunKind::Update, "latest", "operator")
        .ok()
        .unwrap();
    match r
        .updater
        .handle(Request::Update {
            target: "latest".into(),
            actor: "operator".into(),
        })
        .await
    {
        Response::Error { busy, .. } => assert!(busy),
        other => panic!("{other:?}"),
    }
    match r.updater.handle(Request::Status).await {
        Response::Status(s) => {
            assert_eq!(s.running.unwrap().outcome, Outcome::Running);
            assert_eq!(s.current.unwrap().reference, OLD);
            assert_eq!(s.verify, "gh");
            assert_eq!(s.repository, REPO);
        }
        other => panic!("{other:?}"),
    }
}

#[tokio::test]
async fn handle_runs_the_update_in_the_background() {
    let r = rig();
    let id = match r
        .updater
        .handle(Request::Update {
            target: "latest".into(),
            actor: "operator".into(),
        })
        .await
    {
        Response::Accepted { run } => run,
        other => panic!("{other:?}"),
    };
    for _ in 0..200 {
        if r.store.history().first().is_some_and(|h| h.id == id) {
            break;
        }
        tokio::time::sleep(Duration::from_millis(5)).await;
    }
    assert_eq!(r.store.history()[0].outcome, Outcome::Succeeded);
}

#[test]
fn tags_and_commands() {
    assert_eq!(tag_of("latest").as_deref(), Some("latest"));
    assert_eq!(tag_of("pre").as_deref(), Some("pre"));
    assert_eq!(tag_of("v0.3.0").as_deref(), Some("0.3.0"));
    assert_eq!(tag_of("0.3.0-alpha").as_deref(), Some("0.3.0-alpha"));
    assert_eq!(tag_of("vnext").as_deref(), Some("vnext"));
    assert_eq!(tag_of(""), None);
    assert_eq!(tag_of("a b"), None);
    assert_eq!(tag_of("../x"), None);
    assert_eq!(tag_of("-x"), None);
    assert_eq!(
        drain_argv(Duration::from_secs(600)).join(" "),
        "terrarium admin update drain --wait 600"
    );
    assert_eq!(
        backup_path(r#"{"path": "/state/backups/a.zip"}"#).unwrap(),
        "/state/backups/a.zip"
    );
    assert!(backup_path("not json").is_err());
}
