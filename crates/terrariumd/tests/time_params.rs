//! `?since=` and `?until=` take RFC 3339 with any offset or a duration, and
//! compare as UTC (api.md §2.1). A bare clock time is `400`.
#![allow(clippy::unwrap_used, clippy::expect_used)]

mod common;

use common::TestDaemon;
use serde_json::json;

/// Three audit rows and three events on 2026-10-20 at 08:00Z, 09:00Z, and 10:00Z, with
/// the kind and action `t.time`, on card `TER-1`.
async fn seed(td: &TestDaemon) {
    td.post(
        "/api/projects",
        json!({"name": "proj", "card_prefix": "TER"}),
    )
    .await
    .ok();
    td.daemon
        .as_ref()
        .unwrap()
        .state
        .db
        .write("system", |t| {
            for (i, h) in ["08", "09", "10"].iter().enumerate() {
                let at = format!("2026-10-20T{h}:00:00.000Z");
                t.execute(
                    "INSERT INTO audit (id, actor, action, target, project, at)
                     VALUES (?1, 'system', 't.time', ?2, 'proj', ?3)",
                    rusqlite::params![format!("AUDIT{i}"), format!("x{h}"), at],
                )?;
                t.execute(
                    "INSERT INTO events (id, project, card, kind, summary, payload, at)
                     VALUES (?1, 'proj', 'TER-1', 't.time', ?2, '{}', ?3)",
                    rusqlite::params![format!("EVENT{i}"), format!("x{h}"), at],
                )?;
            }
            Ok(())
        })
        .await
        .unwrap();
    // After the seed, so that `at` follows `seq` as in a live log.
    td.post("/api/projects/proj/cards", json!({"title": "c"}))
        .await
        .ok();
}

fn summaries(v: &serde_json::Value, key: &str) -> Vec<String> {
    v.as_array()
        .unwrap()
        .iter()
        .map(|e| e[key].as_str().unwrap().to_owned())
        .collect()
}

#[tokio::test]
async fn audit_since_with_an_offset_compares_as_utc() {
    let td = TestDaemon::start().await;
    seed(&td).await;
    // 19:30+10:00 is 09:30Z. As text it sorts before every row.
    let r = td
        .get("/api/audit?action=t.time&since=2026-10-20T19:30:00%2B10:00")
        .await;
    assert_eq!(summaries(r.ok(), "target"), ["x10"]);
    let r = td
        .get("/api/audit?action=t.time&since=2026-10-20T09:00:00Z")
        .await;
    assert_eq!(summaries(r.ok(), "target"), ["x09", "x10"]);
}

#[tokio::test]
async fn events_since_with_an_offset_compares_as_utc() {
    let td = TestDaemon::start().await;
    seed(&td).await;
    let r = td
        .get("/api/events?kind=t.time&since=2026-10-20T19:30:00%2B10:00")
        .await;
    assert_eq!(summaries(r.ok(), "summary"), ["x10"]);
    let r = td
        .get("/api/events?kind=t.time&since=2026-10-20T08:30:00-00:30")
        .await;
    assert_eq!(summaries(r.ok(), "summary"), ["x09", "x10"]);
    // The card events take the same forms.
    let r = td
        .get("/api/cards/TER-1/events?since=2026-10-20T19:30:00%2B10:00")
        .await;
    let s = summaries(r.ok(), "summary");
    assert_eq!(s[0], "x10", "{s:?}");
    assert_eq!(s.len(), 2, "x10 and the card's own `created` event: {s:?}");
    // A time after every event: nothing.
    let r = td
        .get("/api/events?kind=t.time&since=2099-01-01T00:00:00Z")
        .await;
    assert_eq!(r.ok().as_array().unwrap().len(), 0);
}

#[tokio::test]
async fn a_bare_clock_time_is_400() {
    let td = TestDaemon::start().await;
    seed(&td).await;
    for path in [
        "/api/audit?since=19:53",
        "/api/events?since=19:53",
        "/api/cards/TER-1/events?since=19:53",
        "/api/usage?since=19:53",
        "/api/usage?until=19:53",
        "/api/usage/project?project=proj&since=19:53",
    ] {
        let r = td.get(path).await;
        r.err(400, "bad_request");
        let m = r.body["error"]["message"].as_str().unwrap();
        assert!(m.contains("RFC 3339") && m.contains("30m"), "{path}: {m}");
    }
}

#[tokio::test]
async fn a_duration_counts_back_from_now() {
    let td = TestDaemon::start().await;
    seed(&td).await;
    // The seeded rows are in the past; one new audit row and event now.
    td.post("/api/projects", json!({"name": "later"}))
        .await
        .ok();
    let r = td.get("/api/audit?since=1h").await;
    let rows = r.ok().as_array().unwrap().clone();
    assert!(!rows.is_empty());
    assert!(rows.iter().all(|e| e["action"] != "t.time"), "{rows:?}");
    let r = td.get("/api/events?since=30m").await;
    let rows = r.ok().as_array().unwrap().clone();
    assert!(rows.iter().all(|e| e["kind"] != "t.time"), "{rows:?}");
    td.get("/api/usage?since=2h&until=1m").await.ok();
}

#[tokio::test]
async fn events_since_latest_still_skips_the_backlog() {
    let td = TestDaemon::start().await;
    seed(&td).await;
    let r = td.get("/api/events?since=latest").await;
    assert_eq!(r.ok().as_array().unwrap().len(), 0);
    let r = td.get("/api/events?since=0&kind=t.time").await;
    assert_eq!(r.ok().as_array().unwrap().len(), 3);
}
