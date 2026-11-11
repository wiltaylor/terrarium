//! Notices and the Inbox of the operator (PLAN §12e, docs/api.md §6.8,
//! §6.9, §7.4), with the fake supervisor. No harness and no model turn
//! runs.
#![allow(clippy::unwrap_used, clippy::expect_used)]

use crate::common;

use std::time::Duration;

use common::TestDaemon;
use serde_json::{Value, json};
use terrariumd::waits::Tuning;

fn id(v: &Value) -> String {
    v["id"].as_str().unwrap().to_owned()
}

/// A project `p` with a lead and two workers, and a sysadmin in `ops`.
/// All started, all idle.
struct World {
    td: TestDaemon,
    lead: String,
    w1: String,
    w2: String,
    sys: String,
}

async fn setup() -> World {
    let td = TestDaemon::start().await;
    let state = td.daemon.as_ref().unwrap().state.clone();
    state.waits.set_tuning(Tuning {
        tick: Duration::from_millis(20),
        settle: Duration::from_millis(300),
        wake_debounce: Duration::from_millis(100),
        ..Tuning::default()
    });
    for p in ["p", "ops"] {
        td.project(p).await;
    }
    let lead = id(&td
        .agent("p", "lead", json!({"roles": ["project_admin"]}))
        .await);
    let w1 = id(&td.agent("p", "w1", json!({})).await);
    let w2 = id(&td.agent("p", "w2", json!({})).await);
    let sys = id(&td.agent("ops", "sys", json!({"roles": ["sysadmin"]})).await);
    for a in [&lead, &w1, &w2, &sys] {
        td.post_empty(&format!("/api/agents/{a}/start")).await.ok();
        td.wait_status(a, "idle").await;
    }
    World {
        td,
        lead,
        w1,
        w2,
        sys,
    }
}

async fn decision_card(td: &TestDaemon) -> Value {
    td.post("/api/projects/p/cards", json!({"title": "Decision"}))
        .await.ok()["id"].clone()
}

async fn notify(td: &TestDaemon, agent: &str, body: Value) -> Value {
    let mut body = body;
    body["kind"] = json!("notice");
    if body.get("card").is_none() {
        let project = td.get(&format!("/api/agents/{agent}")).await.body["project"]
            .as_str()
            .unwrap()
            .to_owned();
        body["card"] = td
            .post(
                &format!("/api/projects/{project}/cards"),
                json!({"title": "Requested work"}),
            )
            .await
            .ok()["id"]
            .clone();
    }
    let r = td
        .post_as(agent, &format!("/api/agents/{agent}/questions"), body)
        .await;
    assert_eq!(r.status, 201, "{}", r.body);
    r.body
}

