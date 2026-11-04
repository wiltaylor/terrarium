//! Issue #12: `POST /api/agents/{id}/interrupt` (docs/api.md §6.5), the
//! Stop of the dashboard. The interrupt keys of each harness, the rights
//! of a send, the audit row, and the refusals.
#![allow(clippy::unwrap_used, clippy::expect_used)]

mod common;

use common::TestDaemon;
use serde_json::{Value, json};
use terrarium_core::AgentId;

/// A started agent with a turn that runs until something ends it.
async fn in_turn(td: &TestDaemon, project: &str, name: &str, extra: Value) -> AgentId {
    let a = td.agent(project, name, extra).await;
    let id = a["id"].as_str().unwrap().to_owned();
    td.post_empty(&format!("/api/agents/{id}/start")).await;
    td.wait_status(&id, "idle").await;
    let id: AgentId = id.parse().unwrap();
    td.sup.open_turn(id);
    td.wait_status(&id.to_string(), "working").await;
    id
}

/// Each harness gets its own keys: Esc for Claude Code, Codex, and pi;
/// Esc Esc for Copilot ("esc esc interrupt"). Never Ctrl-C. The turn ends
/// with the reason `interrupted`, and the harness keeps running.
#[tokio::test]
async fn each_harness_gets_its_interrupt_keys() {
    let td = TestDaemon::start().await;
    td.project("p").await;
    for (harness, keys) in [
        ("claude", b"\x1b".to_vec()),
        ("codex", b"\x1b".to_vec()),
        ("copilot", b"\x1b\x1b".to_vec()),
        ("pi", b"\x1b".to_vec()),
    ] {
        let id = in_turn(&td, "p", harness, json!({"harness": harness})).await;
        let r = td.post_empty(&format!("/api/agents/{id}/interrupt")).await;
        assert_eq!(r.status.as_u16(), 202, "{harness}: {}", r.body);
        assert_eq!(r.ok()["interrupted"], true);
        assert_eq!(td.sup.typed(id), keys, "{harness}");
        assert!(!td.sup.typed(id).contains(&0x03), "{harness}: no Ctrl-C");
        td.wait_status(&id.to_string(), "idle").await;
        let v = td.get(&format!("/api/agents/{id}")).await;
        assert_eq!(
            v.ok()["status_reason"],
            "interrupted",
            "{harness}: {}",
            v.body
        );
    }
    let audit = td.get("/api/audit?action=agent.interrupt").await;
    let rows = audit.ok().as_array().unwrap().clone();
    assert_eq!(rows.len(), 4, "{rows:?}");
    assert!(rows.iter().any(|r| r.to_string().contains("\"copilot\"")));
}

/// The rights of a send: a member and the lead of another project get
/// 403; the own lead, a sysadmin, and the operator get 202.
#[tokio::test]
async fn interrupt_needs_the_rights_of_a_send() {
    let td = TestDaemon::start().await;
    td.project("p").await;
    td.project("q").await;
    let id = in_turn(&td, "p", "w", json!({})).await;
    let member = td.agent("p", "m", json!({})).await;
    let lead = td
        .agent("p", "lead", json!({"roles": ["project_admin"]}))
        .await;
    let other_lead = td
        .agent("q", "lead", json!({"roles": ["project_admin"]}))
        .await;
    let url = format!("/api/agents/{id}/interrupt");
    for who in [&member, &other_lead] {
        td.post_as(who["id"].as_str().unwrap(), &url, json!({}))
            .await
            .err(403, "forbidden");
    }
    assert!(td.sup.typed(id).is_empty());
    let r = td
        .post_as(lead["id"].as_str().unwrap(), &url, json!({}))
        .await;
    assert_eq!(r.status.as_u16(), 202, "{}", r.body);
    assert_eq!(td.sup.typed(id), b"\x1b".to_vec());
    let audit = td.get("/api/audit?action=agent.interrupt").await;
    let rows = audit.ok().as_array().unwrap().clone();
    assert_eq!(rows.len(), 1, "{rows:?}");
    assert_eq!(
        rows[0]["actor"],
        format!("agent:{}", lead["id"].as_str().unwrap()),
        "{rows:?}"
    );
}

/// No turn runs: 409 `conflict`, and no key goes out (an Esc on an idle
/// prompt can open a menu). A stopped agent: 409 `not_running`.
#[tokio::test]
async fn no_turn_and_a_stopped_agent_are_refused() {
    let td = TestDaemon::start().await;
    td.project("p").await;
    let a = td.agent("p", "w", json!({})).await;
    let id = a["id"].as_str().unwrap().to_owned();
    let url = format!("/api/agents/{id}/interrupt");
    td.post_empty(&url).await.err(409, "not_running");
    td.post_empty(&format!("/api/agents/{id}/start")).await;
    td.wait_status(&id, "idle").await;
    td.post_empty(&url).await.err(409, "conflict");
    assert!(td.sup.typed(id.parse().unwrap()).is_empty());
    let audit = td.get("/api/audit?action=agent.interrupt").await;
    assert!(audit.ok().as_array().unwrap().is_empty());
}
