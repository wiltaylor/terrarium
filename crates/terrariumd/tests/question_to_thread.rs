//! The "To thread" action of the Inbox (GitHub #30, TER-469, docs/api.md
//! §6.8): the operator moves an open question or notice into a new Inbox
//! thread with the asking agent. The question becomes `in_thread`, out of
//! the open count, and is answered or retracted from the thread.
#![allow(clippy::unwrap_used, clippy::expect_used)]

use crate::common;

use common::TestDaemon;
use serde_json::{Value, json};

fn id(v: &Value) -> String {
    v["id"].as_str().unwrap().to_owned()
}

struct Lab {
    td: TestDaemon,
    lead: String,
    worker: String,
}

async fn lab() -> Lab {
    let td = TestDaemon::start().await;
    td.project("p").await;
    let lead = id(&td
        .agent("p", "lead", json!({"roles": ["project_admin"]}))
        .await);
    let worker = id(&td.agent("p", "w1", json!({})).await);
    Lab { td, lead, worker }
}

impl Lab {
    async fn ask(&self, body: Value) -> Value {
        let r = self
            .td
            .post_as(
                &self.lead,
                &format!("/api/agents/{}/questions", self.lead),
                body,
            )
            .await;
        assert_eq!(r.status, 201, "{}", r.body);
        r.body
    }

    async fn question(&self, qid: &str) -> Value {
        self.td
            .get(&format!("/api/questions/{qid}"))
            .await
            .ok()
            .clone()
    }

    async fn thread(&self, tid: &str) -> Value {
        self.td
            .get(&format!("/api/threads/{tid}"))
            .await
            .ok()
            .clone()
    }

    /// The mail from the operator to the lead, the newest first.
    async fn mail(&self) -> Vec<Value> {
        self.td
            .get(&format!("/api/mail?to={}&from=operator", self.lead))
            .await
            .ok()
            .as_array()
            .unwrap()
            .clone()
    }

    async fn counts(&self) -> Value {
        self.td.get("/api/inbox/counts").await.ok().clone()
    }
}

fn bodies(mail: &[Value]) -> Vec<String> {
    mail.iter()
        .map(|m| m["body"].as_str().unwrap_or_default().to_owned())
        .collect()
}

