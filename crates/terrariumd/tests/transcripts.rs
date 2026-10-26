//! The unified session transcript (docs/api.md §6.6): the sessions list
//! fields, paging, the live stream, the origin tags, rights, and the path
//! rule.
#![allow(clippy::unwrap_used, clippy::expect_used)]

mod common;

use std::io::Write;
use std::path::{Path, PathBuf};
use std::time::Duration;

use common::TestDaemon;
use http_body_util::{BodyExt, Empty};
use hyper::Request;
use hyper::body::Bytes;
use hyper_util::rt::TokioIo;
use serde_json::{Value, json};
use terrariumd::events::EventDraft;
use tokio::net::UnixStream;

/// The Claude fixture of the adapter: three tool turns, then odd records.
fn fixture() -> String {
    std::fs::read_to_string(
        Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("../adapters/claude/tests/fixtures/transcript.jsonl"),
    )
    .unwrap()
}

/// A project, an agent in it, started; and its session.
async fn started(td: &TestDaemon, project: &str, name: &str) -> (Value, Value) {
    // The project may exist already (a second agent).
    td.post("/api/projects", json!({"name": project})).await;
    let a = td.agent(project, name, json!({})).await;
    let id = a["id"].as_str().unwrap().to_owned();
    td.post_empty(&format!("/api/agents/{id}/start")).await.ok();
    td.wait_status(&id, "idle").await;
    let s = session(td, &id).await;
    (a, s)
}

