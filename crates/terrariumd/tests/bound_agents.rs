//! Bound ephemeral agents (TER-74, `crate::bound`, docs/api.md §6.10),
//! with the fake supervisor: the bind checks, the life past the lifetime,
//! the park while the card waits, the refused wake, the unpark in the same
//! session with the failure block as the turn (claude, codex, pi), and the
//! close on a merge, on `done`, and on an archive. No harness and no model
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

/// Poll `f` until it is true. A deadline, not a count of tries: on a
/// loaded build host each GET is slow.
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

/// Project `p` with the delivery columns, the templates `w-claude`,
/// `w-codex`, `w-pi`, a lead that may spawn and bind them (lifetime 1 s,
/// one at a time), and project `q`.
async fn setup() -> (TestDaemon, String) {
    let td = TestDaemon::start().await;
    fast(&td);
    td.project("p").await;
    td.project("q").await;
    let r = td
        .patch(
            "/api/projects/p/board",
            json!({"columns": [
                {"name": "backlog"}, {"name": "ready"},
                {"name": "in_progress", "role": "working"}, {"name": "validating"},
                {"name": "review"}, {"name": "accepted"},
                {"name": "blocked", "role": "blocked"}, {"name": "done"}]}),
        )
        .await;
    assert_eq!(r.status, 200, "{}", r.body);
    for h in ["claude", "codex", "pi"] {
        let r = td
            .post(
                "/api/templates",
                json!({"name": format!("w-{h}"), "project": "p", "spec": {"harness": h}}),
            )
            .await;
        assert_eq!(r.status, 201, "{}", r.body);
    }
    let lead = id(&td
        .agent(
            "p",
            "lead",
            json!({"roles": ["project_admin"],
                   "policy": {"spawn": {"templates": ["project:*"], "max_concurrent": 1,
                                        "max_lifetime": "1s", "bound": true}}}),
        )
        .await);
    (td, lead)
}

async fn card(td: &TestDaemon, project: &str, column: &str) -> String {
    let r = td
        .post(
            &format!("/api/projects/{project}/cards"),
            json!({"title": "Do it", "body": "The goal.", "column": column}),
        )
        .await;
    assert_eq!(r.status, 201, "{}", r.body);
    id(&r.body)
}

async fn spawn_bound(td: &TestDaemon, lead: &str, template: &str, card: &str) -> common::Resp {
    td.post_as(
        lead,
        &format!("/api/agents/{lead}/spawn"),
        json!({"template": template, "task": format!("{card}: do it"), "bind": format!("card:{card}")}),
    )
    .await
}

async fn mv(td: &TestDaemon, card: &str, to: &str, comment: Option<&str>) {
    let mut body = json!({"column": to});
    if let Some(c) = comment {
        body["comment"] = json!(c);
    }
    let r = td.post(&format!("/api/cards/{card}/move"), body).await;
    assert_eq!(r.status, 200, "{}", r.body);
}

async fn spawn_row(td: &TestDaemon, id: &str) -> Value {
    td.get(&format!("/api/spawns/{id}")).await.ok().clone()
}

async fn agent(td: &TestDaemon, id: &str) -> Value {
    td.get(&format!("/api/agents/{id}")).await.ok().clone()
}

async fn parked(td: &TestDaemon, eph: &str) {
    let key = eph.to_owned();
    until("the park stop", || async {
        let a = agent(td, &key).await;
        a["status"] == "stopped" && a["status_reason"] == "parked"
    })
    .await;
}

async fn sessions(td: &TestDaemon, id: &str) -> Vec<Value> {
    td.get(&format!("/api/agents/{id}/sessions"))
        .await
        .ok()
        .as_array()
        .unwrap()
        .clone()
}

/// Emit the event of a passed merge job, as the job queue does.
async fn merged(td: &TestDaemon, card: &str) {
    let card = card.to_owned();
    state(td)
        .db
        .write("system", move |t| {
            t.emit(
                terrariumd::events::EventDraft::new(
                    "job",
                    format!("merge job 01J for {card} is passed"),
                    json!({"change": "ended", "project": "p", "id": "01J", "card": card,
                           "kind": "merge", "state": "passed", "attempt": 1, "outcome": "passed"}),
                )
                .project("p"),
            )?;
            Ok(())
        })
        .await
        .unwrap();
}

