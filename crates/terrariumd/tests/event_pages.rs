//! The JSON pages of `GET /api/events` and `GET /api/agents/{id}/events`:
//! oldest first by default, newest first with `?order=desc` (api.md §9.1).
#![allow(clippy::unwrap_used, clippy::expect_used)]

use crate::common;

use common::TestDaemon;
use serde_json::{Value, json};

/// Ten events of kind `t.page`, `e0` to `e9` in `seq` order. The even ones
/// are in project `pa` and of agent `pa/one`; the odd ones are in `pb`.
/// Every third one (`e0`, `e3`, `e6`, `e9`) has the kind `t.other`.
/// Gives the ULID of `pa/one`.
async fn seed(td: &TestDaemon) -> String {
    td.project("pa").await;
    td.project("pb").await;
    let agent = td.agent("pa", "one", json!({})).await;
    let id = agent["id"].as_str().unwrap().to_owned();
    let agent_id = id.clone();
    td.daemon
        .as_ref()
        .unwrap()
        .state
        .db
        .write("system", move |t| {
            for i in 0..10 {
                let (project, agent) = if i % 2 == 0 {
                    ("pa", Some(agent_id.as_str()))
                } else {
                    ("pb", None)
                };
                let kind = if i % 3 == 0 { "t.other" } else { "t.page" };
                t.execute(
                    "INSERT INTO events (id, project, agent_id, kind, summary, payload, at)
                     VALUES (?1, ?2, ?3, ?4, ?5, '{}', '2026-10-20T08:00:00.000Z')",
                    rusqlite::params![format!("PAGE{i}"), project, agent, kind, format!("e{i}")],
                )?;
            }
            Ok(())
        })
        .await
        .unwrap();
    id
}

fn summaries(v: &Value) -> Vec<String> {
    v.as_array()
        .unwrap()
        .iter()
        .map(|e| e["summary"].as_str().unwrap().to_owned())
        .collect()
}

/// Read every page of `path` (which has a `?`), following `X-Next-Cursor`.
/// Gives the summaries of each page.
async fn pages(td: &TestDaemon, path: &str) -> Vec<Vec<String>> {
    let mut out = Vec::new();
    let mut cursor: Option<String> = None;
    loop {
        let url = match &cursor {
            Some(c) => format!("{path}&cursor={c}"),
            None => path.to_owned(),
        };
        let r = td.get(&url).await;
        out.push(summaries(r.ok()));
        cursor = r.next_cursor();
        if cursor.is_none() {
            return out;
        }
        assert!(out.len() < 20, "the pages do not end: {out:?}");
    }
}

#[tokio::test]
async fn desc_is_newest_first_and_the_cursor_goes_to_older_events() {
    let td = TestDaemon::start().await;
    seed(&td).await;
    let p = pages(&td, "/api/events?project=pb&order=desc&limit=2").await;
    assert_eq!(p, [vec!["e9", "e7"], vec!["e5", "e3"], vec!["e1"]]);
    // The same events oldest first, as before.
    let p = pages(&td, "/api/events?project=pb&limit=2").await;
    assert_eq!(p, [vec!["e1", "e3"], vec!["e5", "e7"], vec!["e9"]]);
    // `asc` is the default.
    let p = pages(&td, "/api/events?project=pb&order=asc&limit=2").await;
    assert_eq!(p, [vec!["e1", "e3"], vec!["e5", "e7"], vec!["e9"]]);
}

#[tokio::test]
async fn desc_with_a_full_last_page_has_no_cursor() {
    let td = TestDaemon::start().await;
    seed(&td).await;
    // Five events, pages of five: one page and no cursor.
    let r = td.get("/api/events?project=pb&order=desc&limit=5").await;
    assert_eq!(summaries(r.ok()), ["e9", "e7", "e5", "e3", "e1"]);
    assert_eq!(r.next_cursor(), None);
    // Pages of four: the cursor is the `seq` of the last event, and the
    // next page has only the one older event.
    let p = pages(&td, "/api/events?project=pb&order=desc&limit=4").await;
    assert_eq!(p, [vec!["e9", "e7", "e5", "e3"], vec!["e1"]]);
}

#[tokio::test]
async fn desc_keeps_the_filters() {
    let td = TestDaemon::start().await;
    let agent = seed(&td).await;
    let p = pages(&td, "/api/events?project=pa&kind=t.page&order=desc&limit=2").await;
    assert_eq!(p, [vec!["e8", "e4"], vec!["e2"]]);
    let p = pages(
        &td,
        &format!("/api/events?agent={agent}&kind=t.other&order=desc&limit=1"),
    )
    .await;
    assert_eq!(p, [vec!["e6"], vec!["e0"]]);
    // The agent key filters as the ULID does.
    let p = pages(
        &td,
        "/api/events?agent=pa/one&kind=t.other&order=desc&limit=1",
    )
    .await;
    assert_eq!(p, [vec!["e6"], vec!["e0"]]);
    // The events of one agent.
    let p = pages(
        &td,
        &format!("/api/agents/{agent}/events?kind=t.page&order=desc&limit=2"),
    )
    .await;
    assert_eq!(p, [vec!["e8", "e4"], vec!["e2"]]);
}

#[tokio::test]
async fn desc_stops_at_since() {
    let td = TestDaemon::start().await;
    seed(&td).await;
    // `since` is the lower bound of every page: e3 and older do not come.
    let p = pages(&td, "/api/events?project=pb&since=PAGE3&order=desc&limit=1").await;
    assert_eq!(p, [vec!["e9"], vec!["e7"], vec!["e5"]]);
    // A `since` after every event: an empty page and no cursor.
    let r = td
        .get("/api/events?project=pb&since=latest&order=desc")
        .await;
    assert_eq!(summaries(r.ok()), Vec::<String>::new());
    assert_eq!(r.next_cursor(), None);
}

#[tokio::test]
async fn desc_of_an_empty_match_is_an_empty_page() {
    let td = TestDaemon::start().await;
    seed(&td).await;
    let r = td.get("/api/events?kind=t.none&order=desc").await;
    assert_eq!(summaries(r.ok()), Vec::<String>::new());
    assert_eq!(r.next_cursor(), None);
}

#[tokio::test]
async fn a_bad_order_or_desc_cursor_is_400() {
    let td = TestDaemon::start().await;
    seed(&td).await;
    td.get("/api/events?order=up").await.err(400, "bad_request");
    td.get("/api/events?order=desc&cursor=PAGE3")
        .await
        .err(400, "bad_request");
    // A stream is oldest first only.
    assert!(td.ws("/api/events?order=desc", None).await.is_err());
    assert!(td.ws("/api/events?order=asc", None).await.is_ok());
}
