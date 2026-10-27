//! `GET /api/agents/{id}/screen` (docs/api.md §6.5): the rights of the PTY
//! attach, a stopped agent, an old runner, and plain text.
#![allow(clippy::unwrap_used, clippy::expect_used)]

mod common;

use common::TestDaemon;
use serde_json::{Value, json};
use terrarium_core::AgentId;

async fn running(td: &TestDaemon, project: &str, name: &str) -> (Value, AgentId) {
    let a = td.agent(project, name, json!({})).await;
    let id = a["id"].as_str().unwrap().to_owned();
    td.post_empty(&format!("/api/agents/{id}/start")).await;
    td.wait_status(&id, "idle").await;
    (a, id.parse().unwrap())
}

#[tokio::test]
async fn the_screen_is_plain_text_for_the_admins_of_the_project() {
    let td = TestDaemon::start().await;
    td.project("p").await;
    td.project("q").await;
    let (_, id) = running(&td, "p", "w").await;
    td.sup.feed_screen(
        id,
        b"\x1b[2J\x1b[H\x1b[1;33mDo you trust this folder?\x1b[0m   \r\n\x1b[7m> 1. Yes\x1b[27m\r\n  2. No\r\n",
    );
    let url = format!("/api/agents/{id}/screen");
    let member = td.agent("p", "m", json!({})).await;
    let lead = td
        .agent("p", "lead", json!({"roles": ["project_admin"]}))
        .await;
    let other_lead = td
        .agent("q", "lead", json!({"roles": ["project_admin"]}))
        .await;
    let sysadmin = td.agent("q", "sys", json!({"roles": ["sysadmin"]})).await;

    td.get_as(member["id"].as_str().unwrap(), &url)
        .await
        .err(403, "forbidden");
    td.get_as(other_lead["id"].as_str().unwrap(), &url)
        .await
        .err(403, "forbidden");

    let r = td.get_as(lead["id"].as_str().unwrap(), &url).await;
    let v = r.ok();
    assert_eq!(v["agent"], "p/w");
    assert_eq!(v["status"], "idle");
    assert_eq!(
        (v["rows"].as_u64(), v["cols"].as_u64()),
        (Some(24), Some(80))
    );
    assert_eq!(
        v["text"], "Do you trust this folder?\n> 1. Yes\n  2. No",
        "{v:#}"
    );
    assert_eq!(v["cursor"], json!({"row": 3, "col": 0}));
    assert!(v["at"].as_str().unwrap().ends_with('Z'));

    td.get_as(sysadmin["id"].as_str().unwrap(), &url).await.ok();
    let op = td.get(&url).await;
    assert!(!op.ok()["text"].as_str().unwrap().contains('\x1b'));
    // `?ansi=true` gives the escape codes that draw the screen.
    let ansi = td.get(&format!("{url}?ansi=true")).await;
    assert!(ansi.ok()["text"].as_str().unwrap().contains('\x1b'));
}

#[tokio::test]
async fn a_stopped_agent_and_an_old_runner_give_409() {
    let td = TestDaemon::start().await;
    td.project("p").await;
    let a = td.agent("p", "idle", json!({})).await;
    let url = format!("/api/agents/{}/screen", a["id"].as_str().unwrap());
    let r = td.get(&url).await;
    r.err(409, "not_running");
    assert!(
        r.body["error"]["message"]
            .as_str()
            .unwrap()
            .contains("not running")
    );

    let (_, id) = running(&td, "p", "w").await;
    td.sup.set_old_runner(id);
    let r = td.get(&format!("/api/agents/{id}/screen")).await;
    r.err(409, "conflict");
    assert_eq!(
        r.body["error"]["message"],
        "runner too old; restart the agent"
    );
}
