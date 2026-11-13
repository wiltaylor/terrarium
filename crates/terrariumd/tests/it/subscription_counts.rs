//! Effective counts must not expose agents outside the reader's project.
#![allow(clippy::unwrap_used)]

use crate::common::TestDaemon;
use serde_json::{Value, json};

fn named<'a>(rows: &'a Value, name: &str) -> &'a Value {
    rows.as_array()
        .unwrap()
        .iter()
        .find(|s| s["name"] == name)
        .unwrap()
}

#[tokio::test]
async fn effective_counts_are_actor_scoped_and_optional_outside_list() {
    let td = TestDaemon::start().await;
    td.project("p").await;
    td.project("q").await;
    let lead = td
        .agent(
            "p",
            "lead",
            json!({"roles": ["project_admin"], "harness": "codex"}),
        )
        .await;
    let member = td.agent("p", "member", json!({"harness": "pi"})).await;
    td.agent("q", "hidden", json!({"harness": "codex"})).await;
    for name in ["assigned", "zero"] {
        let created = td
            .post(
                "/api/subscriptions",
                json!({"name": name, "provider": "openai", "plan": "Pro", "harnesses": ["codex"]}),
            )
            .await;
        assert!(created.ok().get("assigned_agent_count").is_none());
    }
    let c = terrariumd::db::open(&td.state.join("terrarium.db")).unwrap();
    c.execute("INSERT INTO account_defaults(harness, subscription, updated_at) VALUES ('codex','assigned','2026-11-13T00:00:00Z') ON CONFLICT(harness) DO UPDATE SET subscription='assigned'", []).unwrap();
    let all = td.get("/api/subscriptions").await;
    assert_eq!(named(all.ok(), "assigned")["assigned_agent_count"], 2);
    assert_eq!(
        named(all.ok(), "assigned")["assigned_agent_count_scope"],
        "all_projects"
    );
    assert_eq!(named(all.ok(), "zero")["assigned_agent_count"], 0);
    let own = td
        .get_as(lead["id"].as_str().unwrap(), "/api/subscriptions")
        .await;
    let sub = named(own.ok(), "assigned");
    assert_eq!(sub["assigned_agent_count"], 1);
    assert_eq!(sub["assigned_agent_count_scope"], "project");
    assert_eq!(sub["assigned_agent_count_project"], "p");
    td.get_as(member["id"].as_str().unwrap(), "/api/subscriptions")
        .await
        .err(403, "forbidden");
    assert!(
        td.get("/api/subscriptions/assigned")
            .await
            .ok()
            .get("assigned_agent_count")
            .is_none()
    );
}
