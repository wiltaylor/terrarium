//! The idle alert (TER-67), with the fake supervisor: the assignee of a
//! card in progress rests with no wait past the project's `idle_alert`.
//! The card gets `idle_since`, and the leads one mail. A wait, a turn, or
//! `off` give no mail, and a wait or a turn clear the mark. The unit tests
//! of `terrariumd::idle_alert` cover the rules one by one.
#![allow(clippy::unwrap_used, clippy::expect_used)]

mod common;

use std::time::Duration;

use common::TestDaemon;
use serde_json::{Value, json};
use terrarium_core::config::{CompactMaxAge, IdleStop};
use terrariumd::lifecycle::Tuning;

fn state(td: &TestDaemon) -> terrariumd::api::Shared {
    td.daemon.as_ref().unwrap().state.clone()
}

/// A short tick, no idle stop, no auto compact.
fn fast(td: &TestDaemon) {
    state(td).lifecycle.set_tuning(Tuning {
        idle_stop: IdleStop::Off,
        tick: Duration::from_millis(20),
        compact_on_idle: false,
        compact_max_age: CompactMaxAge::Off,
        compact_wait: Duration::from_secs(5),
    });
}

/// Wait until `f` holds, up to [`common::WAIT`].
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

fn id(v: &Value) -> String {
    v["id"].as_str().unwrap().to_owned()
}

async fn card(td: &TestDaemon, c: &str) -> Value {
    td.get(&format!("/api/cards/{c}")).await.ok().clone()
}

/// The idle alert mails to `who` (an agent ID, or `operator`).
async fn alerts(td: &TestDaemon, who: &str) -> Vec<Value> {
    let all = if who == "operator" {
        td.get("/api/mail?to=operator")
            .await
            .ok()
            .as_array()
            .unwrap()
            .clone()
    } else {
        td.mail_of(who).await
    };
    all.into_iter()
        .filter(|m| {
            m["from"] == "system"
                && m["subject"]
                    .as_str()
                    .is_some_and(|s| s.contains("is idle on"))
        })
        .collect()
}

/// The card events of `c` with the change `change`.
async fn changes(td: &TestDaemon, c: &str, change: &str) -> usize {
    td.get("/api/events?kind=card&limit=1000")
        .await
        .ok()
        .as_array()
        .unwrap()
        .iter()
        .filter(|e| e["card"] == c && e["data"]["change"] == change)
        .count()
}

/// Project `project` with the idle alert `alert`, a lead (not started)
/// when `lead`, and the worker `w1`, started and idle, with no card
/// reminder. Returns the lead and the worker IDs.
async fn setup(td: &TestDaemon, project: &str, alert: &str, lead: bool) -> (Option<String>, String) {
    fast(td);
    td.project(project).await;
    td.patch(
        &format!("/api/projects/{project}"),
        json!({"idle_alert": alert}),
    )
    .await
    .ok();
    let l = if lead {
        Some(id(&td
            .agent(project, "lead", json!({"roles": ["project_admin"]}))
            .await))
    } else {
        None
    };
    let w = id(&td
        .agent(project, "w1", json!({"card_reminder": false}))
        .await);
    td.post_empty(&format!("/api/agents/{w}/start")).await.ok();
    td.wait_status(&w, "idle").await;
    (l, w)
}

/// A card that agent `w` made and claimed: in the working column.
async fn claimed(td: &TestDaemon, project: &str, w: &str, title: &str) -> String {
    let c = id(td
        .post_as(w, &format!("/api/projects/{project}/cards"), json!({"title": title}))
        .await
        .ok());
    td.post_as(w, &format!("/api/cards/{c}/claim"), json!({}))
        .await
        .ok();
    c
}

/// A time wait of agent `w`, a minute away: `w` is `waiting`.
async fn a_wait(td: &TestDaemon, w: &str) -> String {
    let at = (chrono::Utc::now() + chrono::Duration::seconds(60))
        .to_rfc3339_opts(chrono::SecondsFormat::Millis, true);
    let r = td
        .post_as(
            w,
            &format!("/api/agents/{w}/waits"),
            json!({"kind": "time", "at": at}),
        )
        .await;
    assert_eq!(r.status, 201, "{}", r.body);
    td.wait_status(w, "waiting").await;
    id(&r.body)
}

/// An idle worker with a card and no wait: after the threshold, one mail
/// to the lead, the mark on the card (also in the lists), and no second
/// mail in the same rest. A wait clears the mark and sends nothing.
#[tokio::test]
async fn an_idle_worker_with_a_card_and_no_wait_mails_the_lead_once() {
    let td = TestDaemon::start().await;
    let (lead, w) = setup(&td, "p", "1s", true).await;
    let lead = lead.unwrap();
    let c = claimed(&td, "p", &w, "Build").await;
    until("the idle mark", || async {
        card(&td, &c).await["idle_since"].is_string()
    })
    .await;
    let since = card(&td, &c).await["idle_since"].clone();
    until("the lead mail", || async { !alerts(&td, &lead).await.is_empty() }).await;
    let mail = alerts(&td, &lead).await;
    assert_eq!(mail.len(), 1, "{mail:#?}");
    assert_eq!(mail[0]["subject"], format!("p/w1 is idle on {c} with no wait"));
    let body = td
        .get_as(&lead, &format!("/api/mail/{}", id(&mail[0])))
        .await
        .ok()["body"]
        .as_str()
        .unwrap()
        .to_owned();
    assert!(body.contains(&format!("holds card {c} (Build)")), "{body}");
    assert!(body.contains("with no registered wait"), "{body}");
    assert!(body.contains("The idle alert of project `p` is 1s"), "{body}");
    assert_eq!(changes(&td, &c, "idle").await, 1);
    // The lists show the mark.
    let listed = td.get("/api/projects/p/cards").await.ok().clone();
    let row = listed
        .as_array()
        .unwrap()
        .iter()
        .find(|k| k["id"] == c)
        .unwrap()
        .clone();
    assert_eq!(row["idle_since"], since);
    let active = td.get("/api/cards").await.ok().clone();
    assert!(
        active
            .as_array()
            .unwrap()
            .iter()
            .any(|k| k["id"] == c && k["idle_since"] == since),
        "{active:#}"
    );
    // Many ticks later: the same mark, no second mail.
    tokio::time::sleep(Duration::from_millis(600)).await;
    assert_eq!(card(&td, &c).await["idle_since"], since);
    assert_eq!(alerts(&td, &lead).await.len(), 1);

    // A wait: the mark goes, and no mail comes while the agent waits.
    a_wait(&td, &w).await;
    until("the mark clears", || async {
        card(&td, &c).await.get("idle_since").is_none()
    })
    .await;
    assert_eq!(changes(&td, &c, "idle_cleared").await, 1);
    tokio::time::sleep(Duration::from_millis(1500)).await;
    assert!(card(&td, &c).await.get("idle_since").is_none());
    assert_eq!(alerts(&td, &lead).await.len(), 1);
}

