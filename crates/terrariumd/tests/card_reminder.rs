//! The card reminder (PLAN §18, "Card reminder"; GitHub issue #20), with
//! the fake supervisor: an agent that goes idle after a turn with cards
//! in the working column gets one `[System:card]` turn, and each card a
//! `reminded` event. A time wait gives `waiting` (TER-50); the unit tests
//! of `terrariumd::card_reminder` cover the status rules.
#![allow(clippy::unwrap_used, clippy::expect_used)]

use crate::common;

use std::time::Duration;

use common::TestDaemon;
use serde_json::{Value, json};
use terrarium_core::AgentId;
use terrarium_core::config::{CompactMaxAge, IdleStop};
use terrariumd::lifecycle::Tuning;
use terrariumd::supervisor::{Supervisor, TurnHold};

fn state(td: &TestDaemon) -> terrariumd::api::Shared {
    td.daemon.as_ref().unwrap().state.clone()
}

fn aid(s: &str) -> AgentId {
    s.parse().unwrap()
}

/// A short tick and the idle stop `idle`. No auto compact.
fn fast(td: &TestDaemon, idle: IdleStop) {
    state(td).lifecycle.set_tuning(Tuning {
        idle_stop: idle,
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

async fn agent(td: &TestDaemon, a: &str) -> Value {
    td.get(&format!("/api/agents/{a}")).await.ok().clone()
}

async fn events(td: &TestDaemon, path: &str) -> Vec<Value> {
    td.get(path).await.ok().as_array().unwrap().clone()
}

/// The `reminded` events of card `card`.
async fn reminded(td: &TestDaemon, card: &str) -> Vec<Value> {
    events(td, "/api/events?kind=card&limit=1000")
        .await
        .into_iter()
        .filter(|e| e["card"] == card && e["data"]["change"] == "reminded")
        .collect()
}

/// The reminder turns that agent `a` got.
fn reminders(td: &TestDaemon, a: &str) -> Vec<String> {
    td.sup
        .prompts(aid(a))
        .into_iter()
        .filter(|p| p.starts_with("[System:card]"))
        .collect()
}

/// A card that agent `w1` made and claimed: it is in the working column.
async fn claimed(td: &TestDaemon, title: &str) -> String {
    let c = td
        .post_as("p/w1", "/api/projects/p/cards", json!({"title": title}))
        .await
        .ok()["id"]
        .as_str()
        .unwrap()
        .to_owned();
    td.post_as("p/w1", &format!("/api/cards/{c}/claim"), json!({}))
        .await
        .ok();
    c
}

/// The project `p` and its agent `w1`, started and idle, with no idle stop.
async fn setup() -> (TestDaemon, String) {
    let td = TestDaemon::start().await;
    fast(&td, IdleStop::Off);
    td.project("p").await;
    let w = td.agent("p", "w1", json!({})).await["id"]
        .as_str()
        .unwrap()
        .to_owned();
    td.post_empty(&format!("/api/agents/{w}/start")).await.ok();
    td.wait_status(&w, "idle").await;
    (td, w)
}

/// One turn to agent `w`, and its end: idle again.
async fn a_turn(td: &TestDaemon, w: &str, text: &str) {
    let want = format!("[User] {text}");
    td.post(&format!("/api/agents/{w}/send"), json!({"text": text}))
        .await
        .ok();
    until("the turn", || async {
        td.sup.prompts(aid(w)).contains(&want)
    })
    .await;
    until("idle after the turn", || async {
        agent(td, w).await["status"] == "idle"
    })
    .await;
}

/// Idle after a turn, with two cards in progress: one reminder that lists
/// both, and a `reminded` event on each. The reminder's own turn ends in
/// idle again, and that idle sends nothing more.
#[tokio::test]
async fn idle_after_a_turn_reminds_once_of_the_cards_in_progress() {
    let (td, w) = setup().await;
    let c1 = claimed(&td, "One").await;
    let c2 = claimed(&td, "Two").await;
    // Not in progress: no reminder of it.
    let other = td
        .post_as("p/w1", "/api/projects/p/cards", json!({"title": "Later"}))
        .await
        .ok()["id"]
        .as_str()
        .unwrap()
        .to_owned();
    a_turn(&td, &w, "work").await;
    until("the reminder", || async { !reminders(&td, &w).is_empty() }).await;
    let r = reminders(&td, &w);
    assert_eq!(r.len(), 1, "{r:#?}");
    assert!(
        r[0].starts_with(&format!(
            "[System:card] Cards {c1}, {c2} are still in progress and assigned to you"
        )),
        "{}",
        r[0]
    );
    assert!(!r[0].contains(&other), "{}", r[0]);
    for c in [&c1, &c2] {
        until("the reminded event", || async {
            reminded(&td, c).await.len() == 1
        })
        .await;
    }
    // The reminder turn ends, and the idle after it sends nothing.
    until("idle after the reminder", || async {
        agent(&td, &w).await["status"] == "idle"
    })
    .await;
    a_turn(&td, &w, "more").await;
    tokio::time::sleep(Duration::from_millis(300)).await;
    assert_eq!(reminders(&td, &w).len(), 1);
    assert_eq!(reminded(&td, &c1).await.len(), 1);
}

/// The ETA of a card passes after its reminder: the next idle after a
/// turn reminds again and names the ETA.
#[tokio::test]
async fn a_passed_eta_makes_the_card_due_again() {
    let (td, w) = setup().await;
    let c = claimed(&td, "Late").await;
    a_turn(&td, &w, "work").await;
    until("the reminder", || async { reminders(&td, &w).len() == 1 }).await;
    assert!(!reminders(&td, &w)[0].contains("ETA has passed"));
    until("idle after the reminder", || async {
        agent(&td, &w).await["status"] == "idle"
    })
    .await;
    // The API refuses an ETA in the past: write it.
    let id = c.clone();
    state(&td)
        .db
        .write("system", move |t| {
            t.execute(
                "UPDATE cards SET eta_at = '2020-01-01T00:00:00.000Z' WHERE id = ?1",
                [&id],
            )?;
            Ok(())
        })
        .await
        .unwrap();
    a_turn(&td, &w, "more").await;
    until("the second reminder", || async {
        reminders(&td, &w).len() == 2
    })
    .await;
    let r = reminders(&td, &w);
    assert!(
        r[1].ends_with(&format!(
            "The ETA has passed: {c} (ETA 2020-01-01T00:00:00.000Z)."
        )),
        "{}",
        r[1]
    );
    until("the second reminded event", || async {
        reminded(&td, &c).await.len() == 2
    })
    .await;
    assert!(
        reminded(&td, &c)
            .await
            .iter()
            .any(|e| e["data"]["eta_passed"] == true)
    );
}

/// No reminder: a start that goes idle with no turn, a card that left
/// the working column, and an agent or a daemon with the reminder off.
#[tokio::test]
async fn no_reminder_without_a_turn_a_card_in_progress_or_with_it_off() {
    let (td, w) = setup().await;
    let c = claimed(&td, "One").await;
    // A wake with no turn: a restart goes idle without working.
    td.post_empty(&format!("/api/agents/{w}/stop")).await.ok();
    td.wait_status(&w, "stopped").await;
    td.post_empty(&format!("/api/agents/{w}/start")).await.ok();
    td.wait_status(&w, "idle").await;
    tokio::time::sleep(Duration::from_millis(300)).await;
    assert!(reminders(&td, &w).is_empty());
    // Off for the agent.
    let r = td
        .patch(&format!("/api/agents/{w}"), json!({"card_reminder": false}))
        .await;
    assert_eq!(r.ok()["card_reminder"], false, "{}", r.body);
    a_turn(&td, &w, "one").await;
    tokio::time::sleep(Duration::from_millis(300)).await;
    assert!(reminders(&td, &w).is_empty());
    // Back to the global default, which is off here.
    state(&td).lifecycle.reminders.set_default(false);
    let r = td
        .patch(&format!("/api/agents/{w}"), json!({"card_reminder": null}))
        .await;
    assert!(r.ok().get("card_reminder").is_none(), "{}", r.body);
    assert_eq!(td.get("/api/health").await.ok()["card_reminder"], false);
    a_turn(&td, &w, "two").await;
    tokio::time::sleep(Duration::from_millis(300)).await;
    assert!(reminders(&td, &w).is_empty());
    // On again, but the card went to review: nothing in progress. The
    // move comes first: the check of the idle after "two" runs in the
    // lifecycle, and under load it can still run after the 300 ms above
    // (TER-110). With the card out of the working column, a late check
    // finds no card either way.
    td.post_as(
        "p/w1",
        &format!("/api/cards/{c}/move"),
        json!({"column": "review"}),
    )
    .await
    .ok();
    state(&td).lifecycle.reminders.set_default(true);
    a_turn(&td, &w, "three").await;
    tokio::time::sleep(Duration::from_millis(300)).await;
    assert!(reminders(&td, &w).is_empty());
    assert!(reminded(&td, &c).await.is_empty());
}

/// With an idle stop, the reminder comes first: the agent gets it, and
/// then stops for idleness.
#[tokio::test]
async fn the_reminder_comes_before_the_idle_stop() {
    let (td, w) = setup().await;
    claimed(&td, "One").await;
    // The agent rested since the setup, and on a loaded machine that rest
    // can pass the 300 ms before the turn lands: an idle stop would then
    // come first. A quota hold (`blocked`, not a rest) clears the rest
    // clock. The turn waits in the hold, and the release makes the agent
    // `idle` and starts the turn at once.
    td.sup
        .hold_turns(
            aid(&w),
            Some(TurnHold {
                until: None,
                reason: "quota".into(),
            }),
        )
        .await
        .unwrap();
    td.wait_status(&w, "blocked").await;
    fast(&td, IdleStop::After(Duration::from_millis(300)));
    td.post(&format!("/api/agents/{w}/send"), json!({"text": "work"}))
        .await
        .ok();
    td.sup.hold_turns(aid(&w), None).await.unwrap();
    // An idle stop after the turn without the reminder leaves the agent
    // stopped, and this wait fails.
    until("the reminder", || async { !reminders(&td, &w).is_empty() }).await;
    let start = std::time::Instant::now();
    loop {
        let a = agent(&td, &w).await;
        if a["status"] == "stopped" {
            break;
        }
        assert!(
            start.elapsed() < common::WAIT,
            "no idle stop after the reminder: {a}\n{:#?}",
            td.sup.prompts(aid(&w))
        );
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    assert_eq!(agent(&td, &w).await["status_reason"], "idle");
    let prompts = td.sup.prompts(aid(&w));
    assert_eq!(prompts.len(), 2, "{prompts:#?}");
    assert_eq!(prompts[0], "[User] work");
    assert!(
        prompts[1].starts_with("[System:card] Card "),
        "{}",
        prompts[1]
    );
}

/// For the idle pass tests: agent `w1` idle, its rest past an idle stop of
/// 1 ms, and a timer that makes no pass of its own (a tick of one hour
/// from before the agent exists), so the test makes each pass. The first
/// read starts the rest clock in case the hub listener has not started it
/// yet.
async fn idle_pass_setup() -> (TestDaemon, String, terrariumd::api::Shared) {
    let td = TestDaemon::start().await;
    let s = state(&td);
    let tuning = |idle_stop| Tuning {
        idle_stop,
        tick: Duration::from_secs(3600),
        compact_on_idle: false,
        compact_max_age: CompactMaxAge::Off,
        compact_wait: Duration::from_secs(5),
    };
    s.lifecycle.set_tuning(tuning(IdleStop::Off));
    td.project("p").await;
    let w = td.agent("p", "w1", json!({})).await["id"]
        .as_str()
        .unwrap()
        .to_owned();
    td.post_empty(&format!("/api/agents/{w}/start")).await.ok();
    td.wait_status(&w, "idle").await;
    s.lifecycle
        .set_tuning(tuning(IdleStop::After(Duration::from_millis(1))));
    terrariumd::lifecycle::idle_read(&s).await.unwrap();
    tokio::time::sleep(Duration::from_millis(20)).await;
    (td, w, s)
}

/// TER-141: the idle pass reads the agents, then awaits the store before
/// it stops one. A turn that starts after the read and is still on at the
/// stop keeps the agent running: the stop checks the live status, not the
/// old read. Without that check the agent stops in its turn, before the
/// card reminder of its idle.
#[tokio::test]
async fn a_turn_after_the_idle_read_is_not_stopped() {
    let (td, w, s) = idle_pass_setup().await;
    let read = terrariumd::lifecycle::idle_read(&s).await.unwrap();
    td.sup.open_turn(aid(&w));
    terrariumd::lifecycle::idle_act(&s, read).await;
    assert_eq!(
        td.sup.status(aid(&w)).await,
        Some(terrarium_core::AgentStatus::Working),
        "the idle stop ended a turn that started after its read"
    );
}

/// TER-141: a turn that started and ended after the read (here the rest
/// clock moves, as `touch` moves it) restarts the rest: no stop on the old
/// read. The next pass, past the idle stop of the new rest, stops the
/// agent, so the setup does reach the stop.
#[tokio::test]
async fn a_rest_clock_that_moved_after_the_idle_read_is_not_stopped() {
    let (td, w, s) = idle_pass_setup().await;
    let read = terrariumd::lifecycle::idle_read(&s).await.unwrap();
    s.lifecycle.touch(&w);
    terrariumd::lifecycle::idle_act(&s, read).await;
    assert_eq!(
        td.sup.status(aid(&w)).await,
        Some(terrarium_core::AgentStatus::Idle),
        "the idle stop used a rest clock that moved after its read"
    );
    tokio::time::sleep(Duration::from_millis(20)).await;
    let read = terrariumd::lifecycle::idle_read(&s).await.unwrap();
    terrariumd::lifecycle::idle_act(&s, read).await;
    assert_eq!(
        td.sup.status(aid(&w)).await,
        Some(terrarium_core::AgentStatus::Stopped)
    );
}

/// TER-50: an agent with a card in progress ends a turn with an open time
/// wait (`waiting`). The wait ends: `idle` after the turn, but its wake
/// mail comes after the debounce. No reminder before that mail: the first
/// turn after the end is the mail. The idle after the mail's turn then
/// reminds once (the card is still in progress).
#[tokio::test]
async fn no_reminder_before_the_wake_mail_of_a_wait_that_ended() {
    let (td, w) = setup().await;
    let st = state(&td);
    st.waits.set_tuning(terrariumd::waits::Tuning {
        tick: Duration::from_millis(20),
        wake_debounce: Duration::from_millis(400),
        ..terrariumd::waits::Tuning::default()
    });
    td.sup.set_mail_push(true);
    let c = claimed(&td, "Build").await;
    // Due in an hour: the test makes it due once the turn ended in
    // `waiting`. A wait due soon could end before the turn on a loaded
    // machine, and the turn then ends in `idle`.
    let at = (chrono::Utc::now() + chrono::Duration::hours(1))
        .to_rfc3339_opts(chrono::SecondsFormat::Millis, true);
    let wait = td
        .post_as(
            &w,
            &format!("/api/agents/{w}/waits"),
            json!({"kind": "time", "at": at}),
        )
        .await;
    assert_eq!(wait.status, 201, "{}", wait.body);
    let wid = wait.body["id"].as_str().unwrap().to_owned();
    // The turn ends with the wait open: `waiting`, no reminder.
    td.post(&format!("/api/agents/{w}/send"), json!({"text": "work"}))
        .await
        .ok();
    until("the turn", || async {
        td.sup.prompts(aid(&w)).contains(&"[User] work".to_owned())
    })
    .await;
    until("waiting after the turn", || async {
        agent(&td, &w).await["status"] == "waiting"
    })
    .await;
    let before = td.sup.prompts(aid(&w)).len();
    assert!(reminders(&td, &w).is_empty());
    let id = wid.clone();
    st.db
        .write("system", move |t| {
            t.execute(
                "UPDATE waits SET due_at = '2020-01-01T00:00:00.000Z' WHERE id = ?1",
                [&id],
            )?;
            Ok(())
        })
        .await
        .unwrap();
    until("the wait ends", || async {
        td.get(&format!("/api/waits/{wid}")).await.body["state"] == "resolved"
    })
    .await;
    until("the wake turn and the reminder after it", || async {
        !reminders(&td, &w).is_empty()
    })
    .await;
    let after: Vec<String> = td.sup.prompts(aid(&w))[before..].to_vec();
    assert!(after[0].starts_with("New mail"), "{after:#?}");
    let r: Vec<&String> = after
        .iter()
        .filter(|p| p.starts_with("[System:card]"))
        .collect();
    assert_eq!(r.len(), 1, "{after:#?}");
    assert!(r[0].contains(&c), "{after:#?}");
    let mail = td
        .get(&format!("/api/mail?to={w}&from=system"))
        .await
        .ok()
        .clone();
    assert!(mail.to_string().contains("wait resolved"), "{mail}");
}