#[tokio::test]
async fn to_thread_opens_a_thread_and_the_answer_settles_it() {
    let lab = lab().await;
    let card = lab
        .td
        .post("/api/projects/p/cards", json!({"title": "Retention"}))
        .await
        .ok()
        .clone();
    let cid = id(&card);
    let q = lab
        .ask(
            json!({"text": "Keep the logs 30 days?", "options": ["yes", "no"],
                    "context": "The disk is 80% full.", "card": cid}),
        )
        .await;
    let qid = id(&q);
    assert_eq!(lab.counts().await["question"], 1);

    // Only the operator moves it.
    lab.td
        .post_as(
            &lab.lead,
            &format!("/api/questions/{qid}/to-thread"),
            json!({}),
        )
        .await
        .err(403, "forbidden");

    let r = lab
        .td
        .post(
            &format!("/api/questions/{qid}/to-thread"),
            json!({"message": "Which logs fill the disk?"}),
        )
        .await;
    assert_eq!(r.status, 201, "{}", r.body);
    let moved = &r.body["question"];
    let tid = id(&r.body["thread"]);
    assert_eq!(moved["status"], "in_thread");
    assert_eq!(moved["thread"], tid.as_str());

    // The thread: the asking agent attached, the operator its opener, the
    // question quoted in the first message, the card linked, waiting on
    // the agent.
    let th = lab.thread(&tid).await;
    assert_eq!(th["agent"], lab.lead.as_str());
    assert_eq!(th["opened_by"], "operator");
    assert_eq!(th["scope"], "p");
    assert_eq!(th["status"], "waiting_agent");
    assert_eq!(th["from_question"], qid.as_str());
    assert_eq!(th["topic"], "Keep the logs 30 days?");
    let first = th["messages"][0]["text"].as_str().unwrap();
    for needle in [
        "> Keep the logs 30 days?",
        "> 1. yes",
        "> 2. no",
        "> The disk is 80% full.",
        "Which logs fill the disk?",
    ] {
        assert!(first.contains(needle), "{needle}\n---\n{first}");
    }
    assert_eq!(th["questions"][0]["id"], qid.as_str());
    assert_eq!(th["cards"][0]["card"], cid.as_str());

    // Out of the open count: no question item, and the thread waits on
    // the agent.
    let counts = lab.counts().await;
    assert_eq!(counts["question"], 0, "{counts}");
    assert_eq!(counts["thread"], 0, "{counts}");
    let open = lab.td.get("/api/questions?status=open").await.ok().clone();
    assert!(open.as_array().unwrap().is_empty(), "{open}");
    let moved_list = lab
        .td
        .get("/api/questions?status=in_thread")
        .await
        .ok()
        .clone();
    assert_eq!(moved_list[0]["id"], qid.as_str());

    // The agent got one mail on the thread of the question.
    let mail = lab.mail().await;
    assert_eq!(mail.len(), 1);
    assert_eq!(mail[0]["thread_id"], q["thread_id"]);
    let body = &bodies(&mail)[0];
    assert!(
        body.contains(&format!("terrarium thread post {tid}")),
        "{body}"
    );
    assert!(body.contains("in_thread"), "{body}");

    // Twice is a conflict; an update of the moved question too.
    lab.td
        .post_empty(&format!("/api/questions/{qid}/to-thread"))
        .await
        .err(409, "conflict");
    lab.td
        .patch_as(
            &lab.lead,
            &format!("/api/questions/{qid}"),
            json!({"text": "Keep them 60 days?"}),
        )
        .await
        .err(409, "conflict");

    // The agent replies in the thread: it waits on the operator. The
    // operator's reply goes to the agent as mail.
    lab.td
        .post_as(
            &lab.lead,
            &format!("/api/threads/{tid}/messages"),
            json!({"text": "The debug logs of the runner."}),
        )
        .await
        .ok();
    assert_eq!(lab.thread(&tid).await["status"], "waiting_operator");
    assert_eq!(lab.counts().await["thread"], 1);
    lab.td
        .post(
            &format!("/api/threads/{tid}/messages"),
            json!({"text": "Then drop the debug logs first."}),
        )
        .await
        .ok();
    let mail = lab.mail().await;
    assert_eq!(mail.len(), 2);
    assert!(
        bodies(&mail)
            .iter()
            .any(|b| b.contains("> Then drop the debug logs first.")),
        "{mail:?}"
    );

    // The answer from the thread settles the question; the link stays.
    lab.td
        .post(
            &format!("/api/questions/{qid}/answer"),
            json!({"option": "yes"}),
        )
        .await
        .ok();
    let q = lab.question(&qid).await;
    assert_eq!(q["status"], "answered");
    assert_eq!(q["thread"], tid.as_str());
    assert_eq!(lab.thread(&tid).await["status"], "waiting_agent");
    assert_eq!(lab.mail().await.len(), 3);
}

#[tokio::test]
async fn the_agent_retracts_from_the_thread_and_asks_a_revised_question() {
    let lab = lab().await;
    let q = lab.ask(json!({"text": "Ship on Friday?"})).await;
    let qid = id(&q);
    let r = lab
        .td
        .post_empty(&format!("/api/questions/{qid}/to-thread"))
        .await;
    assert_eq!(r.status, 201, "{}", r.body);
    let tid = id(&r.body["thread"]);
    // No card, no message: the first message has the question only.
    let th = lab.thread(&tid).await;
    assert!(th["cards"].as_array().unwrap().is_empty());

    // A question wait of the agent on the moved question stays open.
    // The agent retracts it from the thread, and asks a revised one there.
    lab.td
        .post_as(
            &lab.lead,
            &format!("/api/questions/{qid}/retract"),
            json!({"reason": "Asking a narrower question"}),
        )
        .await
        .ok();
    assert_eq!(lab.question(&qid).await["status"], "retracted");
    let revised = lab
        .ask(json!({"text": "Ship on Friday without the migration?", "thread": tid}))
        .await;
    assert_eq!(revised["thread"], tid.as_str());
    let th = lab.thread(&tid).await;
    assert_eq!(th["status"], "waiting_operator");
    assert_eq!(th["questions"].as_array().unwrap().len(), 2);
    // The retract by the agent itself sent no mail: only the move did.
    assert_eq!(lab.mail().await.len(), 1);

    // A close retracts the open revised question.
    lab.td
        .post_empty(&format!("/api/threads/{tid}/close"))
        .await
        .ok();
    assert_eq!(lab.question(&id(&revised)).await["status"], "retracted");
}

