//! Project actions (TER-53, docs/api.md §19.8): a schedule with the spec
//! `manual`. No timer and no event runs it; a person runs it with
//! `POST /api/schedules/{id}/run`. The rights, the audit row, and the
//! events are those of any schedule.
#![allow(clippy::unwrap_used, clippy::expect_used)]

mod common;

use chrono::{Duration, Utc};
use common::TestDaemon;
use serde_json::{Value, json};

fn id(v: &Value) -> String {
    v["id"].as_str().unwrap().to_owned()
}

/// Project `terra` with a lead (project_admin) and a worker, and project
/// `ops` with its own lead.
async fn setup(td: &TestDaemon) -> (String, String, String) {
    td.project("terra").await;
    td.project("ops").await;
    let lead = td
        .agent("terra", "lead", json!({"roles": ["project_admin"]}))
        .await;
    let worker = td.agent("terra", "worker", json!({})).await;
    let ops_lead = td
        .agent("ops", "lead", json!({"roles": ["project_admin"]}))
        .await;
    (id(&lead), id(&worker), id(&ops_lead))
}

fn action(name: &str, to: &str) -> Value {
    json!({"name": name, "project": "terra", "spec": {"kind": "manual"},
           "action": {"type": "mail", "to": to, "subject": "deploy", "body": "deploy now"}})
}

fn names(v: &Value) -> Vec<String> {
    v.as_array()
        .unwrap()
        .iter()
        .map(|s| s["name"].as_str().unwrap().to_owned())
        .collect()
}

#[tokio::test]
async fn an_action_runs_only_on_demand_with_audit_and_events() {
    let td = TestDaemon::start().await;
    let (lead, worker, _) = setup(&td).await;

    // The project's lead makes an action: no next run.
    let r = td
        .post_as(&lead, "/api/schedules", action("notify", "worker"))
        .await;
    assert_eq!(r.status, 201, "{}", r.body);
    let a = r.body.clone();
    let aid = id(&a);
    assert_eq!(a["spec"], json!({"kind": "manual"}));
    assert_eq!(a["enabled"], true);
    assert!(a["next_run_at"].is_null(), "{a}");
    // A time schedule next to it, for the list filter.
    let t = td
        .post_as(
            &lead,
            "/api/schedules",
            json!({"name": "hourly", "project": "terra",
                   "spec": {"kind": "interval", "every": "1h"},
                   "action": {"type": "mail", "to": "worker", "body": "x"}}),
        )
        .await;
    assert_eq!(t.status, 201, "{}", t.body);

    // `kind=action` lists only the actions; `time` and `trigger` leave them out.
    let l = td.get("/api/schedules?project=terra&kind=action").await;
    assert_eq!(names(l.ok()), ["notify"]);
    let l = td.get("/api/schedules?project=terra&kind=time").await;
    assert_eq!(names(l.ok()), ["hourly"]);
    let l = td.get("/api/schedules?project=terra&kind=trigger").await;
    assert!(names(l.ok()).is_empty());
    td.get("/api/schedules?kind=button")
        .await
        .err(400, "bad_request");
    // A member sees the actions of its project.
    let l = td.get_as(&worker, "/api/schedules?kind=action").await;
    assert_eq!(names(l.ok()), ["notify"]);

    // The tick a day ahead runs the time schedule, never the action.
    let state = td.daemon.as_ref().unwrap().state.clone();
    let mut ran = Vec::new();
    for h in terrariumd::scheduler::tick(&state, Utc::now() + Duration::days(1)).await {
        ran.push(h.await.unwrap().unwrap().schedule);
    }
    assert!(!ran.contains(&aid), "{ran:?}");
    let runs = td.get(&format!("/api/schedules/{aid}/runs")).await;
    assert!(runs.ok().as_array().unwrap().is_empty());

    // Run it: the mail goes out, the run is `manual` with the caller.
    let run = td
        .post_empty_as(&lead, &format!("/api/schedules/{aid}/run"))
        .await;
    let run = run.ok().clone();
    assert_eq!(run["status"], "ok", "{run}");
    assert_eq!(run["trigger"], "manual");
    assert_eq!(run["actor"], format!("agent:{lead}"));
    let mid = run["output"]["message_id"].as_str().unwrap().to_owned();
    let mail = td.mail_of(&worker).await;
    assert!(mail.iter().any(|m| m["id"] == mid.as_str()));
    let a = td.get(&format!("/api/schedules/{aid}")).await;
    let a = a.ok();
    assert_eq!(a["runs_count"], 1);
    assert_eq!(a["last_run"]["status"], "ok");
    assert!(a["next_run_at"].is_null(), "{a}");

    // The audit row and the events of the run.
    let rows = td.get("/api/audit?action=schedule.run").await;
    let row = rows
        .ok()
        .as_array()
        .unwrap()
        .iter()
        .find(|r| r["detail"]["run"] == run["id"])
        .cloned()
        .unwrap_or_else(|| panic!("no audit row: {}", rows.body));
    assert_eq!(row["actor"], format!("agent:{lead}"));
    let ev = td.get("/api/events?kind=schedule&limit=1000").await;
    let changes: Vec<&str> = ev
        .ok()
        .as_array()
        .unwrap()
        .iter()
        .filter(|e| e["data"]["schedule"] == aid.as_str())
        .filter_map(|e| e["data"]["change"].as_str())
        .collect();
    for c in ["created", "run_started", "run_finished"] {
        assert!(changes.contains(&c), "{c}: {changes:?}");
    }

    // An enable keeps no next run.
    td.post_empty(&format!("/api/schedules/{aid}/disable"))
        .await
        .ok();
    let e = td.post_empty(&format!("/api/schedules/{aid}/enable")).await;
    assert!(e.ok()["next_run_at"].is_null());
}

