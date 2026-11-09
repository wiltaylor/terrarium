//! The attached agent sets the status of its thread (GitHub #28,
//! docs/api.md §6.13): `waiting_agent` or `tracking` with its message or
//! with `POST /api/threads/{id}/status`, and a card linked after its
//! hand-off message moves the thread into tracking. A question keeps the
//! thread on the operator, and an operator message sets `waiting_agent`.
#![allow(clippy::unwrap_used, clippy::expect_used)]

use crate::common;

use common::TestDaemon;
use serde_json::{Value, json};

struct Lab {
    td: TestDaemon,
    /// `p/lead`: the lead of project `p`.
    lead: String,
    /// `p/arch`: the attached agent.
    arch: String,
    /// The thread of `p/arch`, with a first message of the operator.
    tid: String,
}

fn id(v: &Value) -> String {
    v["id"].as_str().unwrap().to_owned()
}

async fn lab() -> Lab {
    let td = TestDaemon::start().await;
    td.project("p").await;
    let lead = id(&td
        .agent("p", "lead", json!({"roles": ["project_admin"]}))
        .await);
    let arch = id(&td.agent("p", "arch", json!({})).await);
    let r = td
        .post(
            "/api/threads",
            json!({"topic": "Retention", "scope": "p", "agent": "p/arch",
                   "text": "Please add a retention setting."}),
        )
        .await;
    assert_eq!(r.status, 201, "{}", r.body);
    let tid = id(&r.body);
    Lab {
        td,
        lead,
        arch,
        tid,
    }
}

impl Lab {
    async fn post(&self, who: &str, body: Value) -> common::Resp {
        let path = format!("/api/threads/{}/messages", self.tid);
        if who == "operator" {
            self.td.post(&path, body).await
        } else {
            self.td.post_as(who, &path, body).await
        }
    }

    async fn status(&self, who: &str, status: &str) -> common::Resp {
        let path = format!("/api/threads/{}/status", self.tid);
        let body = json!({"status": status});
        if who == "operator" {
            self.td.post(&path, body).await
        } else {
            self.td.post_as(who, &path, body).await
        }
    }

    async fn thread(&self) -> Value {
        self.td
            .get(&format!("/api/threads/{}", self.tid))
            .await
            .ok()
            .clone()
    }

    async fn counted(&self) -> i64 {
        let c = self.td.get("/api/inbox/counts").await.ok().clone();
        c["thread"].as_i64().unwrap()
    }

    async fn card(&self, title: &str) -> String {
        let r = self
            .td
            .post("/api/projects/p/cards", json!({"title": title}))
            .await;
        assert_eq!(r.status, 201, "{}", r.body);
        id(&r.body)
    }

    async fn ask(&self) -> String {
        let r = self
            .td
            .post_as(
                &self.arch,
                &format!("/api/agents/{}/questions", self.arch),
                json!({"text": "Keep 30 days?", "options": ["yes", "no"], "thread": self.tid}),
            )
            .await;
        assert_eq!(r.status, 201, "{}", r.body);
        id(&r.body)
    }
}

#[tokio::test]
async fn the_agent_sets_waiting_agent_or_tracking_with_its_message() {
    let lab = lab().await;
    // A plain message of the agent waits on the operator, as before.
    let r = lab.post(&lab.arch, json!({"text": "Noted."})).await;
    assert_eq!(r.status, 201, "{}", r.body);
    assert_eq!(r.body["status"], "waiting_operator");
    assert_eq!(lab.counted().await, 1);

    // The hand-off message that starts the work: waiting on the agent, not
    // in the Inbox badge.
    let r = lab
        .post(
            &lab.arch,
            json!({"text": "Starting now.", "status": "waiting_agent"}),
        )
        .await;
    assert_eq!(r.status, 201, "{}", r.body);
    assert_eq!(r.body["status"], "waiting_agent");
    assert!(r.body["tracking_at"].is_null(), "{}", r.body);
    assert_eq!(lab.counted().await, 0);

    // With `tracking`, the message moves the thread into tracking.
    let r = lab
        .post(
            &lab.arch,
            json!({"text": "Handed off to the board.", "status": "tracking"}),
        )
        .await;
    assert_eq!(r.status, 201, "{}", r.body);
    assert_eq!(r.body["status"], "tracking");
    assert!(r.body["tracking_at"].is_string(), "{}", r.body);
    assert_eq!(lab.counted().await, 0);
    let th = lab.thread().await;
    assert_eq!(th["messages"].as_array().unwrap().len(), 4, "{th}");

    // The operator sets no status: its message sets waiting_agent.
    lab.post("operator", json!({"text": "Thanks.", "status": "tracking"}))
        .await
        .err(422, "invalid");
    let r = lab.post("operator", json!({"text": "Thanks."})).await;
    assert_eq!(r.status, 201, "{}", r.body);
    assert_eq!(r.body["status"], "waiting_agent");

    // Only waiting_agent and tracking.
    for bad in ["waiting_operator", "closed", "busy"] {
        lab.post(&lab.arch, json!({"text": "x", "status": bad}))
            .await
            .err(422, "invalid");
    }
    assert_eq!(lab.thread().await["messages"].as_array().unwrap().len(), 5);
}

