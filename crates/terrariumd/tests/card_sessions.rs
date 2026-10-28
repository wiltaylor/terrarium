//! The sessions behind board work (PLAN §13, §18; docs/api.md §6.6, §8):
//! an agent comment links to the agent's session, the card lists the
//! sessions that touched it, and both still work after `agent rm`: the
//! session row stays, and its transcript comes from the archive.
#![allow(clippy::unwrap_used, clippy::expect_used)]

mod common;

use std::path::{Path, PathBuf};
use std::time::Duration;

use common::TestDaemon;
use serde_json::{Value, json};

/// The Claude fixture of the adapter: 30 blocks.
fn fixture() -> String {
    std::fs::read_to_string(
        Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("../adapters/claude/tests/fixtures/transcript.jsonl"),
    )
    .unwrap()
}

/// An agent in `project`, started, and its session with a harness session.
async fn started(td: &TestDaemon, project: &str, name: &str) -> (Value, Value) {
    let a = td.agent(project, name, json!({})).await;
    let id = a["id"].as_str().unwrap().to_owned();
    td.post_empty(&format!("/api/agents/{id}/start")).await.ok();
    td.wait_status(&id, "idle").await;
    for _ in 0..200 {
        let r = td.get(&format!("/api/agents/{id}/sessions")).await;
        let s = r.ok()[0].clone();
        if s["harness_session_id"].is_string() {
            return (a, s);
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    panic!("no harness session");
}

fn session_file(td: &TestDaemon, agent: &Value, s: &Value) -> PathBuf {
    common::claude_project_dir(&td.state, agent["id"].as_str().unwrap()).join(format!(
        "{}.jsonl",
        s["harness_session_id"].as_str().unwrap()
    ))
}

#[tokio::test]
async fn comment_sessions_outlive_the_agent() {
    let td = TestDaemon::start().await;
    td.project("p").await;
    let (a, s) = started(&td, "p", "w").await;
    let (aid, sid) = (
        a["id"].as_str().unwrap().to_owned(),
        s["id"].as_str().unwrap().to_owned(),
    );
    std::fs::write(session_file(&td, &a, &s), fixture()).unwrap();

    let card = td
        .post("/api/projects/p/cards", json!({"title": "Work"}))
        .await
        .ok()["id"]
        .as_str()
        .unwrap()
        .to_owned();
    // An agent claims and comments; the operator comments.
    td.post_empty_as(&aid, &format!("/api/cards/{card}/claim"))
        .await
        .ok();
    td.post_as(
        &aid,
        &format!("/api/cards/{card}/comments"),
        json!({"body": "on it"}),
    )
    .await
    .ok();
    td.post(
        &format!("/api/cards/{card}/comments"),
        json!({"body": "thanks"}),
    )
    .await
    .ok();

    let c = td.get(&format!("/api/cards/{card}")).await.ok().clone();
    let agent_comment = &c["comments"][0];
    assert_eq!(agent_comment["session"]["id"], sid.as_str(), "{c:#}");
    assert_eq!(agent_comment["session"]["agent_id"], aid.as_str());
    assert_eq!(agent_comment["session"]["agent"], "w");
    assert_eq!(agent_comment["session"]["project"], "p");
    assert_eq!(agent_comment["session"]["harness"], "claude");
    assert_eq!(agent_comment["session"]["removed"], false);
    assert!(
        c["comments"][1]["session"].is_null(),
        "operator: no session"
    );
    // The claim is a card event of the agent: it carries the session.
    let ev = td.get(&format!("/api/cards/{card}/events")).await;
    let claimed = ev
        .ok()
        .as_array()
        .unwrap()
        .iter()
        .find(|e| e["data"]["change"] == "claimed")
        .cloned()
        .unwrap();
    assert_eq!(claimed["data"]["actor_session"], sid.as_str());

    let list = td.get(&format!("/api/cards/{card}/sessions")).await;
    let list = list.ok().as_array().unwrap().clone();
    assert_eq!(list.len(), 1, "{list:?}");
    assert_eq!(list[0]["id"], sid.as_str());

    let one = td.get(&format!("/api/sessions/{sid}")).await;
    assert_eq!(one.ok()["active"], true);
    assert_eq!(one.ok()["removed"], false);
    let t = td.get(&format!("/api/sessions/{sid}/transcript")).await;
    let t = t.ok();
    assert_eq!(t["session"], sid.as_str());
    assert_eq!(t["blocks"].as_array().unwrap().len(), 30);
    assert_eq!(t["blocks"][0]["session"], sid.as_str());

    // `agent rm`: the agent dir goes to the trash, the session file to
    // the archive.
    let r = td.delete(&format!("/api/agents/{aid}?confirm=true")).await;
    assert_eq!(r.status, 204, "{}", r.body);
    assert!(!td.state.join("agents").join(&aid).exists());
    let archived = td.state.join(format!(
        "archive/agents/{aid}/claude/{sid}/{}.jsonl",
        s["harness_session_id"].as_str().unwrap()
    ));
    assert!(archived.is_file(), "{}", archived.display());

    let c = td.get(&format!("/api/cards/{card}")).await.ok().clone();
    let gone = &c["comments"][0]["session"];
    assert_eq!(gone["id"], sid.as_str());
    assert_eq!(gone["removed"], true);
    assert_eq!(gone["agent"], "w");
    assert_eq!(gone["harness"], "claude");
    assert!(gone["ended_at"].is_string());
    let list = td.get(&format!("/api/cards/{card}/sessions")).await;
    assert_eq!(list.ok()[0]["removed"], true);
    let one = td.get(&format!("/api/sessions/{sid}")).await;
    assert_eq!(one.ok()["active"], false);
    let t = td
        .get(&format!("/api/sessions/{sid}/transcript?limit=5"))
        .await;
    let t = t.ok();
    assert_eq!(t["harness"], "claude");
    assert_eq!(t["active"], false);
    assert_eq!(t["blocks"].as_array().unwrap().len(), 5);
    assert_eq!(t["complete"], false);
    let t = td.get(&format!("/api/sessions/{sid}/transcript")).await;
    assert_eq!(t.ok()["blocks"].as_array().unwrap().len(), 30);

    // A pruned archive: the row stays, the transcript is gone.
    std::fs::remove_file(&archived).unwrap();
    td.get(&format!("/api/sessions/{sid}/transcript"))
        .await
        .err(404, "not_found");
    td.get("/api/sessions/NOPE").await.err(404, "not_found");
}

#[tokio::test]
async fn another_project_cannot_read_the_sessions() {
    let td = TestDaemon::start().await;
    td.project("p").await;
    let (a, s) = started(&td, "p", "w").await;
    let (aid, sid) = (
        a["id"].as_str().unwrap().to_owned(),
        s["id"].as_str().unwrap().to_owned(),
    );
    std::fs::write(session_file(&td, &a, &s), fixture()).unwrap();
    let card = td
        .post("/api/projects/p/cards", json!({"title": "Work"}))
        .await
        .ok()["id"]
        .as_str()
        .unwrap()
        .to_owned();
    td.post_as(
        &aid,
        &format!("/api/cards/{card}/comments"),
        json!({"body": "on it"}),
    )
    .await
    .ok();
    td.project("q").await;
    let other = td.agent("q", "o", json!({})).await;
    let other = other["id"].as_str().unwrap();
    let mate = td.agent("p", "m", json!({})).await;
    let mate = mate["id"].as_str().unwrap();
    for path in [
        format!("/api/sessions/{sid}"),
        format!("/api/sessions/{sid}/transcript"),
        format!("/api/cards/{card}/sessions"),
    ] {
        td.get_as(other, &path).await.err(403, "forbidden");
        assert_eq!(td.get_as(mate, &path).await.status, 200, "{path}");
    }
    // After the delete, the rule is the same: the tombstone's project.
    let r = td.delete(&format!("/api/agents/{aid}?confirm=true")).await;
    assert_eq!(r.status, 204, "{}", r.body);
    td.get_as(other, &format!("/api/sessions/{sid}/transcript"))
        .await
        .err(403, "forbidden");
    let t = td
        .get_as(mate, &format!("/api/sessions/{sid}/transcript"))
        .await;
    assert_eq!(t.ok()["blocks"].as_array().unwrap().len(), 30);
}
