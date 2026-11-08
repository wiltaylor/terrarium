//! The wake rules, the test loop and the overall response sent again
//! (TER-148, TER-144 D4, D5, D10, docs/api.md §6.13), with the fake
//! supervisor: a thread that tracks linked cards of two projects wakes its
//! agent when a card goes to blocked and when all its cards are in review
//! or later; on "found an issue" a bug card on the same thread starts the
//! loop again; after "works" the thread closes. No harness and no model
//! turn runs.
#![allow(clippy::unwrap_used, clippy::expect_used)]

mod common;

use std::time::Duration;

use common::TestDaemon;
use serde_json::{Value, json};
use terrarium_core::AgentId;
use terrarium_core::config::{CompactMaxAge, IdleStop};

fn state(td: &TestDaemon) -> terrariumd::api::Shared {
    td.daemon.as_ref().unwrap().state.clone()
}

fn fast(td: &TestDaemon) {
    let s = state(td);
    s.lifecycle.set_tuning(terrariumd::lifecycle::Tuning {
        idle_stop: IdleStop::Off,
        tick: Duration::from_millis(20),
        compact_on_idle: false,
        compact_max_age: CompactMaxAge::Off,
        compact_wait: Duration::from_secs(5),
    });
    s.lifecycle.spawns.set_tuning(terrariumd::spawn::Tuning {
        done_delay: Duration::from_millis(50),
    });
    s.waits.set_tuning(terrariumd::waits::Tuning {
        tick: Duration::from_millis(20),
        wake_debounce: Duration::from_millis(50),
        ..terrariumd::waits::Tuning::default()
    });
}

fn id(v: &Value) -> String {
    v["id"].as_str().unwrap().to_owned()
}

