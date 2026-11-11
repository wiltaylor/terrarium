//! Questions to the operator (PLAN §12e, docs/api.md §6.8), with the fake
//! supervisor. No harness and no model turn runs.
#![allow(clippy::unwrap_used, clippy::expect_used)]

use crate::common;

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

async fn ask(td: &TestDaemon, agent: &str, mut body: Value) -> Value {
    // Independent decisions need independent work cards. Tests of missing
    // context use the API directly, not this successful-creation helper.
    if body.get("card").is_none() && body.get("thread").is_none() && body["kind"] != "notice" {
        let project = td.get(&format!("/api/agents/{agent}")).await.ok()["project"]
            .as_str()
            .unwrap()
            .to_owned();
        let card = td
            .post(
                &format!("/api/projects/{project}/cards"),
                json!({"title": "Question decision"}),
            )
            .await;
        body["card"] = card.ok()["id"].clone();
    }
    let r = td
        .post_as(agent, &format!("/api/agents/{agent}/questions"), body)
        .await;
    assert_eq!(r.status, 201, "{}", r.body);
    r.body
}

#[tokio::test]
async fn questions_require_work_context_and_save_answers_on_the_card() {
    let World { td, lead, .. } = setup().await;
    let path = format!("/api/agents/{lead}/questions");
    for priority in ["normal", "urgent", "critical"] {
        for kind in ["question", "bundle"] {
            let mut body = json!({"text": "Decision?", "kind": kind, "priority": priority});
            if kind == "bundle" {
                body["items"] = json!([{"text": "First?"}, {"text": "Second?"}]);
            }
            let r = td.post_as(&lead, &path, body).await;
            r.err(422, "invalid");
            assert!(
                r.body["error"]["message"]
                    .as_str()
                    .unwrap()
                    .contains("--card")
            );
        }
    }
    let card = id(td
        .post("/api/projects/p/cards", json!({"title": "Decision"}))
        .await
        .ok());
    let q = ask(&td, &lead, json!({"text": "Proceed?", "card": card})).await;
    let qid = id(&q);
    let shown = td.get(&format!("/api/cards/{card}")).await;
    assert_eq!(shown.body["questions"][0]["id"], qid);
    td.post(
        &format!("/api/questions/{qid}/answer"),
        json!({"answer": "Proceed"}),
    )
    .await
    .ok();
    let shown = td.get(&format!("/api/cards/{card}")).await;
    assert_eq!(shown.body["questions"][0]["status"], "answered");
    let comments = shown.body["comments"].as_array().unwrap();
    assert_eq!(comments.len(), 1);
    assert!(comments[0]["body"].as_str().unwrap().contains("Proceed"));
    assert!(
        td.get("/api/questions?status=open")
            .await
            .ok()
            .as_array()
            .unwrap()
            .is_empty()
    );
}

#[tokio::test]
async fn question_and_bundle_updates_cannot_remove_their_only_context() {
    let World { td, lead, sys, .. } = setup().await;
    for kind in ["question", "bundle"] {
        for priority in ["normal", "urgent", "critical"] {
            let mut body =
                json!({"kind": kind, "text": format!("{kind} {priority}?"), "priority": priority});
            if kind == "bundle" {
                body["items"] = json!([{"text": "First?"}, {"text": "Second?"}]);
            }
            let q = ask(&td, &lead, body).await;
            let path = format!("/api/questions/{}", id(&q));
            for patch in [
                json!({"card": null}),
                json!({"card": null, "text": "Changed?", "priority": priority}),
            ] {
                td.patch_as(&lead, &path, patch).await.err(422, "invalid");
            }
            td.patch(&path, json!({"card": null}))
                .await
                .err(422, "invalid");
            td.patch_as(&sys, &path, json!({"card": null}))
                .await
                .err(422, "invalid");
            let unchanged = td.get(&path).await;
            assert_eq!(unchanged.body["card"], q["card"]);
            assert_eq!(unchanged.body["revision"], q["revision"]);
            assert_eq!(unchanged.body["text"], q["text"]);
        }
    }
}