/// An open wait from before the claim: no mark and no mail past the
/// threshold. With the alert `off`, a rest gives nothing either; a
/// threshold set later marks the card.
#[tokio::test]
async fn no_mail_with_an_open_wait_or_with_the_alert_off() {
    let td = TestDaemon::start().await;
    let (lead, w) = setup(&td, "p", "1s", true).await;
    let lead = lead.unwrap();
    let wait = a_wait(&td, &w).await;
    let c = claimed(&td, "p", &w, "Build").await;
    tokio::time::sleep(Duration::from_millis(1500)).await;
    assert!(card(&td, &c).await.get("idle_since").is_none());
    assert!(alerts(&td, &lead).await.is_empty());

    // Off, then the wait ends without a turn: idle with no wait.
    td.patch("/api/projects/p", json!({"idle_alert": "off"}))
        .await
        .ok();
    let r = td.delete(&format!("/api/waits/{wait}")).await;
    assert!(r.status.is_success(), "{}", r.body);
    td.wait_status(&w, "idle").await;
    tokio::time::sleep(Duration::from_millis(1500)).await;
    assert!(card(&td, &c).await.get("idle_since").is_none());
    assert!(alerts(&td, &lead).await.is_empty());

    td.patch("/api/projects/p", json!({"idle_alert": "1s"}))
        .await
        .ok();
    until("the idle mark", || async {
        card(&td, &c).await["idle_since"].is_string()
    })
    .await;
    until("the lead mail", || async {
        alerts(&td, &lead).await.len() == 1
    })
    .await;
}

/// A turn ends the rest: the mark of the old rest goes, and the new rest
/// past the threshold marks the card again with a later time and a new
/// mail.
#[tokio::test]
async fn a_turn_ends_the_rest_and_a_new_rest_alerts_again() {
    let td = TestDaemon::start().await;
    let (lead, w) = setup(&td, "p", "1s", true).await;
    let lead = lead.unwrap();
    let c = claimed(&td, "p", &w, "Build").await;
    until("the idle mark", || async {
        card(&td, &c).await["idle_since"].is_string()
    })
    .await;
    let first = card(&td, &c).await["idle_since"]
        .as_str()
        .unwrap()
        .to_owned();
    td.post(&format!("/api/agents/{w}/send"), json!({"text": "work"}))
        .await
        .ok();
    until("a later mark", || async {
        card(&td, &c).await["idle_since"]
            .as_str()
            .is_some_and(|s| s > first.as_str())
    })
    .await;
    assert!(changes(&td, &c, "idle_cleared").await >= 1);
    until("the second mail", || async {
        alerts(&td, &lead).await.len() == 2
    })
    .await;
}

/// No lead in the project: the operator gets the mail.
#[tokio::test]
async fn without_a_lead_the_operator_gets_the_mail() {
    let td = TestDaemon::start().await;
    let (_, w) = setup(&td, "solo", "1s", false).await;
    let c = claimed(&td, "solo", &w, "Build").await;
    until("the operator mail", || async {
        !alerts(&td, "operator").await.is_empty()
    })
    .await;
    let mail = alerts(&td, "operator").await;
    assert_eq!(mail.len(), 1, "{mail:#?}");
    assert_eq!(mail[0]["subject"], format!("solo/w1 is idle on {c} with no wait"));
}

/// The setting: a duration, `off` (also `0`), or `null` for the default.
#[tokio::test]
async fn the_idle_alert_setting_takes_a_duration_or_off() {
    let td = TestDaemon::start().await;
    td.project("p").await;
    let p = td.get("/api/projects/p").await.ok().clone();
    assert!(p.get("idle_alert").is_none(), "{p}");
    let r = td
        .patch("/api/projects/p", json!({"idle_alert": "20m"}))
        .await;
    assert_eq!(r.ok()["idle_alert"], "20m");
    let r = td.patch("/api/projects/p", json!({"idle_alert": "0"})).await;
    assert_eq!(r.ok()["idle_alert"], "off");
    let r = td
        .patch("/api/projects/p", json!({"idle_alert": "soon"}))
        .await;
    assert_eq!(r.status, 422, "{}", r.body);
    let r = td
        .patch("/api/projects/p", json!({"idle_alert": null}))
        .await;
    assert!(r.ok().get("idle_alert").is_none(), "{}", r.body);
}
