//! The operator's default recipient is shared, validated, and operator-only.
#![allow(clippy::unwrap_used, clippy::expect_used)]
mod common;
use common::TestDaemon;
use serde_json::json;

#[tokio::test]
async fn default_agent_preferences() {
    let td = TestDaemon::start().await;
    td.project("ops").await;
    let agent = td.agent("ops", "lead", json!({})).await;
    let id = agent["id"].as_str().unwrap();
    let none = json!({"default_agent": null});
    let selected = json!({"default_agent": "ops/lead"});
    assert_eq!(td.get("/api/preferences").await.ok(), &none);
    assert_eq!(
        td.put("/api/preferences", selected.clone()).await.ok(),
        &selected
    );
    assert_eq!(td.get("/api/preferences").await.ok(), &selected);
    td.put("/api/preferences", json!({"default_agent": "ops/missing"}))
        .await
        .err(404, "not_found");
    assert_eq!(td.get("/api/preferences").await.ok(), &selected);
    td.get_as(id, "/api/preferences")
        .await
        .err(403, "forbidden");
    td.put_as(id, "/api/preferences", none.clone())
        .await
        .err(403, "forbidden");
    assert_eq!(td.put("/api/preferences", none.clone()).await.ok(), &none);
    assert_eq!(
        td.put("/api/preferences", json!({"default_agent": id}))
            .await
            .ok(),
        &selected
    );
    assert!(
        td.delete(&format!("/api/agents/{id}"))
            .await
            .status
            .is_success()
    );
    assert_eq!(td.get("/api/preferences").await.ok(), &none);
}
