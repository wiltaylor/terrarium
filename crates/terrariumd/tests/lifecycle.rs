//! The agent lifecycle (PLAN §18, "Wake, idle stop, and pause"), with the
//! fake supervisor: the wake on mail and on turns, the idle stop and its
//! exceptions, the pause, holds, and the wake at the daemon start. No
//! harness and no model turn runs.
#![allow(clippy::unwrap_used, clippy::expect_used)]

mod common;

use std::time::Duration;

use common::TestDaemon;
use serde_json::{Value, json};
use terrarium_core::AgentId;
use terrarium_core::config::IdleStop;
use terrariumd::lifecycle::Tuning;

fn state(td: &TestDaemon) -> terrariumd::api::Shared {
    td.daemon.as_ref().unwrap().state.clone()
}

/// A short tick, and the global idle stop `idle`.
fn fast(td: &TestDaemon, idle: IdleStop) {
    state(td).lifecycle.set_tuning(Tuning {
        idle_stop: idle,
        tick: Duration::from_millis(20),
    });
}

fn id(v: &Value) -> String {
    v["id"].as_str().unwrap().to_owned()
}

fn aid(s: &str) -> AgentId {
    s.parse().unwrap()
}

async fn agent(td: &TestDaemon, path: &str) -> Value {
    td.get(&format!("/api/agents/{path}")).await.ok().clone()
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

async fn events(td: &TestDaemon, agent: &str, kind: &str) -> Vec<Value> {
    td.get(&format!(
        "/api/agents/{agent}/events?kind={kind}&limit=1000"
    ))
    .await
    .ok()
    .as_array()
    .unwrap()
    .clone()
}

async fn setup() -> (TestDaemon, String) {
    let td = TestDaemon::start().await;
    fast(&td, IdleStop::Off);
    td.project("p").await;
    let w = id(&td.agent("p", "w1", json!({})).await);
    (td, w)
}

/// Mail to a stopped agent wakes it with resume of its last session, and
/// the mail goes out as a pointer after the start.
#[tokio::test]
async fn mail_wakes_a_stopped_agent() {
    let (td, w) = setup().await;
    td.sup.set_mail_push(true);
    td.post_empty(&format!("/api/agents/{w}/start")).await.ok();
    td.wait_status(&w, "idle").await;
    let first = agent(&td, &w).await["session"].clone();
    td.post_empty(&format!("/api/agents/{w}/stop")).await.ok();
    td.wait_status(&w, "stopped").await;

    let m = td
        .post("/api/mail", json!({"to": "p/w1", "body": "wake up"}))
        .await
        .ok()
        .clone();
    td.wait_status(&w, "idle").await;
    let woken = events(&td, &w, "agent.woken").await;
    assert_eq!(woken.len(), 1, "{woken:#?}");
    assert_eq!(woken[0]["data"]["cause"], "mail");
    // The wake resumed the harness session of the last run.
    assert_eq!(agent(&td, &w).await["session"], first);
    let mid = id(&m);
    until("the mail goes out", || async {
        td.get(&format!("/api/mail/{mid}")).await.body["state"] != "queued"
    })
    .await;
}

/// Turns to a stopped agent wait in the store and wake it: one wake, and
/// the turns go out in order after the start.
#[tokio::test]
async fn turns_to_a_stopped_agent_queue_and_wake_it_once() {
    let (td, w) = setup().await;
    let path = format!("/api/agents/{w}/send");
    let (a, b, c) = tokio::join!(
        td.post(&path, json!({"text": "one"})),
        td.post(&path, json!({"text": "two"})),
        td.post(&path, json!({"text": "three"})),
    );
    for r in [&a, &b, &c] {
        assert_eq!(r.status, 202, "{}", r.body);
        assert_eq!(r.body["queued"], true, "{}", r.body);
    }
    until("all three turns ran", || async {
        td.sup.prompts(aid(&w)).len() == 3
    })
    .await;
    let prompts = td.sup.prompts(aid(&w));
    let mut sent: Vec<String> = prompts
        .iter()
        .map(|p| p.trim_start_matches("[User] ").to_owned())
        .collect();
    // The three requests raced; each turn went out once, in the order of
    // the store.
    sent.sort();
    assert_eq!(sent, ["one", "three", "two"]);
    assert_eq!(events(&td, &w, "agent.woken").await.len(), 1);
    until("no work waits", || async {
        agent(&td, &w).await.get("queued").is_none()
    })
    .await;
}

/// Sequential turns keep their order.
#[tokio::test]
async fn queued_turns_keep_their_order() {
    let (td, w) = setup().await;
    td.post(&format!("/api/agents/{w}/pause"), json!({}))
        .await
        .ok();
    for t in ["first", "second", "third"] {
        let r = td
            .post(&format!("/api/agents/{w}/send"), json!({"text": t}))
            .await;
        assert_eq!(r.body["queued"], true);
    }
    assert_eq!(agent(&td, &w).await["queued"], 3);
    td.post(&format!("/api/agents/{w}/unpause"), json!({}))
        .await
        .ok();
    until("the turns ran", || async {
        td.sup.prompts(aid(&w)).len() == 3
    })
    .await;
    assert_eq!(
        td.sup.prompts(aid(&w)),
        ["[User] first", "[User] second", "[User] third"]
    );
}

/// An agent at rest for its idle stop stops gracefully, with the reason
/// `idle`. The session ends with the outcome `idle`.
#[tokio::test]
async fn an_idle_agent_stops_after_its_idle_stop() {
    let (td, w) = setup().await;
    fast(&td, IdleStop::After(Duration::from_millis(300)));
    td.post_empty(&format!("/api/agents/{w}/start")).await.ok();
    td.wait_status(&w, "idle").await;
    until("the idle stop", || async {
        agent(&td, &w).await["status"] == "stopped"
    })
    .await;
    let a = agent(&td, &w).await;
    assert_eq!(a["status_reason"], "idle", "{a}");
    assert_eq!(a["paused"], false);
    let sessions = td.get(&format!("/api/agents/{w}/sessions")).await;
    assert_eq!(sessions.ok()[0]["outcome"], "idle", "{}", sessions.body);
    // The next mail wakes it again.
    td.post("/api/mail", json!({"to": "p/w1", "body": "more"}))
        .await
        .ok();
    td.wait_status(&w, "idle").await;
}

/// The per-agent `idle_stop` wins over the global one; `off` never stops.
#[tokio::test]
async fn the_agent_idle_stop_wins_over_the_global_one() {
    let (td, w) = setup().await;
    fast(&td, IdleStop::After(Duration::from_millis(200)));
    let off = id(&td.agent("p", "keeper", json!({"idle_stop": "off"})).await);
    let short = id(&td.agent("p", "short", json!({"idle_stop": "1s"})).await);
    assert_eq!(agent(&td, &off).await["idle_stop"], "off");
    assert_eq!(agent(&td, &short).await["idle_stop"], "1s");
    for a in [&w, &off, &short] {
        td.post_empty(&format!("/api/agents/{a}/start")).await.ok();
        td.wait_status(a, "idle").await;
    }
    // The global 200 ms stops w1 first; `short` is still up.
    until("w1 stops", || async {
        agent(&td, &w).await["status"] == "stopped"
    })
    .await;
    assert_eq!(agent(&td, &short).await["status"], "idle");
    until("short stops after 1 s", || async {
        agent(&td, &short).await["status"] == "stopped"
    })
    .await;
    tokio::time::sleep(Duration::from_millis(300)).await;
    assert_eq!(agent(&td, &off).await["status"], "idle");
    // `null` goes back to the global default.
    let r = td
        .patch(&format!("/api/agents/{off}"), json!({"idle_stop": null}))
        .await;
    assert!(r.ok().get("idle_stop").is_none(), "{}", r.body);
    until("keeper stops with the default", || async {
        agent(&td, &off).await["status"] == "stopped"
    })
    .await;
    // A bad value is refused.
    let r = td
        .patch(&format!("/api/agents/{off}"), json!({"idle_stop": "soon"}))
        .await;
    assert_eq!(r.status, 400, "{}", r.body);
}

/// An agent with queued work or an open `process` wait (a `terrarium run`
/// job) does not stop for idleness.
#[tokio::test]
async fn busy_agents_do_not_idle_stop() {
    let (td, w) = setup().await;
    let runner = id(&td.agent("p", "runner", json!({})).await);
    for a in [&w, &runner] {
        td.post_empty(&format!("/api/agents/{a}/start")).await.ok();
        td.wait_status(a, "idle").await;
    }
    // Mail push is off in the fake: the mail stays queued.
    td.post("/api/mail", json!({"to": "p/w1", "body": "later"}))
        .await
        .ok();
    let key = runner.clone();
    state(&td)
        .db
        .write("system", move |t| {
            t.execute(
                "INSERT INTO waits (id, agent, project, kind, spec, summary, created_by,
                                    created_at)
                 VALUES ('W1', ?1, 'p', 'process', '{}', 'run: make test', 'test', ?2)",
                rusqlite::params![key, terrariumd::db::now()],
            )?;
            Ok(())
        })
        .await
        .unwrap();
    fast(&td, IdleStop::After(Duration::from_millis(100)));
    tokio::time::sleep(Duration::from_millis(500)).await;
    assert_eq!(agent(&td, &w).await["status"], "idle");
    assert_eq!(agent(&td, &w).await["queued"], 1);
    assert_eq!(agent(&td, &runner).await["status"], "idle");
    // The run ends: the agent stops.
    state(&td)
        .db
        .write("system", |t| {
            t.execute("UPDATE waits SET state = 'resolved' WHERE id = 'W1'", [])?;
            Ok(())
        })
        .await
        .unwrap();
    until("the runner stops", || async {
        agent(&td, &runner).await["status"] == "stopped"
    })
    .await;
}