async fn until<F: std::future::Future<Output = bool>>(what: &str, mut f: impl FnMut() -> F) {
    for _ in 0..400 {
        if f().await {
            return;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    panic!("timed out waiting for {what}");
}

/// The mail to `agent` from `from`.
async fn mail(td: &TestDaemon, agent: &str, from: &str) -> Vec<Value> {
    td.get(&format!("/api/mail?to={agent}&from={from}"))
        .await
        .ok()
        .as_array()
        .unwrap()
        .clone()
}

async fn turns(td: &TestDaemon, agent: &str) -> usize {
    td.get(&format!("/api/agents/{agent}/events?kind=turn_start"))
        .await
        .ok()
        .as_array()
        .unwrap()
        .len()
}

/// The `inbox` event changes of one item, oldest first.
async fn inbox_changes(td: &TestDaemon, item: &str) -> Vec<String> {
    td.get("/api/events?kind=inbox&limit=200")
        .await
        .ok()
        .as_array()
        .unwrap()
        .iter()
        .filter(|e| e["data"]["item"] == item)
        .map(|e| e["data"]["change"].as_str().unwrap().to_owned())
        .collect()
}

async fn inbox(td: &TestDaemon, query: &str) -> (Vec<Value>, Value) {
    let r = td.get(&format!("/api/inbox{query}")).await;
    let rows = r.ok().as_array().unwrap().clone();
    // The headers count the open items of the project filter.
    let project = query
        .trim_start_matches('?')
        .split('&')
        .find(|kv| kv.starts_with("project="))
        .map(|kv| format!("?{kv}"))
        .unwrap_or_default();
    let scoped = td.get(&format!("/api/inbox/counts{project}")).await;
    assert_eq!(r.headers["x-inbox-open"], scoped.ok()["open"].to_string());
    let counts = td.get("/api/inbox/counts").await.ok().clone();
    (rows, counts)
}

#[tokio::test]
async fn a_lead_sends_a_notice_and_the_rights_follow_the_roles() {
    let World {
        td, lead, w1, sys, ..
    } = setup().await;
    let n = notify(
        &td,
        &lead,
        json!({"text": "v2 is deployed.", "context": "**All** checks green."}),
    )
    .await;
    assert_eq!(n["kind"], "notice");
    assert_eq!(n["status"], "open");
    assert_eq!(n["options"], json!([]));
    let nid = id(&n);

    // A notice has no options.
    let r = td
        .post_as(
            &lead,
            &format!("/api/agents/{lead}/questions"),
            json!({"kind": "notice", "text": "x", "options": ["a"]}),
        )
        .await;
    r.err(422, "invalid");
    // A worker cannot send one: the error sends it to its lead.
    let r = td
        .post_as(
            &w1,
            &format!("/api/agents/{w1}/questions"),
            json!({"kind": "notice", "text": "done"}),
        )
        .await;
    r.err(403, "forbidden");
    assert!(
        r.body["error"]["message"]
            .as_str()
            .unwrap()
            .contains("terrarium mail send <lead>"),
        "{}",
        r.body
    );
    // The same notice again: a conflict that names the open one. The same
    // text as a question is not a copy.
    let r = td
        .post_as(
            &lead,
            &format!("/api/agents/{lead}/questions"),
            json!({"kind": "notice", "text": "V2 is  deployed.", "card": n["card"]}),
        )
        .await;
    r.err(409, "conflict");
    assert_eq!(r.body["error"]["details"]["question"], nid.as_str());
    let q = td
        .post_as(
            &lead,
            &format!("/api/agents/{lead}/questions"),
            json!({"text": "v2 is deployed.", "card": n["card"]}),
        )
        .await;
    assert_eq!(q.status, 201, "{}", q.body);
    assert_eq!(q.body["kind"], "question");
    let qid = id(&q.body);

    // The wrong verb for the kind: 409.
    td.post(
        &format!("/api/questions/{nid}/answer"),
        json!({"answer": "ok"}),
    )
    .await
    .err(409, "conflict");
    td.post(&format!("/api/questions/{qid}/ack"), json!({}))
        .await
        .err(409, "conflict");
    // The owner does not acknowledge its own notice; a worker cannot.
    td.post_as(&lead, &format!("/api/questions/{nid}/ack"), json!({}))
        .await
        .err(403, "forbidden");
    td.post_as(&w1, &format!("/api/questions/{nid}/ack"), json!({}))
        .await
        .err(404, "not_found");

    // The kind filter of the lists.
    let notices = td.get("/api/questions?kind=notice").await;
    let rows = notices.ok().as_array().unwrap().clone();
    assert_eq!(rows.len(), 1);
    assert_eq!(notices.headers["x-open-questions"], "1");
    let all = td
        .get_as(&lead, &format!("/api/agents/{lead}/questions"))
        .await;
    assert_eq!(all.ok().as_array().unwrap().len(), 2);

    // An update keeps the notice without options.
    td.patch_as(
        &lead,
        &format!("/api/questions/{nid}"),
        json!({"options": ["a", "b"]}),
    )
    .await
    .err(422, "invalid");

    // A sysadmin acknowledges the notice of another agent; a second
    // acknowledgement is a conflict.
    let r = td
        .post_as(
            &sys,
            &format!("/api/questions/{nid}/ack"),
            json!({"text": "Thanks."}),
        )
        .await;
    let done = r.ok();
    assert_eq!(done["status"], "acknowledged");
    assert_eq!(done["answer"], "Thanks.");
    assert_eq!(done["answered_by_name"], "ops/sys");
    td.post_empty(&format!("/api/questions/{nid}/ack"))
        .await
        .err(409, "conflict");

    // A retract of a notice by its owner: no mail.
    let n2 = notify(&td, &lead, json!({"text": "Cache warm."})).await;
    let r = td
        .post_as(
            &lead,
            &format!("/api/questions/{}/retract", id(&n2)),
            json!({}),
        )
        .await;
    assert_eq!(r.ok()["status"], "retracted");

    // The audit and the events.
    let audit = td.get("/api/audit?limit=200").await;
    let actions: Vec<String> = audit
        .ok()
        .as_array()
        .unwrap()
        .iter()
        .map(|r| r["action"].as_str().unwrap().to_owned())
        .collect();
    assert!(actions.contains(&"question.ack".to_owned()), "{actions:?}");
    let ev = td.get("/api/events?kind=question&limit=100").await;
    let acked = ev
        .ok()
        .as_array()
        .unwrap()
        .iter()
        .any(|e| e["data"]["change"] == "acknowledged" && e["data"]["kind"] == "notice");
    assert!(acked);
    let ev = td.get("/api/events?kind=inbox&limit=100").await;
    let items: Vec<(String, String)> = ev
        .ok()
        .as_array()
        .unwrap()
        .iter()
        .map(|e| {
            (
                e["data"]["item"].as_str().unwrap().to_owned(),
                e["data"]["change"].as_str().unwrap().to_owned(),
            )
        })
        .collect();
    assert!(
        items.contains(&(format!("notice:{nid}"), "opened".into())),
        "{items:?}"
    );
    assert!(
        items.contains(&(format!("notice:{nid}"), "closed".into())),
        "{items:?}"
    );
}

/// The acknowledgement is always one mail on the thread of the notice,
/// and the mail wakes the agent.
#[tokio::test]
async fn the_ack_mail_goes_on_the_thread_and_wakes_the_agent() {
    let World { td, lead, .. } = setup().await;
    td.sup.set_mail_push(true);
    let n = notify(&td, &lead, json!({"text": "Release 1.4 is out."})).await;
    let before = turns(&td, &lead).await;
    td.post(
        &format!("/api/questions/{}/ack", id(&n)),
        json!({"text": "Nice work. Start 1.5."}),
    )
    .await
    .ok();
    let m = mail(&td, &lead, "operator").await;
    assert_eq!(m.len(), 1, "{m:?}");
    assert_eq!(m[0]["thread_id"], n["thread_id"]);
    assert_eq!(m[0]["subject"], "Acknowledged: Release 1.4 is out.");
    let body = m[0]["body"].as_str().unwrap();
    assert!(body.contains("> Release 1.4 is out."), "{body}");
    assert!(body.contains("> Nice work. Start 1.5."), "{body}");
    until("the push turn", || async {
        turns(&td, &lead).await > before
    })
    .await;

    // No text: still one mail, that says so.
    let n2 = notify(&td, &lead, json!({"text": "Nightly backup done."})).await;
    td.post_empty(&format!("/api/questions/{}/ack", id(&n2)))
        .await
        .ok();
    let m = mail(&td, &lead, "operator").await;
    assert_eq!(m.len(), 2, "{m:?}");
    assert!(
        m[1]["body"].as_str().unwrap().contains("sent no reply"),
        "{}",
        m[1]["body"]
    );
}

/// `wait: true` on a notice registers a `question` wait. The
/// acknowledgement resolves it, and the agent gets one mail, not two.
#[tokio::test]
async fn a_notice_wait_resolves_with_one_mail() {
    let World { td, lead, .. } = setup().await;
    let n = notify(
        &td,
        &lead,
        json!({"text": "Migration starts at 18:00.", "wait": true}),
    )
    .await;
    let w = n["wait"].clone();
    assert_eq!(w["kind"], "question");
    assert_eq!(w["summary"], "notice Migration starts at 18:00.");
    td.wait_status(&lead, "waiting").await;
    td.post(
        &format!("/api/questions/{}/ack", id(&n)),
        json!({"text": "Go."}),
    )
    .await
    .ok();
    let done = td.get(&format!("/api/waits/{}", id(&w))).await.body;
    assert_eq!(done["state"], "resolved", "{done}");
    assert_eq!(done["result"]["outcome"], "acknowledged");
    assert_eq!(done["result"]["answer"], "Go.");
    assert!(done["notified_at"].is_string(), "{done}");
    td.wait_status(&lead, "idle").await;
    tokio::time::sleep(Duration::from_millis(400)).await;
    assert!(mail(&td, &lead, "system").await.is_empty());
    let m = mail(&td, &lead, "operator").await;
    assert_eq!(m.len(), 1, "{m:?}");
    assert!(
        m[0]["body"]
            .as_str()
            .unwrap()
            .contains("This mail is its wake mail"),
        "{}",
        m[0]["body"]
    );

    // A retract by the agent itself ends a wait with no mail.
    let n2 = notify(&td, &lead, json!({"text": "Disk at 80%.", "wait": true})).await;
    td.wait_status(&lead, "waiting").await;
    td.post_as(
        &lead,
        &format!("/api/questions/{}/retract", id(&n2)),
        json!({}),
    )
    .await
    .ok();
    let done = td
        .get(&format!("/api/waits/{}", id(&n2["wait"])))
        .await
        .body;
    assert_eq!(done["result"]["outcome"], "retracted");
    td.wait_status(&lead, "idle").await;
    assert_eq!(mail(&td, &lead, "operator").await.len(), 1);
}

/// Send 7 agent-to-agent messages on one thread: the 7th is held.
async fn chatty_thread(td: &TestDaemon, from: &str, to: &str) -> (String, Value) {
    let mut thread: Option<String> = None;
    let mut last = Value::Null;
    for i in 1..=7 {
        let mut m = json!({"to": to, "subject": "loop", "body": format!("msg {i}")});
        if let Some(t) = &thread {
            m["thread_id"] = json!(t);
        }
        let r = td.post_as(from, "/api/mail", m).await;
        assert_eq!(r.status, 201, "{}", r.body);
        thread = Some(r.body["thread_id"].as_str().unwrap().to_owned());
        last = r.body.clone();
    }
    assert_eq!(last["held"], true, "{last}");
    (thread.unwrap(), last)
}

/// The Inbox mixes the kinds, urgent first, with the counts; the filters
/// work; an agent cannot read it.
#[tokio::test]
async fn the_inbox_mixes_the_kinds_with_the_counts() {
    let World {
        td, lead, w1, w2, ..
    } = setup().await;
    let (rows, counts) = inbox(&td, "").await;
    assert!(rows.is_empty(), "{rows:?}");
    assert_eq!(counts["open"], 0);

    let q = td
        .post_as(
            &lead,
            &format!("/api/agents/{lead}/questions"),
            json!({"text": "Ship?", "options": ["yes", "no"], "card": decision_card(&td).await}),
        )
        .await
        .ok()
        .clone();
    let n = notify(
        &td,
        &lead,
        json!({"text": "Prod is down.", "priority": "urgent"}),
    )
    .await;
    let m = td
        .post_as(
            &lead,
            "/api/mail",
            json!({"to": "operator", "subject": "FYI", "body": "a long story"}),
        )
        .await
        .ok()
        .clone();
    let (thread, _) = chatty_thread(&td, &w1, "p/w2").await;

    let (rows, counts) = inbox(&td, "").await;
    let ids: Vec<&str> = rows.iter().map(|r| r["id"].as_str().unwrap()).collect();
    assert_eq!(ids.len(), 4, "{rows:#?}");
    // Urgent first.
    assert_eq!(ids[0], format!("notice:{}", id(&n)));
    assert!(ids.contains(&format!("question:{}", id(&q)).as_str()));
    assert!(ids.contains(&format!("mail:{}", id(&m)).as_str()));
    assert!(ids.contains(&format!("thread:{thread}").as_str()));
    assert_eq!(
        counts,
        json!({"open": 4, "urgent": 1, "critical": 0, "question": 1, "notice": 1, "mail": 1, "system": 1, "watch": 0,
               "thread": 0, "thread_urgent": 0, "thread_critical": 0})
    );
    // The pause notice to the operator is the system item, not a mail item.
    let sys = rows.iter().find(|r| r["kind"] == "system").unwrap();
    assert_eq!(sys["project"], "p");
    assert_eq!(sys["system"]["paused"], true);
    assert_eq!(sys["system"]["held"].as_array().unwrap().len(), 1);
    assert_eq!(sys["system"]["limit"], 6);
    assert_eq!(sys["system"]["agent_run"], 7);
    let mail_item = rows.iter().find(|r| r["kind"] == "mail").unwrap();
    assert_eq!(mail_item["project"], "p");
    assert_eq!(mail_item["message"]["body"], "a long story");

    // The filters.
    let (only, _) = inbox(&td, "?kind=notice").await;
    assert_eq!(only.len(), 1);
    let (other, _) = inbox(&td, "?project=ops").await;
    assert!(other.is_empty(), "{other:?}");
    td.get("/api/inbox?kind=nope").await.err(422, "invalid");
    td.get("/api/inbox?status=nope").await.err(422, "invalid");
    td.get_as(&lead, "/api/inbox").await.err(403, "forbidden");
    td.get_as(&lead, "/api/inbox/counts")
        .await
        .err(403, "forbidden");
    let _ = w2;

    // Close the question and the notice: they go to the done list.
    td.post(
        &format!("/api/questions/{}/answer", id(&q)),
        json!({"option": "yes"}),
    )
    .await
    .ok();
    td.post_empty(&format!("/api/questions/{}/ack", id(&n)))
        .await
        .ok();
    let (rows, counts) = inbox(&td, "").await;
    assert_eq!(rows.len(), 2, "{rows:#?}");
    assert_eq!(counts["urgent"], 0);
    let (done, _) = inbox(&td, "?status=done").await;
    let kinds: Vec<&str> = done.iter().map(|r| r["kind"].as_str().unwrap()).collect();
    assert_eq!(kinds.len(), 2, "{done:#?}");
    assert!(kinds.contains(&"question") && kinds.contains(&"notice"));
    assert!(done.iter().all(|r| r["open"] == false));
    let (all, _) = inbox(&td, "?status=all").await;
    assert_eq!(all.len(), 4);
    assert_eq!(all[0]["open"], true);
}

/// The priority `critical`: first in the Inbox and in the agent's list,
/// counted as critical and as urgent, a lead only, and gone from the
/// counts after the acknowledgement.
#[tokio::test]
async fn a_critical_notice_goes_first_and_counts() {
    let World { td, lead, w1, .. } = setup().await;
    let normal = td
        .post_as(
            &lead,
            &format!("/api/agents/{lead}/questions"),
            json!({"text": "Ship?", "card": decision_card(&td).await}),
        )
        .await
        .ok()
        .clone();
    let urgent = notify(
        &td,
        &lead,
        json!({"text": "CI is red.", "priority": "urgent"}),
    )
    .await;
    let critical = notify(
        &td,
        &lead,
        json!({"text": "Prod is down.", "priority": "critical"}),
    )
    .await;
    assert_eq!(critical["priority"], "critical");
    // A worker sends no notice, critical or not.
    td.post_as(
        &w1,
        &format!("/api/agents/{w1}/questions"),
        json!({"kind": "notice", "text": "x", "priority": "critical"}),
    )
    .await
    .err(403, "forbidden");
    td.post_as(
        &lead,
        &format!("/api/agents/{lead}/questions"),
        json!({"kind": "notice", "text": "x", "priority": "panic"}),
    )
    .await
    .err(400, "bad_request");

    let (rows, counts) = inbox(&td, "").await;
    let ids: Vec<&str> = rows.iter().map(|r| r["id"].as_str().unwrap()).collect();
    assert_eq!(
        ids,
        vec![
            format!("notice:{}", id(&critical)),
            format!("notice:{}", id(&urgent)),
            format!("question:{}", id(&normal)),
        ],
        "{rows:#?}"
    );
    assert_eq!(
        (rows[0]["critical"].clone(), rows[0]["urgent"].clone()),
        (json!(true), json!(true))
    );
    assert_eq!(
        (rows[1]["critical"].clone(), rows[1]["urgent"].clone()),
        (json!(false), json!(true))
    );
    assert_eq!(rows[2]["critical"], false);
    assert_eq!(counts["critical"], 1);
    assert_eq!(counts["urgent"], 2);
    let r = td.get("/api/inbox").await;
    assert_eq!(r.headers["x-inbox-critical"], "1");

    // The list of all questions: critical first too, with the header.
    let all = td.get("/api/questions").await;
    assert_eq!(all.headers["x-critical-questions"], "1");
    assert_eq!(all.headers["x-urgent-questions"], "2");
    assert_eq!(all.ok()[0]["id"], critical["id"]);

    // The lead raises a priority; the operator acknowledges both.
    td.patch_as(
        &lead,
        &format!("/api/questions/{}", id(&urgent)),
        json!({"priority": "critical"}),
    )
    .await
    .ok();
    let (_, counts) = inbox(&td, "").await;
    assert_eq!(counts["critical"], 2);
    for n in [&critical, &urgent] {
        td.post_empty(&format!("/api/questions/{}/ack", id(n)))
            .await
            .ok();
    }
    let (rows, counts) = inbox(&td, "").await;
    assert_eq!(rows.len(), 1);
    assert_eq!(
        (counts["critical"].clone(), counts["urgent"].clone()),
        (json!(0), json!(0))
    );
}

/// The acknowledgement of a mail to the operator, with a reply on its
/// thread.
#[tokio::test]
async fn an_operator_mail_ack_replies_on_the_thread() {
    let World { td, lead, .. } = setup().await;
    let m = td
        .post_as(
            &lead,
            "/api/mail",
            json!({"to": "operator", "subject": "Budget", "body": "We need more."}),
        )
        .await
        .ok()
        .clone();
    let mid = id(&m);
    let r = td
        .post(
            &format!("/api/inbox/mail/{mid}/ack"),
            json!({"reply": "Approved: 20 more."}),
        )
        .await;
    let out = r.ok();
    assert_eq!(out["message"]["state"], "replied");
    let replies = out["replies"].as_array().unwrap();
    assert_eq!(replies.len(), 1);
    let got = mail(&td, &lead, "operator").await;
    assert_eq!(got.len(), 1, "{got:?}");
    assert_eq!(got[0]["thread_id"], m["thread_id"]);
    assert_eq!(got[0]["subject"], "Re: Budget");
    assert_eq!(got[0]["body"], "Approved: 20 more.");
    // Acknowledged: not open, and a second ack is a conflict.
    let (rows, _) = inbox(&td, "").await;
    assert!(rows.is_empty(), "{rows:?}");
    td.post_empty(&format!("/api/inbox/mail/{mid}/ack"))
        .await
        .err(409, "conflict");
    let (done, _) = inbox(&td, "?status=done&kind=mail").await;
    assert_eq!(done.len(), 1);
    // With no reply: no mail goes out.
    let m2 = td
        .post_as(
            &lead,
            "/api/mail",
            json!({"to": "operator", "subject": "Note", "body": "Just so you know."}),
        )
        .await
        .ok()
        .clone();
    let out = td
        .post_empty(&format!("/api/inbox/mail/{}/ack", id(&m2)))
        .await;
    assert_eq!(out.ok()["message"]["state"], "read");
    assert!(out.body["replies"].as_array().unwrap().is_empty());
    assert_eq!(mail(&td, &lead, "operator").await.len(), 1);
    // Mail that is not to the operator: not found. An agent: 403.
    let to_agent = td
        .post("/api/mail", json!({"to": "p/lead", "body": "hi"}))
        .await
        .ok()
        .clone();
    td.post_empty(&format!("/api/inbox/mail/{}/ack", id(&to_agent)))
        .await
        .err(404, "not_found");
    td.post_empty_as(&lead, &format!("/api/inbox/mail/{}/ack", id(&m2)))
        .await
        .err(403, "forbidden");
}

/// GitHub #25: operator mail that is read or replied outside the Inbox
/// (the Mail page, the CLI) closes its Inbox item there too: the closed
/// event fires and the counts drop, so the badge follows without a reload.
/// The later ack still answers 409, which the dashboard treats as success.
#[tokio::test]
async fn mail_handled_outside_the_inbox_closes_its_item() {
    let World { td, lead, .. } = setup().await;

    // The operator reads the mail on the Mail page, not in the Inbox.
    let m = td
        .post_as(
            &lead,
            "/api/mail",
            json!({"to": "operator", "subject": "Budget", "body": "We need more."}),
        )
        .await
        .ok()
        .clone();
    let mid = id(&m);
    let (rows, counts) = inbox(&td, "?kind=mail").await;
    assert_eq!(rows.len(), 1, "{rows:?}");
    assert_eq!(counts["mail"], 1);
    let r = td.post_empty(&format!("/api/mail/{mid}/read")).await;
    assert_eq!(r.ok()["state"], "read");
    let (rows, counts) = inbox(&td, "?kind=mail").await;
    assert!(rows.is_empty(), "{rows:?}");
    assert_eq!(counts["mail"], 0);
    let changes = inbox_changes(&td, &format!("mail:{mid}")).await;
    assert!(
        changes.contains(&"opened".to_owned()) && changes.contains(&"closed".to_owned()),
        "{changes:?}"
    );
    // The ack of closed mail still answers 409.
    td.post_empty(&format!("/api/inbox/mail/{mid}/ack"))
        .await
        .err(409, "conflict");

    // A reply outside the Inbox closes its item too, with the project.
    let m2 = td
        .post_as(
            &lead,
            "/api/mail",
            json!({"to": "operator", "subject": "Note", "body": "Just so you know."}),
        )
        .await
        .ok()
        .clone();
    let mid2 = id(&m2);
    let r = td
        .post(
            &format!("/api/mail/{mid2}/reply"),
            json!({"body": "Thanks."}),
        )
        .await;
    let replies = r.ok().clone();
    assert_eq!(replies.as_array().unwrap().len(), 1);
    let (rows, counts) = inbox(&td, "?kind=mail").await;
    assert!(rows.is_empty(), "{rows:?}");
    assert_eq!(counts["mail"], 0);
    let changes = inbox_changes(&td, &format!("mail:{mid2}")).await;
    assert!(
        changes.contains(&"opened".to_owned()) && changes.contains(&"closed".to_owned()),
        "{changes:?}"
    );
    let ev = td.get("/api/events?kind=inbox&limit=200").await;
    let closed = ev
        .ok()
        .as_array()
        .unwrap()
        .iter()
        .find(|e| e["data"]["item"] == format!("mail:{mid2}") && e["data"]["change"] == "closed")
        .unwrap();
    assert_eq!(closed["data"]["project"], "p");

    // The agent reads its answer: no inbox event for mail that is not to
    // the operator.
    let rid = id(&replies[0]);
    td.post_empty_as(&lead, &format!("/api/mail/{rid}/read"))
        .await
        .ok();
    let changes = inbox_changes(&td, &format!("mail:{rid}")).await;
    assert!(changes.is_empty(), "{changes:?}");
}

/// A held message and a paused thread are a system item: a release
/// delivers one held message, and a resume ends the pause.
#[tokio::test]
async fn a_paused_thread_is_a_system_item_until_the_resume() {
    let World {
        td, lead, w1, w2, ..
    } = setup().await;
    td.sup.set_mail_push(true);
    let (thread, held) = chatty_thread(&td, &w1, "p/w2").await;
    let hid = id(&held);
    let (rows, _) = inbox(&td, "?kind=system").await;
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0]["id"], format!("thread:{thread}"));

    // Only the operator acts on the loop guard.
    td.post_empty_as(&lead, &format!("/api/mail/{hid}/release"))
        .await
        .err(403, "forbidden");
    td.post_empty_as(&lead, &format!("/api/mail/threads/{thread}/resume"))
        .await
        .err(403, "forbidden");

    // A release: the message is queued again, and the push delivers it.
    // The thread stays paused, so the item stays.
    let before = turns(&td, &w2).await;
    let r = td.post_empty(&format!("/api/mail/{hid}/release")).await;
    let rel = r.ok();
    assert!(rel["released_at"].is_string(), "{rel}");
    assert_eq!(rel["released_by"], "operator");
    until("the pointer of the released mail", || async {
        turns(&td, &w2).await > before
    })
    .await;
    td.post_empty(&format!("/api/mail/{hid}/release"))
        .await
        .err(409, "conflict");
    let (rows, counts) = inbox(&td, "?kind=system").await;
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0]["system"]["paused"], true);
    assert!(rows[0]["system"]["held"].as_array().unwrap().is_empty());
    assert_eq!(counts["system"], 1);

    // An 8th message is held again.
    td.wait_status(&w2, "idle").await;
    let r = td
        .post_as(
            &w1,
            "/api/mail",
            json!({"to": "p/w2", "body": "msg 8", "thread_id": thread}),
        )
        .await;
    assert_eq!(r.ok()["held"], true);

    // A resume with a note: the note goes to both workers, the held
    // message is released, the item goes, and new mail is not held.
    let r = td
        .post(
            &format!("/api/mail/threads/{thread}/resume"),
            json!({"note": "Stop the loop. w1 decides."}),
        )
        .await;
    let out = r.ok();
    assert_eq!(out["released"].as_array().unwrap().len(), 1);
    assert_eq!(out["note"].as_array().unwrap().len(), 2);
    for w in [&w1, &w2] {
        let notes = mail(&td, w, "operator").await;
        assert_eq!(notes.len(), 1, "{notes:?}");
        assert_eq!(notes[0]["thread_id"], thread.as_str());
    }
    let (rows, counts) = inbox(&td, "").await;
    assert!(rows.is_empty(), "{rows:#?}");
    assert_eq!(counts["system"], 0);
    td.post_empty(&format!("/api/mail/threads/{thread}/resume"))
        .await
        .err(409, "conflict");
    let r = td
        .post_as(
            &w1,
            "/api/mail",
            json!({"to": "p/w2", "body": "ok", "thread_id": thread}),
        )
        .await;
    assert_eq!(r.ok()["held"], false);
    // The pause notice is not open any more.
    let notices = td
        .get(&format!(
            "/api/mail?to=operator&from=system&thread={thread}"
        ))
        .await;
    assert!(
        notices
            .ok()
            .as_array()
            .unwrap()
            .iter()
            .all(|m| m["state"] == "read")
    );
}