#[tokio::test]
async fn a_disabled_action_does_not_run() {
    let td = TestDaemon::start().await;
    let (lead, worker, _) = setup(&td).await;
    let mut body = action("off", "worker");
    body["enabled"] = json!(false);
    let r = td.post_as(&lead, "/api/schedules", body).await;
    assert_eq!(r.status, 201, "{}", r.body);
    let aid = id(&r.body);
    let r = td.post_empty(&format!("/api/schedules/{aid}/run")).await;
    r.err(409, "conflict");
    assert!(
        r.body["error"]["message"]
            .as_str()
            .unwrap()
            .contains("disabled"),
        "{}",
        r.body
    );
    // A test run (`terrarium schedule test`) does not run it either: the
    // action is real, for an exec a deploy (review round 1, finding 1).
    let r = td.post_empty(&format!("/api/schedules/{aid}/test")).await;
    r.err(409, "conflict");
    let runs = td.get(&format!("/api/schedules/{aid}/runs")).await;
    assert!(runs.ok().as_array().unwrap().is_empty());
    assert!(td.mail_of(&worker).await.is_empty(), "no mail went out");
    let rows = td.get("/api/audit?action=schedule.test").await;
    assert!(rows.ok().as_array().unwrap().is_empty());
    // Enabled, the test runs.
    td.post_empty(&format!("/api/schedules/{aid}/enable"))
        .await
        .ok();
    let t = td.post_empty(&format!("/api/schedules/{aid}/test")).await;
    assert_eq!(t.ok()["status"], "ok", "{}", t.body);
    // A disabled time schedule still runs now, as before.
    let t = td
        .post(
            "/api/schedules",
            json!({"name": "t", "project": "terra", "enabled": false,
                   "spec": {"kind": "interval", "every": "1h"},
                   "action": {"type": "mail", "to": "worker", "body": "x"}}),
        )
        .await;
    let tid = id(t.ok());
    let run = td.post_empty(&format!("/api/schedules/{tid}/run")).await;
    assert_eq!(run.ok()["status"], "ok");
}