/// A pause stops a running agent and blocks every wake: mail and turns
/// wait. An unpause with queued work wakes it; with none, it stays
/// stopped. A start of a paused agent is `409`.
#[tokio::test]
async fn pause_stops_and_blocks_wakes_until_the_unpause() {
    let (td, w) = setup().await;
    td.post_empty(&format!("/api/agents/{w}/start")).await.ok();
    td.wait_status(&w, "idle").await;
    let r = td.post_empty(&format!("/api/agents/{w}/pause")).await;
    assert_eq!(r.status, 202, "{}", r.body);
    assert_eq!(r.body["paused"], true);
    td.wait_status(&w, "stopped").await;
    assert_eq!(agent(&td, &w).await["status_reason"], "paused");
    // A second pause changes nothing.
    td.post_empty(&format!("/api/agents/{w}/pause")).await.ok();
    assert_eq!(events(&td, &w, "agent.paused").await.len(), 1);

    td.post("/api/mail", json!({"to": "p/w1", "body": "hold"}))
        .await
        .ok();
    let r = td
        .post(&format!("/api/agents/{w}/send"), json!({"text": "go"}))
        .await;
    assert_eq!(r.body["queued"], true);
    let r = td.post_empty(&format!("/api/agents/{w}/start")).await;
    r.err(409, "conflict");
    let r = td.post_empty(&format!("/api/agents/{w}/restart")).await;
    r.err(409, "conflict");
    tokio::time::sleep(Duration::from_millis(200)).await;
    let a = agent(&td, &w).await;
    assert_eq!(a["status"], "stopped");
    assert_eq!(a["queued"], 2, "{a}");
    assert!(events(&td, &w, "agent.woken").await.is_empty());

    let r = td.post_empty(&format!("/api/agents/{w}/unpause")).await;
    assert_eq!(r.body["paused"], false);
    td.wait_status(&w, "idle").await;
    until("the queued turn ran", || async {
        td.sup.prompts(aid(&w)).contains(&"[User] go".to_owned())
    })
    .await;
    let woken = events(&td, &w, "agent.woken").await;
    assert_eq!(woken[0]["data"]["cause"], "unpause");

    // An unpause with no work leaves the agent stopped.
    let quiet = id(&td.agent("p", "quiet", json!({})).await);
    td.post_empty(&format!("/api/agents/{quiet}/pause"))
        .await
        .ok();
    td.post_empty(&format!("/api/agents/{quiet}/unpause"))
        .await
        .ok();
    tokio::time::sleep(Duration::from_millis(200)).await;
    assert_eq!(agent(&td, &quiet).await["status"], "stopped");
    // The audit rows.
    let audit = td.get("/api/audit?limit=200").await;
    let actions: Vec<&str> = audit
        .ok()
        .as_array()
        .unwrap()
        .iter()
        .filter_map(|r| r["action"].as_str())
        .collect();
    assert!(actions.contains(&"agent.pause"), "{actions:?}");
    assert!(actions.contains(&"agent.unpause"), "{actions:?}");
}