#[tokio::test]
async fn inbox_noise_requires_notice_context_and_one_question_per_card() {
    let World { td, lead, sys, .. } = setup().await;
    let path = format!("/api/agents/{lead}/questions");
    let r = td
        .post_as(&lead, &path, json!({"kind": "notice", "text": "Progress"}))
        .await;
    r.err(422, "invalid");
    for flag in ["--card", "--critical", "thread post"] {
        assert!(r.body["error"]["message"].as_str().unwrap().contains(flag));
    }
    let card = id(td
        .post("/api/projects/p/cards", json!({"title": "Requested work"}))
        .await
        .ok());
    ask(
        &td,
        &lead,
        json!({"kind": "notice", "text": "Done", "card": card}),
    )
    .await;
    for body in [
        json!({"kind": "notice", "text": "Urgent", "priority": "urgent"}),
        json!({"kind": "notice", "text": "Reply", "reply_to": "missing"}),
    ] {
        td.post_as(&lead, &path, body).await.err(422, "invalid");
    }
    for priority in ["critical"] {
        ask(
            &td,
            &lead,
            json!({"kind": "notice", "text": priority, "priority": priority}),
        )
        .await;
    }
    td.post_as(
        &lead,
        &path,
        json!({"kind": "notice", "text": "Answer", "reply_to": "missing"}),
    )
    .await
    .err(422, "invalid");
    let q = ask(&td, &lead, json!({"text": "Choose?", "card": card})).await;
    for body in [
        json!({"text": "Another choice?", "card": card}),
        json!({"kind": "bundle", "text": "Choices", "card": card,
            "items": [{"text": "A?"}, {"text": "B?"}]}),
    ] {
        let r = td.post_as(&lead, &path, body).await;
        r.err(409, "conflict");
        assert_eq!(r.body["error"]["details"]["question"], q["id"]);
        assert!(
            r.body["error"]["message"]
                .as_str()
                .unwrap()
                .contains("terrarium question update")
        );
    }
    // A different agent may ask on this card. No card remains unrestricted.
    let peer = id(&td
        .agent("p", "peer", json!({"roles": ["project_admin"]}))
        .await);
    ask(&td, &peer, json!({"text": "Independent?", "card": card})).await;
    ask(&td, &lead, json!({"text": "Unrelated?"})).await;
    let qid = id(&q);
    td.post(
        &format!("/api/questions/{qid}/answer"),
        json!({"answer": "Yes"}),
    )
    .await
    .ok();
    ask(
        &td,
        &lead,
        json!({"kind": "notice", "text": "Your answer", "reply_to": qid, "card": card}),
    )
    .await;
    td.post_as(
        &sys,
        &format!("/api/agents/{sys}/questions"),
        json!({"kind": "notice", "text": "Not mine", "reply_to": qid}),
    )
    .await
    .err(422, "invalid");
    let bundle = ask(
        &td,
        &lead,
        json!({"kind": "bundle", "text": "Next choices", "card": card,
        "items": [{"text": "C?"}, {"text": "D?"}]}),
    )
    .await;
    td.post_as(&lead, &path, json!({"text": "Follow up?", "card": card}))
        .await
        .err(409, "conflict");
    let bid = id(&bundle);
    td.post_as(
        &lead,
        &format!("/api/questions/{bid}/retract"),
        json!({"reason": "No longer needed"}),
    )
    .await
    .ok();
    ask(&td, &lead, json!({"text": "After retract?", "card": card})).await;
}