#[tokio::test]
async fn the_rights_of_an_action() {
    let td = TestDaemon::start().await;
    let (lead, worker, ops_lead) = setup(&td).await;

    // A member and another project's lead cannot make one.
    td.post_as(&worker, "/api/schedules", action("m", "worker"))
        .await
        .err(403, "forbidden");
    td.post_as(&ops_lead, "/api/schedules", action("m", "worker"))
        .await
        .err(403, "forbidden");
    // The lead's action stays inside the project.
    td.post_as(&lead, "/api/schedules", action("out", "project:ops"))
        .await
        .err(403, "forbidden");
    // A manual action takes a turn (a trigger does not).
    let turn = td
        .post_as(
            &lead,
            "/api/schedules",
            json!({"name": "nudge", "project": "terra", "spec": {"kind": "manual"},
                   "action": {"type": "turn", "agent": "worker", "text": "check the board"}}),
        )
        .await;
    assert_eq!(turn.status, 201, "{}", turn.body);

    let r = td
        .post_as(&lead, "/api/schedules", action("notify", "worker"))
        .await;
    let aid = id(r.ok());
    // A member reads it and cannot run, change, or delete it.
    td.get_as(&worker, &format!("/api/schedules/{aid}"))
        .await
        .ok();
    td.post_empty_as(&worker, &format!("/api/schedules/{aid}/run"))
        .await
        .err(403, "forbidden");
    td.patch_as(
        &worker,
        &format!("/api/schedules/{aid}"),
        json!({"enabled": false}),
    )
    .await
    .err(403, "forbidden");
    td.req(
        hyper::Method::DELETE,
        &format!("/api/schedules/{aid}"),
        None,
        Some(&worker),
    )
    .await
    .err(403, "forbidden");
    // Another project's lead neither sees nor runs it.
    td.get_as(&ops_lead, &format!("/api/schedules/{aid}"))
        .await
        .err(403, "forbidden");
    td.post_empty_as(&ops_lead, &format!("/api/schedules/{aid}/run"))
        .await
        .err(403, "forbidden");
    let l = td.get_as(&ops_lead, "/api/schedules?kind=action").await;
    assert!(names(l.ok()).is_empty());
    // No run came of the refused calls.
    let runs = td.get(&format!("/api/schedules/{aid}/runs")).await;
    assert!(runs.ok().as_array().unwrap().is_empty());
    // The operator runs it, and the lead changes and deletes it.
    let run = td.post_empty(&format!("/api/schedules/{aid}/run")).await;
    assert_eq!(run.ok()["actor"], "operator");
    td.patch_as(
        &lead,
        &format!("/api/schedules/{aid}"),
        json!({"name": "notify-2"}),
    )
    .await
    .ok();
    let del = td
        .req(
            hyper::Method::DELETE,
            &format!("/api/schedules/{aid}"),
            None,
            Some(&lead),
        )
        .await;
    assert_eq!(del.status, 204, "{}", del.body);
}

#[test]
fn a_manual_spec_has_no_time() {
    let spec = terrarium_core::schedule::ScheduleSpec::Manual {};
    assert!(
        terrariumd::store::schedules::next_after(&spec, Utc::now(), Utc::now())
            .unwrap()
            .is_none()
    );
    terrariumd::store::schedules::validate_spec(&spec, Utc::now()).unwrap();
}

/// A project-owned action runs with the project's rights and the
/// operator's paths, as a project-owned schedule does (§19.6). Its card
/// action keeps the GitHub author gate (TER-85): a `close` never moves a
/// held card, and closes a plain card with the same label.
#[tokio::test]
async fn a_project_owned_card_action_keeps_the_github_gate() {
    let td = TestDaemon::start().await;
    let (lead, worker, _) = setup(&td).await;
    td.patch(
        "/api/projects/terra",
        json!({"github_authors": ["wiltaylor"]}),
    )
    .await
    .ok();
    let cards = "/api/projects/terra/cards";
    let held = td
        .post_as(
            &worker,
            cards,
            json!({"title": "GitHub #7: held", "labels": ["github"], "github_author": "eve"}),
        )
        .await;
    let held = held.ok().clone();
    assert_eq!(held["column"], "blocked", "{held}");
    let hid = id(&held);
    td.patch_as(
        &lead,
        &format!("/api/cards/{hid}"),
        json!({"add_labels": ["trigger:k"]}),
    )
    .await
    .ok();
    let a = td
        .post_as(
            &lead,
            "/api/schedules",
            json!({"name": "close-k", "project": "terra", "owner": "project",
                   "spec": {"kind": "manual"},
                   "action": {"type": "card", "op": "close", "key": "k", "column": "ready"}}),
        )
        .await;
    assert_eq!(a.status, 201, "{}", a.body);
    assert_eq!(a.body["owner"], "project");
    let aid = id(&a.body);
    let run = td
        .post_empty_as(&lead, &format!("/api/schedules/{aid}/run"))
        .await;
    let run = run.ok().clone();
    assert_eq!(run["status"], "ok", "{run}");
    assert!(run["output"]["card"].is_null(), "{run}");
    assert_eq!(run["actor"], format!("agent:{lead}"));
    let c = td.get(&format!("/api/cards/{hid}")).await;
    assert_eq!(c.ok()["column"], "blocked", "{}", c.body);
    // A plain card with the label: the action closes it.
    let plain = td
        .post_as(
            &worker,
            cards,
            json!({"title": "Plain", "labels": ["trigger:k"]}),
        )
        .await;
    let pid = id(plain.ok());
    let run = td
        .post_empty_as(&lead, &format!("/api/schedules/{aid}/run"))
        .await;
    let run = run.ok().clone();
    assert_eq!(run["status"], "ok", "{run}");
    assert_eq!(run["output"]["card"], pid.as_str(), "{run}");
    let c = td.get(&format!("/api/cards/{pid}")).await;
    assert_eq!(c.ok()["column"], "ready");
    let c = td.get(&format!("/api/cards/{hid}")).await;
    assert_eq!(c.ok()["column"], "blocked");
}