/// A quota hold keeps a stopped agent from waking; the end of the last
/// hold wakes it when work waits.
#[tokio::test]
async fn a_hold_blocks_the_wake_and_its_end_wakes() {
    let (td, w) = setup().await;
    let st = state(&td);
    terrariumd::quota_policy::hold(&st, &w, "limit:test", None, "usage limit")
        .await
        .unwrap();
    td.post(
        "/api/mail",
        json!({"to": "p/w1", "body": "after the reset"}),
    )
    .await
    .ok();
    tokio::time::sleep(Duration::from_millis(200)).await;
    assert_eq!(agent(&td, &w).await["status"], "stopped");
    assert!(events(&td, &w, "agent.woken").await.is_empty());
    terrariumd::quota_policy::release(&st, &w, "limit:test")
        .await
        .unwrap();
    until("the wake at the hold end", || async {
        !events(&td, &w, "agent.woken").await.is_empty()
    })
    .await;
    let woken = events(&td, &w, "agent.woken").await;
    assert_eq!(woken[0]["data"]["cause"], "hold_end");
    td.wait_status(&w, "idle").await;
}

/// An agent in `error` does not wake; the operator's stop clears it, and
/// the next mail wakes it.
#[tokio::test]
async fn an_agent_in_error_does_not_wake() {
    let (td, w) = setup().await;
    let key = w.clone();
    state(&td)
        .db
        .write("system", move |t| {
            terrariumd::apply_status(
                t,
                &key,
                terrarium_core::AgentStatus::Error,
                Some("crash loop"),
            )
        })
        .await
        .unwrap();
    td.post("/api/mail", json!({"to": "p/w1", "body": "one"}))
        .await
        .ok();
    tokio::time::sleep(Duration::from_millis(200)).await;
    assert_eq!(agent(&td, &w).await["status"], "error");
    td.post_empty(&format!("/api/agents/{w}/stop")).await.ok();
    assert_eq!(agent(&td, &w).await["status"], "stopped");
    td.post("/api/mail", json!({"to": "p/w1", "body": "two"}))
        .await
        .ok();
    td.wait_status(&w, "idle").await;
}

