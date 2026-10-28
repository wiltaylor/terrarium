//! Questions to the operator (PLAN §12e, docs/api.md §6.8), with the fake
//! supervisor. No harness and no model turn runs.
#![allow(clippy::unwrap_used, clippy::expect_used)]

mod common;

use std::time::Duration;

use common::TestDaemon;
use serde_json::{Value, json};
use terrariumd::waits::Tuning;

fn id(v: &Value) -> String {
    v["id"].as_str().unwrap().to_owned()
}

/// A project `p` with a lead, a worker, and a sysadmin in `ops`; a
/// project `q` with its own lead. All started, all idle.
struct World {
    td: TestDaemon,
    lead: String,
    worker: String,
    sys: String,
    other_lead: String,
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
    for p in ["p", "q", "ops"] {
        td.project(p).await;
    }
    let lead = id(&td
        .agent("p", "lead", json!({"roles": ["project_admin"]}))
        .await);
    let worker = id(&td.agent("p", "w1", json!({})).await);
    let sys = id(&td.agent("ops", "sys", json!({"roles": ["sysadmin"]})).await);
    let other_lead = id(&td
        .agent("q", "lead", json!({"roles": ["project_admin"]}))
        .await);
    for a in [&lead, &worker, &sys, &other_lead] {
        td.post_empty(&format!("/api/agents/{a}/start")).await.ok();
        td.wait_status(a, "idle").await;
    }
    World {
        td,
        lead,
        worker,
        sys,
        other_lead,
    }
}

async fn ask(td: &TestDaemon, agent: &str, body: Value) -> Value {
    let r = td
        .post_as(agent, &format!("/api/agents/{agent}/questions"), body)
        .await;
    assert_eq!(r.status, 201, "{}", r.body);
    r.body
}

