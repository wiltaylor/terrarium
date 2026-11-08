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
    td.post_as("other/worker", "/api/agents/other/worker/waits", json!({"kind":"reservation","reservation":qid})).await.err(403, "forbidden");
    let wait = td.post_as("p/worker", "/api/agents/p/worker/waits", json!({"kind":"reservation","reservation":qid})).await;
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
    let wait = td.post_as("p/worker", "/api/agents/p/worker/waits", json!({"kind":"reservation","reservation":id})).await;
    let wid = wait.ok()["id"].as_str().unwrap();
    td.delete(&format!("/api/reservations/{first_id}")).await.ok();
    let state = td.daemon.as_ref().unwrap().state.clone();
    terrariumd::reservations::sweep(&state).await.unwrap();
    assert_eq!(td.get(&format!("/api/waits/{wid}")).await.ok()["state"], "resolved");
    let late = td.post_as("p/worker", "/api/agents/p/worker/waits", json!({"kind":"reservation","reservation":id})).await;
    let late_id = late.ok()["id"].as_str().unwrap();
    state.db.write("system", terrariumd::store::waits::check_reservation_waits).await.unwrap();
    assert_eq!(td.get(&format!("/api/waits/{late_id}")).await.ok()["state"], "resolved");
    client.abort(); server.abort();
}