/// A card that another actor assigns wakes the agent.
#[tokio::test]
async fn a_card_assignment_wakes_the_agent() {
    let (td, w) = setup().await;
    let card = td
        .post("/api/projects/p/cards", json!({"title": "Ship it"}))
        .await
        .ok()
        .clone();
    td.post(
        &format!("/api/cards/{}/assign", id(&card)),
        json!({"assignee": "p/w1"}),
    )
    .await
    .ok();
    td.wait_status(&w, "idle").await;
    assert_eq!(
        events(&td, &w, "agent.woken").await[0]["data"]["cause"],
        "card"
    );
}

/// At the daemon start, an agent with queued turns that is not paused
/// wakes, and its turns go out.
#[tokio::test]
async fn the_daemon_start_wakes_agents_with_queued_turns() {
    let (mut td, w) = setup().await;
    td.post_empty(&format!("/api/agents/{w}/pause")).await.ok();
    td.post(&format!("/api/agents/{w}/send"), json!({"text": "later"}))
        .await
        .ok();
    // Clear the flag in the store only, so no unpause wakes it now.
    let key = w.clone();
    state(&td)
        .db
        .write("system", move |t| {
            t.execute("UPDATE agents SET paused = 0 WHERE id = ?1", [&key])?;
            Ok(())
        })
        .await
        .unwrap();
    assert_eq!(agent(&td, &w).await["status"], "stopped");
    td.boot(None, None).await;
    td.wait_status(&w, "idle").await;
    until("the turn ran", || async {
        td.sup.prompts(aid(&w)) == ["[User] later"]
    })
    .await;
    assert_eq!(
        events(&td, &w, "agent.woken").await[0]["data"]["cause"],
        "daemon_start"
    );
}

/// `GET /api/health` names the global idle stop.
#[tokio::test]
async fn health_names_the_idle_stop() {
    let (td, _) = setup().await;
    fast(&td, IdleStop::After(Duration::from_secs(600)));
    assert_eq!(td.get("/api/health").await.ok()["idle_stop"], "10m");
    fast(&td, IdleStop::Off);
    assert_eq!(td.get("/api/health").await.ok()["idle_stop"], "off");
}
