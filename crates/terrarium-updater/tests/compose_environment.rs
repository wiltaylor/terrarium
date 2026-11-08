//! Run only in the card's isolated scratch Compose project, with the updater's
//! /compose and /deployment read-only mounts and Docker CLI installed.
#![cfg(feature = "bin")]
#![allow(clippy::unwrap_used, clippy::expect_used)]

use terrarium_updater::docker::{DEFAULT_SOCKET, Docker};
use terrarium_updater::engine::Engine;

fn scratch_docker() -> Docker {
    let target = std::env::var("TER_298_SCRATCH_TARGET").expect("scratch target name");
    assert!(target.starts_with("ter-298-scratch-"));
    Docker {
        socket: DEFAULT_SOCKET.into(),
        target: Some(target),
    }
}

#[tokio::test]
#[ignore = "requires the isolated TER-298 scratch Compose project without mounts"]
async fn missing_mounts_stop_resolution() {
    let docker = scratch_docker();
    let old = docker.target().await.unwrap();
    let error = docker.prepare(&old, &old.image).await.unwrap_err();
    assert!(error.contains("mounts are missing"), "{error}");
    assert!(docker.target().await.is_ok(), "original container remains");
}

#[tokio::test]
#[ignore = "requires the isolated TER-298 scratch Compose project with invalid interpolation"]
async fn interpolation_failure_is_redacted_and_leaves_original_container() {
    let docker = scratch_docker();
    let old = docker.target().await.unwrap();
    let error = docker.prepare(&old, &old.image).await.unwrap_err();
    assert!(error.contains("resolution failed"), "{error}");
    assert!(!error.contains("diagnostic-sentinel"));
    assert!(docker.target().await.is_ok(), "original container remains");
}

#[tokio::test]
#[ignore = "requires the isolated TER-298 scratch Compose project"]
async fn update_reads_edited_dotenv_and_recovery_keeps_original_environment() {
    let docker = scratch_docker();
    let old = docker.target().await.unwrap();
    let prepared = docker.prepare(&old, &old.image).await.unwrap();
    assert!(
        prepared.spec["compose_env"]
            .as_array()
            .unwrap()
            .iter()
            .any(|v| v == "TERRARIUM_MAIL_LOOP_LIMIT=30")
    );
    assert!(old.spec.get("compose_env").is_none());
    let mut updated = prepared.clone();
    updated.name = format!("{}-updated", old.name);
    let new = docker.create(&updated, &old.image).await.unwrap();
    let applied = new.spec["container"]["Config"]["Env"].as_array().unwrap();
    let has_new = applied.iter().any(|v| v == "TERRARIUM_MAIL_LOOP_LIMIT=30");
    docker.remove(&new.id).await.unwrap();
    assert!(
        has_new,
        "the created container must have the edited .env value"
    );

    let mut recovery = old.clone();
    recovery.name = format!("{}-recovery", old.name);
    let recovered = docker.create(&recovery, &old.image).await.unwrap();
    let env = recovered.spec["container"]["Config"]["Env"].clone();
    docker.remove(&recovered.id).await.unwrap();
    assert!(
        !env.as_array()
            .unwrap()
            .iter()
            .any(|v| v == "TERRARIUM_MAIL_LOOP_LIMIT=30")
    );
}
