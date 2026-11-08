//! The `audit` frames of the firehose (`GET /api/events?audit=1`,
//! api.md §9.1) and `GET /api/audit?order=desc` (api.md §4). A frame goes
//! only to a caller that may read the audit row: the operator and a
//! sysadmin get every row, a project admin the rows of its project, and
//! another agent none.
#![allow(clippy::unwrap_used, clippy::expect_used)]

use crate::common;

use std::time::Duration;

use common::TestDaemon;
use futures_util::StreamExt;
use serde_json::{Value, json};
use terrariumd::events::EventDraft;
use tokio::net::UnixStream;
use tokio_tungstenite::WebSocketStream;

type Ws = WebSocketStream<UnixStream>;

/// Emit the event `t.sentinel` in project `p`: a frame that every reader
/// of `p` gets after the frames of the earlier writes.
async fn sentinel(td: &TestDaemon) {
    td.daemon
        .as_ref()
        .unwrap()
        .state
        .db
        .write("system", |t| {
            t.emit(EventDraft::new("t.sentinel", "sentinel", json!({})).project("p"))?;
            Ok(())
        })
        .await
        .unwrap();
}

/// Read frames until the `t.sentinel` event, and then for a short time
/// more. Gives the `action` of each `audit` frame.
async fn audit_actions(ws: &mut Ws) -> Vec<String> {
    let mut out = Vec::new();
    let mut seen_sentinel = false;
    loop {
        let wait = if seen_sentinel { 300 } else { 5000 };
        let Ok(next) = tokio::time::timeout(Duration::from_millis(wait), ws.next()).await else {
            assert!(seen_sentinel, "no sentinel event came");
            return out;
        };
        let msg = next.unwrap().unwrap();
        let v: Value = serde_json::from_str(msg.to_text().unwrap()).unwrap();
        match v["kind"].as_str().unwrap() {
            "audit" => {
                assert!(v.get("seq").is_none(), "an audit frame has no seq: {v}");
                out.push(format!(
                    "{}@{}",
                    v["audit"]["action"].as_str().unwrap(),
                    v["audit"]["project"].as_str().unwrap_or("")
                ));
            }
            "t.sentinel" => seen_sentinel = true,
            _ => {}
        }
    }
}

#[tokio::test]
async fn audit_frames_go_to_audit_readers_only() {
    let td = TestDaemon::start().await;
    td.project("p").await;
    td.project("q").await;
    td.agent("p", "member", json!({})).await;
    td.agent("p", "lead", json!({"roles": ["project_admin"]}))
        .await;

    let mut operator = td.ws("/api/events?live=1&audit=1", None).await.unwrap();
    let mut lead = td
        .ws("/api/events?live=1&audit=1", Some("p/lead"))
        .await
        .unwrap();
    let mut member = td
        .ws("/api/events?live=1&audit=1", Some("p/member"))
        .await
        .unwrap();
    // No `audit=1`: no audit frames, also for the operator.
    let mut plain = td.ws("/api/events?live=1", None).await.unwrap();

    // Two audited actions: one in `p`, one in `q`.
    td.agent("p", "new", json!({})).await;
    td.agent("q", "other", json!({})).await;
    sentinel(&td).await;

    let all = audit_actions(&mut operator).await;
    assert!(all.contains(&"agent.create@p".to_owned()), "{all:?}");
    assert!(all.contains(&"agent.create@q".to_owned()), "{all:?}");
    // The project admin gets the rows of its project only.
    let own = audit_actions(&mut lead).await;
    assert!(own.contains(&"agent.create@p".to_owned()), "{own:?}");
    assert!(own.iter().all(|a| a.ends_with("@p")), "{own:?}");
    // A member may not read the audit log: no frame, and the stream works.
    assert_eq!(audit_actions(&mut member).await, Vec::<String>::new());
    assert_eq!(audit_actions(&mut plain).await, Vec::<String>::new());
    // The frames are notifications only: the event log has no audit kind.
    let log = td.get("/api/events?kind=audit").await;
    assert_eq!(log.ok().as_array().unwrap().len(), 0);
}

#[tokio::test]
async fn audit_frames_follow_the_project_of_the_stream() {
    let td = TestDaemon::start().await;
    td.project("p").await;
    td.project("q").await;
    let mut ws = td
        .ws("/api/events?live=1&audit=1&project=p", None)
        .await
        .unwrap();
    td.agent("q", "other", json!({})).await;
    td.agent("p", "new", json!({})).await;
    sentinel(&td).await;
    assert_eq!(audit_actions(&mut ws).await, ["agent.create@p"]);
}

#[tokio::test]
async fn audit_frames_on_sse_have_no_id() {
    let td = TestDaemon::start().await;
    td.project("p").await;
    let stream = UnixStream::connect(&td.socket).await.unwrap();
    let (mut sender, conn) =
        hyper::client::conn::http1::handshake(hyper_util::rt::TokioIo::new(stream))
            .await
            .unwrap();
    tokio::spawn(conn);
    let req = hyper::Request::builder()
        .uri("/api/events?live=1&audit=1&kind=none")
        .header("host", "localhost")
        .header("accept", "text/event-stream")
        .body(http_body_util::Empty::<hyper::body::Bytes>::new())
        .unwrap();
    let resp = sender.send_request(req).await.unwrap();
    let mut body = resp.into_body();
    td.agent("p", "new", json!({})).await;
    let mut text = String::new();
    while !text.contains("\n\n") {
        let frame = tokio::time::timeout(
            Duration::from_secs(5),
            http_body_util::BodyExt::frame(&mut body),
        )
        .await
        .unwrap()
        .unwrap()
        .unwrap();
        text.push_str(std::str::from_utf8(&frame.into_data().unwrap()).unwrap());
    }
    assert!(text.contains("event: audit"), "{text}");
    assert!(text.contains(r#""action":"agent.create""#), "{text}");
    assert!(!text.contains("id:"), "{text}");
}

#[tokio::test]
async fn audit_desc_is_newest_first() {
    let td = TestDaemon::start().await;
    for name in ["a", "b", "c"] {
        td.project(name).await;
    }
    let first = td
        .get("/api/audit?action=project.create&order=desc&limit=2")
        .await;
    let names = |v: &Value| -> Vec<String> {
        v.as_array()
            .unwrap()
            .iter()
            .map(|e| e["target"].as_str().unwrap().to_owned())
            .collect()
    };
    assert_eq!(names(first.ok()), ["c", "b"]);
    let cursor = first.next_cursor().unwrap();
    let rest = td
        .get(&format!(
            "/api/audit?action=project.create&order=desc&limit=2&cursor={cursor}"
        ))
        .await;
    assert_eq!(names(rest.ok()), ["a"]);
    assert!(rest.next_cursor().is_none());
    td.get("/api/audit?order=up").await.err(400, "bad_request");
}
