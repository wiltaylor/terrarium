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
    hot: bool,
    staged: Option<Image>,
    /// The containers by name.
    containers: HashMap<String, Container>,
    images: HashMap<String, Image>,
    /// What the updater did, in order.
    calls: Vec<String>,
    /// The operations that fail: `pull`, `drain`, `backup`, `stop`,
    /// `restore`, `create:<ref>`, `health:<ref>`, `status:<ref>`.
    fail: HashSet<String>,
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
        s.fail.contains(&key)
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
    fn can_hot_swap<'a>(&'a self, _cur: &'a Container, _to: &'a Image) -> BoxFut<'a, Res<bool>> {
        Box::pin(async { Ok(self.0.lock().unwrap().hot) })
    }
    fn prepare<'a>(&'a self, spec: &'a Container) -> BoxFut<'a, Res<Container>> {
        Box::pin(async move {
            if self.0.lock().unwrap().fail.contains("environment") {
                return Err("Compose environment resolution failed (values hidden)".into());
            }
            Ok(spec.clone())
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
                if cmd == "start" && !failed {
                    let mut s = self.0.lock().unwrap();
                    let image = if argv[3] == "b".repeat(64) { s.staged.clone().unwrap() } else { old_image() };
                    s.containers.get_mut("terrarium").unwrap().image = image;
                }
                return Ok(ExecOut { code: i64::from(failed), stdout: if cmd == "probe" { "a".repeat(64) } else { String::new() }, stderr: "control failed".into() });
            }
            let what = match (argv[0].as_str(), argv.get(3).map(String::as_str)) {
                ("terrarium", Some("drain")) => "drain".to_owned(),
                ("terrarium", Some("resume")) => "resume".to_owned(),
                ("terrariumd", _) => "backup".to_owned(),
                _ => format!("status:{}", self.image_of(id).unwrap_or_default()),
            };
            let failed = self.call(format!("{what} {id}"));
            let stdout = if what == "backup" {
                format!(r#"{{"name": "terrarium-backup-1.zip", "path": "{BACKUP}"}}"#)
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
            self.0.lock().unwrap().containers.retain(|_, c| c.id != id);
            Ok(())
        })
    }

    fn create<'a>(&'a self, spec: &'a Container, image: &'a Image) -> BoxFut<'a, Res<Container>> {
        Box::pin(async move {
            if self.call(format!("create:{} new", image.reference)) {
                return Err("create refused".into());
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
            if argv[0] == "python3" {
                let command = &argv[2];
                self.call(format!("hot:{command}"));
                if command == "stage" { self.0.lock().unwrap().staged = Some(image.clone()); }
                return Ok(ExecOut { stdout: if command == "stage" { "b".repeat(64) } else { String::new() }, ..Default::default() });
            }
            if argv[0] == "/opt/terrarium/bundled/bin/terrariumd" && argv[2] == "create" {
                let failed = self.call("offline-backup".into());
                return Ok(ExecOut { code: i64::from(failed), stdout: format!(r#"{{"path":"{BACKUP}"}}"#), ..Default::default() });
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

    fn health<'a>(&'a self, id: &'a str) -> BoxFut<'a, Res<Health>> {
        Box::pin(async move {
            let image = self.image_of(id).unwrap_or_default();
            let s = self.0.lock().unwrap();
            if s.fail.contains(&format!("health:{image}")) {
                return Ok(Health::Unhealthy("the check failed 3 times".into()));
            }
            Ok(Health::Healthy)
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

#[tokio::test]
async fn hot_swap_keeps_container_and_commits_only_after_health() {
    let r = rig();
    r.fake.0.lock().unwrap().hot = true;
    let run = r.updater.run_now(RunKind::Update, "latest", "operator").await;
    assert_eq!(run.outcome, Outcome::Succeeded, "{run:#?}");
    let calls = r.fake.calls();
    assert!(!calls.iter().any(|c| c.starts_with("drain") || c.starts_with("remove") || c.starts_with("stop")));
    assert_eq!(calls.last().unwrap(), "hot:commit");
    assert!(calls.iter().position(|c| c == "hot:pause").unwrap() < calls.iter().position(|c| c == "offline-backup").unwrap());
    let rollback = r.updater.run_now(RunKind::Rollback, "previous", "operator").await;
    assert_eq!(rollback.outcome, Outcome::Succeeded, "{rollback:#?}");
    assert!(r.fake.calls().iter().any(|c| c == "stop c0"));
}

#[tokio::test]
async fn hot_swap_failed_health_restores_before_old_release_admission() {
    let r = rig();
    r.fake.0.lock().unwrap().hot = true;
    r.fake.fail(&format!("health:{}", new_ref()));
    let run = r.updater.run_now(RunKind::Update, "latest", "operator").await;
    assert_eq!(run.outcome, Outcome::RolledBack, "{run:#?}");
    assert_eq!(r.fake.running_image().as_deref(), Some(OLD));
    let calls = r.fake.calls();
    assert_eq!(calls.iter().filter(|c| *c == "hot:pause").count(), 2);
    assert_eq!(calls.iter().filter(|c| *c == "hot:commit").count(), 1);
    assert!(calls.iter().position(|c| c == "hot:restore-database").unwrap() < calls.iter().position(|c| c == "hot:commit").unwrap());
}

#[tokio::test]
async fn ambiguous_commit_never_rewinds_the_journal() {
    let r = rig();
    r.fake.0.lock().unwrap().hot = true;
    r.fake.fail("hot:commit");
    let run = r.updater.run_now(RunKind::Update, "latest", "operator").await;
    assert_eq!(run.outcome, Outcome::Failed);
    assert!(!r.fake.calls().iter().any(|c| c == "hot:restore-database"));
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
            "drain",
            "backup",
            "stop",
            "remove",
            "start",
            "health"
        ]
    );
    assert_eq!(r.verify.seen.lock().unwrap().clone(), [new_ref()]);
    assert_eq!(
        r.fake.calls(),
        [
            format!("pull {}", new_ref()),
            "drain c0".into(),
            "backup c0".into(),
            "stop c0".into(),
            "remove c0".into(),
            format!("create:{} new", new_ref()),
            format!("status:{} c1", new_ref()),
        ]
    );
    assert_eq!(r.fake.running_image(), Some(new_ref()));
    assert_eq!(run.from.unwrap().reference, OLD);
    assert_eq!(run.to.unwrap().version.as_deref(), Some("0.2.0"));
    assert_eq!(run.backup.as_deref(), Some(BACKUP));
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
async fn an_unhealthy_new_image_goes_back_to_the_old_image_and_data() {
    let r = rig();
    r.fake.fail(&format!("health:{}", new_ref()));
    let run = r
        .updater
        .run_now(RunKind::Update, "latest", "operator")
        .await;
    assert_eq!(run.outcome, Outcome::RolledBack, "{run:#?}");
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
            "health"
        ]
    );
    let calls = r.fake.calls();
    let tail: Vec<&str> = calls
        .iter()
        .rev()
        .take(4)
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
            "drain",
            "backup",
            "stop",
            "restore",
            "remove",
            "start",
            "health"
        ]
    );
    assert!(
        r.fake.calls().contains(&format!("restore {OLD} {BACKUP}")),
        "the old image restores the backup of the update"
    );
    assert_eq!(r.fake.running_image().as_deref(), Some(OLD));
    // A rollback of the rollback goes forward again.
    let prev = r.store.previous().unwrap();
    assert_eq!(prev.image.reference, new_ref());
    assert_eq!(r.store.image().as_deref(), Some(OLD));
    assert_eq!(run.actor, "agent:01SYS");
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
