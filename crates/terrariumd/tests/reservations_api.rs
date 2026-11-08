#![allow(clippy::unwrap_used, clippy::expect_used)]
mod common;

use std::time::Duration;
use common::TestDaemon;
use serde_json::json;
use terrarium_core::{limits::LimitsCapability, protocol::{HostCapabilities, ToolCapability}, resources::HostCapacity};
use terrarium_host::{Client, ClientOptions};

async fn setup() -> (TestDaemon, tokio::task::JoinHandle<()>, tokio::task::JoinHandle<()>) {
    let td = TestDaemon::start().await;
    td.project("p").await;
    td.agent("p", "lead", json!({"roles":["project_admin"]})).await;
    td.agent("p", "worker", json!({"policy":{"targets":["p-box"]}})).await;
    td.project("other").await;
    td.agent("other", "worker", json!({})).await;
    let state = td.daemon.as_ref().unwrap().state.clone();
    let (server, addr) = terrariumd::hosts::serve(state.clone(), "127.0.0.1:0").await.unwrap();
    let registered = td.post("/api/hosts", json!({"name":"h"})).await;
    let token = registered.ok()["token"].as_str().unwrap();
    let mut options = ClientOptions::new(format!("ws://{addr}/hosts/connect"), token, "h");
    use std::os::unix::fs::PermissionsExt;
    let docker = td.dir.path().join("reservation-docker");
    std::fs::write(&docker, format!("#!/bin/sh\nprintf '%s\\n' \"$*\" >> '{}/docker.calls'\n[ ! -f '{}/docker.fail' ] || exit 1\n[ \"$1\" = update ] || exit 2\n", td.dir.path().display(), td.dir.path().display())).unwrap();
    std::fs::set_permissions(&docker, std::fs::Permissions::from_mode(0o700)).unwrap();
    options.docker_bin = docker.to_string_lossy().into_owned();
    options.capabilities = HostCapabilities {
        reservations: true,
        capacity: Some(HostCapacity { cpu_ids: vec![0, 1, 2, 3], memory_bytes: 8 << 30 }),
        docker: ToolCapability { present: true, version: Some("fake".into()) },
        limits: Some(LimitsCapability { docker: true, ..Default::default() }),
        ..Default::default()
    };
    let client = tokio::spawn(Client::new(options).run());
    tokio::time::timeout(Duration::from_secs(10), async {
        while !state.executor.hosts.is_online("h") { tokio::time::sleep(Duration::from_millis(10)).await; }
    }).await.unwrap();
    for name in ["p-box", "p-box2"] {
        td.post("/api/targets", json!({"name":name,"kind":"docker","host":"h","project":"p","image":"fake"})).await.ok();
    }
    td.put("/api/hosts/h/capacity", json!({"percentage":50})).await.ok();
    (td, server, client)
}

#[tokio::test]
async fn fixed_docker_limits_update_live_and_failed_ack_keeps_defaults() {
    let (td, server, client) = setup().await;
    let state = td.daemon.as_ref().unwrap().state.clone();
    state.db.write("system", |tx| terrariumd::store::targets::set_state(tx, "p-box", "running", None)).await.unwrap();
    td.patch("/api/targets/p-box", json!({"limits":{"cpu":1,"memory":"64M"}})).await.ok();
    let calls = std::fs::read_to_string(td.dir.path().join("docker.calls")).unwrap();
    assert!(calls.contains("update"));
    assert!(calls.contains("--cpus=1"));
    std::fs::write(td.dir.path().join("docker.fail"), "fail").unwrap();
    let refused = td.patch("/api/targets/p-box", json!({"limits":{"cpu":2}})).await;
    assert!(!refused.status.is_success());
    let saved = state.db.read(|c| terrariumd::store::targets::get(c, "p-box")).await.unwrap();
    assert_eq!(saved.def.limits.unwrap().resolve().unwrap().cpu_millis, Some(1000));
    client.abort(); server.abort();
}