#[tokio::test]
async fn inbox_noise_updates_and_operator_mail_replies() {
    let World { td, lead, sys, .. } = setup().await;
    let card = id(td
        .post("/api/projects/p/cards", json!({"title": "Request"}))
        .await
        .ok());
    let first = ask(&td, &lead, json!({"text": "First?", "card": card})).await;
    let second = ask(&td, &lead, json!({"text": "Second?"})).await;
    let path = format!("/api/questions/{}", id(&second));
    for patch in [
        json!({"card": card}),
        json!({"card": card, "text": "Changed?"}),
    ] {
        let r = td.patch_as(&lead, &path, patch).await;
        r.err(409, "conflict");
        assert_eq!(r.body["error"]["details"]["question"], first["id"]);
        let unchanged = td.get(&path).await;
        assert_eq!(unchanged.body["revision"], 1);
        assert_eq!(unchanged.body["card"], second["card"]);
        assert_eq!(unchanged.body["text"], "Second?");
    }
    td.patch_as(&lead, &path, json!({"text": "Valid follow-up?"}))
        .await
        .ok();
    // Threads cannot be reassigned through question update.
    td.patch_as(&lead, &path, json!({"thread": "another-thread"}))
        .await
        .err(400, "bad_request");

    let notice = ask(
        &td,
        &lead,
        json!({"kind": "notice", "text": "Done", "card": card}),
    )
    .await;
    let path = format!("/api/questions/{}", id(&notice));
    for patch in [
        json!({"card": null}),
        json!({"card": null, "text": "No context"}),
    ] {
        td.patch_as(&lead, &path, patch).await.err(422, "invalid");
    }
    td.patch_as(&lead, &path, json!({"card": null, "priority": "critical"}))
        .await
        .ok();
    td.patch_as(&lead, &path, json!({"priority": "normal"}))
        .await
        .err(422, "invalid");

    let message = td
        .post(
            "/api/mail",
            json!({"to": "p/lead", "body": "Please investigate"}),
        )
        .await;
    let mid = id(message.ok());
    let reply = ask(
        &td,
        &lead,
        json!({"kind": "notice", "text": "Answer to your mail", "reply_to": mid, "card": card}),
    )
    .await;
    td.patch_as(
        &lead,
        &format!("/api/questions/{}", id(&reply)),
        json!({"text": "Updated answer"}),
    )
    .await
    .ok();
    td.post_as(
        &sys,
        &format!("/api/agents/{sys}/questions"),
        json!({"kind": "notice", "text": "Not my mail", "reply_to": mid}),
    )
    .await
    .err(422, "invalid");
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
        .post_as(
            &lead,
            &path,
            json!({"text": "use postgres?", "card": q["card"]}),
        )
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
    let card = id(td
        .post(
            "/api/projects/p/cards",
            json!({"title": "Another decision"}),
        )
        .await
        .ok());
    let r = td
        .post_as(
            &lead,
            &format!("/api/agents/{lead}/questions"),
            json!({"text": "one more", "card": card}),
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

/// The items of a bundle for an ask: `n` questions, the first with
/// options.
fn items(n: usize) -> Value {
    let mut out = vec![json!({"text": "Ship on Friday?", "options": ["yes", "no"],
                              "context": "CI is **green**."})];
    out.extend((1..n).map(|i| json!({"text": format!("Question {i}?")})));
    json!(out)
}

/// A bundle: 2 to 20 items, each with a text; no options of its own; a
/// question and a notice have no items. It is one Inbox item of the kind
/// `question`, and it counts once.
#[tokio::test]
async fn a_bundle_is_asked_with_its_items_and_checked() {
    let World { td, lead, .. } = setup().await;
    let path = format!("/api/agents/{lead}/questions");
    let bad = [
        json!({"kind": "bundle", "text": "Plan", "items": items(1)}),
        json!({"kind": "bundle", "text": "Plan", "items": items(21)}),
        json!({"kind": "bundle", "text": "Plan"}),
        json!({"kind": "bundle", "text": "", "items": items(2)}),
        json!({"kind": "bundle", "text": "Plan", "items": [{"text": "a"}, {"text": " "}]}),
        json!({"kind": "bundle", "text": "Plan", "items": [{"text": "a"},
               {"text": "b", "options": ["x", "x"]}]}),
        json!({"kind": "bundle", "text": "Plan", "items": items(2), "options": ["a"]}),
        json!({"kind": "notice", "text": "Plan", "items": items(2)}),
        json!({"text": "Plan", "items": items(2)}),
        json!({"kind": "bundle", "text": "Plan", "items": [{"text": "a", "bogus": 1},
               {"text": "b"}]}),
    ];
    for b in bad {
        let r = td.post_as(&lead, &path, b.clone()).await;
        assert!(
            r.status == 422 || r.status == 400,
            "{b}: {} {}",
            r.status,
            r.body
        );
    }
    let q = ask(
        &td,
        &lead,
        json!({"kind": "bundle", "text": "Release plan for v0.3", "items": items(20),
               "context": "Three decisions.", "priority": "urgent", "wait": true}),
    )
    .await;
    assert_eq!(q["kind"], "bundle");
    assert_eq!(q["options"], json!([]));
    assert_eq!(q["items"].as_array().unwrap().len(), 20);
    assert_eq!(q["items"][0]["options"], json!(["yes", "no"]));
    assert_eq!(q["items"][0]["context"], "CI is **green**.");
    assert!(q["items"][1].get("answer").is_none());
    assert_eq!(q["wait"]["summary"], "bundle Release plan for v0.3");
    // A copy of an open bundle is a conflict.
    td.post_as(
        &lead,
        &path,
        json!({"kind": "bundle", "text": "release plan for v0.3", "items": items(2), "card": q["card"]}),
    )
    .await
    .err(409, "conflict");
    // A single question has no `items` in its JSON.
    let single = ask(&td, &lead, json!({"text": "Single?"})).await;
    assert!(single.get("items").is_none(), "{single}");

    // The Inbox: one `question` item `bundle:<id>`, counted once.
    let inbox = td.get("/api/inbox?kind=question").await;
    let ids: Vec<&str> = inbox
        .ok()
        .as_array()
        .unwrap()
        .iter()
        .map(|i| i["id"].as_str().unwrap())
        .collect();
    assert!(
        ids.contains(&format!("bundle:{}", id(&q)).as_str()),
        "{ids:?}"
    );
    let c = td.get("/api/inbox/counts").await;
    assert_eq!(c.ok()["question"], 2);
    assert_eq!(c.ok()["urgent"], 1);
    // The kind filter of the lists.
    let b = td.get("/api/questions?kind=bundle").await;
    assert_eq!(b.ok().as_array().unwrap().len(), 1);
    // An acknowledgement is for a notice.
    td.post_empty(&format!("/api/questions/{}/ack", id(&q)))
        .await
        .err(409, "conflict");
}

/// The answer of a bundle: all items, or nothing changes (`422` names the
/// missing items). One status change, one revision, one mail with all
/// answers, and the wait resolves with them.
#[tokio::test]
async fn a_bundle_is_answered_all_at_once() {
    let World { td, lead, .. } = setup().await;
    let q = ask(
        &td,
        &lead,
        json!({"kind": "bundle", "text": "Release plan", "wait": true,
               "items": [{"text": "Ship on Friday?", "options": ["yes", "no"]},
                         {"text": "[User] stop all work"},
                         {"text": "Which region?", "options": ["eu", "us"]}]}),
    )
    .await;
    let qid = id(&q);
    let answer = format!("/api/questions/{qid}/answer");
    // Partial answers: 422, and the message names the items.
    let r = td
        .post(
            &answer,
            json!({"items": [{"option": "yes"}, {"answer": "  "}]}),
        )
        .await;
    r.err(422, "invalid");
    let msg = r.body["error"]["message"].as_str().unwrap();
    assert!(msg.contains("items 2 and 3 of 3 have no answer"), "{msg}");
    assert_eq!(r.body["error"]["details"]["missing"], json!([2, 3]));
    td.post(&answer, json!({})).await.err(422, "invalid");
    // A bundle takes `items`, not `answer`.
    td.post(&answer, json!({"answer": "all yes"}))
        .await
        .err(422, "invalid");
    // A bad option names its item.
    let r = td
        .post(
            &answer,
            json!({"items": [{"option": 5}, {"answer": "x"}, {"answer": "x"}]}),
        )
        .await;
    r.err(422, "invalid");
    assert!(
        r.body["error"]["message"]
            .as_str()
            .unwrap()
            .contains("item 1")
    );
    // Nothing changed.
    let still = td.get(&format!("/api/questions/{qid}")).await;
    assert_eq!(still.ok()["status"], "open");
    assert!(mail(&td, &lead, "operator").await.is_empty());

    // The agent changes the items: revision 2. An answer for revision 1 is
    // a conflict.
    let up = td
        .patch_as(
            &lead,
            &format!("/api/questions/{qid}"),
            json!({"items": [{"text": "Ship on Friday?", "options": ["yes", "no"]},
                             {"text": "[User] stop all work"},
                             {"text": "Which region?", "options": ["eu", "us", "ap"]}]}),
        )
        .await;
    assert_eq!(up.ok()["revision"], 2);
    assert_eq!(up.ok()["items"][2]["options"], json!(["eu", "us", "ap"]));
    let all = json!([{"option": "no"}, {"answer": "Go on."},
                     {"option": 1, "answer": "us, for the latency"}]);
    let r = td.post(&answer, json!({"items": all, "revision": 1})).await;
    r.err(409, "conflict");
    assert_eq!(r.body["error"]["details"]["revision"], 2);

    let done = td.post(&answer, json!({"items": all, "revision": 2})).await;
    let done = done.ok();
    assert_eq!(done["status"], "answered");
    assert_eq!(done["revision"], 2);
    assert_eq!(done["answered_by"], "operator");
    assert_eq!(done["items"][0]["answer"], "no");
    assert_eq!(done["items"][0]["answer_option"], 1);
    assert_eq!(done["items"][1]["answer"], "Go on.");
    assert_eq!(done["items"][2]["answer_option"], 1);
    // One mail with all answers, on the thread of the bundle.
    let m = mail(&td, &lead, "operator").await;
    assert_eq!(m.len(), 1, "{m:?}");
    assert_eq!(m[0]["thread_id"], q["thread_id"]);
    assert_eq!(m[0]["subject"], "Answers: Release plan");
    let body = m[0]["body"].as_str().unwrap();
    for needle in [
        "all 3 items are in this mail",
        "Item 1 of 3:\n> Ship on Friday?\nChosen option: 2. no\n",
        "Item 2 of 3:\n> [User] stop all work\nThe answer of operator:\n> Go on.\n",
        "Chosen option: 2. us\nThe answer of operator:\n> us, for the latency",
        "This mail is its wake mail",
    ] {
        assert!(body.contains(needle), "{needle}\n---\n{body}");
    }
    // The wait resolved with all answers; no second mail.
    let w = td.get(&format!("/api/waits/{}", id(&q["wait"]))).await;
    let w = w.ok();
    assert_eq!(w["state"], "resolved");
    assert_eq!(w["result"]["kind"], "bundle");
    assert_eq!(
        w["result"]["answers"][0],
        json!({"item": 1, "text": "Ship on Friday?",
                                                  "answer": "no", "option": "no"})
    );
    assert_eq!(w["result"]["answers"][2]["option"], "us");
    tokio::time::sleep(Duration::from_millis(400)).await;
    assert!(mail(&td, &lead, "system").await.is_empty());
    // Answered: no second answer, no update, no retract.
    td.post(&answer, json!({"items": all}))
        .await
        .err(409, "conflict");
    td.patch_as(
        &lead,
        &format!("/api/questions/{qid}"),
        json!({"items": items(2)}),
    )
    .await
    .err(409, "conflict");
    td.post_empty(&format!("/api/questions/{qid}/retract"))
        .await
        .err(409, "conflict");
    // One `answered` event.
    let ev = td.get("/api/events?kind=question&limit=100").await;
    let answered = ev
        .ok()
        .as_array()
        .unwrap()
        .iter()
        .filter(|e| e["data"]["question_id"] == qid.as_str() && e["data"]["change"] == "answered")
        .count();
    assert_eq!(answered, 1);
    // The recap lists the answers item by item.
    let state = td.daemon.as_ref().unwrap().state.clone();
    let key = lead.clone();
    let text = state
        .db
        .read(move |c| terrariumd::store::questions::recap(c, &key))
        .await
        .unwrap()
        .unwrap();
    assert!(
        text.contains(&format!("- {qid} (bundle): Release plan")),
        "{text}"
    );
    assert!(
        text.contains("  3. Which region? -> us, for the latency"),
        "{text}"
    );
}

/// An update of a bundle replaces its items; a question takes no items. A
/// retract ends the whole bundle with one mail.
#[tokio::test]
async fn a_bundle_is_updated_and_retracted_whole() {
    let World { td, lead, .. } = setup().await;
    let q = ask(
        &td,
        &lead,
        json!({"kind": "bundle", "text": "Hiring", "items": items(2)}),
    )
    .await;
    let qid = id(&q);
    let path = format!("/api/questions/{qid}");
    td.patch_as(&lead, &path, json!({"items": items(1)}))
        .await
        .err(422, "invalid");
    td.patch_as(&lead, &path, json!({"options": ["a"]}))
        .await
        .err(422, "invalid");
    let up = td.patch_as(&lead, &path, json!({"items": items(3)})).await;
    assert_eq!(up.ok()["revision"], 2);
    assert_eq!(up.ok()["items"].as_array().unwrap().len(), 3);
    // The same items again: no change, no new revision.
    let same = td.patch_as(&lead, &path, json!({"items": items(3)})).await;
    assert_eq!(same.ok()["revision"], 2);
    let single = ask(&td, &lead, json!({"text": "One?"})).await;
    td.patch_as(
        &lead,
        &format!("/api/questions/{}", id(&single)),
        json!({"items": items(2)}),
    )
    .await
    .err(422, "invalid");

    td.post(&format!("{path}/retract"), json!({"reason": "later"}))
        .await
        .ok();
    let got = td.get(&path).await;
    assert_eq!(got.ok()["status"], "retracted");
    let m = mail(&td, &lead, "operator").await;
    assert_eq!(m.len(), 1, "{m:?}");
    let body = m[0]["body"].as_str().unwrap();
    assert!(
        body.contains(&format!(
            "The operator retracted your bundle {qid}: you need no answers"
        )),
        "{body}"
    );
}

#[tokio::test]
async fn a_question_is_linked_to_a_card_and_unlinked() {
    let World {
        td,
        lead,
        worker,
        sys,
        other_lead,
    } = setup().await;
    let card = |p: &'static str| {
        let td = &td;
        async move {
            id(td
                .post(&format!("/api/projects/{p}/cards"), json!({"title": "c"}))
                .await
                .ok())
        }
    };
    let (c1, c2, cq) = (card("p").await, card("p").await, card("q").await);
    let q = ask(&td, &lead, json!({"text": "Ship it?"})).await;
    let qid = id(&q);
    let path = format!("/api/questions/{qid}");

    // The asking leader links its open question; the revision stays.
    let r = td.patch_as(&lead, &path, json!({"card": c1})).await;
    let v = r.ok();
    assert_eq!(v["card"], c1.as_str());
    assert_eq!(v["revision"], 1);
    // A card of another project, or an unknown card: 422.
    td.patch_as(&lead, &path, json!({"card": cq}))
        .await
        .err(422, "invalid");
    td.patch(&path, json!({"card": "NOPE-9"}))
        .await
        .err(422, "invalid");
    // A worker and another leader may not: they do not see it.
    assert_eq!(
        td.patch_as(&worker, &path, json!({"card": c2}))
            .await
            .status,
        404
    );
    assert_eq!(
        td.patch_as(&other_lead, &path, json!({"card": c2}))
            .await
            .status,
        404
    );
    // The operator may link, but not change the text.
    assert_eq!(
        td.patch(&path, json!({"card": c2})).await.ok()["card"],
        c2.as_str()
    );
    assert_eq!(
        td.patch(&path, json!({"card": c1, "text": "x"}))
            .await
            .status,
        403
    );

    // The list filter: only the questions of the card.
    let other = ask(&td, &lead, json!({"text": "Another one?", "card": c1})).await;
    let of = |c: &str| {
        let td = &td;
        let c = c.to_owned();
        async move {
            td.get(&format!("/api/questions?card={c}&status=all"))
                .await
                .ok()
                .as_array()
                .unwrap()
                .iter()
                .map(id)
                .collect::<Vec<_>>()
        }
    };
    assert_eq!(of(&c2).await, vec![qid.clone()]);
    assert_eq!(of(&c1).await, vec![id(&other)]);
    assert_eq!(td.get("/api/questions?card=NOPE-9").await.status, 404);
    let r = td.get_as(&lead, &format!("/api/questions?card={c2}")).await;
    assert_eq!(r.ok().as_array().unwrap().len(), 1);
    assert_eq!(
        td.get_as(&other_lead, &format!("/api/questions?card={c2}"))
            .await
            .status,
        404
    );

    // A closed question: the leader cannot change its card. The operator
    // and a sysadmin may relink it, but cannot remove its only context.
    td.post(&format!("{path}/answer"), json!({"answer": "yes"}))
        .await
        .ok();
    td.patch_as(&lead, &path, json!({"card": null}))
        .await
        .err(409, "conflict");
    let v = td.patch_as(&sys, &path, json!({"card": c1})).await;
    assert_eq!(v.ok()["card"], c1.as_str());
    td.patch(&path, json!({"card": null}))
        .await
        .err(422, "invalid");
    assert!(of(&c1).await.contains(&qid));

    // The `question` event names the new card and the old one.
    let ev = td.get("/api/events?kind=question&limit=100").await;
    let last = ev
        .ok()
        .as_array()
        .unwrap()
        .iter()
        .rfind(|e| e["data"]["question_id"] == qid.as_str())
        .unwrap()
        .clone();
    assert_eq!(last["data"]["change"], "updated");
    assert_eq!(last["data"]["previous_card_id"], c2.as_str());
    assert_eq!(last["data"]["card_id"], c1.as_str());
    // The audit log has each change of the card.
    let audit = td.get("/api/audit?action=question.update&limit=50").await;
    let cards = audit
        .ok()
        .as_array()
        .unwrap()
        .iter()
        .filter(|a| a["detail"]["changed"] == json!(["card"]))
        .count();
    assert_eq!(cards, 3, "{}", audit.body);
}

/// `GET /api/projects/{name}/counts` (TER-154): the blocked cards and the
/// open questions of one project, for the project tabs of the dashboard.
#[tokio::test]
async fn project_counts_give_blocked_cards_and_open_questions() {
    let World {
        td,
        lead,
        worker,
        sys,
        other_lead,
    } = setup().await;
    let empty = td.get("/api/projects/p/counts").await.ok().clone();
    assert_eq!(
        empty,
        json!({"blocked_cards": 0, "questions": {"open": 0, "urgent": 0, "critical": 0}})
    );

    // Two blocked cards, one archived, and one card in another column.
    let mut blocked = Vec::new();
    for title in ["A", "B", "C"] {
        blocked.push(id(td
            .post(
                "/api/projects/p/cards",
                json!({"title": title, "column": "blocked", "comment": "Needs a decision"}),
            )
            .await
            .ok()));
    }
    td.post("/api/projects/p/cards", json!({"title": "Ready"}))
        .await
        .ok();
    td.post(
        "/api/projects/q/cards",
        json!({"title": "Q", "column": "blocked", "comment": "Needs a decision"}),
    )
    .await
    .ok();
    td.delete(&format!("/api/cards/{}", blocked[2])).await.ok();

    // Open questions of each kind that needs a decision; a notice, an
    // answered question, and another project's question do not count.
    ask(&td, &lead, json!({"text": "Normal?"})).await;
    ask(&td, &lead, json!({"text": "Urgent?", "priority": "urgent"})).await;
    ask(
        &td,
        &lead,
        json!({"kind": "bundle", "text": "Choices", "priority": "critical",
            "items": [{"text": "A?"}, {"text": "B?"}]}),
    )
    .await;
    ask(
        &td,
        &lead,
        json!({"kind": "notice", "text": "Done", "card": blocked[0]}),
    )
    .await;
    let answered = ask(&td, &lead, json!({"text": "Answered?"})).await;
    td.post(
        &format!("/api/questions/{}/answer", id(&answered)),
        json!({"answer": "Yes"}),
    )
    .await
    .ok();
    ask(&td, &other_lead, json!({"text": "Other project?"})).await;

    let want = json!({"blocked_cards": 2, "questions": {"open": 3, "urgent": 2, "critical": 1}});
    assert_eq!(td.get("/api/projects/p/counts").await.ok(), &want);
    // The project lead and a sysadmin read the questions too.
    assert_eq!(td.get_as(&lead, "/api/projects/p/counts").await.ok(), &want);
    assert_eq!(td.get_as(&sys, "/api/projects/p/counts").await.ok(), &want);
    // A worker may not list the questions: it reads only the cards.
    assert_eq!(
        td.get_as(&worker, "/api/projects/p/counts").await.ok(),
        &json!({"blocked_cards": 2, "questions": null})
    );
    // An agent of another project may not read the project at all.
    td.get_as(&other_lead, "/api/projects/p/counts")
        .await
        .err(403, "forbidden");
    td.get("/api/projects/nope/counts")
        .await
        .err(404, "not_found");
}
