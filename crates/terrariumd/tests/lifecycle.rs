//! The agent lifecycle (PLAN §18, "Wake, idle stop, and pause", "Auto
//! compact"), with the fake supervisor: the wake on mail and on turns, the
//! idle stop and its exceptions, the pause, holds, the wake at the daemon
//! start, and the auto compact. No harness and no model turn runs.
#![allow(clippy::unwrap_used, clippy::expect_used)]

mod common;

use std::time::Duration;

use common::TestDaemon;
use serde_json::{Value, json};
use terrarium_core::config::{CompactMaxAge, IdleStop};
use terrarium_core::{AgentId, HarnessId};
use terrariumd::lifecycle::Tuning;
use terrariumd::supervisor::FakeCompact;

fn state(td: &TestDaemon) -> terrariumd::api::Shared {
    td.daemon.as_ref().unwrap().state.clone()
}

/// A short tick, and the global idle stop `idle`. No auto compact.
fn fast(td: &TestDaemon, idle: IdleStop) {
    compacting(td, idle, false, CompactMaxAge::Off, Duration::from_secs(5));
}

/// [`fast`] with the global auto compact settings and the time limit of a
/// compaction.
fn compacting(
    td: &TestDaemon,
    idle: IdleStop,
    on_idle: bool,
    max_age: CompactMaxAge,
    wait: Duration,
) {
    state(td).lifecycle.set_tuning(Tuning {
        idle_stop: idle,
        tick: Duration::from_millis(20),
        compact_on_idle: on_idle,
        compact_max_age: max_age,
        compact_wait: wait,
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

/// Start agent `a`, give it one turn, and wait until the turn ended.
async fn start_with_a_turn(td: &TestDaemon, a: &str) {
    td.post_empty(&format!("/api/agents/{a}/start")).await.ok();
    td.wait_status(a, "idle").await;
    let before = td.sup.prompts(aid(a)).len();
    td.post(&format!("/api/agents/{a}/send"), json!({"text": "work"}))
        .await
        .ok();
    until("the turn ran", || async {
        td.sup.prompts(aid(a)).len() > before
    })
    .await;
    td.wait_status(a, "idle").await;
}

/// The `agent.compacted` events of agent `a`, oldest first.
async fn compactions(td: &TestDaemon, a: &str) -> Vec<Value> {
    events(td, a, "agent.compacted").await
}

/// With `compact_on_idle`, an agent at its idle stop compacts first and
/// then stops. Without a turn since the compaction, the next idle stop
/// does not compact again.
#[tokio::test]
async fn compact_on_idle_runs_before_the_idle_stop_once_per_turn() {
    let (td, _) = setup().await;
    let w = id(&td.agent("p", "c1", json!({"compact_on_idle": true})).await);
    assert_eq!(agent(&td, &w).await["compact_on_idle"], true);
    assert_eq!(agent(&td, &w).await["compact_supported"], true);
    start_with_a_turn(&td, &w).await;
    compacting(
        &td,
        IdleStop::After(Duration::from_millis(300)),
        false,
        CompactMaxAge::Off,
        Duration::from_secs(5),
    );
    until("the idle stop", || async {
        agent(&td, &w).await["status"] == "stopped"
    })
    .await;
    let done = compactions(&td, &w).await;
    assert_eq!(done.len(), 1, "{done:#?}");
    assert_eq!(done[0]["data"]["cause"], "idle");
    assert_eq!(done[0]["data"]["ok"], true);
    assert!(done[0]["data"]["duration_ms"].is_u64());
    let a = agent(&td, &w).await;
    assert_eq!(a["status_reason"], "idle", "{a}");
    assert!(a["last_compact_at"].is_string(), "{a}");
    assert_eq!(a["last_compact_ok"], true);
    // The command went to the harness as a raw command, not as a turn.
    assert_eq!(td.sup.typed(aid(&w)), b"/compact\r".to_vec());
    assert!(
        !td.sup
            .prompts(aid(&w))
            .iter()
            .any(|p| p.contains("/compact"))
    );
    // The compaction status came before the stop.
    let statuses = events(&td, &w, "status").await;
    let reasons: Vec<&str> = statuses
        .iter()
        .filter_map(|e| e["data"]["reason"].as_str())
        .collect();
    let at = |r: &str| reasons.iter().position(|x| *x == r).unwrap();
    assert!(at("compacting") < at("compacted"), "{reasons:?}");
    assert!(at("compacted") < at("idle"), "{reasons:?}");
    // No turn since the compaction: the next idle stop does not compact.
    td.post_empty(&format!("/api/agents/{w}/start")).await.ok();
    td.wait_status(&w, "idle").await;
    until("the second idle stop", || async {
        agent(&td, &w).await["status"] == "stopped"
    })
    .await;
    assert_eq!(compactions(&td, &w).await.len(), 1);
    assert_eq!(td.sup.compactions(aid(&w)), 1);
}

/// An agent that never had a turn does not compact: the idle stop goes on.
#[tokio::test]
async fn an_agent_without_a_turn_does_not_compact() {
    let (td, w) = setup().await;
    td.post_empty(&format!("/api/agents/{w}/start")).await.ok();
    td.wait_status(&w, "idle").await;
    compacting(
        &td,
        IdleStop::After(Duration::from_millis(200)),
        true,
        CompactMaxAge::After(Duration::from_millis(1)),
        Duration::from_secs(5),
    );
    until("the idle stop", || async {
        agent(&td, &w).await["status"] == "stopped"
    })
    .await;
    assert_eq!(td.sup.compactions(aid(&w)), 0);
    assert!(compactions(&td, &w).await.is_empty());
}

/// With no idle stop, `compact_on_idle` compacts once after the global
/// idle stop time, and the agent stays up.
#[tokio::test]
async fn compact_on_idle_without_an_idle_stop_compacts_once_and_stays() {
    let (td, _) = setup().await;
    let w = id(&td
        .agent(
            "p",
            "keeper",
            json!({"idle_stop": "off", "compact_on_idle": true}),
        )
        .await);
    start_with_a_turn(&td, &w).await;
    compacting(
        &td,
        IdleStop::After(Duration::from_millis(200)),
        false,
        CompactMaxAge::Off,
        Duration::from_secs(5),
    );
    until("the compaction", || async {
        !compactions(&td, &w).await.is_empty()
    })
    .await;
    tokio::time::sleep(Duration::from_millis(700)).await;
    assert_eq!(agent(&td, &w).await["status"], "idle");
    assert_eq!(compactions(&td, &w).await.len(), 1);
    assert_eq!(compactions(&td, &w).await[0]["data"]["cause"], "idle");
}

/// `compact_max_age`: a session older than the maximum compacts at rest,
/// once for each turn, and the agent goes on.
#[tokio::test]
async fn max_age_compacts_at_the_next_rest() {
    let (td, _) = setup().await;
    let w = id(&td
        .agent("p", "aged", json!({"compact_max_age": "1s"}))
        .await);
    assert_eq!(agent(&td, &w).await["compact_max_age"], "1s");
    start_with_a_turn(&td, &w).await;
    until("the max age compaction", || async {
        !compactions(&td, &w).await.is_empty()
    })
    .await;
    let done = compactions(&td, &w).await;
    assert_eq!(done[0]["data"]["cause"], "max_age");
    td.wait_status(&w, "idle").await;
    // No turn: no second compaction, also after the max age.
    tokio::time::sleep(Duration::from_millis(1300)).await;
    assert_eq!(compactions(&td, &w).await.len(), 1);
    // A new turn: the age counts from the last compaction.
    td.post(&format!("/api/agents/{w}/send"), json!({"text": "more"}))
        .await
        .ok();
    until("the second compaction", || async {
        compactions(&td, &w).await.len() == 2
    })
    .await;
    assert_eq!(agent(&td, &w).await["status"], "idle");
    // `null` goes back to the global default (off).
    let r = td
        .patch(
            &format!("/api/agents/{w}"),
            json!({"compact_max_age": null}),
        )
        .await;
    assert!(r.ok().get("compact_max_age").is_none(), "{}", r.body);
    let r = td
        .patch(
            &format!("/api/agents/{w}"),
            json!({"compact_max_age": "soon"}),
        )
        .await;
    assert_eq!(r.status, 400, "{}", r.body);
}

/// A manual compaction: the rights of `restart`, an audit row, and `409`
/// for an agent that does not run or is paused. A request while the agent
/// compacts waits (`queued`) and does not interrupt the compaction
/// (`crate::reset`).
#[tokio::test]
async fn manual_compaction_rights_and_states() {
    let (td, w) = setup().await;
    let lead = id(&td
        .agent("p", "lead", json!({"roles": ["project_admin"]}))
        .await);
    let peer = id(&td.agent("p", "peer", json!({})).await);
    let path = format!("/api/agents/{w}/compact");
    // Not running.
    td.post_empty(&path).await.err(409, "not_running");
    start_with_a_turn(&td, &w).await;
    // A worker of the project may not; its lead may.
    td.post_empty_as(&peer, &path).await.err(403, "forbidden");
    let r = td.post_empty_as(&lead, &path).await;
    assert_eq!(r.status, 202, "{}", r.body);
    until("the compaction", || async {
        !compactions(&td, &w).await.is_empty()
    })
    .await;
    assert_eq!(compactions(&td, &w).await[0]["data"]["cause"], "manual");
    td.wait_status(&w, "idle").await;
    // A manual compaction needs no turn since the last one.
    td.post_empty(&path).await.ok();
    until("the second compaction", || async {
        compactions(&td, &w).await.len() == 2
    })
    .await;
    td.wait_status(&w, "idle").await;
    // Not idle: a compaction runs. The next request waits for its end.
    td.sup.set_compact(aid(&w), FakeCompact::Silent);
    td.post_empty(&path).await.ok();
    let r = td.post_empty(&path).await;
    assert_eq!(r.ok()["reset"]["state"], "queued", "{}", r.body);
    assert_eq!(td.sup.compactions(aid(&w)), 3, "not interrupted");
    let audit = td.get("/api/audit?limit=200").await;
    let n = audit
        .ok()
        .as_array()
        .unwrap()
        .iter()
        .filter(|r| r["action"] == "agent.compact")
        .count();
    assert_eq!(n, 4);
    // Paused.
    td.post_empty(&format!("/api/agents/{w}/pause")).await.ok();
    td.wait_status(&w, "stopped").await;
    let r = td.post_empty(&path).await;
    r.err(409, "conflict");
    assert!(
        r.body["error"]["message"]
            .as_str()
            .unwrap()
            .contains("paused"),
        "{}",
        r.body
    );
}

/// A held agent does not compact, on idle or by hand.
#[tokio::test]
async fn a_held_agent_does_not_compact() {
    let (td, _) = setup().await;
    let w = id(&td
        .agent("p", "held", json!({"compact_on_idle": true}))
        .await);
    start_with_a_turn(&td, &w).await;
    terrariumd::quota_policy::hold(&state(&td), &w, "limit:test", None, "usage limit")
        .await
        .unwrap();
    td.wait_status(&w, "blocked").await;
    compacting(
        &td,
        IdleStop::After(Duration::from_millis(100)),
        true,
        CompactMaxAge::After(Duration::from_millis(1)),
        Duration::from_secs(5),
    );
    tokio::time::sleep(Duration::from_millis(500)).await;
    assert_eq!(td.sup.compactions(aid(&w)), 0);
    td.post_empty(&format!("/api/agents/{w}/compact"))
        .await
        .err(409, "conflict");
}

/// The settings of an agent whose harness the supervisor cannot compact do
/// nothing: the idle stop goes on, and a manual compaction is `409`.
#[tokio::test]
async fn an_unsupported_harness_is_ignored() {
    let (td, _) = setup().await;
    td.sup.refuse_compact(HarnessId::Pi);
    let w = id(&td
        .agent(
            "p",
            "pi1",
            json!({"harness": "pi", "compact_on_idle": true}),
        )
        .await);
    start_with_a_turn(&td, &w).await;
    let r = td.post_empty(&format!("/api/agents/{w}/compact")).await;
    r.err(409, "conflict");
    assert_eq!(
        r.body["error"]["message"],
        "auto compact not supported for pi"
    );
    compacting(
        &td,
        IdleStop::After(Duration::from_millis(200)),
        true,
        CompactMaxAge::Off,
        Duration::from_secs(5),
    );
    until("the idle stop", || async {
        agent(&td, &w).await["status"] == "stopped"
    })
    .await;
    assert!(compactions(&td, &w).await.is_empty());
    assert_eq!(td.sup.compactions(aid(&w)), 0);
}

/// No end signal: the compaction ends at its time limit with `ok` false.
/// Turns that came during the compaction wait, then run.
#[tokio::test]
async fn a_compaction_without_an_end_times_out_and_the_turns_wait() {
    let (td, w) = setup().await;
    start_with_a_turn(&td, &w).await;
    compacting(
        &td,
        IdleStop::Off,
        false,
        CompactMaxAge::Off,
        Duration::from_millis(600),
    );
    td.sup.set_compact(aid(&w), FakeCompact::Silent);
    td.post_empty(&format!("/api/agents/{w}/compact"))
        .await
        .ok();
    td.wait_status(&w, "working").await;
    assert_eq!(agent(&td, &w).await["status_reason"], "compacting");
    let r = td
        .post(&format!("/api/agents/{w}/send"), json!({"text": "after"}))
        .await;
    assert_eq!(r.status, 202, "{}", r.body);
    tokio::time::sleep(Duration::from_millis(200)).await;
    assert!(!td.sup.prompts(aid(&w)).contains(&"[User] after".to_owned()));
    until("the time limit", || async {
        !compactions(&td, &w).await.is_empty()
    })
    .await;
    let done = compactions(&td, &w).await;
    assert_eq!(done[0]["data"]["ok"], false, "{done:#?}");
    assert!(done[0]["data"]["reason"].is_string());
    until("the waiting turn ran", || async {
        td.sup.prompts(aid(&w)).contains(&"[User] after".to_owned())
    })
    .await;
    let a = agent(&td, &w).await;
    assert_eq!(a["last_compact_ok"], false, "{a}");
}

/// `GET /api/health` names the global auto compact settings and the
/// harnesses that can compact.
#[tokio::test]
async fn health_names_the_auto_compact_defaults() {
    let (td, _) = setup().await;
    let h = td.get("/api/health").await.ok().clone();
    assert_eq!(h["compact_on_idle"], false);
    assert_eq!(h["compact_max_age"], "off");
    compacting(
        &td,
        IdleStop::Off,
        true,
        CompactMaxAge::After(Duration::from_secs(86_400)),
        Duration::from_secs(5),
    );
    let h = td.get("/api/health").await.ok().clone();
    assert_eq!(h["compact_on_idle"], true);
    assert_eq!(h["compact_max_age"], "1d");
    assert_eq!(
        h["compact_harnesses"],
        json!(["claude", "codex", "copilot", "pi"])
    );
}
