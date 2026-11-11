//! Bound ephemeral agents (TER-74, `crate::bound`, docs/api.md §6.10),
//! with the fake supervisor: the bind checks, the life past the lifetime,
//! the park while the card waits, the refused wake, the unpark in the same
//! session with the failure block as the turn (claude, codex, pi), and the
//! close on a merge, on `done`, and on an archive. No harness and no model
//! turn runs.
//!
//! The tests of the card driver run twice ([`both_flows!`], TER-170): with
//! `board_flow = code` (`crate::bound`) and with `board_flow = rules` (the
//! rules `agent-*` of the built-in pack), with the same parks, unparks,
//! turns, and ends.
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

/// Run the test `$name(flow)` once with each board flow (TER-170): the
/// tests `$name::code` and `$name::rules`.
macro_rules! both_flows {
    ($name:ident) => {
        mod $name {
            #[tokio::test]
            async fn code() {
                super::$name("code").await;
            }
            #[tokio::test]
            async fn rules() {
                super::$name("rules").await;
            }
        }
    };
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
    setup_flow("code").await
}

/// [`setup`] with the board flow `flow` (`code` or `rules`) for `p`.
async fn setup_flow(flow: &str) -> (TestDaemon, String) {
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
                {"name": "review"}, {"name": "merge_queue"}, {"name": "accepted"},
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
    if flow != "code" {
        let r = td
            .patch("/api/projects/p", json!({"board_flow": flow}))
            .await;
        assert_eq!(r.status, 200, "{}", r.body);
    }
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
async fn round_trip(harness: &str, flow: &str) {
    let (td, lead) = setup_flow(flow).await;
    let c = card(&td, "p", "in_progress").await;
    let r = spawn_bound(&td, &lead, &format!("w-{harness}"), &c).await;
    assert_eq!(r.status, 201, "{}", r.body);
    let eph = id(&r.body);
    let aid: AgentId = eph.parse().unwrap();
    // Startup idle precedes the task and its harness session metadata.
    td.wait_task_done(&eph).await;
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
    round_trip("claude", "code").await;
}

#[tokio::test]
async fn round_trip_codex() {
    round_trip("codex", "code").await;
}

#[tokio::test]
async fn round_trip_pi() {
    round_trip("pi", "code").await;
}

/// The round trip with the rule pack (TER-170).
#[tokio::test]
async fn round_trip_claude_rules() {
    round_trip("claude", "rules").await;
}

both_flows!(forward_moves_stay_parked_and_returns_wake);
/// Forward delivery moves never send a return turn. Review and merge
/// failures still wake the same agent in both board flows.
async fn forward_moves_stay_parked_and_returns_wake(flow: &str) {
    let (td, lead) = setup_flow(flow).await;
    let c = card(&td, "p", "in_progress").await;
    let eph = id(&spawn_bound(&td, &lead, "w-claude", &c).await.body);
    td.wait_status(&eph, "idle").await;
    for destination in ["ready", "blocked"] {
        mv(&td, &c, "review", None).await;
        parked(&td, &eph).await;
        let before = return_turns(&td, &eph, &c).len();
        for to in ["merge_queue", "accepted"] {
            mv(&td, &c, to, None).await;
            tokio::time::sleep(Duration::from_millis(400)).await;
            let sp = spawn_row(&td, &eph).await;
            assert!(sp["parked_at"].is_string(), "{sp}");
            assert!(sp["unpark_queued_at"].is_null(), "{sp}");
            assert_eq!(agent(&td, &eph).await["status"], "stopped");
            assert_eq!(return_turns(&td, &eph, &c).len(), before);
        }
        mv(&td, &c, destination, Some("Review or merge failed.")).await;
        td.wait_status(&eph, "idle").await;
        until("the failure return turn", || async {
            return_turns(&td, &eph, &c).len() == before + 1
        })
        .await;
    }
}

both_flows!(done_and_archive_close);
both_flows!(renamed_wait_columns_park_and_return);

async fn renamed_wait_columns_park_and_return(flow: &str) {
    let (td, lead) = setup_flow(flow).await;
    let r = td
        .patch(
            "/api/projects/p/board",
            json!({"columns": [
                {"name": "backlog"}, {"name": "ready"},
                {"name": "in_progress", "role": "working"},
                {"name": "qa", "rename_from": "validating", "role": "validating"},
                {"name": "review"}, {"name": "merge_queue"}, {"name": "accepted"},
                {"name": "blocked", "role": "blocked"}, {"name": "done"}
            ]}),
        )
        .await;
    assert_eq!(r.status, 200, "{}", r.body);
    let c = card(&td, "p", "in_progress").await;
    let eph = id(&spawn_bound(&td, &lead, "w-claude", &c).await.body);
    td.wait_status(&eph, "idle").await;
    mv(&td, &c, "qa", None).await;
    parked(&td, &eph).await;
    assert_eq!(spawn_row(&td, &eph).await["park_reason"], "card in qa");
    // Clearing an assignee must not unpark a card still waiting by role.
    let r = td
        .post(&format!("/api/cards/{c}/assign"), json!({"assignee": null}))
        .await;
    assert_eq!(r.status, 200, "{}", r.body);
    tokio::time::sleep(Duration::from_millis(200)).await;
    assert!(spawn_row(&td, &eph).await["parked_at"].is_string());
    mv(&td, &c, "ready", Some("QA failed")).await;
    td.wait_status(&eph, "idle").await;
    assert!(spawn_row(&td, &eph).await["parked_at"].is_null());
}

/// A return to the working column unparks too; `done` and an archive
/// close.
async fn done_and_archive_close(flow: &str) {
    let (td, lead) = setup_flow(flow).await;
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

both_flows!(an_unpark_waits_for_a_place_of_the_owner);
/// An unpark respects `spawn.max_concurrent` (review round 2): with the
/// owner at its limit, a returned card's agent stays parked with the
/// unpark queued, and starts (with its turn) when a place frees.
async fn an_unpark_waits_for_a_place_of_the_owner(flow: &str) {
    let (td, lead) = setup_flow(flow).await;
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

both_flows!(a_move_between_other_columns_does_not_park);
/// A move between two columns that are not wait columns does not park
/// (review round 2).
async fn a_move_between_other_columns_does_not_park(flow: &str) {
    let (td, lead) = setup_flow(flow).await;
    let c = card(&td, "p", "in_progress").await;
    let eph = id(&spawn_bound(&td, &lead, "w-claude", &c).await.body);
    // The status assertion below must not race the initial task turn.
    td.wait_task_done(&eph).await;
    mv(&td, &c, "blocked", Some("Question to lead.")).await;
    mv(&td, &c, "ready", None).await;
    mv(&td, &c, "backlog", None).await;
    tokio::time::sleep(Duration::from_millis(400)).await;
    assert!(spawn_row(&td, &eph).await["parked_at"].is_null());
    assert_eq!(agent(&td, &eph).await["status"], "idle");
}

/// Agent `name` of project `p` for the tests of TER-111, as a worker.
async fn worker(td: &TestDaemon, name: &str) -> String {
    id(&td.agent("p", name, json!({})).await)
}

/// The prompts of `eph` that are the return turn of `card`.
fn return_turns(td: &TestDaemon, eph: &str, card: &str) -> Vec<String> {
    let aid: AgentId = eph.parse().unwrap();
    td.sup
        .prompts(aid)
        .into_iter()
        .filter(|p| p.starts_with(&format!("[System:card {card}]")))
        .collect()
}

/// Move `card` to the column `to` as an event that the driver missed: the
/// card and the event log change, but the hub never sends the event.
async fn hidden_move(td: &TestDaemon, card: &str, to: &str) {
    let (card, to) = (card.to_owned(), to.to_owned());
    state(td)
        .db
        .write("system", move |t| {
            let from: String = t.query_row(
                "SELECT bc.name FROM cards c JOIN board_columns bc ON bc.id = c.column_id
                 WHERE c.id = ?1",
                [&card],
                |r| r.get(0),
            )?;
            t.execute(
                "UPDATE cards SET column_id = (SELECT bc.id FROM board_columns bc
                     JOIN cards c ON c.project = bc.project WHERE c.id = ?1 AND bc.name = ?2)
                 WHERE id = ?1",
                rusqlite::params![card, to],
            )?;
            terrariumd::events::insert(
                t,
                "system",
                terrariumd::events::EventDraft::new(
                    "card",
                    format!("{card} moved {from} -> {to} by operator"),
                    json!({"change": "moved", "from": from, "to": to}),
                )
                .card(card.clone())
                .project("p"),
            )?;
            Ok(())
        })
        .await
        .unwrap();
}

/// TER-111 (A1): an event that the driver missed is repaired by the
/// reconcile (it runs at the driver's start and after a lag). A running
/// agent whose card moved into a wait column parks; a parked agent whose
/// card is back unparks in its own session, with a return turn. An agent
/// bound to a card that waited already is left alone, as the event rule
/// leaves it (review round 2).
#[tokio::test]
async fn reconcile_repairs_a_missed_park_and_unpark() {
    let (td, lead) = setup().await;
    let s = state(&td);
    s.db.write("test rename accepted", |t| {
        t.execute("UPDATE board_columns SET name = 'shipped', role = 'accepted' WHERE project = 'p' AND name = 'accepted'", [])?;
        Ok(())
    }).await.unwrap();
    // Bound to a card in review (a reviewer): no move since the spawn,
    // so no missed park.
    let a = card(&td, "p", "review").await;
    let ea = id(&spawn_bound(&td, &lead, "w-claude", &a).await.body);
    td.wait_status(&ea, "idle").await;
    assert_eq!(terrariumd::bound::reconcile(&s).await, 0);
    assert!(spawn_row(&td, &ea).await["parked_at"].is_null());
    // A missed park: the card moves on to accepted, and the driver does
    // not see the move.
    hidden_move(&td, &a, "shipped").await;
    tokio::time::sleep(Duration::from_millis(300)).await;
    assert!(spawn_row(&td, &ea).await["parked_at"].is_null());
    assert_eq!(terrariumd::bound::reconcile(&s).await, 1);
    parked(&td, &ea).await;
    assert_eq!(spawn_row(&td, &ea).await["park_reason"], "card in accepted");
    // In line now: a second reconcile does nothing.
    assert_eq!(terrariumd::bound::reconcile(&s).await, 0);
    td.post(&format!("/api/agents/{ea}/cancel"), json!({}))
        .await;

    // A missed unpark: parked, and the card is back in in_progress.
    let b = card(&td, "p", "in_progress").await;
    let key = ea.clone();
    until("the place of A is free", || async {
        spawn_row(&td, &key).await["finished_at"].is_string()
    })
    .await;
    let eb = id(&spawn_bound(&td, &lead, "w-codex", &b).await.body);
    td.wait_status(&eb, "idle").await;
    assert!(
        terrariumd::bound::park(&s, &eb, "card in review")
            .await
            .unwrap()
    );
    parked(&td, &eb).await;
    assert_eq!(terrariumd::bound::reconcile(&s).await, 1);
    let key = eb.clone();
    until("the unpark", || async {
        spawn_row(&td, &key).await["parked_at"].is_null()
    })
    .await;
    until("the return turn", || async {
        !return_turns(&td, &eb, &b).is_empty()
    })
    .await;
    let turn = return_turns(&td, &eb, &b).remove(0);
    assert!(turn.contains("it is in in_progress now"), "{turn}");
    assert!(turn.contains("BEGIN CARD BODY"), "{turn}");
    td.wait_status(&eb, "idle").await;
    let after = sessions(&td, &eb).await;
    assert_eq!(after.len(), 1, "{after:?}");
    assert_eq!(after[0]["resumes"], 1, "{after:?}");
    assert_eq!(terrariumd::bound::reconcile(&s).await, 0);
}

both_flows!(a_release_in_a_wait_column_keeps_the_agent_parked);
/// TER-331: assignment releases are shared by both flows, but a release
/// while the card still waits must not queue a return turn or unpark it.
async fn a_release_in_a_wait_column_keeps_the_agent_parked(flow: &str) {
    let (td, lead) = setup_flow(flow).await;
    let w = worker(&td, "w").await;
    let c = card(&td, "p", "in_progress").await;
    let eph = id(&spawn_bound(&td, &lead, "w-claude", &c).await.body);
    td.wait_status(&eph, "idle").await;
    for column in terrariumd::bound::WAIT_COLUMNS {
        mv(&td, &c, column, None).await;
        parked(&td, &eph).await;
        td.post(&format!("/api/cards/{c}/assign"), json!({"assignee": w}))
            .await
            .ok();
        td.post(&format!("/api/cards/{c}/assign"), json!({"assignee": null}))
            .await
            .ok();
        // Let the driver's assignment event run before checking its absence
        // of effects. The positive control below proves release delivery.
        tokio::time::sleep(Duration::from_millis(400)).await;
        let sp = spawn_row(&td, &eph).await;
        assert!(sp["parked_at"].is_string(), "{sp}");
        assert!(sp["unpark_queued_at"].is_null(), "{sp}");
        assert_eq!(agent(&td, &eph).await["status"], "stopped");
        assert!(return_turns(&td, &eph, &c).is_empty());
    }
    mv(&td, &c, "ready", None).await;
    td.wait_status(&eph, "idle").await;
    until("return after leaving the wait column", || async {
        !return_turns(&td, &eph, &c).is_empty()
    })
    .await;
    assert_eq!(return_turns(&td, &eph, &c).len(), 1);
}

/// TER-331: repeated refusals deduplicate, but an assignment event
/// resets that state even when the same worker takes the card again.
#[tokio::test]
async fn an_assignment_change_resets_unpark_refusal_dedup() {
    let (td, lead) = setup().await;
    let w = worker(&td, "w").await;
    let c = card(&td, "p", "in_progress").await;
    let eph = id(&spawn_bound(&td, &lead, "w-claude", &c).await.body);
    td.wait_status(&eph, "idle").await;
    mv(&td, &c, "review", None).await;
    parked(&td, &eph).await;
    let path = format!("/api/cards/{c}/assign");
    let s = state(&td);
    for expected in 1..=2 {
        td.post(&path, json!({"assignee": w})).await.ok();
        let key = eph.clone();
        s.db.write("system", move |t| {
            for _ in 0..3 {
                assert_eq!(
                    terrariumd::bound::unpark_tx(t, &key, None, "test")?.0,
                    terrariumd::bound::Unparked::Refused
                );
            }
            Ok(())
        })
        .await
        .unwrap();
        let events = td
            .get(&format!(
                "/api/agents/{eph}/events?kind=agent.unpark_refused"
            ))
            .await;
        assert_eq!(events.ok().as_array().unwrap().len(), expected);
        td.post(&path, json!({"assignee": null})).await.ok();
    }
}

both_flows!(an_unpark_is_refused_when_another_agent_has_the_card);
/// TER-111 (A2): the bound agent does not unpark when another agent
/// works its card, so two agents never work one card. The unpark that
/// waited for a place is dropped too.
async fn an_unpark_is_refused_when_another_agent_has_the_card(flow: &str) {
    let (td, lead) = setup_flow(flow).await;
    let w = worker(&td, "w").await;
    let a = card(&td, "p", "in_progress").await;
    let b = card(&td, "p", "in_progress").await;
    let ea = id(&spawn_bound(&td, &lead, "w-claude", &a).await.body);
    td.wait_status(&ea, "idle").await;
    mv(&td, &a, "review", None).await;
    parked(&td, &ea).await;
    // B takes the lead's one place.
    let eb = id(&spawn_bound(&td, &lead, "w-claude", &b).await.body);
    td.wait_status(&eb, "idle").await;
    // A's card comes back: the unpark waits for a place.
    mv(&td, &a, "ready", Some("Review: changes.")).await;
    let key = ea.clone();
    until("the queued unpark", || async {
        spawn_row(&td, &key).await["unpark_queued_at"].is_string()
    })
    .await;
    // The worker claims A's card meanwhile.
    let r = td
        .post_as(&w, &format!("/api/cards/{a}/claim"), json!({}))
        .await;
    assert_eq!(r.status, 200, "{}", r.body);
    let key = ea.clone();
    until("the refused unpark", || async {
        spawn_row(&td, &key).await["unpark_queued_at"].is_null()
    })
    .await;
    let ev = td
        .get(&format!(
            "/api/agents/{ea}/events?kind=agent.unpark_refused"
        ))
        .await
        .ok()
        .clone();
    assert!(!ev.as_array().unwrap().is_empty(), "{ev}");
    // The return turn that the queued unpark queued goes with it, so a
    // later unpark does not deliver it (review round 2).
    let key = ea.clone();
    let waiting = state(&td)
        .db
        .read(move |c| terrariumd::store::agents::queued_turns(c, &key))
        .await
        .unwrap();
    let card_tag = format!("[System:card {a}]");
    assert!(
        !waiting.iter().any(|q| q.text.starts_with(&card_tag)),
        "{waiting:?}"
    );
    // B parks: a place frees, but A stays parked, with no turn.
    mv(&td, &b, "validating", None).await;
    parked(&td, &eb).await;
    tokio::time::sleep(Duration::from_millis(400)).await;
    assert!(spawn_row(&td, &ea).await["parked_at"].is_string());
    assert_eq!(agent(&td, &ea).await["status"], "stopped");
    assert!(return_turns(&td, &ea, &a).is_empty());
    // The reconcile keeps it parked too.
    assert_eq!(terrariumd::bound::reconcile(&state(&td)).await, 0);
    assert!(spawn_row(&td, &ea).await["parked_at"].is_string());
    for _ in 0..3 {
        assert_eq!(terrariumd::bound::reconcile(&state(&td)).await, 0);
    }
    let repeated = td
        .get(&format!(
            "/api/agents/{ea}/events?kind=agent.unpark_refused"
        ))
        .await;
    assert_eq!(
        repeated.ok().as_array().unwrap().len(),
        ev.as_array().unwrap().len()
    );

    // Releasing the assignment in place wakes A without a reconcile.
    let r = td
        .post(&format!("/api/cards/{a}/assign"), json!({"assignee": null}))
        .await;
    assert_eq!(r.status, 200, "{}", r.body);
    let key = ea.clone();
    until("unpark after unassign", || async {
        spawn_row(&td, &key).await["parked_at"].is_null()
    })
    .await;
    td.wait_status(&ea, "idle").await;
    until("the release turn", || async {
        !return_turns(&td, &ea, &a).is_empty()
    })
    .await;
    assert_eq!(return_turns(&td, &ea, &a).len(), 1);
    mv(&td, &a, "review", None).await;
    parked(&td, &ea).await;

    // A direct move with another assignee: review -> in_progress by a claim.
    let w2 = worker(&td, "w2").await;
    mv(&td, &b, "review", None).await;
    let r = td
        .post_as(&w2, &format!("/api/cards/{b}/claim"), json!({}))
        .await;
    assert_eq!(r.status, 200, "{}", r.body);
    tokio::time::sleep(Duration::from_millis(400)).await;
    assert!(spawn_row(&td, &eb).await["parked_at"].is_string());
    assert!(return_turns(&td, &eb, &b).is_empty());
}

/// TER-163: a card that goes back into a wait column while its unpark
/// waits for a place drops the queued unpark and its return turn. When a
/// place frees, the agent stays parked.
#[tokio::test]
async fn a_re_park_drops_a_queued_unpark() {
    re_park_drops_a_queued_unpark(false).await;
}

#[tokio::test]
async fn reconcile_drops_a_queued_custom_unpark() {
    re_park_drops_a_queued_unpark(true).await;
}

async fn re_park_drops_a_queued_unpark(missed: bool) {
    let (td, lead) = setup().await;
    let a = card(&td, "p", "in_progress").await;
    let b = card(&td, "p", "in_progress").await;
    let ea = id(&spawn_bound(&td, &lead, "w-claude", &a).await.body);
    td.wait_status(&ea, "idle").await;
    mv(&td, &a, "review", None).await;
    parked(&td, &ea).await;
    // B takes the lead's one place.
    let eb = id(&spawn_bound(&td, &lead, "w-claude", &b).await.body);
    td.wait_status(&eb, "idle").await;
    // A comes back while B runs: the unpark waits.
    mv(&td, &a, "ready", Some("Review: changes.")).await;
    let key = ea.clone();
    until("the queued unpark", || async {
        spawn_row(&td, &key).await["unpark_queued_at"].is_string()
    })
    .await;
    // A custom agent.unpark turn has no default return prefix. An
    // unrelated queued turn must survive the re-park.
    let key = ea.clone();
    state(&td)
        .db
        .write("system", move |t| {
            terrariumd::bound::unpark_tx(t, &key, Some("Custom return"), "rule")?;
            terrariumd::store::agents::queue_turn(t, &key, "Unrelated turn")?;
            Ok(())
        })
        .await
        .unwrap();
    // A goes back to review before a place frees: the unpark is dropped.
    if missed {
        hidden_move(&td, &a, "review").await;
        terrariumd::bound::reconcile(&state(&td)).await;
    } else {
        mv(&td, &a, "review", None).await;
    }
    let key = ea.clone();
    until("the dropped unpark", || async {
        spawn_row(&td, &key).await["unpark_queued_at"].is_null()
    })
    .await;
    let ev = td
        .get(&format!(
            "/api/agents/{ea}/events?kind=agent.unpark_dropped"
        ))
        .await
        .ok()
        .clone();
    assert_eq!(ev.as_array().unwrap().len(), 1, "{ev}");
    let key = ea.clone();
    let waiting = state(&td)
        .db
        .read(move |c| terrariumd::store::agents::queued_turns(c, &key))
        .await
        .unwrap();
    let card_tag = format!("[System:card {a}]");
    assert!(
        !waiting.iter().any(|q| q.text.starts_with(&card_tag)),
        "{waiting:?}"
    );
    assert!(
        !waiting.iter().any(|q| q.text == "Custom return"),
        "{waiting:?}"
    );
    assert!(
        waiting.iter().any(|q| q.text == "Unrelated turn"),
        "{waiting:?}"
    );
    // B parks: a place frees, but A stays parked, with no turn.
    mv(&td, &b, "validating", None).await;
    parked(&td, &eb).await;
    tokio::time::sleep(Duration::from_millis(400)).await;
    let sp = spawn_row(&td, &ea).await;
    assert!(sp["parked_at"].is_string(), "{sp}");
    assert_eq!(agent(&td, &ea).await["status"], "stopped");
    assert!(return_turns(&td, &ea, &a).is_empty());
    // A comes back again: it unparks with one return turn, the new one.
    mv(&td, &a, "ready", Some("Second round.")).await;
    td.wait_status(&ea, "idle").await;
    let key = ea.clone();
    let card_id = a.clone();
    until("the return turn of A", || async {
        !return_turns(&td, &key, &card_id).is_empty()
    })
    .await;
    let turns = return_turns(&td, &ea, &a);
    assert_eq!(turns.len(), 1, "{turns:?}");
    assert!(turns[0].contains("Second round."), "{turns:?}");
}

/// TER-111 (R2-1): the park stop checks the park again. An agent that
/// was unparked after the idle pass read the parked agents runs on.
#[tokio::test]
async fn a_park_stop_spares_an_unparked_agent() {
    let (td, lead) = setup().await;
    let c = card(&td, "p", "in_progress").await;
    let eph = id(&spawn_bound(&td, &lead, "w-claude", &c).await.body);
    // The task turn first: else the check below can see it `working`.
    td.wait_task_done(&eph).await;
    // The idle pass saw it parked; the unpark landed before the stop.
    terrariumd::lifecycle::park_stop(&state(&td), eph.parse().unwrap()).await;
    tokio::time::sleep(Duration::from_millis(1000)).await;
    let a = agent(&td, &eph).await;
    assert_eq!(a["status"], "idle", "{a}");
    assert_ne!(a["status_reason"], "parked", "{a}");
    // Not stopped and woken again: the first session, never resumed.
    let s = sessions(&td, &eph).await;
    assert_eq!(s.len(), 1, "{s:?}");
    assert_eq!(s[0]["resumes"], 0, "{s:?}");
}

/// TER-183: the unpark reaches the stop while it owns the WakeGuard.
/// Its Busy wake must be retried after stop, preserving the session.
#[tokio::test]
async fn an_unpark_during_a_park_stop_wakes_after_the_stop() {
    let (td, lead) = setup().await;
    let s = state(&td);
    // A card reminder after the spawn task would add an unrelated turn
    // while stop is gated. This test controls the task and return turns only.
    s.lifecycle.reminders.set_default(false);
    // The card stays in_progress while we park manually. Reconciliation
    // would unpark it on the next fast tick, before the gated stop enters.
    // Drive the park stop and unpark explicitly instead of racing that tick.
    s.lifecycle.set_tuning(terrariumd::lifecycle::Tuning {
        idle_stop: IdleStop::Off,
        tick: Duration::from_secs(3600),
        compact_on_idle: false,
        compact_max_age: CompactMaxAge::Off,
        compact_wait: Duration::from_secs(5),
    });
    let c = card(&td, "p", "in_progress").await;
    let eph = id(&spawn_bound(&td, &lead, "w-claude", &c).await.body);
    let agent_id = eph.parse().unwrap();
    // Startup emits idle before the queued spawn task runs. Wait for its
    // stored turn_end so the following idle cannot be that startup idle.
    until("the spawn task ends", || async {
        !td.get(&format!("/api/agents/{eph}/events?kind=turn_end"))
            .await
            .ok()
            .as_array()
            .unwrap()
            .is_empty()
    })
    .await;
    td.wait_status(&eph, "idle").await;
    let ends = td
        .get(&format!("/api/agents/{eph}/events?kind=turn_end"))
        .await
        .ok()
        .as_array()
        .unwrap()
        .len();
    let before = sessions(&td, &eph).await;
    let hold = terrariumd::lifecycle::WakeGuard::take(&s.lifecycle, agent_id).unwrap();
    terrariumd::bound::park(&s, &eph, "test stop race")
        .await
        .unwrap();
    let (entered, release) = td.sup.gate_stop(agent_id);
    td.sup.settle_stops(true);
    drop(hold);
    let stopping = tokio::spawn({
        let s = s.clone();
        async move { terrariumd::lifecycle::park_stop(&s, agent_id).await }
    });
    tokio::time::timeout(Duration::from_secs(60), entered)
        .await
        .unwrap()
        .unwrap();
    let turn = format!("[System:card {c}] unpark inside stop");
    assert_eq!(
        terrariumd::bound::unpark(&s, &eph, Some(turn.clone()), "test race")
            .await
            .unwrap(),
        terrariumd::bound::Unparked::Woken
    );
    assert_eq!(
        terrariumd::lifecycle::wake(&s, agent_id, "test race").await,
        terrariumd::lifecycle::Woke::Busy
    );
    assert_eq!(agent(&td, &eph).await["status"], "stopped");
    release.send(()).unwrap();
    stopping.await.unwrap();
    until("the resumed return turn", || async {
        td.sup.prompts(agent_id).contains(&turn)
    })
    .await;
    // A recorded prompt precedes working in the event pump. Do not mistake
    // the resume's startup idle for the idle after this return turn either.
    until("the return turn ends", || async {
        td.get(&format!("/api/agents/{eph}/events?kind=turn_end"))
            .await
            .ok()
            .as_array()
            .unwrap()
            .len()
            > ends
    })
    .await;
    td.wait_status(&eph, "idle").await;
    let after = sessions(&td, &eph).await;
    assert_eq!(after.len(), 1, "{after:?}");
    assert_eq!(after[0]["id"], before[0]["id"]);
    assert_eq!(after[0]["resumes"], 1, "{after:?}");
    assert_eq!(
        td.sup
            .prompts(agent_id)
            .iter()
            .filter(|p| **p == turn)
            .count(),
        1
    );
}

/// TER-111 (R2-2): a parked agent whose harness still runs holds its
/// owner's place until it is stopped.
#[tokio::test]
async fn a_parked_agent_holds_its_place_until_it_stops() {
    let (td, lead) = setup().await;
    let s = state(&td);
    let c = card(&td, "p", "in_progress").await;
    let eph = id(&spawn_bound(&td, &lead, "w-claude", &c).await.body);
    td.wait_task_done(&eph).await;
    // A wake on the way keeps the parked agent running.
    let hold = terrariumd::lifecycle::WakeGuard::take(&s.lifecycle, eph.parse().unwrap()).unwrap();
    mv(&td, &c, "review", None).await;
    let key = eph.clone();
    until("the park", || async {
        spawn_row(&td, &key).await["parked_at"].is_string()
    })
    .await;
    tokio::time::sleep(Duration::from_millis(200)).await;
    assert_eq!(agent(&td, &eph).await["status"], "idle");
    let plain = json!({"template": "w-claude", "task": "other"});
    td.post_as(&lead, &format!("/api/agents/{lead}/spawn"), plain.clone())
        .await
        .err(409, "conflict");
    // Stopped: the place is free.
    drop(hold);
    parked(&td, &eph).await;
    let r = td
        .post_as(&lead, &format!("/api/agents/{lead}/spawn"), plain)
        .await;
    assert_eq!(r.status, 201, "{}", r.body);
}

/// TER-685: the wake of an unpark that finds another wake on the way (one
/// that read the agent while it was parked, and refuses it) wakes again
/// when that one ends. Before, the agent stayed stopped with its return
/// turn ('never reached idle; last stopped').
#[tokio::test]
async fn an_unpark_wake_behind_a_refused_wake_wakes_again() {
    let (td, lead) = setup().await;
    let s = state(&td);
    let c = card(&td, "p", "in_progress").await;
    let eph = id(&spawn_bound(&td, &lead, "w-claude", &c).await.body);
    let agent_id: AgentId = eph.parse().unwrap();
    td.wait_task_done(&eph).await;
    mv(&td, &c, "review", None).await;
    parked(&td, &eph).await;
    // The wake on the way read the park.
    let hold = terrariumd::lifecycle::WakeGuard::take(&s.lifecycle, agent_id).unwrap();
    let turn = format!("[System:card {c}] unpark behind a wake");
    assert_eq!(
        terrariumd::bound::unpark(&s, &eph, Some(turn.clone()), "test race")
            .await
            .unwrap(),
        terrariumd::bound::Unparked::Woken
    );
    tokio::time::sleep(Duration::from_millis(300)).await;
    assert_eq!(agent(&td, &eph).await["status"], "stopped");
    // The wake on the way ends without a start.
    drop(hold);
    until("the resumed return turn", || async {
        td.sup.prompts(agent_id).contains(&turn)
    })
    .await;
    td.wait_status(&eph, "idle").await;
}

/// TER-111 (R2-3): an unpark that queues behind an older one runs the
/// queue at once when the owner has room, not at the next spawn tick.
#[tokio::test]
async fn a_queued_unpark_runs_at_once_when_the_owner_has_room() {
    let (td, lead) = setup().await;
    let s = state(&td);
    let (a, d, b) = (
        card(&td, "p", "in_progress").await,
        card(&td, "p", "in_progress").await,
        card(&td, "p", "in_progress").await,
    );
    let mut parked_ids = Vec::new();
    for c in [&a, &d] {
        let e = id(&spawn_bound(&td, &lead, "w-claude", c).await.body);
        td.wait_status(&e, "idle").await;
        mv(&td, c, "review", None).await;
        parked(&td, &e).await;
        parked_ids.push(e);
    }
    let (ea, ed) = (parked_ids[0].clone(), parked_ids[1].clone());
    let eb = id(&spawn_bound(&td, &lead, "w-claude", &b).await.body);
    td.wait_status(&eb, "idle").await;
    // A comes back while B holds the one place: queued.
    mv(&td, &a, "ready", None).await;
    let key = ea.clone();
    until("the queued unpark of A", || async {
        spawn_row(&td, &key).await["unpark_queued_at"].is_string()
    })
    .await;
    // No spawn tick from now on: only an unpark runs the queue.
    s.lifecycle.set_tuning(terrariumd::lifecycle::Tuning {
        idle_stop: IdleStop::Off,
        tick: Duration::from_secs(3600),
        compact_on_idle: false,
        compact_max_age: CompactMaxAge::Off,
        compact_wait: Duration::from_secs(5),
    });
    // The operator gives the lead room; that alone runs no queue.
    td.patch(
        &format!("/api/agents/{lead}"),
        json!({"policy": {"spawn": {"templates": ["project:*"], "max_concurrent": 3,
                                    "max_lifetime": "1s", "bound": true}}}),
    )
    .await
    .ok();
    // D comes back: it queues behind A, and the queue runs at once.
    mv(&td, &d, "ready", None).await;
    // Without the run after the queue, it waits for the next tick (1 h).
    until("the queue runs: A and D unparked", || async {
        spawn_row(&td, &ea).await["parked_at"].is_null()
            && spawn_row(&td, &ed).await["parked_at"].is_null()
    })
    .await;
    td.wait_status(&ea, "idle").await;
    td.wait_status(&ed, "idle").await;
}

/// TER-111 review round 2: a parked agent whose harness still runs holds
/// its place, so when its card comes back it unparks at once, with no
/// queue and no stop and resume.
#[tokio::test]
async fn a_parked_agent_that_still_runs_unparks_in_its_own_place() {
    let (td, lead) = setup().await;
    let s = state(&td);
    let c = card(&td, "p", "in_progress").await;
    let eph = id(&spawn_bound(&td, &lead, "w-claude", &c).await.body);
    td.wait_task_done(&eph).await;
    // A wake on the way keeps the parked agent running (max_concurrent 1).
    let hold = terrariumd::lifecycle::WakeGuard::take(&s.lifecycle, eph.parse().unwrap()).unwrap();
    mv(&td, &c, "review", None).await;
    let key = eph.clone();
    until("the park", || async {
        spawn_row(&td, &key).await["parked_at"].is_string()
    })
    .await;
    tokio::time::sleep(Duration::from_millis(200)).await;
    assert_eq!(agent(&td, &eph).await["status"], "idle");
    // The card comes back while the agent still runs.
    mv(&td, &c, "ready", Some("Review: changes.")).await;
    let key = eph.clone();
    until("the unpark", || async {
        spawn_row(&td, &key).await["parked_at"].is_null()
    })
    .await;
    let sp = spawn_row(&td, &eph).await;
    assert!(sp["unpark_queued_at"].is_null(), "{sp}");
    let ev = td
        .get(&format!(
            "/api/agents/{eph}/events?kind=agent.unpark_queued"
        ))
        .await
        .ok()
        .clone();
    assert!(ev.as_array().unwrap().is_empty(), "{ev}");
    // The hold stands for a wake on the way: the unpark's wake found it
    // busy, and the wake on the way gives the queued turn when it ends.
    drop(hold);
    terrariumd::lifecycle::wake(&s, eph.parse().unwrap(), "test").await;
    until("the return turn", || async {
        !return_turns(&td, &eph, &c).is_empty()
    })
    .await;
    td.wait_status(&eph, "idle").await;
    // Never stopped: the first session, not resumed.
    let a = agent(&td, &eph).await;
    assert_ne!(a["status_reason"], "parked", "{a}");
    let ss = sessions(&td, &eph).await;
    assert_eq!(ss.len(), 1, "{ss:?}");
    assert_eq!(ss[0]["resumes"], 0, "{ss:?}");
}

/// TER-111 review round 2: the driver runs the reconcile at its start, so
/// a move that it missed while the daemon was down is repaired.
#[tokio::test]
async fn the_driver_reconciles_at_its_start() {
    let (mut td, lead) = setup().await;
    let c = card(&td, "p", "in_progress").await;
    let eph = id(&spawn_bound(&td, &lead, "w-claude", &c).await.body);
    td.wait_status(&eph, "idle").await;
    hidden_move(&td, &c, "review").await;
    tokio::time::sleep(Duration::from_millis(300)).await;
    assert!(spawn_row(&td, &eph).await["parked_at"].is_null());
    td.boot(None, None).await;
    fast(&td);
    let key = eph.clone();
    until("the park at the start", || async {
        spawn_row(&td, &key).await["parked_at"].is_string()
    })
    .await;
    assert_eq!(spawn_row(&td, &eph).await["park_reason"], "card in review");
}

/// TER-170 review, minor 6: with `board_flow = rules` the reconcile leaves
/// the cards of the project alone (it returns 0), and a move that the hub
/// never sent still parks the agent: the rule engine reads the event log
/// from its own cursor, so the rule `agent-park` sees it.
#[tokio::test]
async fn with_the_rule_pack_the_engine_not_the_reconcile_repairs_a_missed_move() {
    let (td, lead) = setup_flow("rules").await;
    let c = card(&td, "p", "in_progress").await;
    let eph = id(&spawn_bound(&td, &lead, "w-claude", &c).await.body);
    td.wait_status(&eph, "idle").await;
    hidden_move(&td, &c, "review").await;
    assert_eq!(terrariumd::bound::reconcile(&state(&td)).await, 0);
    parked(&td, &eph).await;
    let sp = spawn_row(&td, &eph).await;
    assert_eq!(sp["park_reason"], "card in review", "{sp}");
}