/// The bind checks: an unknown kind, a card of another project, a done
/// card, a second agent on one card, and an unknown card.
#[tokio::test]
async fn bind_checks() {
    let (td, lead) = setup().await;
    let c = card(&td, "p", "in_progress").await;
    let r = td
        .post_as(
            &lead,
            &format!("/api/agents/{lead}/spawn"),
            json!({"template": "w-claude", "task": "x", "bind": "thing:1"}),
        )
        .await;
    r.err(422, "invalid");
    let other = card(&td, "q", "backlog").await;
    spawn_bound(&td, &lead, "w-claude", &other)
        .await
        .err(422, "invalid");
    let done = card(&td, "p", "done").await;
    spawn_bound(&td, &lead, "w-claude", &done)
        .await
        .err(409, "conflict");
    spawn_bound(&td, &lead, "w-claude", "P-999")
        .await
        .err(404, "not_found");
    let r = spawn_bound(&td, &lead, "w-claude", &c).await;
    assert_eq!(r.status, 201, "{}", r.body);
    assert_eq!(r.body["bound"], format!("card:{c}"));
    // A second agent on the same card. The lead's one place is taken
    // too, but the bind check comes first.
    let r = spawn_bound(&td, &lead, "w-claude", &c).await;
    r.err(409, "conflict");
    assert!(
        r.body["error"]["message"]
            .as_str()
            .unwrap()
            .contains("bound agent already"),
        "{}",
        r.body
    );
}

/// A bound agent outlives the owner's `max_lifetime` (1 s): its deadline
/// is the 30-day cap. A parked agent holds no place in `max_concurrent`.
#[tokio::test]
async fn a_bound_agent_outlives_the_lifetime_and_a_parked_one_holds_no_place() {
    let (td, lead) = setup().await;
    let c = card(&td, "p", "in_progress").await;
    let r = spawn_bound(&td, &lead, "w-claude", &c).await;
    assert_eq!(r.status, 201, "{}", r.body);
    let eph = id(&r.body);
    let deadline = chrono::DateTime::parse_from_rfc3339(r.body["deadline_at"].as_str().unwrap())
        .unwrap()
        .with_timezone(&chrono::Utc);
    assert!(
        deadline - chrono::Utc::now() > chrono::Duration::days(29),
        "{deadline}"
    );
    // Past the 1 s lifetime: still open.
    tokio::time::sleep(Duration::from_millis(1500)).await;
    assert!(spawn_row(&td, &eph).await["finished_at"].is_null());
    // The system prompt facts: bound, so no lifetime text.
    let s = state(&td);
    let key = eph.clone();
    let info =
        s.db.read(move |c| terrariumd::spawn::prompt_info(c, &key))
            .await
            .unwrap()
            .unwrap();
    assert_eq!(info.bound.as_deref(), Some(format!("card:{c}").as_str()));
    // max_concurrent 1: a plain spawn is refused while the bound agent runs.
    let plain = json!({"template": "w-claude", "task": "other"});
    td.post_as(&lead, &format!("/api/agents/{lead}/spawn"), plain.clone())
        .await
        .err(409, "conflict");
    // Parked, it holds no place.
    td.wait_status(&eph, "idle").await;
    mv(&td, &c, "review", None).await;
    parked(&td, &eph).await;
    let r = td
        .post_as(&lead, &format!("/api/agents/{lead}/spawn"), plain)
        .await;
    assert_eq!(r.status, 201, "{}", r.body);
}