/// A resume with no note sends no mail and still ends the pause.
#[tokio::test]
async fn a_resume_without_a_note_sends_no_mail() {
    let World { td, w1, w2, .. } = setup().await;
    let (thread, _) = chatty_thread(&td, &w1, "p/w2").await;
    let out = td
        .post_empty(&format!("/api/mail/threads/{thread}/resume"))
        .await;
    assert!(out.ok()["note"].as_array().unwrap().is_empty());
    assert!(mail(&td, &w1, "operator").await.is_empty());
    assert!(mail(&td, &w2, "operator").await.is_empty());
    let r = td
        .post_as(
            &w1,
            "/api/mail",
            json!({"to": "p/w2", "body": "next", "thread_id": thread}),
        )
        .await;
    assert_eq!(r.ok()["held"], false);
    td.post_empty("/api/mail/threads/nope/resume")
        .await
        .err(404, "not_found");
}

/// The mail log filters: project, time, and text.
#[tokio::test]
async fn the_mail_log_filters_by_project_time_and_text() {
    let World { td, lead, sys, .. } = setup().await;
    td.post_as(
        &lead,
        "/api/mail",
        json!({"to": "operator", "subject": "Alpha", "body": "the red fox"}),
    )
    .await
    .ok();
    td.post_as(
        &sys,
        "/api/mail",
        json!({"to": "operator", "subject": "Beta", "body": "the blue whale"}),
    )
    .await
    .ok();
    let subjects = |q: &str| {
        let td = &td;
        let q = q.to_owned();
        async move {
            td.get(&format!("/api/mail?{q}"))
                .await
                .ok()
                .as_array()
                .unwrap()
                .iter()
                .map(|m| m["subject"].as_str().unwrap().to_owned())
                .collect::<Vec<_>>()
        }
    };
    assert_eq!(subjects("project=p").await, ["Alpha"]);
    assert_eq!(subjects("project=ops").await, ["Beta"]);
    assert_eq!(subjects("q=WHALE").await, ["Beta"]);
    assert_eq!(subjects("q=alpha").await, ["Alpha"]);
    assert!(subjects("since=2999-01-01T00:00:00Z").await.is_empty());
    assert_eq!(subjects("until=2999-01-01T00:00:00Z").await.len(), 2);
    td.get("/api/mail?since=yesterday")
        .await
        .err(422, "invalid");
}

