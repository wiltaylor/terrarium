//! Schedules (docs/api.md §19): the API, the rights per role, and the
//! tick with a fake clock. Each tick runs at a time hours ahead of the
//! real clock, so the daemon's own timer never runs these schedules.
#![allow(clippy::unwrap_used, clippy::expect_used)]

use crate::common;

use chrono::{DateTime, Duration, Utc};
use common::{TestDaemon, write};
use serde_json::{Value, json};
use terrarium_core::config::{CompactMaxAge, IdleStop};
use terrariumd::api::Shared;
use terrariumd::lifecycle::Tuning;
use terrariumd::store::schedules::Run;

fn state(td: &TestDaemon) -> Shared {
    td.daemon.as_ref().unwrap().state.clone()
}

/// Run one tick at `now` and wait for the runs it started.
async fn tick(td: &TestDaemon, now: DateTime<Utc>) -> Vec<Run> {
    let mut out = Vec::new();
    for h in terrariumd::scheduler::tick(&state(td), now).await {
        out.push(h.await.unwrap().unwrap());
    }
    out
}

fn hours(n: i64) -> DateTime<Utc> {
    Utc::now() + Duration::hours(n)
}

fn id(v: &Value) -> String {
    v["id"].as_str().unwrap().to_owned()
}

/// A project `terra` with a lead (project_admin), a worker, and a project
/// `ops` with an agent.
async fn setup(td: &TestDaemon) -> (String, String, String) {
    td.project("terra").await;
    td.project("ops").await;
    let lead = td
        .agent("terra", "lead", json!({"roles": ["project_admin"]}))
        .await;
    let worker = td.agent("terra", "worker", json!({})).await;
    let other = td.agent("ops", "dev", json!({})).await;
    (id(&lead), id(&worker), id(&other))
}

fn mail_body(project: Option<&str>, name: &str, to: &str) -> Value {
    json!({"name": name, "project": project,
           "spec": {"kind": "interval", "every": "1h"},
           "action": {"type": "mail", "to": to, "subject": "reminder", "body": "check the board"}})
}

#[tokio::test]
async fn operator_schedule_comments_are_system_authored() {
    let td = TestDaemon::start().await;
    setup(&td).await;
    let s = td
        .post(
            "/api/schedules",
            json!({
                "name": "card-note", "project": "terra",
                "spec": {"kind": "interval", "every": "1h"},
                "action": {"type": "card", "op": "open", "key": "note",
                           "title": "Scheduled card", "body": "scheduled note"}
            }),
        )
        .await
        .ok()
        .clone();
    let sid = id(&s);
    td.post_empty(&format!("/api/schedules/{sid}/run"))
        .await
        .ok();
    let runs = tick(&td, hours(2)).await;
    assert_eq!(runs.len(), 1);
    let cards = td.get("/api/projects/terra/cards").await;
    let cid = id(&cards.ok()[0]);
    let card = td.get(&format!("/api/cards/{cid}")).await;
    let comments = card.ok()["comments"].as_array().unwrap();
    assert!(!comments.is_empty());
    for comment in comments {
        assert_eq!(comment["author"], "system", "{comment:#}");
    }
}

#[tokio::test]
async fn crud_validation_run_now_events_and_audit() {
    let td = TestDaemon::start().await;
    let (_lead, worker, _) = setup(&td).await;

    // Validation names the field; a cron error shows the expression.
    let mut bad = mail_body(Some("terra"), "standup", "worker");
    bad["spec"] = json!({"kind": "cron", "expr": "*/15 * * *"});
    let r = td.post("/api/schedules", bad.clone()).await;
    r.err(422, "invalid");
    let msg = r.body["error"]["message"].as_str().unwrap();
    assert!(
        msg.starts_with("spec.expr:") && msg.contains("`*/15 * * *`"),
        "{msg}"
    );
    assert_eq!(r.body["error"]["details"]["field"], "spec.expr");
    bad["spec"] = json!({"kind": "interval", "every": "30s"});
    let r = td.post("/api/schedules", bad.clone()).await;
    r.err(422, "invalid");
    assert_eq!(r.body["error"]["details"]["field"], "spec.every");
    bad["spec"] = json!({"kind": "cron", "expr": "* * * * *", "tz": "Nowhere/City"});
    td.post("/api/schedules", bad.clone())
        .await
        .err(422, "invalid");
    bad["spec"] = json!({"kind": "weekly"});
    let r = td.post("/api/schedules", bad.clone()).await;
    r.err(422, "invalid");
    assert_eq!(r.body["error"]["details"]["field"], "spec");
    let mut bad = mail_body(Some("terra"), "standup", "nobody");
    let r = td.post("/api/schedules", bad.clone()).await;
    r.err(422, "invalid");
    assert_eq!(r.body["error"]["details"]["field"], "action.to");
    bad["action"] =
        json!({"type": "exec", "target": "nope", "cmd": ["true"], "timeout_secs": 4000});
    let r = td.post("/api/schedules", bad.clone()).await;
    assert_eq!(r.body["error"]["details"]["field"], "action.timeout_secs");
    bad["name"] = json!("Bad Name");
    let r = td.post("/api/schedules", bad).await;
    assert_eq!(r.body["error"]["details"]["field"], "name");

    // Create, a cron spec in a zone.
    let mut body = mail_body(Some("terra"), "standup", "worker");
    body["spec"] = json!({"kind": "cron", "expr": "0 9 * * MON-FRI", "tz": "Europe/London"});
    let r = td.post("/api/schedules", body.clone()).await;
    assert_eq!(r.status, 201, "{}", r.body);
    let s = r.body.clone();
    let sid = id(&s);
    assert_eq!(s["project"], "terra");
    assert_eq!(s["enabled"], true);
    assert_eq!(s["catch_up"], "skip");
    assert_eq!(s["max_concurrency"], 1);
    assert_eq!(s["created_by"], "operator");
    assert_eq!(s["run_as"], "operator");
    assert_eq!(s["source"], "api");
    assert_eq!(s["runs_count"], 0);
    assert!(s["last_run"].is_null());
    assert!(s["next_run_at"].as_str().unwrap().ends_with(":00.000Z"));
    // The name is unique in its scope.
    td.post("/api/schedules", body).await.err(409, "conflict");

    // Show by ID and by project/name; list with filters.
    assert_eq!(
        td.get(&format!("/api/schedules/{sid}")).await.ok()["name"],
        "standup"
    );
    assert_eq!(
        td.get("/api/schedules/terra%2Fstandup").await.ok()["id"],
        sid
    );
    let global = td
        .post("/api/schedules", mail_body(None, "nightly", "operator"))
        .await;
    assert_eq!(global.status, 201, "{}", global.body);
    assert!(global.body["project"].is_null());
    let all = td.get("/api/schedules").await;
    assert_eq!(all.ok().as_array().unwrap().len(), 2);
    let terra = td.get("/api/schedules?project=terra").await;
    assert_eq!(
        terra.ok().as_array().unwrap().len(),
        2,
        "global + the project's"
    );
    let ops = td.get("/api/schedules?project=ops").await;
    assert_eq!(ops.ok().as_array().unwrap().len(), 1);
    td.post_empty(&format!("/api/schedules/{sid}/disable"))
        .await
        .ok();
    let off = td.get("/api/schedules?project=terra&enabled=false").await;
    assert_eq!(off.ok().as_array().unwrap().len(), 1);
    let s = td.get(&format!("/api/schedules/{sid}")).await;
    assert!(s.ok()["next_run_at"].is_null());
    let s = td.post_empty(&format!("/api/schedules/{sid}/enable")).await;
    assert!(s.ok()["next_run_at"].is_string());

    // PATCH: a new spec moves the next run.
    let p = td
        .patch(
            &format!("/api/schedules/{sid}"),
            json!({"spec": {"kind": "interval", "every": "2h"}, "catch_up": "once"}),
        )
        .await;
    assert_eq!(p.ok()["catch_up"], "once");
    assert_eq!(p.body["spec"]["every"], "2h");
    td.patch(
        &format!("/api/schedules/{sid}"),
        json!({"spec": {"kind": "interval", "every": "10s"}}),
    )
    .await
    .err(422, "invalid");

    // Run now: the mail goes through the send path, from the run_as.
    let run = td.post_empty(&format!("/api/schedules/{sid}/run")).await;
    let run = run.ok().clone();
    assert_eq!(run["status"], "ok", "{run}");
    assert_eq!(run["trigger"], "manual");
    assert_eq!(run["schedule"], sid);
    assert!(run["finished_at"].is_string());
    let mid = run["output"]["message_id"].as_str().unwrap().to_owned();
    let mail = td.mail_of(&worker).await;
    let m = mail.iter().find(|m| m["id"] == mid.as_str()).unwrap();
    assert_eq!(m["from"], "operator");
    assert_eq!(m["subject"], "reminder");
    assert_eq!(m["body"], "check the board");
    let s = td.get(&format!("/api/schedules/{sid}")).await;
    let s = s.ok();
    assert_eq!(s["runs_count"], 1);
    assert_eq!(s["last_run"]["status"], "ok");
    assert_eq!(s["last_run"]["run_id"], run["id"]);

    // The run log, newest first, with a cursor.
    td.post_empty(&format!("/api/schedules/{sid}/run"))
        .await
        .ok();
    let runs = td.get(&format!("/api/schedules/{sid}/runs?limit=1")).await;
    let first = runs.ok()[0].clone();
    assert_ne!(first["id"], run["id"]);
    let cursor = runs.next_cursor().unwrap();
    let older = td
        .get(&format!(
            "/api/schedules/{sid}/runs?limit=1&cursor={cursor}"
        ))
        .await;
    assert_eq!(older.ok()[0]["id"], run["id"]);

    // Events and audit rows.
    let ev = td.get("/api/events?kind=schedule&limit=1000").await;
    let changes: Vec<String> = ev
        .ok()
        .as_array()
        .unwrap()
        .iter()
        .map(|e| e["data"]["change"].as_str().unwrap_or_default().to_owned())
        .collect();
    for c in [
        "created",
        "disabled",
        "enabled",
        "updated",
        "run_started",
        "run_finished",
    ] {
        assert!(changes.iter().any(|x| x == c), "{c}: {changes:?}");
    }
    let fin = ev
        .body
        .as_array()
        .unwrap()
        .iter()
        .find(|e| e["data"]["change"] == "run_finished")
        .unwrap();
    assert_eq!(fin["data"]["schedule"], sid);
    assert_eq!(fin["data"]["status"], "ok");
    assert!(fin["data"]["run"].is_string());

    let del = td.delete(&format!("/api/schedules/{sid}")).await;
    assert_eq!(del.status, 204);
    td.get(&format!("/api/schedules/{sid}"))
        .await
        .err(404, "not_found");
    for action in [
        "schedule.create",
        "schedule.update",
        "schedule.run",
        "schedule.delete",
    ] {
        let a = td.get(&format!("/api/audit?action={action}")).await;
        let rows = a.ok().as_array().unwrap();
        assert!(!rows.is_empty(), "{action}");
        assert_eq!(rows[0]["actor"], "operator", "{action}");
    }
    let ev = td.get("/api/events?kind=schedule&limit=1000").await;
    assert!(
        ev.ok()
            .as_array()
            .unwrap()
            .iter()
            .any(|e| e["data"]["change"] == "deleted")
    );
}

