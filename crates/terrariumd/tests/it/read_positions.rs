#![allow(clippy::unwrap_used)]
use crate::common::TestDaemon;
use serde_json::json;

#[tokio::test]
async fn visits_are_persistent_monotonic_bounded_and_operator_only() {
    let td = TestDaemon::start().await;
    td.project("meta").await;
    td.agent("meta", "lead", json!({"roles":["project_admin"]})).await;
    let thread = td.post("/api/threads", json!({"topic":"Read me", "scope":"meta", "agent":"lead"})).await.ok().clone();
    let card = td.post("/api/projects/meta/cards", json!({"title":"Read comments"})).await.ok().clone();
    let mail = td.post("/api/mail", json!({"to":"meta/lead", "subject":"Read mail", "body":"Hello"})).await.ok().clone();
    for (kind, id) in [("thread", &thread["id"]), ("card", &card["id"]), ("mail", &mail["thread_id"])] {
        let path = format!("/api/read-positions/{kind}/{}", id.as_str().unwrap());
        let open = |at: &str| json!({"through":at});
        let first = td.post(&path, open("2026-01-01T00:00:00Z")).await;
        assert!(first.ok()["previous"].is_null());
        let second = td.post(&path, open("2026-01-02T00:00:00Z")).await;
        assert_eq!(second.ok()["previous"], "2026-01-01T00:00:00.000Z");
        let stale = td.post(&path, open("2026-01-01T00:00:00Z")).await;
        assert_eq!(stale.ok()["read_at"], "2026-01-02T00:00:00.000Z");
        assert_eq!(td.post_as("meta/lead", &path, open("2026-01-03T00:00:00Z")).await.status, 403);
        assert_eq!(td.post(&path, open("2999-01-01T00:00:00Z")).await.status, 400);
        assert_eq!(td.post(&path, open("invalid")).await.status, 400);
    }
    for kind in ["thread", "card", "mail"] {
        assert_eq!(td.post(&format!("/api/read-positions/{kind}/missing"), json!({"through":"2026-01-01T00:00:00Z"})).await.status, 404);
    }
    let conn = terrariumd::db::open(&td.state.join("terrarium.db")).unwrap();
    let count: i64 = conn.query_row("SELECT count(*) FROM read_positions", [], |r| r.get(0)).unwrap();
    assert_eq!(count, 3);
    let plan: String = conn.query_row("EXPLAIN QUERY PLAN SELECT read_at FROM read_positions WHERE owner='operator' AND kind='card' AND item='x'", [], |r| r.get(3)).unwrap();
    assert!(plan.contains("PRIMARY KEY"), "{plan}");
}