/// The recap of a new session lists the open notices and the
/// acknowledgements since the last turn.
#[tokio::test]
async fn the_recap_lists_open_and_acknowledged_notices() {
    let World { td, lead, .. } = setup().await;
    let state = td.daemon.as_ref().unwrap().state.clone();
    let recap = |agent: String| {
        let state = state.clone();
        async move {
            state
                .db
                .read(move |c| terrariumd::store::questions::recap(c, &agent))
                .await
                .unwrap()
        }
    };
    let open = notify(&td, &lead, json!({"text": "Deploy at noon."})).await;
    let acked = notify(&td, &lead, json!({"text": "Tests green."})).await;
    td.post(
        &format!("/api/questions/{}/ack", id(&acked)),
        json!({"text": "Good."}),
    )
    .await
    .ok();
    let text = recap(lead.clone()).await.unwrap();
    assert!(
        text.contains("Open notices, not acknowledged yet:"),
        "{text}"
    );
    assert!(
        text.contains(&format!("- {}: Deploy at noon.", id(&open))),
        "{text}"
    );
    assert!(
        text.contains("Notices acknowledged since your last turn:"),
        "{text}"
    );
    assert!(text.contains("reply from operator: Good."), "{text}");
}

/// GitHub #24: the operator asks an agent a question from the Inbox. The
/// mail goes to the agent on a new thread, and the Inbox watches it: the
/// watch waits while the operator spoke last, closes with the agent's
/// answer (a mail item that names the watch), and opens again with the
/// operator's reply. The badge does not count a watch.
#[tokio::test]
async fn the_operator_asks_an_agent_and_the_inbox_watches_the_answer() {
    let World { td, w1, .. } = setup().await;
    let (_, before) = inbox(&td, "").await;
    let r = td
        .post(
            "/api/inbox/asks",
            json!({"to": "p/w1", "text": "Is the deploy done?\nSay when."}),
        )
        .await;
    assert_eq!(r.status, 201, "{}", r.body);
    let m = r.body["message"].clone();
    let thread = r.body["watch"]["thread_id"].as_str().unwrap().to_owned();
    assert_eq!(m["from_kind"], "operator");
    assert_eq!(m["to"], w1.as_str());
    assert_eq!(
        m["subject"],
        "Question from the operator: Is the deploy done?"
    );
    assert!(m["body"].as_str().unwrap().contains("terrarium mail reply"));
    assert_eq!(r.body["watch"]["agent_name"], "p/w1");
    assert_eq!(r.body["watch"]["waiting"], true);

    let (rows, counts) = inbox(&td, "?kind=watch").await;
    assert_eq!(rows.len(), 1, "{rows:#?}");
    assert_eq!(rows[0]["id"], format!("watch:{thread}"));
    assert_eq!(rows[0]["open"], true);
    assert_eq!(rows[0]["project"], "p");
    assert_eq!(counts["watch"], 1);
    assert_eq!(
        counts["open"], before["open"],
        "a watch is not in the badge"
    );

    // The agent answers: the watch closes, and the answer is a mail item
    // that names the watch.
    let mid = id(&m);
    let a = td
        .post_as(
            &w1,
            &format!("/api/mail/{mid}/reply"),
            json!({"body": "Yes, at 10:00."}),
        )
        .await;
    assert_eq!(a.status, 201, "{}", a.body);
    let (rows, counts) = inbox(&td, "?kind=watch").await;
    assert!(rows.is_empty(), "{rows:#?}");
    assert_eq!(counts["watch"], 0);
    let (done, _) = inbox(&td, "?kind=watch&status=done").await;
    assert_eq!(done.len(), 1);
    assert_eq!(done[0]["watch"]["replies"], 1);
    let (mails, _) = inbox(&td, "?kind=mail").await;
    let answer = mails
        .iter()
        .find(|i| i["message"]["thread_id"] == thread.as_str())
        .expect("the answer is a mail item");
    assert_eq!(answer["watch"]["thread_id"], thread.as_str());
    assert_eq!(answer["message"]["body"], "Yes, at 10:00.");

    // Another response: the operator's reply opens the watch again.
    let aid = answer["message"]["id"].as_str().unwrap().to_owned();
    td.post(
        &format!("/api/inbox/mail/{aid}/ack"),
        json!({"reply": "And the smoke test?"}),
    )
    .await
    .ok();
    let (rows, _) = inbox(&td, "?kind=watch").await;
    assert_eq!(rows.len(), 1, "{rows:#?}");

    // Cancel: the watch closes; a second cancel is 409.
    td.delete(&format!("/api/inbox/watches/{thread}"))
        .await
        .ok();
    let (rows, counts) = inbox(&td, "?kind=watch").await;
    assert!(rows.is_empty());
    assert_eq!(counts["watch"], 0);
    td.delete(&format!("/api/inbox/watches/{thread}"))
        .await
        .err(409, "conflict");
    td.delete("/api/inbox/watches/nope")
        .await
        .err(404, "not_found");
}