/// The whole path on one harness: park on validating, no wake on mail,
/// unpark on the return to ready in the same session with the failure
/// block, park again on review, and close on the merge.
async fn round_trip(harness: &str) {
    let (td, lead) = setup().await;
    let c = card(&td, "p", "in_progress").await;
    let r = spawn_bound(&td, &lead, &format!("w-{harness}"), &c).await;
    assert_eq!(r.status, 201, "{}", r.body);
    let eph = id(&r.body);
    let aid: AgentId = eph.parse().unwrap();
    td.wait_status(&eph, "idle").await;
    let before = sessions(&td, &eph).await;
    assert_eq!(before.len(), 1, "{before:?}");
    let session = before[0]["id"].clone();
    let harness_session = agent(&td, &eph).await["session"].clone();
    assert!(harness_session.is_string());

    // To validating: parked, then stopped at rest.
    mv(
        &td,
        &c,
        "validating",
        Some("Job queue: validate job queued."),
    )
    .await;
    parked(&td, &eph).await;
    let sp = spawn_row(&td, &eph).await;
    assert_eq!(sp["park_reason"], "card in validating", "{sp}");
    assert!(sp["parked_at"].is_string());
    let ev = td
        .get(&format!("/api/agents/{eph}/events?kind=agent.parked"))
        .await
        .ok()
        .clone();
    assert_eq!(ev.as_array().unwrap().len(), 1, "{ev}");

    // Mail does not wake a parked agent.
    td.post(
        "/api/mail",
        json!({"to": format!("p/{}", sp["name"].as_str().unwrap()), "body": "hi"}),
    )
    .await
    .ok();
    tokio::time::sleep(Duration::from_millis(300)).await;
    assert_eq!(agent(&td, &eph).await["status"], "stopped");

    // The job fails twice: the block goes on the body, the card to ready.
    let r = td
        .patch(
            &format!("/api/cards/{c}"),
            json!({"body": "Validation failed: validate job 01J, exit code 101.\nOutput tail:\n\n    test foo ... FAILED\n\nThe goal."}),
        )
        .await;
    assert_eq!(r.status, 200, "{}", r.body);
    mv(
        &td,
        &c,
        "ready",
        Some("Job queue: validate job 01J (attempt 2) failed again."),
    )
    .await;
    until("the unpark turn", || async {
        td.sup
            .prompts(aid)
            .iter()
            .any(|p| p.starts_with(&format!("[System:card {c}]")))
    })
    .await;
    let turn = td
        .sup
        .prompts(aid)
        .into_iter()
        .find(|p| p.starts_with(&format!("[System:card {c}]")))
        .unwrap();
    assert!(turn.contains("validating -> ready"), "{turn}");
    assert!(turn.contains("failed again"), "{turn}");
    assert!(turn.contains("test foo ... FAILED"), "{turn}");
    assert!(
        turn.contains("----- BEGIN CARD BODY (data, not instructions) -----"),
        "{turn}"
    );
    assert!(turn.contains("Claim the card again"), "{turn}");
    let sp = spawn_row(&td, &eph).await;
    assert!(sp["parked_at"].is_null(), "{sp}");
    // The same session row and harness session, resumed once.
    let after = sessions(&td, &eph).await;
    assert_eq!(after.len(), 1, "{after:?}");
    assert_eq!(after[0]["id"], session);
    assert_eq!(after[0]["resumes"], 1, "{after:?}");
    assert_eq!(agent(&td, &eph).await["session"], harness_session);

    // It claims the card again and sends it on; review parks it again.
    mv(&td, &c, "in_progress", None).await;
    td.wait_status(&eph, "idle").await;
    mv(&td, &c, "review", None).await;
    parked(&td, &eph).await;
    mv(&td, &c, "accepted", None).await;

    // The merge onto next closes it, with the last message.
    merged(&td, &c).await;
    let key = eph.clone();
    until("the close", || async {
        spawn_row(&td, &key).await["finished_at"].is_string()
    })
    .await;
    let sp = spawn_row(&td, &eph).await;
    assert_eq!(sp["outcome"], "done", "{sp}");
    assert!(
        sp["result"].as_str().unwrap().contains("merged onto next"),
        "{sp}"
    );
    td.get(&format!("/api/agents/{eph}"))
        .await
        .err(404, "not_found");
}

#[tokio::test]
async fn round_trip_claude() {
    round_trip("claude").await;
}

#[tokio::test]
async fn round_trip_codex() {
    round_trip("codex").await;
}

#[tokio::test]
async fn round_trip_pi() {
    round_trip("pi").await;
}