#[tokio::test]
async fn a_notice_moves_and_is_acknowledged_and_a_close_retracts_in_thread() {
    let lab = lab().await;
    let card = id(lab.td.post("/api/projects/p/cards", json!({"title": "Backup recovery"})).await.ok());
    let n = lab
        .ask(json!({"kind": "notice", "text": "The backup failed twice.", "priority": "urgent", "card": card}))
        .await;
    let nid = id(&n);
    let r = lab
        .td
        .post_empty(&format!("/api/questions/{nid}/to-thread"))
        .await;
    assert_eq!(r.status, 201, "{}", r.body);
    assert_eq!(r.body["question"]["status"], "in_thread");
    let counts = lab.counts().await;
    assert_eq!(counts["notice"], 0, "{counts}");
    assert_eq!(counts["urgent"], 0, "{counts}");
    lab.td
        .post(
            &format!("/api/questions/{nid}/ack"),
            json!({"text": "Seen, the disk is replaced."}),
        )
        .await
        .ok();
    assert_eq!(lab.question(&nid).await["status"], "acknowledged");

    // A second question, moved, then the thread closes: it is retracted.
    let q = lab.ask(json!({"text": "Rotate the keys now?"})).await;
    let qid = id(&q);
    let r = lab
        .td
        .post_empty(&format!("/api/questions/{qid}/to-thread"))
        .await;
    let tid = id(&r.body["thread"]);
    lab.td
        .post_empty(&format!("/api/threads/{tid}/close"))
        .await
        .ok();
    let q = lab.question(&qid).await;
    assert_eq!(q["status"], "retracted");
    assert_eq!(q["retract_reason"], format!("thread {tid} was closed"));
}

#[tokio::test]
async fn bundles_tool_runs_and_settled_questions_do_not_move() {
    let lab = lab().await;
    let b = lab
        .ask(json!({"kind": "bundle", "text": "Release plan",
                    "items": [{"text": "Friday?"}, {"text": "Region?"}]}))
        .await;
    lab.td
        .post_empty(&format!("/api/questions/{}/to-thread", id(&b)))
        .await
        .err(409, "conflict");

    let q = lab.ask(json!({"text": "Use the new runner?"})).await;
    let qid = id(&q);
    lab.td
        .post(
            &format!("/api/questions/{qid}/answer"),
            json!({"answer": "yes"}),
        )
        .await
        .ok();
    lab.td
        .post_empty(&format!("/api/questions/{qid}/to-thread"))
        .await
        .err(409, "conflict");

    // A question asked in a thread is in its thread already.
    let th = lab
        .td
        .post(
            "/api/threads",
            json!({"topic": "Runner", "scope": "p", "agent": "p/lead"}),
        )
        .await
        .ok()
        .clone();
    let inq = lab
        .ask(json!({"text": "Which runner?", "thread": th["id"]}))
        .await;
    lab.td
        .post_empty(&format!("/api/questions/{}/to-thread", id(&inq)))
        .await
        .err(409, "conflict");

    // A message that is too long is invalid; an unknown field a bad body.
    let q2 = lab.ask(json!({"text": "Another one?"})).await;
    lab.td
        .post(
            &format!("/api/questions/{}/to-thread", id(&q2)),
            json!({"message": "x".repeat(8001)}),
        )
        .await
        .err(422, "invalid");
    lab.td
        .post(
            &format!("/api/questions/{}/to-thread", id(&q2)),
            json!({"text": "x"}),
        )
        .await
        .err(400, "bad_request");
    // A worker is no reader of the question.
    lab.td
        .post_as(
            &lab.worker,
            &format!("/api/questions/{}/to-thread", id(&q2)),
            json!({}),
        )
        .await
        .err(404, "not_found");
}

#[tokio::test]
async fn a_deleted_agent_retracts_its_moved_questions() {
    let lab = lab().await;
    let q = lab.ask(json!({"text": "Merge the batch?"})).await;
    let qid = id(&q);
    lab.td
        .post_empty(&format!("/api/questions/{qid}/to-thread"))
        .await;
    // The to-thread call starts a turn for the asking agent, so its session
    // can be live here. The test is about the retraction, not the confirm
    // guard (TER-594).
    lab.td
        .delete(&format!("/api/agents/{}?confirm=true", lab.lead))
        .await
        .ok();
    assert_eq!(lab.question(&qid).await["status"], "retracted");
}