/// GitHub #24: the operator forwards a question of a lead to another
/// agent. The mail quotes the question with its choices and context; the
/// question stays open for the operator's answer.
#[tokio::test]
async fn a_forward_quotes_the_question_and_keeps_it_open() {
    let World { td, lead, w2, .. } = setup().await;
    let q = td
        .post_as(
            &lead,
            &format!("/api/agents/{lead}/questions"),
            json!({"text": "Which database?", "options": ["sqlite", "postgres"],
                   "context": "We have 3 users.", "card": decision_card(&td).await}),
        )
        .await;
    assert_eq!(q.status, 201, "{}", q.body);
    let qid = id(&q.body);
    let r = td
        .post(
            "/api/inbox/asks",
            json!({"to": w2, "question": qid, "text": "You know the load best."}),
        )
        .await;
    assert_eq!(r.status, 201, "{}", r.body);
    let body = r.body["message"]["body"].as_str().unwrap();
    assert!(body.starts_with("You know the load best."), "{body}");
    assert!(body.contains("question of p/lead"), "{body}");
    assert!(body.contains("> Which database?"), "{body}");
    assert!(body.contains("> 2. postgres"), "{body}");
    assert!(body.contains("> We have 3 users."), "{body}");
    assert_eq!(
        r.body["message"]["subject"],
        "Forwarded question from p/lead: Which database?"
    );
    assert_eq!(r.body["watch"]["question_id"], qid.as_str());
    assert_eq!(
        td.get(&format!("/api/questions/{qid}")).await.ok()["status"],
        "open"
    );
    let (rows, _) = inbox(&td, "?kind=watch").await;
    assert_eq!(rows[0]["watch"]["question_id"], qid.as_str());
}