#[tokio::test]
async fn active_docker_budget_overrides_edits_and_retract_restores_defaults() {
    let (td, server, client) = setup().await;
    let state = td.daemon.as_ref().unwrap().state.clone();
    state.db.write("system", |tx| terrariumd::store::targets::set_state(tx, "p-box", "running", None)).await.unwrap();
    let granted = td.post("/api/hosts/h/reservations", json!({
        "target":"p-box","amounts":{"cpu_millis":2000},"until":"30m"
    })).await;
    let id = granted.ok()["id"].as_str().unwrap();
    td.patch("/api/targets/p-box", json!({"limits":{"cpu":1}})).await.ok();
    let calls = std::fs::read_to_string(td.dir.path().join("docker.calls")).unwrap();
    assert!(calls.lines().last().unwrap().contains("--cpus=2"));
    td.delete(&format!("/api/reservations/{id}")).await.ok();
    let calls = std::fs::read_to_string(td.dir.path().join("docker.calls")).unwrap();
    assert!(calls.lines().last().unwrap().contains("--cpus=1"));
    assert!(calls.lines().all(|line| !line.starts_with("stop ") && !line.starts_with("rm ")));
    let saved = state.db.read(|c| terrariumd::store::targets::get(c, "p-box")).await.unwrap();
    assert_eq!(saved.state, "running");
    td.patch("/api/targets/p-box", json!({"limits":null})).await.ok();
    let calls = std::fs::read_to_string(td.dir.path().join("docker.calls")).unwrap();
    let released = calls.lines().filter(|line| line.starts_with("update ")).next_back().unwrap();
    assert!(released.contains("--cpus=4"));
    assert!(released.contains("--memory=8589934592"));
    assert!(released.contains("--cpuset-cpus=0,1,2,3"));
    client.abort(); server.abort();
}

#[tokio::test]
async fn permanent_docker_refuses_create_only_io_reservations() {
    let (td, server, client) = setup().await;
    td.put("/api/hosts/h/capacity", json!({"percentage":50,"io":{"8:0":{"read_bps":1000000}}})).await.ok();
    let response = td.post("/api/hosts/h/reservations", json!({
        "target":"p-box", "amounts":{"io":{"8:0":{"read_bps":1000}}}, "until":"30m"
    })).await;
    response.err(409, "conflict");
    assert!(response.body.to_string().contains("cannot be cleared without recreating"));
    client.abort(); server.abort();
}

#[tokio::test]
async fn capacity_configuration_and_grants_obey_lease_roles() {
    let (td, server, client) = setup().await;
    td.put_as("p/lead", "/api/hosts/h/capacity", json!({"percentage":100})).await.err(403, "forbidden");
    let body = json!({"target":"p-box","amounts":{"cpu_millis":2000},"until":"30m"});
    td.post_as("p/worker", "/api/hosts/h/reservations", body.clone()).await.err(403, "forbidden");
    let grant = td.post_as("p/lead", "/api/hosts/h/reservations", body).await;
    let id = grant.ok()["id"].as_str().unwrap();
    assert_eq!(grant.body["state"], "active");
    td.get_as("other/worker", &format!("/api/reservations/{id}")).await.err(403, "forbidden");
    assert!(td.get_as("other/worker", "/api/hosts/h/reservations").await.ok().as_array().unwrap().is_empty());
    td.post_as("p/lead", "/api/hosts/h/reservations", json!({"target":"p-box2","amounts":{"cpu_millis":1000},"until":"30m"})).await.err(409, "conflict");
    client.abort(); server.abort();
}

#[tokio::test]
async fn queued_grant_cancels_and_holder_receives_warning_mail() {
    let (td, server, client) = setup().await;
    let grant = td.post("/api/hosts/h/reservations", json!({"target":"p-box","amounts":{"cpu_millis":2000},"until":"4m"})).await;
    let id = grant.ok()["id"].as_str().unwrap().to_owned();
    let queued = td.post("/api/hosts/h/reservations", json!({"target":"p-box2","amounts":{"cpu_millis":1000},"until":"30m","queue":true,"queue_timeout":"10m"})).await;
    assert_eq!(queued.ok()["state"], "queued");
    let qid = queued.body["id"].as_str().unwrap();
    td.post_as("other/worker", "/api/agents/other%2Fworker/waits", json!({"kind":"reservation","reservation":qid})).await.err(403, "forbidden");
    let wait = td.post_as("p/worker", "/api/agents/p%2Fworker/waits", json!({"kind":"reservation","reservation":qid})).await;
    let wid = wait.ok()["id"].as_str().unwrap();
    assert_eq!(wait.body["state"], "open");
    td.post(&format!("/api/reservations/{qid}/cancel"), json!({})).await.ok();
    assert_eq!(td.get(&format!("/api/waits/{wid}")).await.ok()["state"], "failed");
    let state = td.daemon.as_ref().unwrap().state.clone();
    terrariumd::reservations::sweep(&state).await.unwrap();
    let (mail, pointers) = state.db.read(move |c| {
        let mail: i64 = c.query_row("SELECT count(*) FROM messages WHERE subject=?1", [format!("Reservation {id}: warning")], |r| r.get(0))?;
        let pointers: i64 = c.query_row("SELECT count(*) FROM events WHERE kind='reservation_pointer'", [], |r| r.get(0))?;
        Ok((mail,pointers))
    }).await.unwrap();
    assert_eq!(mail, 1);
    assert!(pointers >= 2);
    client.abort(); server.abort();
}

