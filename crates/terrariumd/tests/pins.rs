//! Pinned conversations (docs/api.md §6.11): pin and unpin by ULID and by
//! `project/name`, the order, a second pin, an unknown agent, the rights
//! (operator only), and the cascade when an agent is deleted.
#![allow(clippy::unwrap_used, clippy::expect_used)]

use crate::common;

use common::TestDaemon;
use serde_json::{Value, json};

fn agents(v: &Value) -> Vec<String> {
    v.as_array()
        .unwrap()
        .iter()
        .map(|p| {
            format!(
                "{}/{}",
                p["project"].as_str().unwrap(),
                p["name"].as_str().unwrap()
            )
        })
        .collect()
}

#[tokio::test]
async fn pin_unpin_order_and_cascade() {
    let td = TestDaemon::start().await;
    td.project("ops").await;
    let lead = td.agent("ops", "lead", json!({})).await;
    let lead_id = lead["id"].as_str().unwrap().to_owned();
    let dev = td.agent("ops", "dev", json!({})).await;
    let dev_id = dev["id"].as_str().unwrap().to_owned();

    assert_eq!(td.get("/api/pins").await.ok(), &json!([]));

    // Pin by ULID, then by project/name: the list keeps the pin order.
    let r = td.put(&format!("/api/pins/{dev_id}"), json!({})).await;
    assert_eq!(agents(r.ok()), ["ops/dev"]);
    let r = td.put("/api/pins/ops%2Flead", json!({})).await;
    assert_eq!(agents(r.ok()), ["ops/dev", "ops/lead"]);
    assert_eq!(r.body[1]["agent"], lead_id);
    assert!(r.body[1]["pinned_at"].is_string());

    // A second pin changes nothing.
    let r = td.put(&format!("/api/pins/{dev_id}"), json!({})).await;
    assert_eq!(agents(r.ok()), ["ops/dev", "ops/lead"]);

    // An unknown agent is a 404.
    td.put("/api/pins/ops%2Fnobody", json!({}))
        .await
        .err(404, "not_found");

    // Unpin; a second unpin is no error. Pin again: it goes to the end.
    let r = td.delete(&format!("/api/pins/{dev_id}")).await;
    assert_eq!(agents(r.ok()), ["ops/lead"]);
    let r = td.delete(&format!("/api/pins/{dev_id}")).await;
    assert_eq!(agents(r.ok()), ["ops/lead"]);
    let r = td.put(&format!("/api/pins/{dev_id}"), json!({})).await;
    assert_eq!(agents(r.ok()), ["ops/lead", "ops/dev"]);

    // The audit has the pins.
    let audit = td.get("/api/audit?order=desc&limit=100").await;
    assert!(
        audit.body.to_string().contains("agent.pin"),
        "{}",
        audit.body
    );

    // Only the operator has pins.
    td.get_as(&lead_id, "/api/pins").await.err(403, "forbidden");
    td.put_as(&lead_id, &format!("/api/pins/{dev_id}"), json!({}))
        .await
        .err(403, "forbidden");

    // Delete an agent: its pin goes too.
    let r = td.delete(&format!("/api/agents/{dev_id}")).await;
    assert!(r.status.is_success(), "{}", r.body);
    assert_eq!(agents(td.get("/api/pins").await.ok()), ["ops/lead"]);
    // Unpin of a deleted agent is no error.
    let r = td.delete(&format!("/api/pins/{dev_id}")).await;
    assert_eq!(agents(r.ok()), ["ops/lead"]);
}
