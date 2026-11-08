//! Bookmark persistence, validation and operator-only access.
#![allow(clippy::unwrap_used, clippy::expect_used)]
use crate::common;
use common::TestDaemon;
use hyper::Method;
use serde_json::json;

#[tokio::test]
async fn edits_keep_order_and_reject_duplicate_or_missing_source() {
    let mut td = TestDaemon::start().await;
    for name in ["first", "second"] {
        td.put(
            "/api/bookmarks",
            json!({"title": name, "href": format!("https://{name}.test")}),
        )
        .await
        .ok();
    }
    let expected = json!([
        {"title": "Edited", "href": "https://new.test/"},
        {"title": "second", "href": "https://second.test/"}
    ]);
    assert_eq!(td.put("/api/bookmarks", json!({"title": "Edited", "href": "https://new.test", "previous_href": "https://first.test"})).await.ok(), &expected);
    for previous in ["https://new.test", "https://missing.test"] {
        td.put(
            "/api/bookmarks",
            json!({"title": "Bad", "href": "https://second.test", "previous_href": previous}),
        )
        .await
        .err(422, "invalid");
    }
    assert_eq!(td.get("/api/bookmarks").await.ok(), &expected);
    td.boot(None, None).await;
    assert_eq!(td.get("/api/bookmarks").await.ok(), &expected);
}

#[tokio::test]
async fn shared_list_deduplicates_and_removes() {
    let mut td = TestDaemon::start().await;
    let bookmark = json!({"title": " Example ", "href": "https://example.com"});
    td.put("/api/bookmarks", bookmark.clone()).await.ok();
    td.put("/api/bookmarks", bookmark).await.ok();
    assert_eq!(
        td.get("/api/bookmarks").await.ok(),
        &json!([{"title": "Example", "href": "https://example.com/"}])
    );
    td.put(
        "/api/bookmarks",
        json!({"title": "Wiki", "href": "/p/demo/knowledge/wiki/a%20b#part"}),
    )
    .await
    .ok();
    let removed = td
        .req(
            Method::DELETE,
            "/api/bookmarks",
            Some(json!({"href": "https://example.com"})),
            None,
        )
        .await;
    assert_eq!(
        removed.ok(),
        &json!([{"title": "Wiki", "href": "/p/demo/knowledge/wiki/a%20b#part"}])
    );
    assert_eq!(td.get("/api/bookmarks").await.ok(), removed.ok());
    td.boot(None, None).await;
    assert_eq!(td.get("/api/bookmarks").await.ok(), removed.ok());
}

#[tokio::test]
async fn rejects_unsafe_urls_blank_titles_and_agents() {
    let td = TestDaemon::start().await;
    for href in [
        "javascript:alert(1)",
        "data:text/html,hi",
        "//evil.test",
        "/settings",
        "/p/demo/knowledge/wikix",
        "https://user:pass@example.com",
        "/\\evil.test/p/demo/knowledge/wiki/a",
    ] {
        td.put("/api/bookmarks", json!({"title": "Bad", "href": href}))
            .await
            .err(422, "invalid");
    }
    td.put(
        "/api/bookmarks",
        json!({"title": "  ", "href": "https://example.com"}),
    )
    .await
    .err(422, "invalid");
    td.project("ops").await;
    td.agent("ops", "lead", json!({})).await;
    td.get_as("ops/lead", "/api/bookmarks")
        .await
        .err(403, "forbidden");
    td.put_as(
        "ops/lead",
        "/api/bookmarks",
        json!({"title": "No", "href": "https://example.com"}),
    )
    .await
    .err(403, "forbidden");
}