#[tokio::test]
async fn rights_per_role() {
    let td = TestDaemon::start().await;
    let (lead, worker, other) = setup(&td).await;
    let ops_lead = td
        .agent("ops", "lead", json!({"roles": ["project_admin"]}))
        .await;
    let ops_lead = id(&ops_lead);

    // A project_admin: inside its own project.
    for to in ["worker", "project:terra", "operator"] {
        let r = td
            .post_as(
                &lead,
                "/api/schedules",
                mail_body(Some("terra"), &format!("m-{}", to.replace(':', "-")), to),
            )
            .await;
        assert_eq!(r.status, 201, "{to}: {}", r.body);
        assert_eq!(r.body["created_by"], format!("agent:{lead}"));
        assert_eq!(r.body["run_as"], format!("agent:{lead}"));
    }
    // The project defaults to the caller's.
    let mut body = mail_body(None, "own", "worker");
    body.as_object_mut().unwrap().remove("project");
    let r = td.post_as(&lead, "/api/schedules", body).await;
    assert_eq!(r.ok()["project"], "terra");
    // Mail to another project's agent, or its inbox: 403.
    for to in ["ops/dev", "project:ops"] {
        let r = td
            .post_as(&lead, "/api/schedules", mail_body(Some("terra"), "x", to))
            .await;
        r.err(403, "forbidden");
        assert!(
            r.body["error"]["message"]
                .as_str()
                .unwrap()
                .contains("stays inside"),
            "{}",
            r.body
        );
    }
    // A turn on another project's agent, a schedule of another project, a
    // global schedule: 403.
    let turn = |p: Option<&str>, agent: &str| {
        json!({"name": "t", "project": p, "spec": {"kind": "interval", "every": "1h"},
               "action": {"type": "turn", "agent": agent, "text": "go"}})
    };
    td.post_as(&lead, "/api/schedules", turn(Some("terra"), &other))
        .await
        .err(403, "forbidden");
    td.post_as(&lead, "/api/schedules", turn(Some("ops"), "dev"))
        .await
        .err(403, "forbidden");
    td.post_as(&lead, "/api/schedules", turn(None, "worker"))
        .await
        .err(403, "forbidden");
    // A member: read only.
    td.post_as(&worker, "/api/schedules", turn(Some("terra"), "worker"))
        .await
        .err(403, "forbidden");
    let list = td.get_as(&worker, "/api/schedules").await;
    let list = list.ok().as_array().unwrap().clone();
    assert_eq!(list.len(), 4);
    assert!(list.iter().all(|s| s["project"] == "terra"));
    let sid = id(&list[0]);
    td.get_as(&worker, &format!("/api/schedules/{sid}"))
        .await
        .ok();
    td.post_empty_as(&worker, &format!("/api/schedules/{sid}/run"))
        .await
        .err(403, "forbidden");
    td.req(
        hyper::Method::DELETE,
        &format!("/api/schedules/{sid}"),
        None,
        Some(&worker),
    )
    .await
    .err(403, "forbidden");
    td.patch_as(
        &worker,
        &format!("/api/schedules/{sid}"),
        json!({"enabled": false}),
    )
    .await
    .err(403, "forbidden");
    // Another project's agents do not see it, nor its admin edit it.
    td.get_as(&other, &format!("/api/schedules/{sid}"))
        .await
        .err(403, "forbidden");
    td.get_as(&other, "/api/schedules?project=terra")
        .await
        .err(403, "forbidden");
    td.patch_as(
        &ops_lead,
        &format!("/api/schedules/{sid}"),
        json!({"enabled": false}),
    )
    .await
    .err(403, "forbidden");
    // The global list is for sysadmins and the operator.
    td.get_as(&lead, "/api/schedules?project=terra").await.ok();
    let g = td
        .post("/api/schedules", mail_body(None, "g", "operator"))
        .await;
    assert_eq!(g.status, 201);
    let mine = td.get_as(&lead, "/api/schedules?project=terra").await;
    assert!(
        mine.ok()
            .as_array()
            .unwrap()
            .iter()
            .all(|s| !s["project"].is_null())
    );
    td.get_as(&lead, &format!("/api/schedules/{}", id(&g.body)))
        .await
        .err(403, "forbidden");
    // An edit of the action by another actor moves run_as to it.
    let p = td
        .patch(
            &format!("/api/schedules/{sid}"),
            json!({"action": {"type": "mail", "to": "worker", "body": "new"}}),
        )
        .await;
    assert_eq!(p.ok()["run_as"], "operator");
    assert_eq!(p.body["created_by"], format!("agent:{lead}"));
}