async fn status(td: &TestDaemon, agent: &str) -> String {
    td.get(&format!("/api/agents/{agent}")).await.body["status"]
        .as_str()
        .unwrap()
        .to_owned()
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

#[tokio::test]
async fn a_lead_asks_and_the_rights_follow_the_roles() {
    let World {
        td,
        lead,
        worker,
        sys,
        other_lead,
    } = setup().await;
    let q = ask(
        &td,
        &lead,
        json!({"text": "Ship v2 on Friday?", "options": ["yes", "no"],
               "context": "**CI** is green.", "priority": "urgent"}),
    )
    .await;
    assert_eq!(q["status"], "open");
    assert_eq!(q["priority"], "urgent");
    assert_eq!(q["revision"], 1);
    assert_eq!(q["agent_name"], "p/lead");
    assert_eq!(q["options"], json!(["yes", "no"]));
    assert!(q["thread_id"].is_string());
    assert!(q.get("wait").is_none());
    let qid = id(&q);

    // A worker cannot ask: the error says where questions go.
    let r = td
        .post_as(
            &worker,
            &format!("/api/agents/{worker}/questions"),
            json!({"text": "may I?"}),
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
    // An agent asks for itself only; the operator does not ask.
    td.post_as(
        &lead,
        &format!("/api/agents/{worker}/questions"),
        json!({"text": "x"}),
    )
    .await
    .err(403, "forbidden");
    td.post(
        &format!("/api/agents/{lead}/questions"),
        json!({"text": "x"}),
    )
    .await
    .err(403, "forbidden");

    // Read: the owner, the operator, a sysadmin; not a lead of another
    // project, not a worker.
    td.get_as(&lead, &format!("/api/questions/{qid}"))
        .await
        .ok();
    td.get(&format!("/api/questions/{qid}")).await.ok();
    td.get_as(&sys, &format!("/api/questions/{qid}")).await.ok();
    td.get_as(&other_lead, &format!("/api/questions/{qid}"))
        .await
        .err(404, "not_found");
    td.get_as(&worker, &format!("/api/questions/{qid}"))
        .await
        .err(404, "not_found");
    td.get_as(&other_lead, &format!("/api/agents/{lead}/questions"))
        .await
        .err(403, "forbidden");
    // The global list: urgent first; a lead sees its own project only.
    ask(&td, &lead, json!({"text": "Which logo?"})).await;
    let all = td.get("/api/questions").await;
    let rows = all.ok().as_array().unwrap().clone();
    assert_eq!(rows.len(), 2);
    assert_eq!(rows[0]["id"], qid.as_str());
    assert_eq!(all.headers["x-open-questions"], "2");
    assert_eq!(all.headers["x-urgent-questions"], "1");
    let theirs = td.get_as(&other_lead, "/api/questions").await;
    assert!(theirs.ok().as_array().unwrap().is_empty());
    td.get_as(&worker, "/api/questions")
        .await
        .err(403, "forbidden");

    // Answer: not the asking agent, not a lead; a sysadmin may.
    td.post_as(
        &lead,
        &format!("/api/questions/{qid}/answer"),
        json!({"answer": "yes"}),
    )
    .await
    .err(403, "forbidden");
    td.post_as(
        &other_lead,
        &format!("/api/questions/{qid}/answer"),
        json!({"answer": "yes"}),
    )
    .await
    .err(404, "not_found");
    let r = td
        .post_as(
            &sys,
            &format!("/api/questions/{qid}/answer"),
            json!({"option": "yes"}),
        )
        .await;
    let done = r.ok();
    assert_eq!(done["status"], "answered");
    assert_eq!(done["answer"], "yes");
    assert_eq!(done["answer_option"], 0);
    assert_eq!(done["answered_by"], sys.as_str());
    assert_eq!(done["answered_by_name"], "ops/sys");
    // A second answer is a conflict.
    td.post(
        &format!("/api/questions/{qid}/answer"),
        json!({"answer": "no"}),
    )
    .await
    .err(409, "conflict");

    // A sysadmin cannot answer its own question.
    let own = ask(&td, &sys, json!({"text": "Add a host?"})).await;
    td.post_as(
        &sys,
        &format!("/api/questions/{}/answer", id(&own)),
        json!({"answer": "yes"}),
    )
    .await
    .err(403, "forbidden");

    // The audit log has each change.
    let audit = td.get("/api/audit?limit=200").await;
    let actions: Vec<String> = audit
        .ok()
        .as_array()
        .unwrap()
        .iter()
        .map(|r| r["action"].as_str().unwrap().to_owned())
        .collect();
    assert!(actions.contains(&"question.ask".to_owned()), "{actions:?}");
    assert!(
        actions.contains(&"question.answer".to_owned()),
        "{actions:?}"
    );
}

#[tokio::test]
async fn limits_updates_and_retracts() {
    let World { td, lead, .. } = setup().await;
    let path = format!("/api/agents/{lead}/questions");
    let bad = [
        json!({"text": ""}),
        json!({"text": "x".repeat(2001)}),
        json!({"text": "x", "options": (0..11).map(|i| i.to_string()).collect::<Vec<_>>()}),
        json!({"text": "x", "options": ["y".repeat(201)]}),
        json!({"text": "x", "context": "z".repeat(16 * 1024 + 1)}),
        json!({"text": "x", "card": "NOPE-1"}),
    ];
    for b in bad {
        let r = td.post_as(&lead, &path, b.clone()).await;
        assert!(
            r.status == 422 || r.status == 404,
            "{b}: {} {}",
            r.status,
            r.body
        );
    }
    let q = ask(&td, &lead, json!({"text": "Use  Postgres?"})).await;
    let qid = id(&q);
    // The same question again: a conflict that names the open one.
    let r = td
        .post_as(&lead, &path, json!({"text": "use postgres?"}))
        .await;
    r.err(409, "conflict");
    assert_eq!(r.body["error"]["details"]["question"], qid.as_str());

    // An update bumps the revision; no change keeps it.
    let up = td
        .patch_as(
            &lead,
            &format!("/api/questions/{qid}"),
            json!({"text": "Use Postgres 17?", "options": ["yes", "no"], "priority": "urgent"}),
        )
        .await;
    let up = up.ok();
    assert_eq!(up["revision"], 2);
    assert_eq!(up["options"], json!(["yes", "no"]));
    let same = td
        .patch_as(
            &lead,
            &format!("/api/questions/{qid}"),
            json!({"text": "Use Postgres 17?"}),
        )
        .await;
    assert_eq!(same.ok()["revision"], 2);
    let cleared = td
        .patch_as(
            &lead,
            &format!("/api/questions/{qid}"),
            json!({"options": null, "priority": "normal"}),
        )
        .await;
    assert_eq!(cleared.ok()["options"], json!([]));
    // The operator does not edit a question.
    td.patch(&format!("/api/questions/{qid}"), json!({"text": "x"}))
        .await
        .err(403, "forbidden");

    // Answered: no edit, no retract, no second answer.
    td.post(
        &format!("/api/questions/{qid}/answer"),
        json!({"answer": "Yes, 17."}),
    )
    .await
    .ok();
    td.patch_as(
        &lead,
        &format!("/api/questions/{qid}"),
        json!({"text": "y"}),
    )
    .await
    .err(409, "conflict");
    td.post_as(&lead, &format!("/api/questions/{qid}/retract"), json!({}))
        .await
        .err(409, "conflict");

    // A retract by the lead itself: no mail.
    let q2 = ask(&td, &lead, json!({"text": "Rename the repo?"})).await;
    let r = td
        .post_as(
            &lead,
            &format!("/api/questions/{}/retract", id(&q2)),
            json!({"reason": "decided myself"}),
        )
        .await;
    let r = r.ok();
    assert_eq!(r["status"], "retracted");
    assert_eq!(r["retract_reason"], "decided myself");
    assert_eq!(r["retracted_by"], format!("agent:{lead}"));
    td.post(
        &format!("/api/questions/{}/answer", id(&q2)),
        json!({"answer": "late"}),
    )
    .await
    .err(409, "conflict");
    // A retract by the operator, with no body: one mail on the thread.
    let q3 = ask(&td, &lead, json!({"text": "Buy a GPU?"})).await;
    td.post_empty(&format!("/api/questions/{}/retract", id(&q3)))
        .await
        .ok();
    let m = mail(&td, &lead, "operator").await;
    let retracted: Vec<&Value> = m
        .iter()
        .filter(|x| x["subject"].as_str().unwrap().starts_with("Retracted: "))
        .collect();
    assert_eq!(retracted.len(), 1, "{m:?}");
    assert_eq!(retracted[0]["thread_id"], q3["thread_id"]);

    // The default list: open, and answered in the last 24 h.
    let list = td
        .get_as(&lead, &format!("/api/agents/{lead}/questions"))
        .await;
    let ids: Vec<&str> = list
        .ok()
        .as_array()
        .unwrap()
        .iter()
        .map(|q| q["id"].as_str().unwrap())
        .collect();
    assert_eq!(ids, [qid.as_str()]);
    let all = td
        .get_as(&lead, &format!("/api/agents/{lead}/questions?all=true"))
        .await;
    assert_eq!(all.ok().as_array().unwrap().len(), 3);
    let retracted = td
        .get_as(
            &lead,
            &format!("/api/agents/{lead}/questions?status=retracted"),
        )
        .await;
    assert_eq!(retracted.ok().as_array().unwrap().len(), 2);

    // The `question` events.
    let ev = td.get("/api/events?kind=question&limit=100").await;
    let changes: Vec<&str> = ev
        .ok()
        .as_array()
        .unwrap()
        .iter()
        .map(|e| e["data"]["change"].as_str().unwrap())
        .collect();
    for c in ["asked", "updated", "answered", "retracted"] {
        assert!(changes.contains(&c), "{changes:?}");
    }
}

#[tokio::test]
async fn at_most_fifty_open_questions() {
    let World { td, lead, .. } = setup().await;
    for i in 0..50 {
        ask(&td, &lead, json!({"text": format!("question {i}")})).await;
    }
    let r = td
        .post_as(
            &lead,
            &format!("/api/agents/{lead}/questions"),
            json!({"text": "one more"}),
        )
        .await;
    r.err(429, "rate_limited");
}

/// The answer is one mail from the operator on the thread of the question,
/// and the mail wakes the agent (the push pointer of the fake supervisor).
#[tokio::test]
async fn the_answer_mail_goes_on_the_thread_and_wakes_the_agent() {
    let World { td, lead, .. } = setup().await;
    td.sup.set_mail_push(true);
    let q = ask(
        &td,
        &lead,
        json!({"text": "Which region?", "options": ["eu-west", "us-east"]}),
    )
    .await;
    let qid = id(&q);
    let turns_before = td
        .get(&format!("/api/agents/{lead}/events?kind=turn_start"))
        .await
        .ok()
        .as_array()
        .unwrap()
        .len();
    td.post(
        &format!("/api/questions/{qid}/answer"),
        json!({"option": 1, "answer": "us-east, for the latency"}),
    )
    .await
    .ok();
    let m = mail(&td, &lead, "operator").await;
    assert_eq!(m.len(), 1, "{m:?}");
    assert_eq!(m[0]["thread_id"], q["thread_id"]);
    assert_eq!(m[0]["subject"], "Answer: Which region?");
    let body = m[0]["body"].as_str().unwrap();
    assert!(body.contains("> Which region?"), "{body}");
    assert!(body.contains("Chosen option: 2. us-east"), "{body}");
    assert!(body.contains("> us-east, for the latency"), "{body}");
    // The push typed a turn, and the mail is delivered.
    until("the push turn", || async {
        td.get(&format!("/api/agents/{lead}/events?kind=turn_start"))
            .await
            .ok()
            .as_array()
            .unwrap()
            .len()
            > turns_before
    })
    .await;
    until("the mail delivered", || async {
        mail(&td, &lead, "operator").await[0]["state"] != "queued"
    })
    .await;
}

/// `wait: true` registers a `question` wait: the agent is `waiting` on it.
/// The answer resolves it with the answer, the agent goes back to `idle`,
/// and it gets one mail (the answer), not a second wake mail from
/// `system`.
#[tokio::test]
async fn a_question_wait_resolves_with_the_answer_and_one_mail() {
    let World { td, lead, .. } = setup().await;
    let q = ask(
        &td,
        &lead,
        json!({"text": "Deploy now?", "wait": true, "timeout": "2h"}),
    )
    .await;
    let qid = id(&q);
    let w = q["wait"].clone();
    assert_eq!(w["kind"], "question");
    assert_eq!(w["state"], "open");
    assert_eq!(w["spec"]["question"], qid.as_str());
    assert_eq!(w["summary"], "question Deploy now?");
    assert_eq!(status(&td, &lead).await, "waiting");
    let a = td.get(&format!("/api/agents/{lead}")).await;
    assert_eq!(a.body["waiting_on"], "question Deploy now?");

    // A second wait through the waits API; a wait on a question that is
    // not the waiter's own is not found.
    let r = td
        .post_as(
            &lead,
            &format!("/api/agents/{lead}/waits"),
            json!({"kind": "question", "question": qid}),
        )
        .await;
    assert_eq!(r.status, 201, "{}", r.body);
    let w2 = r.body.clone();

    td.post(
        &format!("/api/questions/{qid}/answer"),
        json!({"answer": "Yes, go."}),
    )
    .await
    .ok();
    for wid in [id(&w), id(&w2)] {
        let done = td.get(&format!("/api/waits/{wid}")).await.body;
        assert_eq!(done["state"], "resolved", "{done}");
        assert_eq!(done["result"]["outcome"], "answered");
        assert_eq!(done["result"]["answer"], "Yes, go.");
        assert!(done["notified_at"].is_string(), "{done}");
    }
    td.wait_status(&lead, "idle").await;
    // Past the wake debounce: still no system mail, one answer mail.
    tokio::time::sleep(Duration::from_millis(400)).await;
    assert!(mail(&td, &lead, "system").await.is_empty());
    let m = mail(&td, &lead, "operator").await;
    assert_eq!(m.len(), 1, "{m:?}");
    let body = m[0]["body"].as_str().unwrap();
    assert!(body.contains("This mail is its wake mail"), "{body}");

    // A wait on an answered question is a conflict.
    let r = td
        .post_as(
            &lead,
            &format!("/api/agents/{lead}/waits"),
            json!({"kind": "question", "question": qid}),
        )
        .await;
    r.err(409, "conflict");
}

/// A retract by the agent itself ends its wait with no mail at all.
#[tokio::test]
async fn a_retract_by_the_agent_ends_the_wait_with_no_mail() {
    let World { td, lead, .. } = setup().await;
    let q = ask(
        &td,
        &lead,
        json!({"text": "Hire a designer?", "wait": true}),
    )
    .await;
    assert_eq!(status(&td, &lead).await, "waiting");
    td.post_as(
        &lead,
        &format!("/api/questions/{}/retract", id(&q)),
        json!({}),
    )
    .await
    .ok();
    let done = td.get(&format!("/api/waits/{}", id(&q["wait"]))).await.body;
    assert_eq!(done["state"], "resolved");
    assert_eq!(done["result"]["outcome"], "retracted");
    td.wait_status(&lead, "idle").await;
    tokio::time::sleep(Duration::from_millis(400)).await;
    assert!(mail(&td, &lead, "system").await.is_empty());
    assert!(mail(&td, &lead, "operator").await.is_empty());
}

/// The recap of a new session lists the open questions and the answers
/// since the last turn.
#[tokio::test]
async fn the_recap_lists_open_and_answered_questions() {
    let World {
        td, lead, worker, ..
    } = setup().await;
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
    assert_eq!(recap(lead.clone()).await, None);
    assert_eq!(recap(worker.clone()).await, None);
    let open = ask(
        &td,
        &lead,
        json!({"text": "Merge the fork?", "priority": "urgent"}),
    )
    .await;
    let done = ask(&td, &lead, json!({"text": "Pick a name?"})).await;
    td.post(
        &format!("/api/questions/{}/answer", id(&done)),
        json!({"answer": "terra"}),
    )
    .await
    .ok();
    let text = recap(lead.clone()).await.unwrap();
    assert!(
        text.contains(&format!("- {} [urgent]: Merge the fork?", id(&open))),
        "{text}"
    );
    assert!(text.contains("Answered since your last turn"), "{text}");
    assert!(text.contains("answer from operator: terra"), "{text}");
    assert!(text.contains("terrarium question ls"), "{text}");
    // A turn after the answer: the answer is not new any more.
    td.post(
        &format!("/api/agents/{lead}/send"),
        json!({"text": "carry on"}),
    )
    .await
    .ok();
    until("the turn", || async {
        !td.get(&format!("/api/agents/{lead}/events?kind=turn_start"))
            .await
            .ok()
            .as_array()
            .unwrap()
            .is_empty()
    })
    .await;
    let text = recap(lead.clone()).await.unwrap();
    assert!(!text.contains("Pick a name?"), "{text}");
    assert!(text.contains("Merge the fork?"), "{text}");
}

/// A deleted agent's open questions are retracted.
#[tokio::test]
async fn a_deleted_agent_retracts_its_questions() {
    let World { td, lead, .. } = setup().await;
    let q = ask(&td, &lead, json!({"text": "Keep the cache?"})).await;
    td.post_empty(&format!("/api/agents/{lead}/stop"))
        .await
        .ok();
    td.wait_status(&lead, "stopped").await;
    let r = td
        .req(
            hyper::Method::DELETE,
            &format!("/api/agents/{lead}?confirm=true"),
            None,
            None,
        )
        .await;
    assert!(r.status.is_success(), "{}", r.body);
    let got = td.get(&format!("/api/questions/{}", id(&q))).await;
    assert_eq!(got.ok()["status"], "retracted");
}
