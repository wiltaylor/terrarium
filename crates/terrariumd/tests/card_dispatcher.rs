//! Dispatcher tests run real transactions with a fake harness.
#![allow(clippy::unwrap_used, clippy::expect_used)]
use crate::common::TestDaemon;
use serde_json::{Value, json};
use terrariumd::card_dispatcher::dispatch;

async fn setup(limit: i64, concurrent: u32, daily: u32) -> TestDaemon {
    let td = TestDaemon::start().await;
    td.project("p").await;
    td.patch("/api/projects/p/board", json!({"columns": [
        {"name":"backlog"}, {"name":"ready"},
        {"name":"coding", "role":"working", "wip_limit":limit},
        {"name":"review", "role":"review"},
        {"name":"blocked", "role":"blocked"}, {"name":"done"}
    ]})).await.ok();
    for name in ["card-small", "card-impl", "card-impl-hard"] {
        td.post("/api/templates", json!({"name":name,"project":"p",
            "spec":{"harness":"codex"}})).await.ok();
    }
    td.agent("p", "lead", json!({"roles":["project_admin"],"policy":{
        "spawn":{"templates":["project:*"],"bound":true,
            "max_concurrent":concurrent,"max_per_day":daily}
    }})).await;
    td
}

fn state(td: &TestDaemon) -> terrariumd::api::Shared {
    td.daemon.as_ref().unwrap().state.clone()
}

async fn card(td: &TestDaemon, kind: &str) -> String {
    td.post("/api/projects/p/cards", json!({"title":"Task", "column":"ready",
        "labels":[format!("type:{kind}")]})).await.ok()["id"].as_str().unwrap().into()
}

async fn enabled(td: &TestDaemon) {
    td.patch("/api/projects/p", json!({"card_dispatcher":"lead"})).await.ok();
}

async fn get(td: &TestDaemon, id: &str) -> Value {
    td.get(&format!("/api/cards/{id}")).await.ok().clone()
}

#[tokio::test]
async fn off_by_default_and_disabled_explicitly() {
    let td = setup(2, 2, 10).await;
    let id = card(&td, "small").await;
    assert!(!dispatch(&state(&td), "p").await.unwrap());
    enabled(&td).await;
    td.patch("/api/projects/p", json!({"card_dispatcher":"off"})).await.ok();
    assert!(!dispatch(&state(&td), "p").await.unwrap());
    assert!(get(&td, &id).await["agent"].is_null());
}

#[tokio::test]
async fn claims_in_order_with_role_based_wip_and_no_duplicate() {
    let td = setup(1, 3, 10).await;
    let first = card(&td, "impl").await;
    let second = card(&td, "hard").await;
    enabled(&td).await;
    assert!(dispatch(&state(&td), "p").await.unwrap());
    let c = get(&td, &first).await;
    assert_eq!(c["column"], "coding");
    assert_eq!(c["agent"], c["assignee"]);
    assert_eq!(c["agent_name"], first.to_ascii_lowercase());
    assert!(!dispatch(&state(&td), "p").await.unwrap());
    assert!(get(&td, &second).await["agent"].is_null());
}

#[tokio::test]
async fn concurrent_dispatchers_cannot_overfill_wip() {
    let td = setup(1, 3, 10).await;
    let first = card(&td, "small").await;
    let second = card(&td, "small").await;
    enabled(&td).await;
    let s = state(&td);
    let (a, b) = tokio::join!(dispatch(&s, "p"), dispatch(&s, "p"));
    assert_eq!(usize::from(a.unwrap_or(false)) + usize::from(b.unwrap_or(false)), 1);
    assert_eq!(get(&td, &first).await["column"], "coding");
    assert!(get(&td, &second).await["agent"].is_null());
}

#[tokio::test]
async fn skips_unfinished_dependencies_and_unknown_types() {
    let td = setup(2, 3, 10).await;
    let dependency = card(&td, "unknown").await;
    let blocked = card(&td, "small").await;
    td.patch(&format!("/api/cards/{blocked}"), json!({"depends_on":[dependency]})).await.ok();
    let eligible = card(&td, "hard").await;
    enabled(&td).await;
    assert!(dispatch(&state(&td), "p").await.unwrap());
    assert!(get(&td, &blocked).await["agent"].is_null());
    assert_eq!(get(&td, &eligible).await["column"], "coding");
}

#[tokio::test]
async fn daily_spawn_cap_is_not_bypassed_by_daemon() {
    let td = setup(3, 3, 1).await;
    card(&td, "small").await;
    let second = card(&td, "small").await;
    enabled(&td).await;
    assert!(dispatch(&state(&td), "p").await.unwrap());
    let error = dispatch(&state(&td), "p").await.unwrap_err();
    assert!(error.message.contains("max_per_day"), "{}", error.message);
    assert!(get(&td, &second).await["agent"].is_null());
    assert_eq!(get(&td, &second).await["column"], "ready");
}

#[tokio::test]
async fn concurrent_spawn_cap_leaves_card_ready() {
    let td = setup(3, 1, 10).await;
    card(&td, "small").await;
    let second = card(&td, "small").await;
    enabled(&td).await;
    assert!(dispatch(&state(&td), "p").await.unwrap());
    let error = dispatch(&state(&td), "p").await.unwrap_err();
    assert!(error.message.contains("max_concurrent"), "{}", error.message);
    assert!(get(&td, &second).await["agent"].is_null());
}

#[tokio::test]
async fn missing_owner_leaves_card_unclaimed() {
    let td = setup(2, 2, 10).await;
    let id = card(&td, "small").await;
    td.patch("/api/projects/p", json!({"card_dispatcher":"missing"})).await.ok();
    assert!(dispatch(&state(&td), "p").await.is_err());
    assert!(get(&td, &id).await["assignee"].is_null());
    assert!(get(&td, &id).await["agent"].is_null());
}