#[tokio::test]
async fn the_per_project_cap() {
    let td = TestDaemon::start().await;
    setup(&td).await;
    let max = terrariumd::store::schedules::max_per_project();
    for i in 0..max {
        let r = td
            .post(
                "/api/schedules",
                mail_body(Some("terra"), &format!("s{i}"), "worker"),
            )
            .await;
        assert_eq!(r.status, 201, "{}", r.body);
    }
    let r = td
        .post(
            "/api/schedules",
            mail_body(Some("terra"), "one-more", "worker"),
        )
        .await;
    r.err(409, "conflict");
    assert!(
        r.body["error"]["message"]
            .as_str()
            .unwrap()
            .contains("TERRARIUM_MAX_SCHEDULES_PER_PROJECT")
    );
    // Another project and the global scope have room.
    td.post("/api/schedules", mail_body(Some("ops"), "s0", "dev"))
        .await
        .ok();
}

#[tokio::test]
async fn the_tick_runs_due_mail_and_the_loop_guard_holds_it() {
    let td = TestDaemon::start().await;
    let (lead, worker, _) = setup(&td).await;
    let r = td
        .post_as(
            &lead,
            "/api/schedules",
            mail_body(Some("terra"), "remind", "worker"),
        )
        .await;
    let sid = id(r.ok());
    let next = r.body["next_run_at"].as_str().unwrap().to_owned();

    // Not due yet: nothing runs.
    assert!(tick(&td, Utc::now()).await.is_empty());
    // Due: one run, sent as the lead (the run_as), with the next run on.
    let runs = tick(&td, hours(2)).await;
    assert_eq!(runs.len(), 1);
    assert_eq!(runs[0].status, "ok", "{:?}", runs[0]);
    assert_eq!(runs[0].trigger, "cron");
    assert_eq!(runs[0].actor, format!("agent:{lead}"));
    let mail = td.mail_of(&worker).await;
    assert_eq!(mail.len(), 1);
    assert_eq!(mail[0]["from"], lead.as_str());
    assert_eq!(mail[0]["state"], "queued");
    let s = td.get(&format!("/api/schedules/{sid}")).await;
    let later = s.ok()["next_run_at"].as_str().unwrap().to_owned();
    assert!(later > next, "{later} > {next}");
    // The same tick time again: the next run is past it.
    assert!(tick(&td, hours(2)).await.is_empty());

    // On a thread with six agent-to-agent messages, the loop guard holds
    // the scheduled message: it is written, with no pointer.
    let first = td
        .post_as(&lead, "/api/mail", json!({"to": "worker", "body": "hi"}))
        .await;
    let thread = first.ok()["thread_id"].as_str().unwrap().to_owned();
    for i in 0..5 {
        let (from, to) = if i % 2 == 0 {
            (&worker, "lead")
        } else {
            (&lead, "worker")
        };
        td.post_as(
            from,
            "/api/mail",
            json!({"to": to, "body": "x", "thread_id": thread}),
        )
        .await
        .ok();
    }
    let body = json!({"name": "on-thread", "project": "terra",
                      "spec": {"kind": "interval", "every": "1h"},
                      "action": {"type": "mail", "to": "worker", "body": "ping", "thread": thread}});
    let r = td.post_as(&lead, "/api/schedules", body).await;
    let tid = id(r.ok());
    let run = td
        .post_empty_as(&lead, &format!("/api/schedules/{tid}/run"))
        .await;
    let run = run.ok();
    assert_eq!(run["status"], "ok", "{run}");
    assert_eq!(run["output"]["held"], true, "{run}");
    assert!(run["summary"].as_str().unwrap().contains("loop guard"));
    let m = td
        .get(&format!(
            "/api/mail/{}",
            run["output"]["message_id"].as_str().unwrap()
        ))
        .await;
    assert_eq!(m.ok()["held"], true);
    assert_eq!(m.body["thread_paused"], true);
}

fn turn_body(name: &str, start: bool) -> Value {
    json!({"name": name, "project": "terra", "spec": {"kind": "interval", "every": "1h"},
           "action": {"type": "turn", "agent": "worker", "text": format!("turn from {name}"),
                      "start_if_stopped": start}})
}

/// The `turn_start` events with this prompt. The event pump records
/// them a moment after the supervisor emits them.
async fn turn_starts(td: &TestDaemon, text: &str) -> usize {
    let mut n = 0;
    for _ in 0..100 {
        let ev = td.get("/api/events?kind=turn_start&limit=1000").await;
        n = ev
            .ok()
            .as_array()
            .unwrap()
            .iter()
            .filter(|e| e["data"]["prompt"] == text)
            .count();
        if n > 0 {
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(20)).await;
    }
    n
}

#[tokio::test]
async fn turn_actions_follow_the_agent_state() {
    let td = TestDaemon::start().await;
    let (lead, worker, _) = setup(&td).await;
    let nudge = id(td
        .post_as(&lead, "/api/schedules", turn_body("nudge", false))
        .await
        .ok());
    let start = id(td
        .post_as(&lead, "/api/schedules", turn_body("wake", true))
        .await
        .ok());

    // Stopped, no start_if_stopped: the turn goes in as a send gives it,
    // queued, and the wake starts the agent (TER-210).
    let run = td.post_empty(&format!("/api/schedules/{nudge}/run")).await;
    let run = run.ok();
    assert_eq!(run["status"], "ok", "{run}");
    assert_eq!(run["output"]["started"], true);
    assert_eq!(run["output"]["delivery"], "queued");
    assert_eq!(
        turn_starts(&td, "[System:schedule nudge] turn from nudge").await,
        1
    );
    td.wait_status(&worker, "idle").await;
    td.post_empty(&format!("/api/agents/{worker}/stop"))
        .await
        .ok();
    td.wait_status(&worker, "stopped").await;

    // start_if_stopped: the run starts the agent, then types the turn.
    let run = td.post_empty(&format!("/api/schedules/{start}/run")).await;
    let run = run.ok();
    assert_eq!(run["status"], "ok", "{run}");
    assert_eq!(run["output"]["started"], true);
    assert_eq!(
        turn_starts(&td, "[System:schedule wake] turn from wake").await,
        1
    );
    let audit = td.get("/api/audit?action=agent.start").await;
    assert_eq!(audit.ok()[0]["actor"], format!("agent:{lead}"));

    // A running agent: the turn goes in as an operator send does.
    td.wait_status(&worker, "idle").await;
    let run = td.post_empty(&format!("/api/schedules/{nudge}/run")).await;
    assert_eq!(run.ok()["status"], "ok", "{}", run.body);
    assert_eq!(run.body["output"]["started"], false);
    assert_eq!(
        turn_starts(&td, "[System:schedule nudge] turn from nudge").await,
        2
    );
    let audit = td.get("/api/audit?action=agent.send").await;
    assert_eq!(audit.ok()[0]["actor"], format!("agent:{lead}"));
}

