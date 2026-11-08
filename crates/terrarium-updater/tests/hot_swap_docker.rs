//! Run only on the disposable Docker target via docker/test-hot-swap-container.py.
#![cfg(feature = "bin")]
#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::sync::Arc;
use std::time::Duration;
use terrarium_updater::docker::Docker;
use terrarium_updater::engine::{
    BoxFut, Container, Engine, ExecOut, Health, Registry, Res, Verifier,
};
use terrarium_updater::machine::{Config, Updater};
use terrarium_updater::protocol::{Image, Outcome, RunKind};
use terrarium_updater::store::Store;

// Only registry resolution/pull and Compose preparation are replaced. All
// staging, backup, signalling, health and rollback use the real Docker engine.
struct Local {
    docker: Docker,
    next: String,
}
impl Engine for Local {
    fn target(&self) -> BoxFut<'_, Res<Container>> {
        self.docker.target()
    }
    fn can_hot_swap<'a>(&'a self, c: &'a Container, i: &'a Image) -> BoxFut<'a, Res<bool>> {
        self.docker.can_hot_swap(c, i)
    }
    fn image<'a>(&'a self, r: &'a str) -> BoxFut<'a, Res<Option<Image>>> {
        self.docker.image(r)
    }
    fn pull<'a>(&'a self, _: &'a str) -> BoxFut<'a, Res<Image>> {
        Box::pin(async {
            self.docker
                .image(&self.next)
                .await?
                .ok_or_else(|| "fixture image missing".into())
        })
    }
    fn exec<'a>(&'a self, id: &'a str, argv: Vec<String>) -> BoxFut<'a, Res<ExecOut>> {
        self.docker.exec(id, argv)
    }
    fn stop<'a>(&'a self, id: &'a str) -> BoxFut<'a, Res<()>> {
        self.docker.stop(id)
    }
    fn remove<'a>(&'a self, id: &'a str) -> BoxFut<'a, Res<()>> {
        self.docker.remove(id)
    }
    fn create<'a>(&'a self, c: &'a Container, i: &'a Image) -> BoxFut<'a, Res<Container>> {
        self.docker.create(c, i)
    }
    fn run_once<'a>(
        &'a self,
        c: &'a Container,
        i: &'a Image,
        argv: Vec<String>,
    ) -> BoxFut<'a, Res<ExecOut>> {
        self.docker.run_once(c, i, argv)
    }
    fn health<'a>(&'a self, id: &'a str) -> BoxFut<'a, Res<Health>> {
        self.docker.health(id)
    }
}
struct Release;
impl Registry for Release {
    fn resolve<'a>(&'a self, _: &'a str, _: &'a str) -> BoxFut<'a, Res<String>> {
        Box::pin(async { Ok("sha256:fixture".into()) })
    }
}
impl Verifier for Release {
    fn mode(&self) -> String {
        "off".into()
    }
    fn verify<'a>(&'a self, _: &'a str) -> BoxFut<'a, Res<String>> {
        Box::pin(async { unreachable!() })
    }
}

#[tokio::test]
#[ignore = "requires owned disposable Docker fixtures"]
async fn real_container_hot_swap_and_failed_health_rollback() {
    let target = std::env::var("TER261_CONTAINER").unwrap();
    assert!(target.starts_with("ter-261-"));
    let docker = Docker {
        socket: "/var/run/docker.sock".into(),
        target: Some(target),
    };
    let before = docker.target().await.unwrap();
    // Exec is unavailable in a paused container. Status must still read the
    // selected release metadata through the shared volume for recovery.
    let paused = std::process::Command::new("docker")
        .args(["pause", &before.id])
        .status()
        .unwrap();
    assert!(paused.success());
    let offline = docker.target().await;
    let resumed = std::process::Command::new("docker")
        .args(["unpause", &before.id])
        .status()
        .unwrap();
    assert!(resumed.success());
    assert_eq!(offline.unwrap().image, before.image);
    let changed = docker.image("ter-261-changed-base").await.unwrap().unwrap();
    assert!(!docker.can_hot_swap(&before, &changed).await.unwrap());
    let dir = tempfile::tempdir().unwrap();
    let store = Store::new(dir.path(), 10);
    for (next, expected) in [
        ("ter-261-good", Outcome::Succeeded),
        ("ter-261-bad", Outcome::RolledBack),
    ] {
        let engine = Arc::new(Local {
            docker: docker.clone(),
            next: next.into(),
        });
        let updater = Arc::new(Updater::new(
            engine,
            Arc::new(Release),
            Arc::new(Release),
            store.clone(),
            Config {
                health_timeout: Duration::from_secs(5),
                health_poll: Duration::from_millis(100),
                ..Default::default()
            },
        ));
        let result = updater
            .run_now(RunKind::Update, "test", "scratch-test")
            .await;
        assert_eq!(result.outcome, expected, "{result:#?}");
        assert_eq!(
            docker.target().await.unwrap().id,
            before.id,
            "container must survive"
        );
        let probe = docker
            .exec(
                &before.id,
                vec![
                    "sh".into(),
                    "-c".into(),
                    "kill -0 $(cat /run/terrarium/survivor.pid)".into(),
                ],
            )
            .await
            .unwrap();
        assert_eq!(probe.code, 0, "unrelated live process must survive");
    }
    let identity = docker.target().await.unwrap();
    assert_eq!(identity.image.reference, "ter-261-good");
    let check = docker.exec(&before.id, vec!["python3".into(), "-c".into(), "import sqlite3; c=sqlite3.connect('/state/terrarium.db'); assert not c.execute(\"SELECT name FROM sqlite_master WHERE name='failed_migration'\").fetchall()".into()]).await.unwrap();
    assert_eq!(check.code, 0, "failed image migration was restored");
    let updater = Arc::new(Updater::new(
        Arc::new(Local {
            docker: docker.clone(),
            next: "unused".into(),
        }),
        Arc::new(Release),
        Arc::new(Release),
        store,
        Config {
            health_timeout: Duration::from_secs(30),
            health_poll: Duration::from_millis(100),
            ..Default::default()
        },
    ));
    let rollback = updater
        .run_now(RunKind::Rollback, "previous", "scratch-test")
        .await;
    assert_eq!(rollback.outcome, Outcome::Succeeded, "{rollback:#?}");
    let restored = docker.target().await.unwrap();
    assert_ne!(
        restored.id, before.id,
        "explicit rollback must replace the container"
    );
    assert_eq!(restored.image.reference, "ter-261-old");
}