#[tokio::test]
async fn the_status_verb_is_for_the_attached_agent_and_a_question_wins() {
    let lab = lab().await;
    lab.post(&lab.arch, json!({"text": "Will do."})).await;
    assert_eq!(lab.thread().await["status"], "waiting_operator");

    // Another agent and the operator do not set it.
    lab.status(&lab.lead, "waiting_agent")
        .await
        .err(403, "forbidden");
    lab.status("operator", "waiting_agent")
        .await
        .err(403, "forbidden");
    lab.status(&lab.arch, "waiting_operator")
        .await
        .err(422, "invalid");

    let r = lab.status(&lab.arch, "waiting_agent").await;
    assert_eq!(r.status, 200, "{}", r.body);
    assert_eq!(r.body["status"], "waiting_agent");
    assert_eq!(lab.counted().await, 0);

    // An open question keeps the thread on the operator.
    let q = lab.ask().await;
    assert_eq!(lab.thread().await["status"], "waiting_operator");
    for s in ["waiting_agent", "tracking"] {
        lab.status(&lab.arch, s).await.err(409, "conflict");
        lab.post(&lab.arch, json!({"text": "x", "status": s}))
            .await
            .err(409, "conflict");
    }
    let th = lab.thread().await;
    assert_eq!(th["status"], "waiting_operator");
    assert_eq!(th["messages"].as_array().unwrap().len(), 2, "{th}");

    lab.td
        .post(
            &format!("/api/questions/{q}/answer"),
            json!({"option": "yes"}),
        )
        .await
        .ok();
    let r = lab.status(&lab.arch, "tracking").await;
    assert_eq!(r.status, 200, "{}", r.body);
    assert_eq!(r.body["status"], "tracking");
    assert!(r.body["tracking_at"].is_string(), "{}", r.body);
}

#[tokio::test]
async fn a_card_linked_after_the_hand_off_message_tracks_the_thread() {
    let lab = lab().await;
    let link = |card: String| {
        let td = &lab.td;
        let path = format!("/api/threads/{}/cards", lab.tid);
        async move { td.post(&path, json!({"card": card})).await }
    };

    // The last message is the operator's: the link changes nothing.
    let c1 = lab.card("Retention setting").await;
    let r = link(c1).await;
    assert_eq!(r.status, 200, "{}", r.body);
    assert_eq!(r.body["status"], "waiting_agent");

    // An open question: no tracking.
    let q = lab.ask().await;
    let r = link(lab.card("Retention docs").await).await;
    assert_eq!(r.body["status"], "waiting_operator", "{}", r.body);
    lab.td
        .post(
            &format!("/api/questions/{q}/answer"),
            json!({"option": "yes"}),
        )
        .await
        .ok();

    // The agent's hand-off message, then the lead makes the card in the
    // thread: the thread tracks.
    lab.post(&lab.arch, json!({"text": "The lead will make the card."}))
        .await;
    assert_eq!(lab.thread().await["status"], "waiting_operator");
    let r = lab
        .td
        .post_as(
            &lab.lead,
            "/api/projects/p/cards",
            json!({"title": "Retention migration", "inbox_thread": lab.tid}),
        )
        .await;
    assert_eq!(r.status, 201, "{}", r.body);
    let th = lab.thread().await;
    assert_eq!(th["status"], "tracking", "{th}");
    assert!(th["tracking_at"].is_string(), "{th}");
    assert_eq!(lab.counted().await, 0);

    // An operator message still sets waiting_agent.
    lab.post("operator", json!({"text": "And the docs?"})).await;
    assert_eq!(lab.thread().await["status"], "waiting_agent");
}

#[tokio::test]
async fn a_card_link_by_the_lead_tracks_after_the_agents_last_message() {
    let lab = lab().await;
    lab.post(&lab.arch, json!({"text": "Handing this to the board."}))
        .await;
    let card = lab.card("Retention setting").await;
    let r = lab
        .td
        .post_as(
            &lab.lead,
            &format!("/api/threads/{}/cards", lab.tid),
            json!({"card": card}),
        )
        .await;
    assert_eq!(r.status, 200, "{}", r.body);
    assert_eq!(r.body["status"], "tracking", "{}", r.body);
    assert_eq!(r.body["cards"].as_array().unwrap().len(), 1, "{}", r.body);
}