/// TER-210: a one-shot `at` turn schedule for an agent that its idle stop
/// stopped starts the agent and delivers the turn; it is not skipped.
#[tokio::test]
async fn an_at_turn_starts_an_idle_stopped_agent() {
    let td = TestDaemon::start().await;
    let (lead, worker, _) = setup(&td).await;
    let tuning = |idle| {
        state(&td).lifecycle.set_tuning(Tuning {
            idle_stop: idle,
            tick: std::time::Duration::from_millis(20),
            compact_on_idle: false,
            compact_max_age: CompactMaxAge::Off,
            compact_wait: std::time::Duration::from_secs(5),
        })
    };
    td.post_empty(&format!("/api/agents/{worker}/start"))
        .await
        .ok();
    td.wait_status(&worker, "idle").await;
    tuning(IdleStop::After(std::time::Duration::from_millis(200)));
    td.wait_status(&worker, "stopped").await;
    let a = td.get(&format!("/api/agents/{worker}")).await;
    assert_eq!(a.ok()["status_reason"], "idle", "{}", a.body);
    tuning(IdleStop::Off);

    let body = json!({"name": "change", "project": "terra",
                      "spec": {"kind": "at", "at": hours(1).to_rfc3339()},
                      "action": {"type": "turn", "agent": "worker", "text": "do the change"}});
    let sid = id(td.post_as(&lead, "/api/schedules", body).await.ok());
    let runs = tick(&td, hours(2)).await;
    assert_eq!(runs.len(), 1);
    assert_eq!(runs[0].status, "ok", "{:?}", runs[0]);
    assert!(runs[0].summary.starts_with("woke "), "{:?}", runs[0]);
    assert_eq!(
        turn_starts(&td, "[System:schedule change] do the change").await,
        1
    );
    let s = td.get(&format!("/api/schedules/{sid}")).await;
    assert_eq!(s.ok()["last_run"]["status"], "ok", "{}", s.body);
}

/// TER-210: a turn for a stopped agent that may not wake (paused) fails
/// with the reason, keeps the turn queued, and mails the schedule's owner.
#[tokio::test]
async fn a_turn_that_cannot_start_the_agent_mails_the_owner() {
    let td = TestDaemon::start().await;
    let (lead, worker, _) = setup(&td).await;
    td.post(&format!("/api/agents/{worker}/pause"), json!({}))
        .await
        .ok();
    let sid = id(td
        .post_as(&lead, "/api/schedules", turn_body("paused", false))
        .await
        .ok());
    let run = td.post_empty(&format!("/api/schedules/{sid}/run")).await;
    let run = run.ok();
    assert_eq!(run["status"], "failed", "{run}");
    assert!(run["error"].as_str().unwrap().contains("paused"), "{run}");
    assert_eq!(run["output"]["delivery"], "queued");
    let a = td.get(&format!("/api/agents/{worker}")).await;
    assert_eq!(a.ok()["status"], "stopped");
    assert_eq!(a.body["queued"], 1, "{}", a.body);
    let mail = td.mail_of(&lead).await;
    assert_eq!(mail.len(), 1, "{mail:?}");
    assert_eq!(mail[0]["from"], "system");
    assert!(
        mail[0]["subject"]
            .as_str()
            .unwrap()
            .contains("turn not delivered"),
        "{mail:?}"
    );
    // The unpause delivers the queued turn.
    td.post(&format!("/api/agents/{worker}/unpause"), json!({}))
        .await
        .ok();
    assert_eq!(
        turn_starts(&td, "[System:schedule paused] turn from paused").await,
        1
    );
}

#[tokio::test]
async fn agent_verbs_and_overlapping_runs() {
    let td = TestDaemon::start().await;
    let (lead, worker, _) = setup(&td).await;
    let verb = |name: &str, v: &str| {
        json!({"name": name, "project": "terra", "spec": {"kind": "cron", "expr": "0 3 * * *"},
               "action": {"type": "agent", "agent": "worker", "verb": v}})
    };
    let start = id(td
        .post_as(&lead, "/api/schedules", verb("up", "start"))
        .await
        .ok());
    let restart = id(td
        .post_as(&lead, "/api/schedules", verb("again", "restart"))
        .await
        .ok());
    let stop = id(td
        .post_as(&lead, "/api/schedules", verb("down", "stop"))
        .await
        .ok());
    let run = td.post_empty(&format!("/api/schedules/{start}/run")).await;
    assert_eq!(run.ok()["status"], "ok", "{}", run.body);
    assert_eq!(run.body["output"]["verb"], "start");
    td.wait_status(&worker, "idle").await;
    // Start again: the agent runs already, so the run fails with the reason.
    let run = td.post_empty(&format!("/api/schedules/{start}/run")).await;
    assert_eq!(run.ok()["status"], "failed");
    assert!(
        run.body["error"]
            .as_str()
            .unwrap()
            .contains("running already")
    );
    let run = td
        .post_empty(&format!("/api/schedules/{restart}/run"))
        .await;
    assert_eq!(run.ok()["status"], "ok", "{}", run.body);
    td.wait_status(&worker, "idle").await;
    let run = td.post_empty(&format!("/api/schedules/{stop}/run")).await;
    assert_eq!(run.ok()["status"], "ok", "{}", run.body);
    assert_eq!(run.body["output"]["status"], "stopped");

    // A run that is still running makes the next trigger `skipped`.
    let st = state(&td);
    let sid = stop.clone();
    st.db
        .write("test", move |t| {
            t.execute(
                "INSERT INTO schedule_runs (id, schedule, trigger, status, started_at, actor)
                 VALUES ('01JRUNNINGRUNNINGRUNNINGRU', ?1, 'cron', 'running', '2026-01-01T00:00:00.000Z', 'operator')",
                [&sid],
            )?;
            Ok(())
        })
        .await
        .unwrap();
    let runs = tick(&td, hours(30)).await;
    let mine: Vec<&Run> = runs.iter().filter(|r| r.schedule == stop).collect();
    assert_eq!(mine.len(), 1);
    assert_eq!(mine[0].status, "skipped");
    assert_eq!(mine[0].summary, "skipped: still running");
    let run = td.post_empty(&format!("/api/schedules/{stop}/run")).await;
    assert_eq!(run.ok()["summary"], "skipped: still running");
    // At a daemon start, a run left `running` fails.
    terrariumd::scheduler::startup(&st, Utc::now()).await;
    let runs = td.get(&format!("/api/schedules/{stop}/runs")).await;
    let old = runs
        .ok()
        .as_array()
        .unwrap()
        .iter()
        .find(|r| r["id"] == "01JRUNNINGRUNNINGRUNNINGRU")
        .unwrap()
        .clone();
    assert_eq!(old["status"], "failed");
    assert_eq!(old["error"], "the daemon stopped during the run");
}