/// Poll `f` until it is true, with a deadline.
async fn until<F: std::future::Future<Output = bool>>(what: &str, mut f: impl FnMut() -> F) {
    let start = std::time::Instant::now();
    while start.elapsed() < Duration::from_secs(60) {
        if f().await {
            return;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    panic!("timed out waiting for {what}");
}

const BLOCKED_TURN: &str = "A card that this thread tracks went to blocked.";
const REVIEW_TURN: &str = "All cards that this thread tracks are in review or later.";

struct Lab {
    td: TestDaemon,
    /// `p/lead`, the report-to lead of the `p` threads.
    lead: String,
}

/// Projects `meta`, `p` and `q`, each with a lead; the templates `consult`
/// (a member of `p`) and `arch` (an architect in `p`: it reads `q` too).
async fn lab() -> Lab {
    let td = TestDaemon::start().await;
    fast(&td);
    for p in ["meta", "p", "q"] {
        td.project(p).await;
    }
    td.agent("meta", "lead", json!({"roles": ["project_admin"]}))
        .await;
    td.agent("q", "lead", json!({"roles": ["project_admin"]}))
        .await;
    let lead = id(&td
        .agent("p", "lead", json!({"roles": ["project_admin"]}))
        .await);
    for (name, spec) in [
        ("consult", json!({"harness": "claude"})),
        ("arch", json!({"harness": "claude", "roles": ["architect"]})),
    ] {
        let r = td
            .post(
                "/api/templates",
                json!({"name": name, "project": "p", "spec": spec}),
            )
            .await;
        assert_eq!(r.status, 201, "{}", r.body);
    }
    Lab { td, lead }
}

impl Lab {
    async fn thread(&self, tid: &str) -> Value {
        self.td
            .get(&format!("/api/threads/{tid}"))
            .await
            .ok()
            .clone()
    }

    async fn agent(&self, eph: &str) -> Value {
        self.td
            .get(&format!("/api/agents/{eph}"))
            .await
            .ok()
            .clone()
    }

    async fn spawn_row(&self, eph: &str) -> Value {
        self.td
            .get(&format!("/api/spawns/{eph}"))
            .await
            .ok()
            .clone()
    }

    /// Wait until `eph` is parked and stopped at rest.
    async fn parked(&self, eph: &str) {
        until("the park stop", || async {
            let a = self.agent(eph).await;
            a["status"] == "stopped" && a["status_reason"] == "parked"
        })
        .await;
    }

    /// The prompts of `eph` that start with `start`.
    fn prompts(&self, eph: &str, start: &str) -> Vec<String> {
        let aid: AgentId = eph.parse().unwrap();
        self.td
            .sup
            .prompts(aid)
            .into_iter()
            .filter(|p| p.starts_with(start))
            .collect()
    }

    /// Wait for the `n`th prompt of `eph` that starts with `start`.
    async fn nth_prompt(&self, eph: &str, start: &str, n: usize) -> String {
        until(&format!("turn {n} `{start}`"), || async {
            self.prompts(eph, start).len() >= n
        })
        .await;
        self.prompts(eph, start).swap_remove(n - 1)
    }

    /// A thread of `p` with an agent from `template`, past its first turn.
    async fn started(&self, template: &str, topic: &str) -> (String, String) {
        let r = self
            .td
            .post(
                "/api/threads",
                json!({"topic": topic, "scope": "p", "template": template,
                       "text": "Plan the cache."}),
            )
            .await;
        assert_eq!(r.status, 201, "{}", r.body);
        let tid = id(&r.body);
        let eph = r.body["agent"].as_str().unwrap().to_owned();
        self.nth_prompt(&eph, &format!("[Thread:{tid}]"), 1).await;
        self.td.wait_status(&eph, "idle").await;
        (tid, eph)
    }

    /// A card of `project`, made by the operator.
    async fn card(&self, project: &str, title: &str) -> String {
        let r = self
            .td
            .post(
                &format!("/api/projects/{project}/cards"),
                json!({"title": title}),
            )
            .await;
        assert_eq!(r.status, 201, "{}", r.body);
        id(&r.body)
    }

    async fn link(&self, tid: &str, card: &str) {
        let r = self
            .td
            .post(&format!("/api/threads/{tid}/cards"), json!({"card": card}))
            .await;
        assert_eq!(r.status, 200, "{}", r.body);
    }

    /// The operator moves `card` to `column`.
    async fn mv(&self, card: &str, column: &str, comment: Option<&str>) {
        let mut body = json!({"column": column});
        if let Some(c) = comment {
            body["comment"] = json!(c);
        }
        let r = self.td.post(&format!("/api/cards/{card}/move"), body).await;
        assert_eq!(r.status, 200, "{}", r.body);
    }

    async fn post_as(&self, eph: &str, tid: &str, text: &str) -> Value {
        let r = self
            .td
            .post_as(
                eph,
                &format!("/api/threads/{tid}/messages"),
                json!({"text": text}),
            )
            .await;
        assert_eq!(r.status, 201, "{}", r.body);
        r.body
    }

    async fn track(&self, eph: &str, tid: &str) {
        let r = self
            .td
            .post_empty_as(eph, &format!("/api/threads/{tid}/track"))
            .await;
        assert_eq!(r.status, 200, "{}", r.body);
        assert_eq!(r.body["status"], "tracking", "{}", r.body);
    }

    /// The agent asks "Does it work?" in the thread; the operator answers
    /// `answer`. Returns when the agent woke on the answer.
    async fn ask_works(&self, eph: &str, tid: &str, answer: &str) {
        let r = self
            .td
            .post_as(
                eph,
                &format!("/api/agents/{eph}/questions"),
                json!({"text": "Does it work?", "options": ["works", "found an issue"],
                       "thread": tid}),
            )
            .await;
        assert_eq!(r.status, 201, "{}", r.body);
        let qid = id(&r.body);
        assert_eq!(self.thread(tid).await["status"], "waiting_operator");
        self.parked(eph).await;
        self.td
            .post(
                &format!("/api/questions/{qid}/answer"),
                json!({"option": answer}),
            )
            .await
            .ok();
        assert_eq!(self.thread(tid).await["status"], "waiting_agent");
        until("the wake on the answer", || async {
            self.spawn_row(eph).await["parked_at"].is_null()
        })
        .await;
        self.td.wait_status(eph, "idle").await;
    }

    /// The `thread.card_wake` audit rows of thread `tid`, oldest first.
    async fn wakes(&self, tid: &str) -> Vec<String> {
        let rows = self
            .td
            .get("/api/audit?action=thread.card_wake")
            .await
            .ok()
            .clone();
        let mut out: Vec<(i64, String)> = rows
            .as_array()
            .unwrap()
            .iter()
            .filter(|r| r["target"] == tid)
            .map(|r| {
                (
                    r["seq"].as_i64().unwrap_or_default(),
                    r["detail"]["reason"]
                        .as_str()
                        .unwrap_or_default()
                        .to_owned(),
                )
            })
            .collect();
        out.sort();
        out.into_iter().map(|(_, r)| r).collect()
    }
}

#[tokio::test]
async fn a_thread_with_cards_of_two_projects_wakes_and_closes_after_works() {
    let lab = lab().await;
    let (tid, eph) = lab.started("arch", "Cache layer").await;
    let pc = lab.card("p", "Cache in p").await;
    let qc = lab.card("q", "Cache in q").await;
    lab.link(&tid, &format!("p:{pc}")).await;
    lab.link(&tid, &format!("q:{qc}")).await;
    lab.track(&eph, &tid).await;
    lab.parked(&eph).await;
    let tag = format!("[Thread:{tid}] ");
    let blocked = format!("{tag}{BLOCKED_TURN}");
    let review = format!("{tag}{REVIEW_TURN}");

    // A move between two columns before review wakes nothing.
    lab.mv(&pc, "in_progress", None).await;
    lab.mv(&qc, "in_progress", None).await;
    assert_eq!(lab.thread(&tid).await["status"], "tracking");

    // A linked card of `q` goes to blocked: the agent wakes with the
    // cards of both projects and the move comment.
    lab.mv(&qc, "blocked", Some("Need the cache host.")).await;
    let th = lab.thread(&tid).await;
    assert_eq!(th["status"], "waiting_agent", "{th:#}");
    assert!(th["tracking_at"].is_string(), "{th:#}");
    let turn = lab.nth_prompt(&eph, &blocked, 1).await;
    assert!(
        turn.contains(&format!("- q:{qc} [blocked (blocked)] Cache in q")),
        "{turn}"
    );
    assert!(
        turn.contains(&format!("- p:{pc} [in_progress] Cache in p")),
        "{turn}"
    );
    assert!(turn.contains("> Need the cache host."), "{turn}");
    assert!(turn.contains("is tracking"), "{turn}");
    lab.td.wait_status(&eph, "idle").await;
    // Its reply puts the thread back into tracking.
    let th = lab
        .post_as(&eph, &tid, "The q card waits on the cache host.")
        .await;
    assert_eq!(th["status"], "tracking", "{th}");
    lab.parked(&eph).await;

    // Out of blocked, and one card in review: not all cards are in review.
    lab.mv(&qc, "in_progress", None).await;
    lab.mv(&pc, "review", None).await;
    assert_eq!(lab.thread(&tid).await["status"], "tracking");
    // The last card reaches review: all are in review or later.
    lab.mv(&qc, "review", None).await;
    assert_eq!(lab.thread(&tid).await["status"], "waiting_agent");
    let turn = lab.nth_prompt(&eph, &review, 1).await;
    assert!(turn.contains(&format!("- p:{pc} [review]")), "{turn}");
    assert!(turn.contains(&format!("- q:{qc} [review]")), "{turn}");
    assert!(turn.contains("found an issue"), "{turn}");
    lab.td.wait_status(&eph, "idle").await;
    assert_eq!(lab.wakes(&tid).await, ["blocked", "review"]);

    // A move between two columns of review or later wakes nothing more.
    lab.mv(&pc, "merge_queue", None).await;
    assert_eq!(lab.wakes(&tid).await, ["blocked", "review"]);

    // The test loop (D5): the agent posts the test steps and asks; the
    // operator found an issue.
    lab.post_as(
        &eph,
        &tid,
        "Shipped in 0.6.0-alpha. Test: open the cache page.",
    )
    .await;
    lab.parked(&eph).await;
    lab.ask_works(&eph, &tid, "found an issue").await;
    // The lead makes a bug card on the same thread; the agent tracks again.
    let r = lab
        .td
        .post_as(
            &lab.lead,
            "/api/projects/p/cards",
            json!({"title": "Cache page is empty", "inbox_thread": tid}),
        )
        .await;
    assert_eq!(r.status, 201, "{}", r.body);
    let bug = id(&r.body);
    lab.track(&eph, &tid).await;
    lab.parked(&eph).await;
    assert_eq!(lab.thread(&tid).await["cards"].as_array().unwrap().len(), 3);
    lab.mv(&bug, "in_progress", None).await;
    assert_eq!(lab.thread(&tid).await["status"], "tracking");
    lab.mv(&bug, "review", None).await;
    assert_eq!(lab.thread(&tid).await["status"], "waiting_agent");
    let turn = lab.nth_prompt(&eph, &review, 2).await;
    assert!(
        turn.contains(&format!("- p:{bug} [review] Cache page is empty")),
        "{turn}"
    );
    assert!(turn.contains(&format!("- p:{pc} [merge_queue]")), "{turn}");
    lab.td.wait_status(&eph, "idle").await;
    assert_eq!(lab.wakes(&tid).await, ["blocked", "review", "review"]);

    // Works: the agent proposes the close and the operator confirms it.
    lab.ask_works(&eph, &tid, "works").await;
    let r = lab
        .td
        .post_as(
            &eph,
            &format!("/api/threads/{tid}/close"),
            json!({"summary": "Cache layer works.",
                   "links": [{"kind": "card", "ref": pc}, {"kind": "card", "ref": bug}]}),
        )
        .await;
    assert_eq!(r.status, 200, "{}", r.body);
    let r = lab
        .td
        .post(
            &format!("/api/threads/{tid}/close"),
            json!({"confirm": true}),
        )
        .await;
    assert_eq!(r.status, 200, "{}", r.body);
    assert_eq!(r.body["status"], "closed", "{}", r.body);
    until("the close of the agent", || async {
        lab.spawn_row(&eph).await["finished_at"].is_string()
    })
    .await;

    // A closed thread wakes nothing.
    lab.mv(&qc, "blocked", Some("Again.")).await;
    assert_eq!(lab.wakes(&tid).await, ["blocked", "review", "review"]);
    assert_eq!(lab.thread(&tid).await["status"], "closed");
}

#[tokio::test]
async fn tracking_ready_cards_and_archiving_unfinished_cards_wake_review() {
    let lab = lab().await;
    for archive in [false, true] {
        let (tid, eph) = lab.started("consult", "Ready work").await;
        let ready = lab.card("p", "Ready").await;
        lab.link(&tid, &format!("p:{ready}")).await;
        lab.mv(&ready, "review", None).await;
        let dropped = if archive {
            let card = lab.card("p", "Dropped").await;
            lab.link(&tid, &format!("p:{card}")).await;
            Some(card)
        } else {
            None
        };
        let r = lab
            .td
            .post_empty_as(&eph, &format!("/api/threads/{tid}/track"))
            .await;
        assert_eq!(r.status, 200, "{}", r.body);
        if let Some(card) = dropped {
            lab.parked(&eph).await;
            lab.td.delete(&format!("/api/cards/{card}")).await.ok();
        }
        assert_eq!(lab.thread(&tid).await["status"], "waiting_agent");
        lab.nth_prompt(&eph, &format!("[Thread:{tid}] {REVIEW_TURN}"), 1)
            .await;
        assert_eq!(lab.wakes(&tid).await, ["review"]);
    }
}

#[tokio::test]
async fn card_events_and_agent_replies_preserve_open_question_waits() {
    let lab = lab().await;
    let (tid, eph) = lab.started("consult", "Question pending").await;
    let card = lab.card("p", "Work").await;
    lab.link(&tid, &format!("p:{card}")).await;
    lab.track(&eph, &tid).await;
    lab.parked(&eph).await;
    lab.td
        .post_as(
            &eph,
            &format!("/api/agents/{eph}/questions"),
            json!({"text": "Proceed?", "options": ["yes", "no"], "thread": tid}),
        )
        .await
        .ok();
    lab.parked(&eph).await;
    lab.mv(&card, "blocked", Some("Need a decision.")).await;
    lab.mv(&card, "review", None).await;
    assert_eq!(lab.thread(&tid).await["status"], "waiting_operator");
    assert!(lab.wakes(&tid).await.is_empty());
    let th = lab
        .post_as(&eph, &tid, "Still waiting for your answer.")
        .await;
    assert_eq!(th["status"], "waiting_operator");
    lab.parked(&eph).await;
}

#[tokio::test]
async fn only_a_thread_that_tracks_wakes_on_its_cards() {
    let lab = lab().await;
    let (tid, eph) = lab.started("consult", "Not tracking").await;
    let pc = lab.card("p", "Work").await;
    lab.link(&tid, &format!("p:{pc}")).await;
    // The agent's message makes the thread wait on the operator: no
    // tracking, so a blocked card and a card in review wake nothing.
    let th = lab.post_as(&eph, &tid, "Two options below.").await;
    assert_eq!(th["status"], "waiting_operator");
    lab.parked(&eph).await;
    lab.mv(&pc, "blocked", Some("Stuck.")).await;
    lab.mv(&pc, "review", None).await;
    assert!(lab.wakes(&tid).await.is_empty());
    assert_eq!(lab.thread(&tid).await["status"], "waiting_operator");
    assert!(
        lab.prompts(&eph, &format!("[Thread:{tid}] All cards"))
            .is_empty()
    );

    // A thread that tracks with an archived card and a card in review:
    // the archived card does not count.
    let (t2, e2) = lab.started("consult", "Archived").await;
    let a = lab.card("p", "Dropped").await;
    let b = lab.card("p", "Kept").await;
    lab.link(&t2, &format!("p:{a}")).await;
    lab.link(&t2, &format!("p:{b}")).await;
    lab.track(&e2, &t2).await;
    lab.parked(&e2).await;
    let r = lab.td.delete(&format!("/api/cards/{a}")).await;
    assert!(r.status.is_success(), "{} {}", r.status, r.body);
    lab.mv(&b, "review", None).await;
    assert_eq!(lab.wakes(&t2).await, ["review"]);
    let turn = lab
        .nth_prompt(&e2, &format!("[Thread:{t2}] {REVIEW_TURN}"), 1)
        .await;
    assert!(
        turn.contains(&format!("- p:{a} [archived] Dropped")),
        "{turn}"
    );
}

#[tokio::test]
async fn the_overall_response_can_be_sent_twice() {
    let lab = lab().await;
    let (tid, eph) = lab.started("consult", "Scope").await;
    let resp = format!("[Thread:{tid}] The operator's overall response");
    let again = format!("[Thread:{tid}] The operator's new overall response.");

    let r = lab
        .td
        .post(
            &format!("/api/threads/{tid}/respond"),
            json!({"text": "Approve A."}),
        )
        .await;
    assert_eq!(r.status, 200, "{}", r.body);
    assert_eq!(r.body["response"]["text"], "Approve A.");
    let turn = lab.nth_prompt(&eph, &resp, 1).await;
    assert!(turn.ends_with("\nApprove A."), "{turn}");
    assert!(lab.prompts(&eph, &again).is_empty());
    lab.td.wait_status(&eph, "idle").await;

    // The handoff and the move into tracking; then an extra requirement
    // (D10): the agent says it is new scope and the operator responds
    // again.
    lab.track(&eph, &tid).await;
    lab.parked(&eph).await;
    lab.td
        .post(
            &format!("/api/threads/{tid}/messages"),
            json!({"text": "Also cache the cards."}),
        )
        .await
        .ok();
    lab.nth_prompt(&eph, &format!("[Thread:{tid}] Also cache"), 1)
        .await;
    lab.td.wait_status(&eph, "idle").await;
    let th = lab
        .post_as(&eph, &tid, "New scope. Options: O1 amend, O2 follow-up.")
        .await;
    assert_eq!(th["status"], "tracking");
    lab.parked(&eph).await;
    let r = lab
        .td
        .post(
            &format!("/api/threads/{tid}/respond"),
            json!({"text": "Approve A and O2."}),
        )
        .await;
    assert_eq!(r.status, 200, "{}", r.body);
    // The newest response is the record; both stay in the conversation.
    assert_eq!(r.body["response"]["text"], "Approve A and O2.");
    assert_eq!(r.body["status"], "waiting_agent");
    assert!(r.body["tracking_at"].is_string(), "{}", r.body);
    let kinds: Vec<&str> = r.body["messages"]
        .as_array()
        .unwrap()
        .iter()
        .filter(|m| m["kind"] == "response")
        .map(|m| m["text"].as_str().unwrap())
        .collect();
    assert_eq!(kinds, ["Approve A.", "Approve A and O2."]);
    // The second turn says it replaces the first, and the thread tracks.
    let turn = lab.nth_prompt(&eph, &again, 1).await;
    assert!(turn.contains("replaces the earlier one"), "{turn}");
    assert!(turn.contains("Approve A and O2."), "{turn}");
    assert!(turn.contains("is tracking"), "{turn}");
    assert_eq!(lab.prompts(&eph, &resp).len(), 1);
    lab.td.wait_status(&eph, "idle").await;
    let th = lab.post_as(&eph, &tid, "Handed off O2 to the lead.").await;
    assert_eq!(th["status"], "tracking");
    // Two audit rows, one per response.
    let rows = lab
        .td
        .get("/api/audit?action=thread.respond")
        .await
        .ok()
        .clone();
    let n = rows
        .as_array()
        .unwrap()
        .iter()
        .filter(|r| r["target"] == tid.as_str())
        .count();
    assert_eq!(n, 2);
}