/// A return to the working column unparks too; `done` and an archive
/// close.
#[tokio::test]
async fn done_and_archive_close() {
    let (td, lead) = setup().await;
    let s = state(&td);
    s.lifecycle.spawns.set_tuning(terrariumd::spawn::Tuning {
        done_delay: Duration::from_millis(50),
    });
    let a = card(&td, "p", "in_progress").await;
    let eph = id(&spawn_bound(&td, &lead, "w-claude", &a).await.body);
    td.wait_status(&eph, "idle").await;
    mv(&td, &a, "review", None).await;
    parked(&td, &eph).await;
    mv(&td, &a, "in_progress", Some("Review: fix the name.")).await;
    td.wait_status(&eph, "idle").await;
    assert!(spawn_row(&td, &eph).await["parked_at"].is_null());
    mv(&td, &a, "done", None).await;
    let key = eph.clone();
    until("the close on done", || async {
        spawn_row(&td, &key).await["finished_at"].is_string()
    })
    .await;
    assert_eq!(spawn_row(&td, &eph).await["outcome"], "done");

    let b = card(&td, "p", "in_progress").await;
    let eph = id(&spawn_bound(&td, &lead, "w-claude", &b).await.body);
    td.wait_status(&eph, "idle").await;
    td.delete(&format!("/api/cards/{b}")).await;
    let key = eph.clone();
    until("the close on archive", || async {
        spawn_row(&td, &key).await["finished_at"].is_string()
    })
    .await;
    assert_eq!(spawn_row(&td, &eph).await["outcome"], "cancelled");
}

/// TER-75: a bound agent is a card agent. `include=card` lists it with
/// its card, also while it is parked and has no assignee, and the card
/// names it (`agent`, `agent_name`) until the close.
#[tokio::test]
async fn a_bound_agent_shows_with_its_card() {
    let (td, lead) = setup().await;
    let a = card(&td, "p", "in_progress").await;
    let s = spawn_bound(&td, &lead, "w-claude", &a).await;
    assert_eq!(s.status, 201, "{}", s.body);
    let eph = id(&s.body);
    td.wait_status(&eph, "idle").await;
    let k = td.get(&format!("/api/cards/{a}")).await.ok().clone();
    assert_eq!(k["agent"], eph.as_str(), "{k}");
    assert_eq!(k["agent_name"], s.body["name"], "{k}");
    mv(&td, &a, "review", None).await;
    parked(&td, &eph).await;
    let list = td
        .get("/api/agents?project=p&include=card")
        .await
        .ok()
        .clone();
    let row = list
        .as_array()
        .unwrap()
        .iter()
        .find(|x| x["id"] == eph.as_str())
        .cloned()
        .unwrap_or_else(|| panic!("no card agent in {list}"));
    assert_eq!(row["current_card"], a.as_str(), "{row}");
    let board = td.get("/api/projects/p/board").await.ok().clone();
    let tile = board["cards"]
        .as_array()
        .unwrap()
        .iter()
        .find(|c| c["id"] == a.as_str())
        .cloned()
        .unwrap();
    assert_eq!(tile["agent_name"], s.body["name"], "{tile}");
    mv(&td, &a, "done", None).await;
    let key = eph.clone();
    until("the close on done", || async {
        spawn_row(&td, &key).await["finished_at"].is_string()
    })
    .await;
    let k = td.get(&format!("/api/cards/{a}")).await.ok().clone();
    assert!(k["agent"].is_null(), "{k}");
    let list = td
        .get("/api/agents?project=p&include=card")
        .await
        .ok()
        .clone();
    assert!(
        !list
            .as_array()
            .unwrap()
            .iter()
            .any(|x| x["id"] == eph.as_str()),
        "{list}"
    );
}

/// The bind right (review round 2): an owner without `spawn.bound` gets
/// 403; with it, the cap is its `spawn.max_bound_lifetime`. A lead may not
/// give the right.
#[tokio::test]
async fn binding_is_a_right_with_the_policy_cap() {
    let (td, lead) = setup().await;
    let w = id(&td
        .agent(
            "p",
            "w",
            json!({"policy": {"spawn": {"templates": ["project:*"]}}}),
        )
        .await);
    let c = card(&td, "p", "in_progress").await;
    let r = spawn_bound(&td, &w, "w-claude", &c).await;
    r.err(403, "forbidden");
    assert!(
        r.body["error"]["message"]
            .as_str()
            .unwrap()
            .contains("spawn-bound"),
        "{}",
        r.body
    );
    // A lead cannot give the right.
    td.patch_as(
        &lead,
        &format!("/api/agents/{w}"),
        json!({"policy": {"spawn": {"templates": ["project:*"], "bound": true}}}),
    )
    .await
    .err(403, "forbidden");
    // The operator gives it, with a 2-day cap.
    td.patch(
        &format!("/api/agents/{w}"),
        json!({"policy": {"spawn": {"templates": ["project:*"], "bound": true,
                                    "max_bound_lifetime": "2d"}}}),
    )
    .await
    .ok();
    let r = spawn_bound(&td, &w, "w-claude", &c).await;
    assert_eq!(r.status, 201, "{}", r.body);
    let left = chrono::DateTime::parse_from_rfc3339(r.body["deadline_at"].as_str().unwrap())
        .unwrap()
        .with_timezone(&chrono::Utc)
        - chrono::Utc::now();
    assert!(
        left > chrono::Duration::hours(47) && left <= chrono::Duration::hours(48),
        "{left}"
    );
}