#[tokio::test]
async fn rights_are_checked_again_at_each_run() {
    let td = TestDaemon::start().await;
    let (lead, _worker, _) = setup(&td).await;
    let body = mail_body(Some("terra"), "remind", "worker");
    let sid = id(td.post_as(&lead, "/api/schedules", body).await.ok());
    // The lead loses its role: the run is skipped with the reason.
    td.put(&format!("/api/agents/{lead}/roles"), json!({"roles": []}))
        .await
        .ok();
    let runs = tick(&td, hours(2)).await;
    assert_eq!(runs[0].status, "skipped");
    assert!(
        runs[0].summary.contains("project_admin"),
        "{}",
        runs[0].summary
    );
    let s = td.get(&format!("/api/schedules/{sid}")).await;
    assert_eq!(s.ok()["consecutive_skips"], 1);
    // The role comes back: the run is ok, and the count starts again.
    td.put(
        &format!("/api/agents/{lead}/roles"),
        json!({"roles": ["project_admin"]}),
    )
    .await
    .ok();
    let runs = tick(&td, hours(4)).await;
    assert_eq!(runs[0].status, "ok", "{:?}", runs[0]);
    assert_eq!(
        td.get(&format!("/api/schedules/{sid}")).await.body["consecutive_skips"],
        0
    );

    // The role goes again: three skips, then the schedule is disabled,
    // and the operator gets mail. (An `agent rm` of the creator disables
    // it at once: tests/monitors.rs,
    // `a_deleted_run_as_agent_disables_its_monitors_and_schedules`.)
    td.put(&format!("/api/agents/{lead}/roles"), json!({"roles": []}))
        .await
        .ok();
    for (i, h) in [6, 8, 10].into_iter().enumerate() {
        let runs = tick(&td, hours(h)).await;
        assert_eq!(runs.len(), 1, "tick {i}");
        assert_eq!(runs[0].status, "skipped");
        assert!(
            runs[0].summary.contains("project_admin"),
            "{}",
            runs[0].summary
        );
    }
    let s = td.get(&format!("/api/schedules/{sid}")).await;
    let s = s.ok();
    assert_eq!(s["enabled"], false);
    assert!(s["next_run_at"].is_null());
    assert!(
        s["disabled_reason"]
            .as_str()
            .unwrap()
            .contains("3 runs in a row")
    );
    assert!(tick(&td, hours(12)).await.is_empty());
    let mail = td.get("/api/mail?to=operator").await;
    let notice = mail
        .ok()
        .as_array()
        .unwrap()
        .iter()
        .find(|m| m["subject"] == "Schedule terra/remind disabled")
        .cloned()
        .unwrap();
    assert_eq!(notice["from"], "system");
    let ev = td.get("/api/events?kind=schedule&limit=1000").await;
    assert!(
        ev.ok()
            .as_array()
            .unwrap()
            .iter()
            .any(|e| e["data"]["change"] == "disabled" && e["data"]["schedule"] == sid.as_str())
    );
}

#[tokio::test]
async fn catch_up_once_or_skip_at_a_daemon_start() {
    let td = TestDaemon::start().await;
    let (_lead, worker, _) = setup(&td).await;
    let mut once = mail_body(Some("terra"), "once", "worker");
    once["catch_up"] = json!("once");
    let once = id(td.post("/api/schedules", once).await.ok());
    let skip = id(td
        .post("/api/schedules", mail_body(Some("terra"), "skip", "worker"))
        .await
        .ok());
    // The daemon was down from the first run on: at a start a day later,
    // `once` runs one time, `skip` does not run; both move on.
    let start = hours(24);
    let runs = {
        let mut out = Vec::new();
        for h in terrariumd::scheduler::startup(&state(&td), start).await {
            out.push(h.await.unwrap().unwrap());
        }
        out
    };
    assert_eq!(runs.len(), 1);
    assert_eq!(runs[0].schedule, once);
    assert_eq!(runs[0].trigger, "catch_up");
    assert_eq!(runs[0].status, "ok");
    assert_eq!(td.mail_of(&worker).await.len(), 1);
    for sid in [&once, &skip] {
        let s = td.get(&format!("/api/schedules/{sid}")).await;
        let next = s.ok()["next_run_at"].as_str().unwrap();
        let next = DateTime::parse_from_rfc3339(next).unwrap();
        assert!(next > start, "{sid}: {next}");
    }
    assert!(
        td.get(&format!("/api/schedules/{skip}/runs"))
            .await
            .ok()
            .as_array()
            .unwrap()
            .is_empty()
    );
    // A tick at the start time runs neither again.
    assert!(tick(&td, start).await.is_empty());
}

#[tokio::test]
async fn runs_keep_the_newest_500() {
    let td = TestDaemon::start().await;
    setup(&td).await;
    let a = id(td
        .post("/api/schedules", mail_body(Some("terra"), "a", "worker"))
        .await
        .ok());
    let b = id(td
        .post("/api/schedules", mail_body(Some("terra"), "b", "worker"))
        .await
        .ok());
    let st = state(&td);
    let (ka, kb) = (a.clone(), b.clone());
    let n = st
        .db
        .write("test", move |t| {
            for i in 0..520 {
                t.execute(
                    "INSERT INTO schedule_runs (id, schedule, trigger, status, started_at, actor)
                     VALUES (?1, ?2, 'cron', 'ok', '2026-01-01T00:00:00.000Z', 'operator')",
                    rusqlite::params![format!("01J{i:023}"), ka],
                )?;
            }
            t.execute(
                "INSERT INTO schedule_runs (id, schedule, trigger, status, started_at, actor)
                 VALUES ('01JB', ?1, 'cron', 'ok', '2026-01-01T00:00:00.000Z', 'operator')",
                [&kb],
            )?;
            terrariumd::store::schedules::prune_runs(t, terrariumd::store::schedules::KEEP_RUNS)
        })
        .await
        .unwrap();
    assert_eq!(n, 20);
    let count = |sid: String| {
        let st = st.clone();
        async move {
            st.db
                .read(move |c| {
                    Ok(c.query_row(
                        "SELECT count(*), min(id) FROM schedule_runs WHERE schedule = ?1",
                        [&sid],
                        |r| Ok((r.get::<_, i64>(0)?, r.get::<_, String>(1)?)),
                    )?)
                })
                .await
                .unwrap()
        }
    };
    let (n, oldest) = count(a).await;
    assert_eq!(n, 500);
    assert_eq!(oldest, format!("01J{:023}", 20), "the oldest 20 went");
    assert_eq!(count(b).await.0, 1);
}

const FLEET: &str = r#"
name = "sched"

[[projects]]
name = "fl"

[[projects.agents]]
name = "lead"
harness = "claude"

[[projects.schedules]]
name = "standup"
catch_up = "once"
spec = { kind = "cron", expr = "0 9 * * MON-FRI", tz = "Europe/London" }
action = { type = "mail", to = "lead", subject = "standup", body = "post your status" }

[[projects.schedules]]
name = "wake"
spec = { kind = "interval", every = "1h" }
action = { type = "turn", agent = "lead", text = "check the board", start_if_stopped = true }
"#;

