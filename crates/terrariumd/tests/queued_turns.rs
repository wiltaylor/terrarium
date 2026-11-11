//! The queued turns of an agent (TER-54): `GET /api/agents/{id}/queued`
//! lists the turns that wait in the store for the agent's wake, and
//! `DELETE /api/agents/{id}/queued/{turn}` drops one. With the fake
//! supervisor; no harness runs.
#![allow(clippy::unwrap_used, clippy::expect_used)]

use crate::common;

use std::time::Duration;

use common::TestDaemon;
use hyper::Method;
use serde_json::{Value, json};
use terrarium_core::AgentId;
use terrarium_core::config::{CompactMaxAge, IdleStop};
use terrariumd::lifecycle::Tuning;

fn state(td: &TestDaemon) -> terrariumd::api::Shared {
    td.daemon.as_ref().unwrap().state.clone()
}

fn aid(s: &str) -> AgentId {
    s.parse().unwrap()
}

async fn until<F: std::future::Future<Output = bool>>(what: &str, mut f: impl FnMut() -> F) {
    let start = std::time::Instant::now();
    while !f().await {
        assert!(
            start.elapsed() < common::WAIT,
            "timed out waiting for {what}"
        );
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
}

/// Project `p` with the worker `w1` (paused, so nothing wakes it), the
/// lead `lead`, and three queued turns of `w1`.
async fn setup() -> (TestDaemon, String) {
    let td = TestDaemon::start().await;
    state(&td).lifecycle.set_tuning(Tuning {
        idle_stop: IdleStop::Off,
        tick: Duration::from_millis(20),
        compact_on_idle: false,
        compact_max_age: CompactMaxAge::Off,
        compact_wait: Duration::from_secs(5),
    });
    td.project("p").await;
    let w = td.agent("p", "w1", json!({})).await["id"]
        .as_str()
        .unwrap()
        .to_owned();
    td.agent("p", "lead", json!({"roles": ["project_admin"]}))
        .await;
    td.post(&format!("/api/agents/{w}/pause"), json!({}))
        .await
        .ok();
    for t in ["first", "second", "third"] {
        let r = td
            .post(&format!("/api/agents/{w}/send"), json!({"text": t}))
            .await;
        assert_eq!(r.body["queued"], true, "{}", r.body);
    }
    (td, w)
}

async fn queued(td: &TestDaemon, w: &str) -> Vec<Value> {
    td.get(&format!("/api/agents/{w}/queued")).await.ok()["items"]
        .as_array()
        .unwrap()
        .clone()
}

fn bodies(items: &[Value]) -> Vec<String> {
    items
        .iter()
        .map(|i| i["body"].as_str().unwrap().to_owned())
        .collect()
}

#[tokio::test]
async fn held_turns_are_redacted_stable_and_removable_by_the_lead() {
    use terrariumd::supervisor::{Supervisor, TurnHold};
    let (td, w) = setup().await;
    td.post(
        "/api/secrets",
        json!({"name": "held-test", "kind": "token",
        "fields": {"value": "held-test-secret-value"}}),
    )
    .await
    .ok();
    td.post(&format!("/api/agents/{w}/start"), json!({}))
        .await
        .ok();
    td.wait_status(&w, "idle").await;
    td.sup
        .hold_turns(
            aid(&w),
            Some(TurnHold {
                until: None,
                reason: "limit:sub".into(),
            }),
        )
        .await
        .unwrap();
    for text in ["first held", "held-test-secret-value", "third held"] {
        td.sup
            .send_turn(aid(&w), format!("[User] {text}"))
            .await
            .unwrap();
    }
    let response = td
        .get_as("p/w1", &format!("/api/agents/{w}/queued"))
        .await
        .ok();
    assert!(!response.to_string().contains("held-test-secret-value"));
    let held: Vec<_> = response["items"]
        .as_array()
        .unwrap()
        .iter()
        .filter(|t| t["source"] == "hold")
        .collect();
    assert_eq!(held.len(), 3);
    let id = held[1]["id"].as_str().unwrap();
    let path = format!("/api/agents/{w}/queued/{id}?source=hold");
    td.req(Method::DELETE, &path, None, Some("p/w1"))
        .await
        .err(403, "forbidden");
    td.agent("p", "sys", json!({"roles": ["sysadmin"]})).await;
    td.req(Method::DELETE, &path, None, Some("p/sys"))
        .await
        .err(403, "forbidden");
    assert_eq!(
        td.req(Method::DELETE, &path, None, Some("p/lead"))
            .await
            .status,
        204
    );
    assert_eq!(td.delete(&path).await.status, 404);
    let remaining: Vec<_> = queued(&td, &w)
        .await
        .into_iter()
        .filter(|t| t["source"] == "hold")
        .collect();
    assert_eq!(bodies(&remaining), ["first held", "third held"]);
    assert_eq!(remaining[0]["id"], held[0]["id"]);
    assert_eq!(remaining[1]["id"], held[2]["id"]);
}

#[tokio::test]
async fn runner_list_errors_preserve_store_items_with_a_marker() {
    for (unsupported, expected) in [(true, "unsupported"), (false, "unavailable")] {
        let (td, w) = setup().await;
        td.sup.fail_pending(aid(&w), unsupported);
        let response = td.get(&format!("/api/agents/{w}/queued")).await;
        let response = response.ok();
        assert_eq!(response["queues"]["runner"], expected);
        let items = response["items"].as_array().unwrap();
        assert_eq!(bodies(items), ["first", "second", "third"]);
        assert!(items.iter().all(|t| t["source"] == "store"));
    }
}

#[tokio::test]
async fn live_queue_remove_keeps_order_and_refuses_a_turn_that_started() {
    use terrariumd::supervisor::Supervisor;
    let td = TestDaemon::start().await;
    td.project("p").await;
    let w = td.agent("p", "w", json!({})).await["id"]
        .as_str()
        .unwrap()
        .to_owned();
    td.agent("p", "lead", json!({"roles": ["project_admin"]}))
        .await;
    td.post(&format!("/api/agents/{w}/start"), json!({}))
        .await
        .ok();
    td.wait_status(&w, "idle").await;
    td.sup.reset_gate(aid(&w), true);
    for body in ["first", "second", "third"] {
        td.post(&format!("/api/agents/{w}/send"), json!({"text": body}))
            .await
            .ok();
    }
    let items = queued(&td, &w).await;
    assert_eq!(bodies(&items), ["first", "second", "third"]);
    assert!(items.iter().all(|t| t["source"] == "supervisor"));
    let path = format!(
        "/api/agents/{w}/queued/{}?source=supervisor",
        items[1]["id"].as_str().unwrap()
    );
    td.req(Method::DELETE, &path, None, Some("p/w"))
        .await
        .err(403, "forbidden");
    assert_eq!(
        td.req(Method::DELETE, &path, None, Some("p/lead"))
            .await
            .status,
        204
    );
    assert_eq!(bodies(&queued(&td, &w).await), ["first", "third"]);
    td.sup.reset_gate(aid(&w), false);
    until("both surviving turns typed", || async {
        td.sup.prompts(aid(&w)).len() == 2
    })
    .await;
    let path = format!(
        "/api/agents/{w}/queued/{}?source=supervisor",
        items[0]["id"].as_str().unwrap()
    );
    assert_eq!(td.delete(&path).await.status, 404);
    assert_eq!(td.sup.prompts(aid(&w)), ["[User] first", "[User] third"]);
}

/// The list is oldest first, with the typed text, the body after the tag,
/// and the time. A member of the project reads it; another project does
/// not see the agent.
#[tokio::test]
async fn the_list_shows_the_turns_in_order() {
    let (td, w) = setup().await;
    let items = queued(&td, &w).await;
    assert_eq!(bodies(&items), ["first", "second", "third"]);
    assert_eq!(items[0]["text"], "[User] first");
    assert_eq!(items[0]["source"], "store");
    assert!(items[0]["created_at"].as_str().unwrap().ends_with('Z'));
    let ids: Vec<i64> = items.iter().map(|i| i["id"].as_i64().unwrap()).collect();
    assert!(ids.windows(2).all(|p| p[0] < p[1]), "{ids:?}");
    td.agent("p", "w2", json!({})).await;
    let r = td.get_as("p/w2", &format!("/api/agents/{w}/queued")).await;
    assert_eq!(r.status, 200, "{}", r.body);
    td.project("other").await;
    td.agent("other", "x", json!({})).await;
    let r = td
        .get_as("other/x", &format!("/api/agents/{w}/queued"))
        .await;
    assert!(
        r.status == 403 || r.status == 404,
        "{} {}",
        r.status,
        r.body
    );
}

/// A remove: the operator and the project's lead; not a member. The turns
/// after it keep their order, a second remove is 404, the audit names the
/// turn, and the wake types only the turns that are left.
#[tokio::test]
async fn a_remove_keeps_the_order_and_needs_a_lead() {
    let (td, w) = setup().await;
    let items = queued(&td, &w).await;
    let second = items[1]["id"].as_i64().unwrap();
    let first = items[0]["id"].as_i64().unwrap();
    td.agent("p", "w2", json!({})).await;
    let path = format!("/api/agents/{w}/queued/{second}");
    td.req(Method::DELETE, &path, None, Some("p/w2"))
        .await
        .err(403, "forbidden");
    let r = td.req(Method::DELETE, &path, None, Some("p/lead")).await;
    assert_eq!(r.status, 204, "{}", r.body);
    let r = td.delete(&path).await;
    assert_eq!(r.status, 404, "{}", r.body);
    td.delete(&format!("{path}?source=unknown"))
        .await
        .err(422, "invalid");
    let r = td.delete(&format!("/api/agents/{w}/queued/{first}")).await;
    assert_eq!(r.status, 204, "{}", r.body);
    assert_eq!(bodies(&queued(&td, &w).await), ["third"]);
    let audit = td.get("/api/audit?action=agent.queued.remove").await;
    let rows = audit.ok().clone();
    let text = rows.to_string();
    assert!(text.contains(&format!("\"turn\":{second}")), "{text}");
    assert!(text.contains(&format!("\"turn\":{first}")), "{text}");
    td.post(&format!("/api/agents/{w}/unpause"), json!({}))
        .await
        .ok();
    until("the turn ran", || async {
        !td.sup.prompts(aid(&w)).is_empty()
    })
    .await;
    tokio::time::sleep(Duration::from_millis(200)).await;
    assert_eq!(td.sup.prompts(aid(&w)), ["[User] third"]);
    assert!(queued(&td, &w).await.is_empty());
}

/// A remove during a wake: the wake hands the turns over one at a time
/// under the agent's turn lock. A remove that gets the lock before the
/// wake reaches its turn wins (204, never typed); the other turns go out
/// in order. A remove of a turn that went out is 404.
#[tokio::test]
async fn a_remove_during_a_wake_is_exact() {
    let (td, w) = setup().await;
    let items = queued(&td, &w).await;
    let (first, second) = (
        items[0]["id"].as_i64().unwrap(),
        items[1]["id"].as_i64().unwrap(),
    );
    let lock = state(&td).lifecycle.turn_lock(aid(&w));
    let held = lock.clone().lock_owned().await;
    // The unpause answers after its wake handed the turns over, so it runs
    // beside the rest.
    let unpause = async {
        td.post(&format!("/api/agents/{w}/unpause"), json!({}))
            .await
            .ok();
    };
    let path = format!("/api/agents/{w}/queued/{second}");
    let rest = async {
        // The wake starts the agent and waits for the lock at the first
        // turn.
        td.wait_status(&w, "idle").await;
        tokio::time::sleep(Duration::from_millis(300)).await;
        assert!(td.sup.prompts(aid(&w)).is_empty());
        // The remove queues behind the wake on the fair lock: the wake
        // hands over the first turn, then the remove runs before the
        // second.
        let remove = td.delete(&path);
        let release = async {
            tokio::time::sleep(Duration::from_millis(300)).await;
            drop(held);
        };
        let (r, ()) = tokio::join!(remove, release);
        r
    };
    let ((), r) = tokio::join!(unpause, rest);
    assert_eq!(r.status, 204, "{}", r.body);
    until("the turns ran", || async {
        td.sup.prompts(aid(&w)).len() >= 2
    })
    .await;
    tokio::time::sleep(Duration::from_millis(200)).await;
    assert_eq!(td.sup.prompts(aid(&w)), ["[User] first", "[User] third"]);
    let r = td.delete(&format!("/api/agents/{w}/queued/{first}")).await;
    assert_eq!(r.status, 404, "{}", r.body);
    assert!(queued(&td, &w).await.is_empty());
}