#[tokio::test]
async fn reservation_wait_resolves_on_start_and_reconciles_already_active() {
    let (td, server, client) = setup().await;
    let first = td.post("/api/hosts/h/reservations", json!({"target":"p-box","amounts":{"cpu_millis":2000},"until":"30m"})).await;
    let first_id = first.ok()["id"].as_str().unwrap();
    let queued = td.post("/api/hosts/h/reservations", json!({"target":"p-box2","amounts":{"cpu_millis":1000},"until":"30m","queue":true})).await;
    let id = queued.ok()["id"].as_str().unwrap();
    let wait = td.post_as("p/worker", "/api/agents/p%2Fworker/waits", json!({"kind":"reservation","reservation":id})).await;
    let wid = wait.ok()["id"].as_str().unwrap();
    td.delete(&format!("/api/reservations/{first_id}")).await.ok();
    let state = td.daemon.as_ref().unwrap().state.clone();
    terrariumd::reservations::sweep(&state).await.unwrap();
    assert_eq!(td.get(&format!("/api/waits/{wid}")).await.ok()["state"], "resolved");
    let late = td.post_as("p/worker", "/api/agents/p%2Fworker/waits", json!({"kind":"reservation","reservation":id})).await;
    let late_id = late.ok()["id"].as_str().unwrap();
    state.db.write("system", terrariumd::store::waits::check_reservation_waits).await.unwrap();
    assert_eq!(td.get(&format!("/api/waits/{late_id}")).await.ok()["state"], "resolved");
    client.abort(); server.abort();
}

#[tokio::test]
async fn exclusive_budget_pins_owner_and_excludes_shared_workloads() {
    use terrarium_core::{config::TargetKind, resources::Amounts};
    use terrariumd::store::{reservations::{self as r, Holder, Request}, targets};
    let (td, server, client) = setup().await;
    let state = td.daemon.as_ref().unwrap().state.clone();
    state.db.write("system", |tx| {
        let r = r::request(tx, Request {
            host:"h".into(), project:"p".into(), holder:Holder::Target("p-box".into()), target:"p-box".into(),
            amounts:Amounts { cpu_millis:1000, ..Default::default() }, exclusive:true,
            reason:"test".into(), duration_ms:1_800_000, queue_ms:None,
        }, chrono::Utc::now().timestamp_millis())?;
        r::activate(tx, &r.id)?;
        Ok(())
    }).await.unwrap();
    let (owner, shared) = state.db.read(|c| Ok((targets::get(c,"p-box")?.def, targets::get(c,"p-box2")?.def))).await.unwrap();
    assert_eq!(terrariumd::reservations::docker_budget(&state,&owner).unwrap().unwrap().cpus, Some(vec![0]));
    assert_eq!(terrariumd::reservations::docker_budget(&state,&shared).unwrap().unwrap().cpus, Some(vec![1,2,3]));
    let mut host = shared.clone(); host.kind = TargetKind::Host;
    assert!(terrariumd::reservations::exec_budget(&state,&host,Some("p/worker")).unwrap().is_some());
    for kind in [TargetKind::Vmlab, TargetKind::Ssh] {
        host.kind = kind;
        assert!(terrariumd::reservations::lifecycle_admission(&state,&host).is_err());
        assert!(terrariumd::reservations::exec_budget(&state,&host,Some("p/worker")).is_err());
    }
    host.kind = TargetKind::Docker;
    host.image = None;
    assert!(terrariumd::reservations::lifecycle_admission(&state,&host).is_err());
    assert!(terrariumd::reservations::exec_budget(&state,&host,Some("p/worker")).is_err());
    client.abort(); server.abort();
}