#[tokio::test]
async fn fleet_apply_and_prune() {
    let td = TestDaemon::start().await;
    let r = td.post("/api/fleet/apply", json!({"content": FLEET})).await;
    let plan = r.ok();
    let kinds: Vec<(String, String)> = plan["entries"]
        .as_array()
        .unwrap()
        .iter()
        .map(|e| {
            (
                e["kind"].as_str().unwrap().to_owned(),
                e["action"].as_str().unwrap().to_owned(),
            )
        })
        .collect();
    assert!(
        kinds.contains(&("schedule".into(), "create".into())),
        "{kinds:?}"
    );
    let s = td.get("/api/schedules/fl%2Fstandup").await;
    let s = s.ok();
    assert_eq!(s["source"], "fleet:sched");
    assert_eq!(s["catch_up"], "once");
    assert_eq!(s["run_as"], "operator");
    // Again: unchanged.
    let r = td.post("/api/fleet/plan", json!({"content": FLEET})).await;
    let e = r.ok()["entries"]
        .as_array()
        .unwrap()
        .iter()
        .find(|e| e["key"] == "fl/standup")
        .unwrap()
        .clone();
    assert_eq!(e["action"], "unchanged", "{e}");
    // An API edit shows as drift; the next apply writes the file again.
    td.post_empty("/api/schedules/fl%2Fstandup/disable")
        .await
        .ok();
    let r = td.post("/api/fleet/plan", json!({"content": FLEET})).await;
    let e = r.ok()["entries"]
        .as_array()
        .unwrap()
        .iter()
        .find(|e| e["key"] == "fl/standup")
        .unwrap()
        .clone();
    assert_eq!(e["action"], "update", "{e}");
    assert_eq!(e["drift"], json!(["enabled"]));
    td.post("/api/fleet/apply", json!({"content": FLEET}))
        .await
        .ok();
    assert_eq!(
        td.get("/api/schedules/fl%2Fstandup").await.body["enabled"],
        true
    );
    // A schedule the file drops: `delete` in the plan, gone with --prune.
    let less = FLEET
        .split("[[projects.schedules]]\nname = \"wake\"")
        .next()
        .unwrap();
    let r = td.post("/api/fleet/apply", json!({"content": less})).await;
    let e = r.ok()["entries"]
        .as_array()
        .unwrap()
        .iter()
        .find(|e| e["key"] == "fl/wake")
        .unwrap()
        .clone();
    assert_eq!(e["action"], "delete");
    assert_eq!(e["result"], "skipped");
    td.get("/api/schedules/fl%2Fwake").await.ok();
    td.post("/api/fleet/apply", json!({"content": less, "prune": true}))
        .await
        .ok();
    td.get("/api/schedules/fl%2Fwake")
        .await
        .err(404, "not_found");
    // A bad spec in a fleet is an error of the plan, at its place.
    let bad = FLEET.replace("every = \"1h\"", "every = \"5s\"");
    let r = td.post("/api/fleet/plan", json!({"content": bad})).await;
    let errs = r.body.to_string();
    assert!(errs.contains("projects.fl.schedules.wake"), "{errs}");
    // An export of the project has the schedules.
    let x = td.get("/api/fleet/export?project=fl").await;
    assert!(
        x.ok()["content"]
            .as_str()
            .unwrap()
            .contains("[[projects.schedules]]"),
        "{}",
        x.body
    );
}

#[tokio::test]
async fn project_files_declare_schedules() {
    let td = TestDaemon::with_config(|c| {
        write(c, "projects/filed.toml", "description = \"x\"\n\n[[schedules]]\nname = \"ping\"\nspec = { kind = \"interval\", every = \"1h\" }\naction = { type = \"mail\", to = \"operator\", body = \"ping\" }\n");
    })
    .await;
    let s = td.get("/api/schedules/filed%2Fping").await;
    assert_eq!(s.ok()["source"], "file");
    td.patch("/api/schedules/filed%2Fping", json!({"enabled": false}))
        .await
        .err(409, "file_managed");
    td.delete("/api/schedules/filed%2Fping")
        .await
        .err(409, "file_managed");
    // Run-now is fine.
    let run = td.post_empty("/api/schedules/filed%2Fping/run").await;
    assert_eq!(run.ok()["status"], "ok", "{}", run.body);
    // The file drops it: a reload deletes it.
    write(&td.config, "projects/filed.toml", "description = \"x\"\n");
    td.post_empty("/api/reload").await.ok();
    td.get("/api/schedules/filed%2Fping")
        .await
        .err(404, "not_found");
}

// ---- probe actions: a daemon with the fake HTTP client of the probes ----

const ANTHROPIC: &str = "https://api.anthropic.com/api/oauth/usage";

#[derive(Default, Clone)]
struct FakeHttp {
    answers: std::sync::Arc<std::sync::Mutex<std::collections::HashMap<String, (u16, String)>>>,
}

impl terrariumd::probes::HttpGet for FakeHttp {
    fn get(
        &self,
        req: terrariumd::probes::HttpRequest,
    ) -> terrariumd::supervisor::BoxFuture<'_, Result<terrariumd::probes::HttpResponse, String>>
    {
        let a = self.answers.lock().unwrap().get(&req.url).cloned();
        Box::pin(async move {
            a.map(|(status, body)| terrariumd::probes::HttpResponse { status, body })
                .ok_or_else(|| "connection refused".into())
        })
    }
}

async fn probe_daemon(http: FakeHttp) -> TestDaemon {
    use std::sync::Arc;
    let dir = tempfile::tempdir().unwrap();
    let config = dir.path().join("config");
    write(
        &config,
        "subscriptions/anthropic-max.toml",
        "provider = \"anthropic\"\nplan = \"Max 20x\"\nharnesses = [\"claude\"]\nprobe = \"anthropic-oauth-usage\"\n",
    );
    let state_dir = dir.path().join("state");
    let socket = dir.path().join("api.sock");
    let home = dir.path().join("auth/claude");
    std::fs::create_dir_all(&home).unwrap();
    std::fs::write(
        home.join(".credentials.json"),
        r#"{"claudeAiOauth": {"accessToken": "tok", "refreshToken": "r", "expiresAt": 4102444800000}}"#,
    )
    .unwrap();
    let usage = terrariumd::usage_collector::UsageConfig {
        homes: [(terrarium_core::HarnessId::Claude, home)]
            .into_iter()
            .collect(),
        probe_interval: std::time::Duration::from_secs(3600),
        probe_tick: std::time::Duration::from_secs(3600),
        poll_interval: std::time::Duration::from_secs(3600),
        detect_on_start: false,
        http: Arc::new(http),
        ..Default::default()
    };
    let sup = Arc::new(terrariumd::supervisor::InMemorySupervisor::new(
        common::DELAY,
    ));
    let daemon = terrariumd::start_with_usage(
        terrariumd::DaemonConfig {
            socket: socket.clone(),
            listen: None,
            token: None,
            state_dir: state_dir.clone(),
            config_dir: config.clone(),
            work_dir: state_dir.with_file_name("work"),
            max_targets_per_project: terrariumd::settings::DEFAULT_MAX_TARGETS_PER_PROJECT,
            repos_dir: std::path::PathBuf::from("/repos"),
            supervisor: sup.clone(),
            insecure_agent_header: true,
            artifact_listen: None,
            web: Default::default(),
        },
        usage,
    )
    .await
    .unwrap();
    TestDaemon {
        daemon: Some(daemon),
        dir,
        socket,
        config,
        state: state_dir,
        sup,
        web: Default::default(),
        artifact_listen: None,
    }
}