async fn session(td: &TestDaemon, agent: &str) -> Value {
    for _ in 0..200 {
        let r = td.get(&format!("/api/agents/{agent}/sessions")).await;
        let s = r.ok()[0].clone();
        if s["harness_session_id"].is_string() {
            return s;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    panic!("no harness session");
}

/// The session file of the agent's harness session, in its own dir.
fn session_file(td: &TestDaemon, agent: &Value, s: &Value) -> PathBuf {
    common::claude_project_dir(&td.state, agent["id"].as_str().unwrap()).join(format!(
        "{}.jsonl",
        s["harness_session_id"].as_str().unwrap()
    ))
}

fn append(path: &Path, text: &str) {
    let mut f = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(path)
        .unwrap();
    f.write_all(text.as_bytes()).unwrap();
}

fn user_line(uuid: &str, text: &str) -> String {
    json!({"type": "user", "uuid": uuid, "timestamp": "2026-10-26T01:00:00.000Z",
           "message": {"role": "user", "content": text}})
    .to_string()
        + "\n"
}

fn transcript_url(agent: &Value, s: &Value) -> String {
    format!(
        "/api/agents/{}/sessions/{}/transcript",
        agent["id"].as_str().unwrap(),
        s["id"].as_str().unwrap()
    )
}

#[tokio::test]
async fn sessions_list_carries_the_transcript_fields() {
    let td = TestDaemon::start().await;
    let (a, s) = started(&td, "p", "w").await;
    let id = a["id"].as_str().unwrap();
    // No file yet: no transcript.
    let r = td.get(&format!("/api/agents/{id}/sessions")).await;
    let row = &r.ok()[0];
    assert_eq!(row["transcript"], false);
    assert_eq!(row["blocks"], 0);
    assert_eq!(row["active"], true);
    assert!(row["title"].is_null());

    std::fs::write(session_file(&td, &a, &s), fixture()).unwrap();
    let r = td.get(&format!("/api/agents/{id}/sessions")).await;
    let row = &r.ok()[0];
    assert_eq!(row["transcript"], true);
    assert_eq!(row["blocks"], 30);
    assert_eq!(row["active"], true);
    assert_eq!(
        row["title"],
        "You now have access to target vlab until 2026-10-25T10:13:44.170Z. Run `terrariu"
    );
    assert_eq!(row["title"].as_str().unwrap().chars().count(), 80);

    td.post_empty(&format!("/api/agents/{id}/stop")).await.ok();
    td.wait_status(id, "stopped").await;
    let r = td.get(&format!("/api/agents/{id}/sessions")).await;
    assert_eq!(r.ok()[0]["active"], false);
    assert_eq!(r.ok()[0]["blocks"], 30);
}

#[tokio::test]
async fn transcript_pages_with_after_and_limit() {
    let td = TestDaemon::start().await;
    let (a, s) = started(&td, "p", "w").await;
    std::fs::write(session_file(&td, &a, &s), fixture()).unwrap();
    let url = transcript_url(&a, &s);

    let r = td.get(&format!("{url}?limit=5")).await;
    let b = r.ok();
    assert_eq!(b["session"], s["id"]);
    assert_eq!(b["harness"], "claude");
    assert_eq!(b["active"], true);
    assert_eq!(b["complete"], false);
    assert_eq!(b["next_after"], 5);
    let blocks = b["blocks"].as_array().unwrap();
    let seqs: Vec<u64> = blocks.iter().map(|b| b["seq"].as_u64().unwrap()).collect();
    assert_eq!(seqs, [1, 2, 3, 4, 5]);
    let kinds: Vec<&str> = blocks.iter().map(|b| b["kind"].as_str().unwrap()).collect();
    assert_eq!(
        kinds,
        ["text", "tool_call", "tool_result", "thinking", "text"]
    );
    assert_eq!(blocks[0]["session"], s["id"]);
    assert_eq!(blocks[0]["origin"], "user");
    assert_eq!(
        blocks[1]["source"]["file"],
        format!("{}.jsonl", s["harness_session_id"].as_str().unwrap())
    );
    assert_eq!(
        blocks[2]["result"]["tool_id"], blocks[1]["tool"]["id"],
        "the result names its call"
    );

    let r = td.get(&format!("{url}?after=5")).await;
    let b = r.ok();
    assert_eq!(b["complete"], true);
    assert_eq!(b["next_after"], 30);
    assert_eq!(b["blocks"][0]["seq"], 6);
    assert_eq!(b["blocks"].as_array().unwrap().len(), 25);

    // Past the end: nothing, complete, `next_after` stays.
    let r = td.get(&format!("{url}?after=30")).await;
    assert_eq!(r.ok()["blocks"], json!([]));
    assert_eq!(r.ok()["next_after"], 30);
    assert_eq!(r.ok()["complete"], true);

    // The harness session ID names the session too.
    let r = td
        .get(&format!(
            "/api/agents/{}/sessions/{}/transcript?limit=1",
            a["id"].as_str().unwrap(),
            s["harness_session_id"].as_str().unwrap()
        ))
        .await;
    assert_eq!(r.ok()["session"], s["id"]);

    let r = td
        .get(&format!(
            "/api/agents/{}/sessions/NOPE/transcript",
            a["id"].as_str().unwrap()
        ))
        .await;
    r.err(404, "not_found");
}

#[tokio::test]
async fn operator_and_pointer_turns_are_tagged_from_the_delivery_log() {
    let td = TestDaemon::start().await;
    let (a, s) = started(&td, "p", "w").await;
    let id = a["id"].as_str().unwrap();
    let file = session_file(&td, &a, &s);
    std::fs::write(&file, "").unwrap();
    let typed = "Run `ls` and reply done.";
    td.post(&format!("/api/agents/{id}/send"), json!({"text": typed}))
        .await
        .ok();
    // A pointer that the daemon typed (the push listener writes this row).
    let pointer = "New mail from p/x (thread T1). Run `terrarium mail read --thread T1`.";
    let (agent, text) = (id.to_owned(), pointer.to_owned());
    td.daemon
        .as_ref()
        .unwrap()
        .state
        .db
        .write("system", move |t| {
            terrariumd::store::transcripts::record_delivery(
                t,
                &agent,
                terrarium_core::transcript::Origin::AgentMailPointer,
                &text,
            )
        })
        .await
        .unwrap();
    // The harness keeps the text with other line ends: still a match.
    append(&file, &user_line("u1", "Run `ls`\r\nand reply done."));
    append(&file, &user_line("u2", pointer));
    append(&file, &user_line("u3", "typed in the terminal"));
    let r = td.get(&transcript_url(&a, &s)).await;
    let origins: Vec<&str> = r.ok()["blocks"]
        .as_array()
        .unwrap()
        .iter()
        .map(|b| b["origin"].as_str().unwrap())
        .collect();
    assert_eq!(origins, ["operator", "agent_mail_pointer", "user"]);
}

/// Read SSE frames from an open response until `pred` holds for one, or
/// the time is up. Returns all frames read, as `(event, data)`.
async fn sse_until(
    body: &mut hyper::body::Incoming,
    buf: &mut String,
    pred: impl Fn(&str, &Value) -> bool,
) -> Vec<(String, Value)> {
    let mut frames = Vec::new();
    let end = tokio::time::Instant::now() + Duration::from_secs(10);
    loop {
        while let Some(i) = buf.find("\n\n") {
            let raw: String = buf.drain(..i + 2).collect();
            let mut event = String::new();
            let mut data = String::new();
            for l in raw.lines() {
                if let Some(e) = l
                    .strip_prefix("event: ")
                    .or_else(|| l.strip_prefix("event:"))
                {
                    event = e.trim().to_owned();
                } else if let Some(d) = l.strip_prefix("data: ").or_else(|| l.strip_prefix("data:"))
                {
                    data.push_str(d);
                }
            }
            if event.is_empty() {
                continue;
            }
            let v: Value = serde_json::from_str(&data).unwrap_or(Value::Null);
            let done = pred(&event, &v);
            frames.push((event, v));
            if done {
                return frames;
            }
        }
        let left = end.saturating_duration_since(tokio::time::Instant::now());
        assert!(!left.is_zero(), "no matching frame; got {frames:?}");
        match tokio::time::timeout(left, body.frame()).await {
            Ok(Some(Ok(f))) => {
                if let Some(d) = f.data_ref() {
                    buf.push_str(&String::from_utf8_lossy(d));
                }
            }
            other => panic!("the stream ended: {other:?}; frames {frames:?}"),
        }
    }
}

async fn open_sse(td: &TestDaemon, path: &str) -> hyper::Response<hyper::body::Incoming> {
    let stream = UnixStream::connect(&td.socket).await.unwrap();
    let (mut sender, conn) = hyper::client::conn::http1::handshake(TokioIo::new(stream))
        .await
        .unwrap();
    tokio::spawn(conn);
    let req = Request::builder()
        .uri(path)
        .header("host", "localhost")
        .header("accept", "text/event-stream")
        .body(Empty::<Bytes>::new())
        .unwrap();
    let resp = sender.send_request(req).await.unwrap();
    assert_eq!(resp.status(), 200);
    resp
}

#[tokio::test]
async fn a_growing_file_streams_new_blocks_and_updates() {
    let td = TestDaemon::start().await;
    let (a, s) = started(&td, "p", "w").await;
    let file = session_file(&td, &a, &s);
    let text = fixture();
    let first: String = text.lines().take(20).map(|l| format!("{l}\n")).collect();
    std::fs::write(&file, &first).unwrap();
    let page = td.get(&transcript_url(&a, &s)).await;
    let last = page.ok()["next_after"].as_u64().unwrap();
    assert!(last > 0);

    let resp = open_sse(
        &td,
        &format!("{}/stream?after={last}", transcript_url(&a, &s)),
    )
    .await;
    let mut body = resp.into_body();
    let mut buf = String::new();
    let frames = sse_until(&mut body, &mut buf, |e, _| e == "status").await;
    let (_, status) = frames.last().unwrap();
    assert_eq!(status["status"], "idle");
    assert_eq!(status["active"], true);
    assert!(
        frames.iter().all(|(e, _)| e != "block"),
        "nothing after `after` yet: {frames:?}"
    );

    // The rest of the file: new blocks from `last + 1`, in order.
    let rest: String = text.lines().skip(20).map(|l| format!("{l}\n")).collect();
    append(&file, &rest);
    let frames = sse_until(&mut body, &mut buf, |e, d| e == "block" && d["seq"] == 30).await;
    let seqs: Vec<u64> = frames
        .iter()
        .filter(|(e, _)| e == "block")
        .map(|(_, d)| d["seq"].as_u64().unwrap())
        .collect();
    assert_eq!(seqs, ((last + 1)..=30).collect::<Vec<_>>());

    // A new part of an assistant message moves its usage: the earlier
    // block comes again as an `update`.
    let msg = |part: Value, uuid: &str| {
        json!({"type": "assistant", "uuid": uuid, "timestamp": "2026-10-26T01:00:01.000Z",
               "message": {"id": "msg_live", "model": "claude-haiku-5-5", "role": "assistant",
                           "content": [part], "usage": {"input_tokens": 3, "output_tokens": 4}}})
        .to_string()
            + "\n"
    };
    append(
        &file,
        &msg(json!({"type": "thinking", "thinking": "hm"}), "l1"),
    );
    sse_until(&mut body, &mut buf, |e, d| e == "block" && d["seq"] == 31).await;
    append(&file, &msg(json!({"type": "text", "text": "done"}), "l2"));
    let frames = sse_until(&mut body, &mut buf, |e, d| e == "block" && d["seq"] == 32).await;
    let update = frames
        .iter()
        .find(|(e, d)| e == "update" && d["seq"] == 31)
        .expect("an update of the thinking block");
    assert!(update.1.get("usage").is_none());
    let (_, done) = frames.last().unwrap();
    assert_eq!(done["usage"]["output"], 4);

    // The agent stops: the session is no longer the active one.
    let id = a["id"].as_str().unwrap();
    td.post_empty(&format!("/api/agents/{id}/stop")).await.ok();
    let frames = sse_until(&mut body, &mut buf, |e, d| {
        e == "status" && d["status"] == "ended"
    })
    .await;
    assert_eq!(frames.last().unwrap().1["active"], false);
}

#[tokio::test]
async fn another_project_cannot_read_a_transcript() {
    let td = TestDaemon::start().await;
    let (a, s) = started(&td, "p", "w").await;
    std::fs::write(session_file(&td, &a, &s), fixture()).unwrap();
    td.project("q").await;
    let other = td.agent("q", "o", json!({})).await;
    let mate = td.agent("p", "m", json!({})).await;
    let url = transcript_url(&a, &s);
    let r = td.get_as(other["id"].as_str().unwrap(), &url).await;
    r.err(403, "forbidden");
    let r = td
        .get_as(
            other["id"].as_str().unwrap(),
            &format!("/api/agents/{}/sessions", a["id"].as_str().unwrap()),
        )
        .await;
    r.err(403, "forbidden");
    let r = td
        .get_as(other["id"].as_str().unwrap(), &format!("{url}/stream"))
        .await;
    r.err(403, "forbidden");
    // A member of the project reads it.
    let r = td.get_as(mate["id"].as_str().unwrap(), &url).await;
    assert_eq!(r.ok()["blocks"].as_array().unwrap().len(), 30);
}

/// Emit `session_started` for the agent's current session, as the pump
/// would, with a path the harness reported.
async fn report_path(td: &TestDaemon, a: &Value, s: &Value, path: &str) {
    let (id, name, project, sid, hs, path) = (
        a["id"].as_str().unwrap().to_owned(),
        a["name"].as_str().unwrap().to_owned(),
        a["project"].as_str().unwrap().to_owned(),
        s["id"].as_str().unwrap().to_owned(),
        s["harness_session_id"].as_str().unwrap().to_owned(),
        path.to_owned(),
    );
    td.daemon
        .as_ref()
        .unwrap()
        .state
        .db
        .write("system", move |t| {
            t.emit(
                EventDraft::new(
                    "session_started",
                    "test",
                    json!({"harness_session": hs, "transcript_path": path}),
                )
                .agent(id, name)
                .project(project)
                .session(Some(sid)),
            )
        })
        .await
        .unwrap();
}

#[tokio::test]
async fn a_path_outside_the_agent_dirs_is_refused() {
    let td = TestDaemon::start().await;
    let (a, s) = started(&td, "p", "w").await;
    let (b, bs) = started(&td, "p", "x").await;
    let hs = s["harness_session_id"].as_str().unwrap();
    // A file with the right name in another agent's own dir.
    let theirs = common::claude_project_dir(&td.state, b["id"].as_str().unwrap())
        .join(format!("{hs}.jsonl"));
    std::fs::write(&theirs, fixture()).unwrap();
    report_path(&td, &a, &s, theirs.to_str().unwrap()).await;
    let r = td.get(&transcript_url(&a, &s)).await;
    r.err(403, "forbidden");
    assert!(
        r.body["error"]["message"]
            .as_str()
            .unwrap()
            .contains("outside the agent's own dirs")
    );
    // A file outside every agent dir.
    let outside = td.dir.path().join(format!("{hs}.jsonl"));
    std::fs::write(&outside, fixture()).unwrap();
    report_path(&td, &a, &s, outside.to_str().unwrap()).await;
    td.get(&transcript_url(&a, &s)).await.err(403, "forbidden");
    // A symlink in the agent's own dir to that file.
    let own = session_file(&td, &a, &s);
    std::os::unix::fs::symlink(&outside, &own).unwrap();
    report_path(&td, &a, &s, own.to_str().unwrap()).await;
    td.get(&transcript_url(&a, &s)).await.err(403, "forbidden");
    let r = td
        .get(&format!(
            "/api/agents/{}/sessions",
            a["id"].as_str().unwrap()
        ))
        .await;
    assert_eq!(r.ok()[0]["transcript"], false);
    // The file of the other agent's own session is fine for it.
    let _ = bs;
}
