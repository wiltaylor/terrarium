//! TER-208: the input-box watchdog (`terrariumd::input_watchdog`). The
//! runner moves an idle agent with unsubmitted text in its input box to
//! `blocked` with `INPUT_PENDING_REASON`. The daemon stores the reason and
//! mails the leads (or the operator). Another `blocked` reason mails nobody.
#![allow(clippy::unwrap_used, clippy::expect_used)]

use crate::common;

use std::time::Duration;

use common::TestDaemon;
use serde_json::{Value, json};
use terrarium_core::adapter::INPUT_PENDING_REASON;
use terrarium_core::{AgentId, AgentStatus, Event};

const SUBJECT: &str = "p/w has unsubmitted text in its input box";

async fn running(td: &TestDaemon, project: &str, name: &str, def: Value) -> (String, AgentId) {
    let a = td.agent(project, name, def).await;
    let id = a["id"].as_str().unwrap().to_owned();
    td.post_empty(&format!("/api/agents/{id}/start")).await;
    td.wait_status(&id, "idle").await;
    let aid = id.parse().unwrap();
    (id, aid)
}

fn blocked(reason: &str) -> Event {
    Event::StatusChanged {
        from: AgentStatus::Idle,
        to: AgentStatus::Blocked,
        reason: Some(reason.to_owned()),
    }
}

/// The `input_watchdog` events of agent `id`, once `n` of them are stored.
async fn watchdog_events(td: &TestDaemon, id: &str, n: usize) -> Vec<Value> {
    for _ in 0..200 {
        let r = td
            .get(&format!("/api/agents/{id}/events?kind=input_watchdog"))
            .await;
        let list = r.ok().as_array().unwrap().clone();
        if list.len() >= n {
            return list;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    panic!("no {n} input_watchdog events of {id}");
}

#[tokio::test]
async fn a_stuck_input_box_blocks_the_agent_and_mails_the_lead() {
    let td = TestDaemon::start().await;
    td.project("p").await;
    let lead = td
        .agent("p", "lead", json!({"roles": ["project_admin"]}))
        .await;
    let lead_id = lead["id"].as_str().unwrap();
    let (id, aid) = running(&td, "p", "w", json!({})).await;

    td.sup.emit_harness(aid, blocked(INPUT_PENDING_REASON));
    td.wait_status(&id, "blocked").await;
    let ev = watchdog_events(&td, &id, 1).await;
    assert_eq!(ev.len(), 1, "{ev:#?}");
    let a = td.get(&format!("/api/agents/{id}")).await.ok().clone();
    assert_eq!(a["status"], "blocked");
    let status = td
        .get(&format!("/api/agents/{id}/events?kind=status"))
        .await;
    let last = status.ok().as_array().unwrap().last().unwrap().clone();
    assert_eq!(last["data"]["reason"], INPUT_PENDING_REASON);

    let mail = td.mail_of(lead_id).await;
    let m = mail
        .iter()
        .find(|m| m["subject"] == SUBJECT)
        .unwrap_or_else(|| panic!("{mail:#?}"));
    assert_eq!(m["from"], "system");
    let body = td
        .get_as(lead_id, &format!("/api/mail/{}", m["id"].as_str().unwrap()))
        .await
        .ok()["body"]
        .as_str()
        .unwrap()
        .to_owned();
    assert!(body.contains("terrarium agent screen p/w"), "{body}");
    assert!(body.contains("terrarium agent keys p/w enter"), "{body}");
}

#[tokio::test]
async fn with_no_lead_the_operator_gets_the_mail() {
    let td = TestDaemon::start().await;
    td.project("p").await;
    let (id, aid) = running(&td, "p", "w", json!({})).await;
    td.sup.emit_harness(aid, blocked(INPUT_PENDING_REASON));
    watchdog_events(&td, &id, 1).await;
    let mail = td.get("/api/mail?to=operator").await;
    assert!(
        mail.ok()
            .as_array()
            .unwrap()
            .iter()
            .any(|m| m["subject"] == SUBJECT),
        "{}",
        mail.body
    );
}

#[tokio::test]
async fn another_blocked_reason_mails_nobody() {
    let td = TestDaemon::start().await;
    td.project("p").await;
    let lead = td
        .agent("p", "lead", json!({"roles": ["project_admin"]}))
        .await;
    let (id, aid) = running(&td, "p", "w", json!({})).await;
    td.sup.emit_harness(aid, blocked("stalled"));
    td.wait_status(&id, "blocked").await;
    let ev = td
        .get(&format!("/api/agents/{id}/events?kind=input_watchdog"))
        .await;
    assert!(ev.ok().as_array().unwrap().is_empty(), "{}", ev.body);
    let mail = td.mail_of(lead["id"].as_str().unwrap()).await;
    assert!(!mail.iter().any(|m| m["subject"] == SUBJECT), "{mail:#?}");
}