#[tokio::test]
async fn probe_actions_run_the_quota_probe() {
    let http = FakeHttp::default();
    let fixture = std::fs::read_to_string(format!(
        "{}/tests/fixtures/probes/anthropic_oauth_usage.json",
        env!("CARGO_MANIFEST_DIR")
    ))
    .unwrap();
    http.answers
        .lock()
        .unwrap()
        .insert(ANTHROPIC.into(), (200, fixture));
    let td = probe_daemon(http.clone()).await;
    td.project("terra").await;
    td.project("ops").await;
    let lead = id(&td
        .agent("terra", "lead", json!({"roles": ["project_admin"]}))
        .await);
    let ops_lead = id(&td
        .agent("ops", "lead", json!({"roles": ["project_admin"]}))
        .await);
    let probe = json!({"name": "quota", "spec": {"kind": "interval", "every": "1h"},
                       "action": {"type": "probe", "subscription": "anthropic-max"}});
    // No agent of the project uses the subscription yet: 403.
    td.post_as(&lead, "/api/schedules", probe.clone())
        .await
        .err(403, "forbidden");
    td.agent("terra", "worker", json!({"subscription": "anthropic-max"}))
        .await;
    let r = td.post_as(&lead, "/api/schedules", probe.clone()).await;
    let sid = id(r.ok());
    td.post_as(&ops_lead, "/api/schedules", probe)
        .await
        .err(403, "forbidden");
    // The run reads the windows through the probe.
    let runs = tick(&td, hours(2)).await;
    assert_eq!(runs.len(), 1);
    let run = &runs[0];
    assert_eq!(run.schedule, sid);
    assert_eq!(run.status, "ok", "{run:?}");
    assert_eq!(run.output["probe_status"], "ok");
    // 5h, 7d, and the weekly limit of one model (`weekly_scoped`).
    assert_eq!(run.output["windows"].as_array().unwrap().len(), 3);
    assert!(run.summary.contains("5h"), "{}", run.summary);
    let s = td.get("/api/subscriptions/anthropic-max").await;
    assert_eq!(s.ok()["probe_status"], "ok");
    // A provider error makes the run `failed`, with the probe's error.
    http.answers
        .lock()
        .unwrap()
        .insert(ANTHROPIC.into(), (500, "{}".into()));
    let run = td.post_empty(&format!("/api/schedules/{sid}/run")).await;
    assert_eq!(run.ok()["status"], "failed", "{}", run.body);
    assert!(run.body["error"].is_string());
}

// ------------------------------------------- start-of-day (GitHub #29)

use chrono::Datelike;
use terrariumd::scheduler::{DayStartWatch, day_start_pass};
use terrariumd::store::schedules::StartCause;

/// Midnight UTC nine days ahead. It and the four days after it fall on
/// weekdays that are neither today nor tomorrow; [`allowed_days`] lists
/// only those, so the daemon's own timer never plans these runs.
fn day0() -> DateTime<Utc> {
    (Utc::now() + Duration::days(9))
        .date_naive()
        .and_hms_opt(0, 0, 0)
        .unwrap()
        .and_utc()
}

fn at(day: i64, h: u32, m: u32) -> DateTime<Utc> {
    day0() + Duration::days(day) + Duration::hours(h.into()) + Duration::minutes(m.into())
}

fn day_text(day: i64) -> String {
    (day0() + Duration::days(day)).date_naive().to_string()
}

fn allowed_days() -> Vec<String> {
    (2..7)
        .map(|k| {
            (Utc::now() + Duration::days(k))
                .weekday()
                .to_string()
                .to_lowercase()
        })
        .collect()
}

fn day_start_body(name: &str, extra: Value) -> Value {
    let mut spec = json!({"kind": "day_start", "weekdays": allowed_days()});
    for (k, v) in extra.as_object().unwrap() {
        spec[k] = v.clone();
    }
    json!({"name": name, "project": "terra", "spec": spec,
           "action": {"type": "mail", "to": "worker", "subject": "morning", "body": "start the day"}})
}

/// One start-of-day pass at `now` with a fake reachability.
async fn pass(
    td: &TestDaemon,
    now: DateTime<Utc>,
    cause: StartCause,
    watch: &mut DayStartWatch,
    up: bool,
) -> Vec<String> {
    day_start_pass(&state(td), now, cause, watch, move |_, _| up).await
}

async fn schedule(td: &TestDaemon, sid: &str) -> Value {
    td.get(&format!("/api/schedules/{sid}")).await.ok().clone()
}

#[tokio::test]
async fn day_start_runs_once_per_day_whatever_the_starts() {
    let td = TestDaemon::start().await;
    let (_lead, worker, _) = setup(&td).await;
    let sid = id(td
        .post("/api/schedules", day_start_body("morning", json!({})))
        .await
        .ok());
    let s = schedule(&td, &sid).await;
    assert!(s["next_run_at"].is_null(), "{s:#}");
    assert!(s["day_start_done"].is_null());
    let mut w = DayStartWatch::default();

    // The daemon starts: the run of the day is planned and runs.
    assert_eq!(
        pass(&td, at(0, 8, 0), StartCause::DaemonStart, &mut w, true).await,
        vec![sid.clone()]
    );
    let runs = tick(&td, at(0, 8, 0)).await;
    assert_eq!(runs.len(), 1);
    assert_eq!(runs[0].trigger, "day_start");
    assert_eq!(runs[0].status, "ok", "{:?}", runs[0]);
    let start = runs[0].day_start.as_ref().unwrap();
    assert_eq!(start.cause, "daemon_start");
    let s = schedule(&td, &sid).await;
    assert_eq!(s["day_start_done"], day_text(0));
    assert!(s["next_run_at"].is_null());
    assert_eq!(s["day_start_last"]["cause"], "daemon_start");
    let runs = td.get(&format!("/api/schedules/{sid}/runs")).await;
    assert_eq!(runs.ok()[0]["day_start"]["cause"], "daemon_start");

    // Every other start on the same day plans nothing: a resume, a host
    // reconnect, a tick, and a daemon start again.
    assert!(
        pass(&td, at(0, 10, 0), StartCause::Resume, &mut w, true)
            .await
            .is_empty()
    );
    assert!(
        pass(&td, at(0, 10, 1), StartCause::DayChange, &mut w, false)
            .await
            .is_empty()
    );
    assert!(
        pass(&td, at(0, 10, 2), StartCause::DayChange, &mut w, true)
            .await
            .is_empty()
    );
    assert!(
        terrariumd::scheduler::startup(&state(&td), at(0, 11, 0))
            .await
            .is_empty()
    );
    let mut fresh = DayStartWatch::default();
    assert!(
        pass(&td, at(0, 11, 0), StartCause::DaemonStart, &mut fresh, true)
            .await
            .is_empty()
    );
    assert!(tick(&td, at(0, 23, 59)).await.is_empty());

    // The day changes while the daemon is up: the next day runs once.
    assert_eq!(
        pass(&td, at(1, 0, 0), StartCause::DayChange, &mut w, true).await,
        vec![sid.clone()]
    );
    let runs = tick(&td, at(1, 0, 0)).await;
    assert_eq!(runs.len(), 1);
    assert_eq!(runs[0].day_start.as_ref().unwrap().cause, "day_change");
    assert!(tick(&td, at(1, 0, 1)).await.is_empty());
    assert_eq!(schedule(&td, &sid).await["day_start_done"], day_text(1));
    assert_eq!(td.mail_of(&worker).await.len(), 2);
}

#[tokio::test]
async fn day_start_on_resume_from_sleep() {
    let td = TestDaemon::start().await;
    setup(&td).await;
    let sid = id(td
        .post("/api/schedules", day_start_body("morning", json!({})))
        .await
        .ok());
    let mut w = DayStartWatch::default();
    pass(&td, at(0, 22, 0), StartCause::DaemonStart, &mut w, true).await;
    assert_eq!(tick(&td, at(0, 22, 0)).await.len(), 1);
    // The next tick is one tick on: no resume. A tick after a gap of
    // hours (the machine slept) is a resume.
    assert_eq!(
        w.tick_cause(at(0, 22, 0) + Duration::seconds(10)),
        StartCause::DayChange
    );
    let cause = w.tick_cause(at(1, 7, 30));
    assert_eq!(cause, StartCause::Resume);
    assert_eq!(
        pass(&td, at(1, 7, 30), cause, &mut w, true).await,
        vec![sid.clone()]
    );
    let runs = tick(&td, at(1, 7, 30)).await;
    assert_eq!(runs.len(), 1);
    let start = runs[0].day_start.as_ref().unwrap();
    assert_eq!(start.cause, "resume");
    assert!(start.at.starts_with(&day_text(1)), "{start:?}");
    // A daemon start later that day does not run it again.
    let mut fresh = DayStartWatch::default();
    assert!(
        pass(&td, at(1, 9, 0), StartCause::DaemonStart, &mut fresh, true)
            .await
            .is_empty()
    );
    assert!(tick(&td, at(1, 9, 0)).await.is_empty());
}

