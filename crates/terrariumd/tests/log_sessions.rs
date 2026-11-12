//! The session links of the log rows (docs/api.md §4, §7, §9): an event,
//! a message, and an audit row of an agent carry `session_id` and
//! `session_label` when the reader may read that session. An event
//! records its session when it is written; an older row finds it by the
//! agent and the time. A session of a deleted agent links while its
//! archived file is there.
#![allow(clippy::unwrap_used, clippy::expect_used)]

use crate::common;

use std::time::Duration;

use common::TestDaemon;
use futures_util::StreamExt;
use serde_json::{Value, json};
use terrariumd::events::EventDraft;
use terrariumd::store::mail::{self, Address, Outgoing};

/// An agent in `project`, started, and its current session.
async fn started(td: &TestDaemon, project: &str, name: &str) -> (String, String) {
    let a = td.agent(project, name, json!({})).await;
    let id = a["id"].as_str().unwrap().to_owned();
    td.post_empty(&format!("/api/agents/{id}/start")).await.ok();
    td.wait_status(&id, "idle").await;
    for _ in 0..200 {
        let r = td.get(&format!("/api/agents/{id}/sessions")).await;
        if let Some(s) = r.ok()[0]["id"].as_str() {
            return (id, s.to_owned());
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    panic!("no session");
}

/// Run one write as `actor` on the daemon's store.
async fn write<T: Send + 'static>(
    td: &TestDaemon,
    actor: &str,
    f: impl for<'c> FnOnce(&mut terrariumd::db::Tx<'c>) -> terrariumd::error::ApiResult<T>
    + Send
    + 'static,
) -> T {
    td.daemon
        .as_ref()
        .unwrap()
        .state
        .db
        .write(actor, f)
        .await
        .unwrap()
}

fn find<'a>(rows: &'a Value, key: &str, value: &str) -> &'a Value {
    rows.as_array()
        .unwrap()
        .iter()
        .find(|r| r[key] == value)
        .unwrap_or_else(|| panic!("no row with {key} = {value}: {rows:#}"))
}

fn assert_link(row: &Value, session: &str) {
    assert_eq!(row["session_id"], session, "{row:#}");
    let label = row["session_label"].as_str().unwrap();
    assert!(
        label.starts_with("w · ") && label.ends_with(" UTC"),
        "{label}"
    );
}

fn assert_no_link(row: &Value) {
    assert!(row.get("session_id").is_none(), "{row:#}");
    assert!(row.get("session_label").is_none(), "{row:#}");
}

#[tokio::test]
async fn thread_messages_link_to_the_authors_session() {
    let td = TestDaemon::start().await;
    td.project("p").await;
    let (aid, sid) = started(&td, "p", "w").await;
    let thread = td.post("/api/threads", json!({
        "topic": "Session links", "scope": "p", "agent": "w", "text": "Operator message"
    })).await.ok().clone();
    let path = format!("/api/threads/{}", thread["id"].as_str().unwrap());
    td.post_as(&aid, &format!("{path}/messages"), json!({"text": "Agent message"})).await.ok();
    for response in [td.get(&path).await, td.get_as(&aid, &path).await] {
        let rows = &response.ok()["messages"];
        assert_no_link(&rows[0]);
        assert_eq!(rows[1]["author"], aid);
        assert_link(&rows[1], &sid);
    }
}

#[tokio::test]
async fn events_mail_and_audit_rows_link_to_the_session() {
    let td = TestDaemon::start().await;
    td.project("p").await;
    let (aid, sid) = started(&td, "p", "w").await;

    // A new event of the agent records the current session.
    let id = aid.clone();
    let ev = write(&td, "system", move |t| {
        t.emit(EventDraft::new("t.note", "note", json!({})).agent(id, "w"))
    })
    .await;
    assert_eq!(ev.session.as_deref(), Some(sid.as_str()));
    // A row from before the daemon recorded sessions: found by the time.
    let id = aid.clone();
    write(&td, "system", move |t| {
        t.execute(
            "INSERT INTO events (id, agent_id, agent_name, project, kind, payload, at)
             VALUES ('01OLDEVENT0000000000000000', ?1, 'w', 'p', 't.old', '{}', ?2)",
            rusqlite::params![id, terrariumd::db::now()],
        )?;
        Ok(())
    })
    .await;
    // A message and an audit row of the agent.
    td.post_as(
        &aid,
        "/api/mail",
        json!({"to": "operator", "subject": "hi", "body": "hello"}),
    )
    .await
    .ok();
    write(&td, &format!("agent:{aid}"), |t| {
        t.audit("t.action", "x", Some("p"), json!({}))
    })
    .await;

    for path in [
        "/api/events?kind=t.note",
        &format!("/api/agents/{aid}/events"),
    ] {
        let rows = td.get(path).await.ok().clone();
        assert_link(find(&rows, "kind", "t.note"), &sid);
    }
    let rows = td.get(&format!("/api/agents/{aid}/events")).await;
    let old = find(rows.ok(), "kind", "t.old");
    assert!(old.get("session").is_none(), "no recorded session: {old:#}");
    assert_link(old, &sid);
    let desc = td.get("/api/events?order=desc&kind=t.old").await;
    assert_link(&desc.ok()[0], &sid);

    let rows = td.get("/api/mail").await.ok().clone();
    let m = find(&rows, "subject", "hi");
    assert_link(m, &sid);
    let one = td
        .get(&format!("/api/mail/{}", m["id"].as_str().unwrap()))
        .await;
    assert_link(one.ok(), &sid);

    let rows = td.get("/api/audit?action=t.action").await;
    assert_link(&rows.ok()[0], &sid);
    // An operator row has no session.
    write(&td, "operator", |t| {
        t.audit("t.op", "x", Some("p"), json!({}))
    })
    .await;
    let rows = td.get("/api/audit?action=t.op").await;
    assert_no_link(&rows.ok()[0]);
}

