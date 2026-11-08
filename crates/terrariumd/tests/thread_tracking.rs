//! The thread status `tracking` (TER-145, TER-144 D1 and D9,
//! docs/api.md §6.13), with the fake supervisor: the attached agent moves
//! its thread into tracking after the handoff and parks, the reminder and
//! the stale close leave the thread alone, and an operator post wakes the
//! agent with a `[Thread:<id>]` turn; its reply puts the thread back into
//! tracking. No harness and no model turn runs.
#![allow(clippy::unwrap_used, clippy::expect_used)]

use crate::common;

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

struct Lab {
    td: TestDaemon,
    /// `p/lead`, the report-to lead of the `p` threads.
    lead: String,
}

/// Projects `meta` and `p`, each with a lead, the template `consult`
/// (claude, a member of `p`), and the member `p/w`.
async fn lab() -> Lab {
    let td = TestDaemon::start().await;
    fast(&td);
    for p in ["meta", "p"] {
        td.project(p).await;
    }
    td.agent("meta", "lead", json!({"roles": ["project_admin"]}))
        .await;
    let lead = id(&td
        .agent("p", "lead", json!({"roles": ["project_admin"]}))
        .await);
    let r = td
        .post(
            "/api/templates",
            json!({"name": "consult", "project": "p", "spec": {"harness": "claude"}}),
        )
        .await;
    assert_eq!(r.status, 201, "{}", r.body);
    td.agent("p", "w", json!({})).await;
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

    /// Wait until `eph` is parked with `reason` and stopped at rest.
    async fn parked(&self, eph: &str, reason: &str) {
        until(&format!("the park stop ({reason})"), || async {
            let a = self.agent(eph).await;
            a["status"] == "stopped"
                && a["status_reason"] == "parked"
                && self.spawn_row(eph).await["park_reason"] == reason
        })
        .await;
    }

    /// Wait for a prompt of `eph` that starts with `start`, and give it.
    async fn prompt(&self, eph: &str, start: &str) -> String {
        let aid: AgentId = eph.parse().unwrap();
        until(&format!("a turn `{start}`"), || async {
            self.td
                .sup
                .prompts(aid)
                .iter()
                .any(|p| p.starts_with(start))
        })
        .await;
        self.td
            .sup
            .prompts(aid)
            .into_iter()
            .rfind(|p| p.starts_with(start))
            .unwrap()
    }

    /// A thread of `p` with a spawned agent, past its first turn: the
    /// thread ID and the agent.
    async fn started(&self, topic: &str) -> (String, String) {
        let r = self
            .td
            .post(
                "/api/threads",
                json!({"topic": topic, "scope": "p", "template": "consult",
                       "text": "Plan the cache."}),
            )
            .await;
        assert_eq!(r.status, 201, "{}", r.body);
        let tid = id(&r.body);
        let eph = r.body["agent"].as_str().unwrap().to_owned();
        self.prompt(&eph, &format!("[Thread:{tid}]")).await;
        self.td.wait_status(&eph, "idle").await;
        (tid, eph)
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

    async fn track_as(&self, who: &str, tid: &str) -> common::Resp {
        self.td
            .post_empty_as(who, &format!("/api/threads/{tid}/track"))
            .await
    }
}

#[tokio::test]
async fn the_agent_tracks_and_an_operator_post_wakes_it() {
    let lab = lab().await;
    let (tid, eph) = lab.started("Cache layer").await;

    // Only the attached agent moves the thread into tracking.
    let r = lab.track_as(&lab.lead, &tid).await;
    assert_eq!(r.status, 403, "{}", r.body);
    let r = lab
        .td
        .post_empty(&format!("/api/threads/{tid}/track"))
        .await;
    assert_eq!(r.status, 403, "{}", r.body);

    // An open question waits on the operator: no tracking yet.
    let r = lab
        .td
        .post_as(
            &eph,
            &format!("/api/agents/{eph}/questions"),
            json!({"text": "Cache cards too?", "options": ["yes", "no"], "thread": tid}),
        )
        .await;
    assert_eq!(r.status, 201, "{}", r.body);
    let qid = id(&r.body);
    let r = lab.track_as(&eph, &tid).await;
    assert_eq!(r.status, 409, "{}", r.body);
    lab.parked(&eph, "thread waits on the operator").await;
    lab.td
        .post(
            &format!("/api/questions/{qid}/answer"),
            json!({"option": "yes"}),
        )
        .await
        .ok();
    let key = eph.clone();
    until("the wake on the answer", || async {
        lab.spawn_row(&key).await["parked_at"].is_null()
    })
    .await;
    lab.td.wait_status(&eph, "idle").await;

    // The handoff, a close proposal, then the move into tracking: the
    // proposal is withdrawn and the agent parks.
    let r = lab
        .td
        .post_as(
            &eph,
            &format!("/api/threads/{tid}/close"),
            json!({"summary": "Handed off."}),
        )
        .await;
    assert_eq!(r.status, 200, "{}", r.body);
    let r = lab.track_as(&eph, &tid).await;
    assert_eq!(r.status, 200, "{}", r.body);
    assert_eq!(r.body["status"], "tracking", "{}", r.body);
    assert!(r.body["tracking_at"].is_string(), "{}", r.body);
    assert!(r.body["close_proposal"].is_null(), "{}", r.body);
    // The proposal parked it already; the park keeps its first reason.
    lab.parked(&eph, "thread waits on the operator").await;

    // It lists under `tracking`, and is not an open Inbox item.
    let rows = lab
        .td
        .get("/api/threads?status=tracking")
        .await
        .ok()
        .clone();
    let rows = rows.as_array().unwrap();
    assert!(rows.iter().any(|t| t["id"] == tid.as_str()), "{rows:?}");
    let open = lab
        .td
        .get("/api/threads?status=waiting_operator")
        .await
        .ok()
        .clone();
    assert!(
        !open
            .as_array()
            .unwrap()
            .iter()
            .any(|t| t["id"] == tid.as_str())
    );
    let items = lab.td.get("/api/inbox?status=open").await.ok().clone();
    assert!(
        !items
            .as_array()
            .unwrap()
            .iter()
            .any(|i| i["id"] == format!("inbox-thread:{tid}")),
        "{items:#}"
    );

    // An operator post wakes it with a [Thread:<id>] turn that says the
    // thread tracks.
    let r = lab
        .td
        .post(
            &format!("/api/threads/{tid}/messages"),
            json!({"text": "How far is it?"}),
        )
        .await;
    assert_eq!(r.status, 201, "{}", r.body);
    assert_eq!(r.body["status"], "waiting_agent");
    assert!(r.body["tracking_at"].is_string(), "{}", r.body);
    let turn = lab
        .prompt(&eph, &format!("[Thread:{tid}] How far is it?"))
        .await;
    assert!(turn.contains("is tracking"), "{turn}");
    lab.td.wait_status(&eph, "idle").await;
    assert!(lab.spawn_row(&eph).await["parked_at"].is_null());

    // Its reply puts the thread back into tracking, not waiting_operator.
    let th = lab
        .post_as(&eph, &tid, "Two of three cards are in review.")
        .await;
    assert_eq!(th["status"], "tracking", "{th}");
    lab.parked(&eph, "thread is tracking").await;

    // The overall response wakes it too, and the reply tracks again.
    lab.td
        .post(
            &format!("/api/threads/{tid}/respond"),
            json!({"text": "Fine."}),
        )
        .await
        .ok();
    let turn = lab
        .prompt(
            &eph,
            &format!("[Thread:{tid}] The operator's overall response"),
        )
        .await;
    assert!(turn.contains("is tracking"), "{turn}");
    lab.td.wait_status(&eph, "idle").await;
    let th = lab.post_as(&eph, &tid, "Noted.").await;
    assert_eq!(th["status"], "tracking", "{th}");
    lab.parked(&eph, "thread is tracking").await;

    // The close ends tracking and the agent.
    lab.td
        .post(
            &format!("/api/threads/{tid}/close"),
            json!({"summary": "Done."}),
        )
        .await
        .ok();
    until("the close of the agent", || async {
        lab.spawn_row(&eph).await["finished_at"].is_string()
    })
    .await;
    assert_eq!(lab.thread(&tid).await["status"], "closed");
    let r = lab.track_as(&eph, &tid).await;
    assert_ne!(r.status, 200, "{}", r.body);
}

#[tokio::test]
async fn a_tracking_thread_gets_no_reminder_and_no_stale_close() {
    let lab = lab().await;
    let (tid, eph) = lab.started("Long work").await;
    let r = lab.track_as(&eph, &tid).await;
    assert_eq!(r.status, 200, "{}", r.body);
    lab.parked(&eph, "thread is tracking").await;
    // A control thread that waits on the operator (the named agent `p/w`
    // does not run).
    let r = lab
        .td
        .post(
            "/api/threads",
            json!({"topic": "Control", "scope": "p", "agent": "p/w"}),
        )
        .await;
    assert_eq!(r.status, 201, "{}", r.body);
    assert_eq!(r.body["status"], "waiting_operator");
    let control = id(&r.body);

    state(&lab.td)
        .lifecycle
        .threads
        .set_tuning(terrariumd::thread_close::Tuning {
            reminder_after: Duration::from_millis(200),
            stale_after: Duration::from_millis(1500),
        });
    until("the stale close of the control thread", || async {
        lab.thread(&control).await["status"] == "closed"
    })
    .await;
    assert_eq!(lab.thread(&control).await["close_reason"], "stale");
    // Two more sweeps past both times.
    tokio::time::sleep(Duration::from_millis(500)).await;
    let th = lab.thread(&tid).await;
    assert_eq!(th["status"], "tracking", "{th:#}");
    assert!(th["reminded_at"].is_null(), "{th:#}");
    assert!(th["close_reason"].is_null(), "{th:#}");
    let reminders = lab
        .td
        .get("/api/mail?to=operator&from=system")
        .await
        .ok()
        .clone();
    let about_tid: Vec<_> = reminders
        .as_array()
        .unwrap()
        .iter()
        .filter(|m| m["body"].as_str().is_some_and(|b| b.contains(&tid)))
        .collect();
    assert!(about_tid.is_empty(), "{about_tid:?}");
    assert!(
        !lab.td
            .mail_of(&lab.lead)
            .await
            .iter()
            .any(|m| m["subject"] == format!("Thread {tid} closed as stale"))
    );
    // The agent stays parked: it costs nothing.
    assert_eq!(lab.agent(&eph).await["status"], "stopped");
    assert!(lab.spawn_row(&eph).await["finished_at"].is_null());
}