#[tokio::test]
async fn day_start_waits_for_the_host_agent_of_its_target() {
    let td = TestDaemon::start().await;
    setup(&td).await;
    let sid = id(td
        .post(
            "/api/schedules",
            day_start_body("morning", json!({"delay": "10m"})),
        )
        .await
        .ok());
    let mut w = DayStartWatch::default();
    // The daemon starts with the host agent offline: no run is planned.
    assert!(
        pass(&td, at(0, 8, 0), StartCause::DaemonStart, &mut w, false)
            .await
            .is_empty()
    );
    assert!(
        pass(&td, at(0, 8, 1), StartCause::DayChange, &mut w, false)
            .await
            .is_empty()
    );
    assert!(tick(&td, at(0, 9, 0)).await.is_empty());
    // It reconnects: the run is planned at the reconnect plus the delay.
    assert_eq!(
        pass(&td, at(0, 9, 0), StartCause::DayChange, &mut w, true).await,
        vec![sid.clone()]
    );
    let s = schedule(&td, &sid).await;
    let next = DateTime::parse_from_rfc3339(s["next_run_at"].as_str().unwrap()).unwrap();
    assert_eq!(next, at(0, 9, 10));
    assert!(tick(&td, at(0, 9, 5)).await.is_empty());
    let runs = tick(&td, at(0, 9, 10)).await;
    assert_eq!(runs.len(), 1);
    assert_eq!(runs[0].day_start.as_ref().unwrap().cause, "host_reconnect");

    // The next day: planned, then the host goes offline before the run.
    // The plan is dropped; the reconnect plans it again.
    assert_eq!(
        pass(&td, at(1, 8, 0), StartCause::DayChange, &mut w, true).await,
        vec![sid.clone()]
    );
    assert!(
        pass(&td, at(1, 8, 5), StartCause::DayChange, &mut w, false)
            .await
            .is_empty()
    );
    assert!(schedule(&td, &sid).await["next_run_at"].is_null());
    assert!(tick(&td, at(1, 8, 10)).await.is_empty());
    assert_eq!(
        pass(&td, at(1, 8, 30), StartCause::DayChange, &mut w, true).await,
        vec![sid.clone()]
    );
    let runs = tick(&td, at(1, 8, 40)).await;
    assert_eq!(runs.len(), 1);
    assert_eq!(runs[0].day_start.as_ref().unwrap().cause, "host_reconnect");
}

#[tokio::test]
async fn day_start_window_delay_and_weekdays() {
    let td = TestDaemon::start().await;
    setup(&td).await;
    let sid = id(td
        .post(
            "/api/schedules",
            day_start_body(
                "window",
                json!({"not_before": "07:00", "not_after": "09:00", "delay": "5m"}),
            ),
        )
        .await
        .ok());
    let mut w = DayStartWatch::default();
    // A start before `not_before` runs at it.
    pass(&td, at(0, 6, 0), StartCause::DaemonStart, &mut w, true).await;
    let s = schedule(&td, &sid).await;
    let next = DateTime::parse_from_rfc3339(s["next_run_at"].as_str().unwrap()).unwrap();
    assert_eq!(next, at(0, 7, 0));
    assert!(tick(&td, at(0, 6, 30)).await.is_empty());
    assert_eq!(tick(&td, at(0, 7, 0)).await.len(), 1);
    // A start whose run (start plus delay) is after `not_after`: no run.
    assert!(
        pass(&td, at(1, 8, 58), StartCause::DaemonStart, &mut w, true)
            .await
            .is_empty()
    );
    // A planned run that the machine slept past `not_after` is dropped.
    assert_eq!(
        pass(&td, at(1, 8, 50), StartCause::DaemonStart, &mut w, true).await,
        vec![sid.clone()]
    );
    assert!(
        pass(&td, at(1, 9, 30), StartCause::Resume, &mut w, true)
            .await
            .is_empty()
    );
    assert!(tick(&td, at(1, 9, 30)).await.is_empty());
    assert!(schedule(&td, &sid).await["next_run_at"].is_null());
    // Day 5 is on today's weekday, which `weekdays` leaves out.
    assert!(
        pass(&td, at(5, 8, 0), StartCause::DaemonStart, &mut w, true)
            .await
            .is_empty()
    );

    // A run planned late on one day that the machine slept through to
    // the next is dropped; the resume plans the new day's run once.
    let late = id(td
        .post(
            "/api/schedules",
            day_start_body("late", json!({"delay": "30m"})),
        )
        .await
        .ok());
    let mut w = DayStartWatch::default();
    pass(&td, at(2, 23, 50), StartCause::DaemonStart, &mut w, true).await;
    pass(&td, at(3, 7, 0), StartCause::Resume, &mut w, true).await;
    let runs: Vec<Run> = tick(&td, at(3, 7, 30))
        .await
        .into_iter()
        .filter(|r| r.schedule == late)
        .collect();
    assert_eq!(runs.len(), 1);
    assert_eq!(runs[0].day_start.as_ref().unwrap().cause, "resume");
    assert_eq!(schedule(&td, &late).await["day_start_done"], day_text(3));
}

#[tokio::test]
async fn day_start_has_no_catch_up_at_a_daemon_start() {
    let td = TestDaemon::start().await;
    setup(&td).await;
    let mut body = day_start_body("morning", json!({"delay": "10m"}));
    body["catch_up"] = json!("once");
    let sid = id(td.post("/api/schedules", body).await.ok());
    let mut w = DayStartWatch::default();
    pass(&td, at(0, 8, 0), StartCause::DaemonStart, &mut w, true).await;
    // The daemon stops before the run and starts an hour later: no
    // catch-up run; the plan is dropped and the start plans it again.
    assert!(
        terrariumd::scheduler::startup(&state(&td), at(0, 9, 0))
            .await
            .is_empty()
    );
    assert!(schedule(&td, &sid).await["next_run_at"].is_null());
    let mut w = DayStartWatch::default();
    assert_eq!(
        pass(&td, at(0, 9, 0), StartCause::DaemonStart, &mut w, true).await,
        vec![sid.clone()]
    );
    let runs = tick(&td, at(0, 9, 10)).await;
    assert_eq!(runs.len(), 1);
    assert_eq!(runs[0].trigger, "day_start");
}

#[tokio::test]
async fn day_start_spec_errors_name_the_field() {
    let td = TestDaemon::start().await;
    setup(&td).await;
    for (extra, field) in [
        (json!({"not_before": "7am"}), "spec.not_before"),
        (json!({"not_after": "25:00"}), "spec.not_after"),
        (
            json!({"not_before": "10:00", "not_after": "09:00"}),
            "spec.not_after",
        ),
        (json!({"delay": "13h"}), "spec.delay"),
        (json!({"delay": "soon"}), "spec.delay"),
        (json!({"weekdays": ["funday"]}), "spec.weekdays"),
        (json!({"tz": "Nowhere/City"}), "spec.tz"),
    ] {
        let r = td
            .post("/api/schedules", day_start_body("bad", extra.clone()))
            .await;
        r.err(422, "invalid");
        assert_eq!(r.body["error"]["details"]["field"], field, "{extra}");
    }
}