/// TER-495: an `in_thread` question keeps its thread on the operator, as an
/// open one does. The asking agent sets no `waiting_agent` or `tracking`
/// until it is settled, so the thread does not drop out of the Inbox.
#[tokio::test]
async fn an_in_thread_question_keeps_the_thread_on_the_operator() {
    let lab = lab().await;
    let q = lab.ask(json!({"text": "Drop the old index?"})).await;
    let qid = id(&q);
    let r = lab
        .td
        .post_empty(&format!("/api/questions/{qid}/to-thread"))
        .await;
    assert_eq!(r.status, 201, "{}", r.body);
    let tid = id(&r.body["thread"]);

    for status in ["waiting_agent", "tracking"] {
        lab.td
            .post_as(
                &lab.lead,
                &format!("/api/threads/{tid}/status"),
                json!({"status": status}),
            )
            .await
            .err(409, "conflict");
        lab.td
            .post_as(
                &lab.lead,
                &format!("/api/threads/{tid}/messages"),
                json!({"text": "On it.", "status": status}),
            )
            .await
            .err(409, "conflict");
    }
    lab.td
        .post_empty_as(&lab.lead, &format!("/api/threads/{tid}/track"))
        .await
        .err(409, "conflict");

    // A plain reply of the agent: the thread waits on the operator, and the
    // Inbox counts it.
    lab.td
        .post_as(
            &lab.lead,
            &format!("/api/threads/{tid}/messages"),
            json!({"text": "It is unused since March."}),
        )
        .await
        .ok();
    assert_eq!(lab.thread(&tid).await["status"], "waiting_operator");
    assert_eq!(lab.counts().await["thread"], 1);

    // Settled, the agent sets its status again.
    lab.td
        .post(
            &format!("/api/questions/{qid}/answer"),
            json!({"answer": "yes"}),
        )
        .await
        .ok();
    let th = lab
        .td
        .post_as(
            &lab.lead,
            &format!("/api/threads/{tid}/status"),
            json!({"status": "tracking"}),
        )
        .await
        .ok()
        .clone();
    assert_eq!(th["status"], "tracking");
}

/// TER-514: the operator moved the question into the thread and decides
/// there. Its `waiting_agent` passes the `in_thread` question, the agent's
/// stays refused, and the question stays in the thread.
#[tokio::test]
async fn the_operator_sets_waiting_agent_past_an_in_thread_question() {
    let lab = lab().await;
    let q = lab.ask(json!({"text": "Drop the old index?"})).await;
    let qid = id(&q);
    let r = lab
        .td
        .post_empty(&format!("/api/questions/{qid}/to-thread"))
        .await;
    assert_eq!(r.status, 201, "{}", r.body);
    let tid = id(&r.body["thread"]);
    lab.td
        .post_as(
            &lab.lead,
            &format!("/api/threads/{tid}/messages"),
            json!({"text": "It is unused since March."}),
        )
        .await
        .ok();
    assert_eq!(lab.thread(&tid).await["status"], "waiting_operator");
    assert_eq!(lab.counts().await["thread"], 1);

    let th = lab
        .td
        .post(
            &format!("/api/threads/{tid}/status"),
            json!({"status": "waiting_agent"}),
        )
        .await
        .ok()
        .clone();
    assert_eq!(th["status"], "waiting_agent");
    assert_eq!(lab.counts().await["thread"], 0);
    assert_eq!(lab.question(&qid).await["status"], "in_thread");

    lab.td
        .post_as(
            &lab.lead,
            &format!("/api/threads/{tid}/status"),
            json!({"status": "waiting_agent"}),
        )
        .await
        .err(409, "conflict");
}

/// TER-495: the quotes of the first message can make it longer than a
/// thread message while each part passes its own limit. The refusal says
/// why and what to do, and nothing moves.
#[tokio::test]
async fn a_first_message_over_the_thread_limit_is_explained() {
    let lab = lab().await;
    // Each line of the context gets a "> ": 16000 bytes quote to 32000.
    let q = lab
        .ask(json!({"text": "x".repeat(1500), "context": "a\n".repeat(8000)}))
        .await;
    let qid = id(&q);
    let r = lab
        .td
        .post_empty(&format!("/api/questions/{qid}/to-thread"))
        .await;
    r.err(422, "invalid");
    let msg = r.body["error"]["message"].as_str().unwrap();
    assert!(msg.contains("first message of the thread"), "{msg}");
    assert!(msg.contains("answer it in the Inbox"), "{msg}");
    assert!(!msg.contains("your message"), "{msg}");
    assert_eq!(lab.question(&qid).await["status"], "open");

    // A message within its own limit that does not fit with the quote.
    let q = lab
        .ask(json!({"text": "Rotate now?", "context": "c".repeat(16000)}))
        .await;
    let qid = id(&q);
    let r = lab
        .td
        .post(
            &format!("/api/questions/{qid}/to-thread"),
            json!({"message": "\u{20ac}".repeat(8000)}),
        )
        .await;
    r.err(422, "invalid");
    let msg = r.body["error"]["message"].as_str().unwrap();
    assert!(msg.contains("your message"), "{msg}");
    assert!(msg.contains("send a shorter message"), "{msg}");
    assert_eq!(lab.question(&qid).await["status"], "open");
    let threads = lab.td.get("/api/threads").await.ok().clone();
    assert!(threads.as_array().unwrap().is_empty(), "{threads}");
}