#[tokio::test]
async fn a_stopped_agent_has_no_session_for_later_rows() {
    let td = TestDaemon::start().await;
    td.project("p").await;
    let (aid, _) = started(&td, "p", "w").await;
    td.post_empty(&format!("/api/agents/{aid}/stop")).await.ok();
    td.wait_status(&aid, "stopped").await;
    let id = aid.clone();
    write(&td, "system", move |t| {
        t.emit(EventDraft::new("t.after", "after", json!({})).agent(id, "w"))
    })
    .await;
    let rows = td.get("/api/events?kind=t.after").await;
    assert_no_link(&rows.ok()[0]);
}

#[tokio::test]
async fn a_reader_sees_only_the_sessions_it_may_read() {
    let td = TestDaemon::start().await;
    td.project("p").await;
    td.project("q").await;
    let (aid, sid) = started(&td, "p", "w").await;
    let other = td.agent("q", "o", json!({})).await;
    let other = other["id"].as_str().unwrap().to_owned();
    let admin = td.agent("q", "root", json!({"roles": ["sysadmin"]})).await;
    let admin = admin["id"].as_str().unwrap().to_owned();

    // An event of `w` in project q, and a message from `w` to `o`: `o`
    // reads both rows, but not the session of project p.
    let (id, to) = (aid.clone(), other.clone());
    write(&td, "system", move |t| {
        t.emit(
            EventDraft::new("t.cross", "cross", json!({}))
                .agent(id.clone(), "w")
                .project("q"),
        )?;
        let from = Address::Agent {
            id,
            name: "w".into(),
            project: "p".into(),
        };
        let to = Address::Agent {
            id: to,
            name: "o".into(),
            project: "q".into(),
        };
        mail::send(
            t,
            Outgoing {
                from: &from,
                to: &to,
                subject: "cross",
                body: "b",
                thread: None,
                in_reply_to: None,
            },
        )
    })
    .await;

    let rows = td.get_as(&other, "/api/events?kind=t.cross").await;
    assert_no_link(&rows.ok()[0]);
    let mail = td.get_as(&other, "/api/mail").await.ok().clone();
    let m = find(&mail, "subject", "cross");
    assert_no_link(m);
    let one = td
        .get_as(&other, &format!("/api/mail/{}", m["id"].as_str().unwrap()))
        .await;
    assert_no_link(one.ok());
    // The live stream follows the same rule.
    let mut ws = td
        .ws("/api/events?since=0&kind=t.cross", Some(&other))
        .await
        .unwrap();
    let frame = tokio::time::timeout(Duration::from_secs(5), ws.next())
        .await
        .unwrap()
        .unwrap()
        .unwrap();
    let frame: Value = serde_json::from_str(frame.to_text().unwrap()).unwrap();
    assert_eq!(frame["kind"], "t.cross");
    assert_no_link(&frame);

    // A sysadmin reads every project; the operator too.
    let rows = td.get_as(&admin, "/api/events?kind=t.cross").await;
    assert_link(&rows.ok()[0], &sid);
    let mut ws = td
        .ws("/api/events?since=0&kind=t.cross", None)
        .await
        .unwrap();
    let frame = tokio::time::timeout(Duration::from_secs(5), ws.next())
        .await
        .unwrap()
        .unwrap()
        .unwrap();
    let frame: Value = serde_json::from_str(frame.to_text().unwrap()).unwrap();
    assert_link(&frame, &sid);
}

#[tokio::test]
async fn a_deleted_agent_links_while_its_archive_is_there() {
    let td = TestDaemon::start().await;
    td.project("p").await;
    let (aid, sid) = started(&td, "p", "w").await;
    let id = aid.clone();
    write(&td, "system", move |t| {
        t.emit(EventDraft::new("t.note", "note", json!({})).agent(id, "w"))
    })
    .await;
    let r = td.delete(&format!("/api/agents/{aid}?confirm=true")).await;
    assert_eq!(r.status, 204, "{}", r.body);
    // The fake harness wrote no session file: no archive, no link.
    let rows = td.get("/api/events?kind=t.note").await;
    assert_no_link(&rows.ok()[0]);

    // An archived file: the link comes back, with the agent's name.
    let rel = format!("archive/agents/{aid}/claude/{sid}/x.jsonl");
    common::write(&td.state, &rel, "{}\n");
    let (s, r) = (sid.clone(), rel.clone());
    write(&td, "system", move |t| {
        t.execute(
            "UPDATE sessions SET archive = ?2 WHERE id = ?1",
            rusqlite::params![s, r],
        )?;
        Ok(())
    })
    .await;
    let rows = td.get("/api/events?kind=t.note").await;
    assert_link(&rows.ok()[0], &sid);

    // The prune removed it: no link.
    std::fs::remove_file(td.state.join(&rel)).unwrap();
    let rows = td.get("/api/events?kind=t.note").await;
    assert_no_link(&rows.ok()[0]);
}