/// An unpark respects `spawn.max_concurrent` (review round 2): with the
/// owner at its limit, a returned card's agent stays parked with the
/// unpark queued, and starts (with its turn) when a place frees.
#[tokio::test]
async fn an_unpark_waits_for_a_place_of_the_owner() {
    let (td, lead) = setup().await;
    let a = card(&td, "p", "in_progress").await;
    let b = card(&td, "p", "in_progress").await;
    let ea = id(&spawn_bound(&td, &lead, "w-claude", &a).await.body);
    td.wait_status(&ea, "idle").await;
    mv(&td, &a, "review", None).await;
    parked(&td, &ea).await;
    // A parked agent holds no place: B runs (max_concurrent 1).
    let r = spawn_bound(&td, &lead, "w-claude", &b).await;
    assert_eq!(r.status, 201, "{}", r.body);
    let eb = id(&r.body);
    td.wait_status(&eb, "idle").await;
    // A comes back while B runs: the unpark waits.
    mv(&td, &a, "ready", Some("Review: changes.")).await;
    let key = ea.clone();
    until("the queued unpark", || async {
        spawn_row(&td, &key).await["unpark_queued_at"].is_string()
    })
    .await;
    tokio::time::sleep(Duration::from_millis(300)).await;
    let sp = spawn_row(&td, &ea).await;
    assert!(sp["parked_at"].is_string(), "{sp}");
    assert_eq!(agent(&td, &ea).await["status"], "stopped");
    let ev = td
        .get(&format!("/api/agents/{ea}/events?kind=agent.unpark_queued"))
        .await
        .ok()
        .clone();
    assert_eq!(ev.as_array().unwrap().len(), 1, "{ev}");
    // B parks: the place frees, and A starts with its turn.
    mv(&td, &b, "validating", None).await;
    td.wait_status(&ea, "idle").await;
    let sp = spawn_row(&td, &ea).await;
    assert!(sp["parked_at"].is_null(), "{sp}");
    assert!(sp["unpark_queued_at"].is_null(), "{sp}");
    let aid: AgentId = ea.parse().unwrap();
    let key = a.clone();
    until("the return turn of A", || async {
        td.sup
            .prompts(aid)
            .iter()
            .any(|p| p.starts_with(&format!("[System:card {key}]")))
    })
    .await;
    // B came back too while A runs: B waits now.
    mv(&td, &b, "ready", None).await;
    let key = eb.clone();
    until("the queued unpark of B", || async {
        spawn_row(&td, &key).await["unpark_queued_at"].is_string()
    })
    .await;
    // A finishes (its card is done): B starts.
    mv(&td, &a, "done", None).await;
    let key = eb.clone();
    until("B unparked", || async {
        spawn_row(&td, &key).await["parked_at"].is_null()
    })
    .await;
    td.wait_status(&eb, "idle").await;
}

/// A move between two columns that are not wait columns does not park
/// (review round 2).
#[tokio::test]
async fn a_move_between_other_columns_does_not_park() {
    let (td, lead) = setup().await;
    let c = card(&td, "p", "in_progress").await;
    let eph = id(&spawn_bound(&td, &lead, "w-claude", &c).await.body);
    td.wait_status(&eph, "idle").await;
    mv(&td, &c, "blocked", Some("Question to lead.")).await;
    mv(&td, &c, "ready", None).await;
    mv(&td, &c, "backlog", None).await;
    tokio::time::sleep(Duration::from_millis(400)).await;
    assert!(spawn_row(&td, &eph).await["parked_at"].is_null());
    assert_eq!(agent(&td, &eph).await["status"], "idle");
}