/// GitHub #24: only the operator asks and cancels; a bad target or an
/// empty question is refused.
#[tokio::test]
async fn only_the_operator_asks_and_a_bad_ask_is_refused() {
    let World { td, lead, .. } = setup().await;
    let r = td
        .post_as(&lead, "/api/inbox/asks", json!({"to": "p/w1", "text": "x"}))
        .await;
    r.err(403, "forbidden");
    td.post("/api/inbox/asks", json!({"to": "p/w1", "text": "  "}))
        .await
        .err(422, "invalid");
    td.post("/api/inbox/asks", json!({"to": "operator", "text": "x"}))
        .await
        .err(422, "invalid");
    td.post("/api/inbox/asks", json!({"to": "p/nobody", "text": "x"}))
        .await
        .err(404, "not_found");
    td.post(
        "/api/inbox/asks",
        json!({"to": "p/w1", "question": "01JQ0000000000000000000000"}),
    )
    .await
    .err(404, "not_found");
    let (rows, _) = inbox(&td, "?kind=watch&status=all").await;
    assert!(rows.is_empty(), "{rows:#?}");
    let r = td
        .req(
            hyper::Method::DELETE,
            "/api/inbox/watches/x",
            None,
            Some(&lead),
        )
        .await;
    r.err(403, "forbidden");
}
