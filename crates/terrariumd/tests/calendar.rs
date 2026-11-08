//! The calendar (docs/api.md §33): the rights per role and grant, the
//! global calendar, the recurrence in the API, the watcher mail, the bus
//! events, and the start and end notices of the tick, exactly once also
//! across a daemon restart and after a downtime. The tick tests run at
//! times days ahead of the real clock, so the daemon's own timer never
//! touches these events.
#![allow(clippy::unwrap_used, clippy::expect_used)]

use crate::common;

use chrono::{DateTime, Duration, DurationRound, Utc};
use common::TestDaemon;
use serde_json::{Value, json};
use terrariumd::api::Shared;

fn state(td: &TestDaemon) -> Shared {
    td.daemon.as_ref().unwrap().state.clone()
}

fn id(v: &Value) -> String {
    v["id"].as_str().unwrap().to_owned()
}

fn stamp(t: DateTime<Utc>) -> String {
    terrariumd::time_param::stored(t)
}

/// A time two days ahead, on a whole minute.
fn base() -> DateTime<Utc> {
    (Utc::now() + Duration::days(2))
        .duration_trunc(Duration::minutes(1))
        .unwrap()
}

async fn events(td: &TestDaemon, kind: &str) -> Vec<Value> {
    td.get(&format!("/api/events?kind={kind}"))
        .await
        .ok()
        .as_array()
        .unwrap()
        .clone()
}

/// Mail from `system` to `who` whose subject has `needle`.
async fn notices(td: &TestDaemon, who: &str, needle: &str) -> Vec<Value> {
    td.mail_of(who)
        .await
        .into_iter()
        .filter(|m| {
            m["from"] == "system" && m["subject"].as_str().unwrap_or_default().contains(needle)
        })
        .collect()
}

async fn operator_notices(td: &TestDaemon, needle: &str) -> Vec<Value> {
    td.get("/api/mail?to=operator")
        .await
        .ok()
        .as_array()
        .unwrap()
        .iter()
        .filter(|m| m["subject"].as_str().unwrap_or_default().contains(needle))
        .cloned()
        .collect()
}

async fn delete_as(td: &TestDaemon, agent: &str, path: &str) -> common::Resp {
    td.req(hyper::Method::DELETE, path, None, Some(agent)).await
}

/// Project `terra` with a lead (project_admin) and two workers, project
/// `ops` with an agent, and a sysadmin in `ops`.
async fn setup(td: &TestDaemon) -> (String, String, String, String, String) {
    td.project("terra").await;
    td.project("ops").await;
    let lead = td
        .agent("terra", "lead", json!({"roles": ["project_admin"]}))
        .await;
    let worker = td.agent("terra", "worker", json!({})).await;
    let helper = td.agent("terra", "helper", json!({})).await;
    let other = td.agent("ops", "dev", json!({})).await;
    let sys = td
        .agent("ops", "root", json!({"roles": ["sysadmin"]}))
        .await;
    (id(&lead), id(&worker), id(&helper), id(&other), id(&sys))
}

fn body(title: &str, start: DateTime<Utc>) -> Value {
    json!({"title": title, "kind": "maintenance", "start": stamp(start),
           "end": stamp(start + Duration::hours(1))})
}

#[tokio::test]
async fn rights_grants_global_and_audit() {
    let td = TestDaemon::start().await;
    let (lead, worker, helper, other, sys) = setup(&td).await;
    let t0 = base();
    let path = "/api/projects/terra/calendar/events";

    // A worker reads, but does not write without the grant.
    let r = td.post_as(&worker, path, body("w", t0)).await;
    r.err(403, "forbidden");
    assert!(
        r.body["error"]["message"]
            .as_str()
            .unwrap()
            .contains("grant"),
        "{}",
        r.body
    );
    // An agent of another project neither reads nor writes.
    td.post_as(&other, path, body("o", t0))
        .await
        .err(403, "forbidden");
    td.get_as(&other, "/api/projects/terra/calendar")
        .await
        .err(403, "forbidden");

    // The lead and the operator write.
    let by_lead = td.post_as(&lead, path, body("lead's", t0)).await;
    assert_eq!(by_lead.status, 201, "{}", by_lead.body);
    let by_lead = id(&by_lead.body);
    let by_op = td.post(path, body("operator's", t0)).await;
    assert_eq!(by_op.status, 201, "{}", by_op.body);
    assert_eq!(by_op.body["created_by"], "operator");

    // A worker cannot grant; the lead grants the worker.
    td.put_as(
        &worker,
        &format!("/api/projects/terra/calendar/grants/{helper}"),
        json!({}),
    )
    .await
    .err(403, "forbidden");
    let g = td
        .put_as(
            &lead,
            "/api/projects/terra/calendar/grants/worker",
            json!({}),
        )
        .await;
    assert_eq!(g.status, 200, "{}", g.body);
    assert_eq!(g.body["calendar_write"], true);
    assert_eq!(g.body["changed"], true);
    let list = td
        .get_as(&worker, "/api/projects/terra/calendar/grants")
        .await;
    assert_eq!(list.ok().as_array().unwrap().len(), 1);
    assert_eq!(list.body[0]["name"], "worker");
    // A lead grants only in its own project.
    td.put_as(
        &lead,
        &format!("/api/projects/ops/calendar/grants/{other}"),
        json!({}),
    )
    .await
    .err(403, "forbidden");
    // The grant is a field of the policy.
    let a = td.get(&format!("/api/agents/{worker}")).await;
    assert_eq!(a.body["policy"]["calendar_write"], true, "{}", a.body);

    // With the grant: add, change, and remove the own events only.
    let own = td.post_as(&worker, path, body("worker's", t0)).await;
    assert_eq!(own.status, 201, "{}", own.body);
    let own = id(&own.body);
    let r = td
        .patch_as(
            &worker,
            &format!("/api/calendar/events/{own}"),
            json!({"title": "mine"}),
        )
        .await;
    assert_eq!(r.status, 200, "{}", r.body);
    assert_eq!(r.body["updated_by"], format!("agent:{worker}"));
    td.patch_as(
        &worker,
        &format!("/api/calendar/events/{by_lead}"),
        json!({"title": "x"}),
    )
    .await
    .err(403, "forbidden");
    delete_as(&td, &worker, &format!("/api/calendar/events/{by_lead}"))
        .await
        .err(403, "forbidden");
    td.put_as(
        &worker,
        &format!("/api/calendar/events/{by_lead}/assignees/helper"),
        json!({}),
    )
    .await
    .err(403, "forbidden");
    // The helper (no grant) cannot change the worker's event.
    td.patch_as(
        &helper,
        &format!("/api/calendar/events/{own}"),
        json!({"title": "x"}),
    )
    .await
    .err(403, "forbidden");
    // The lead changes and removes any event of the project.
    let r = td
        .patch_as(
            &lead,
            &format!("/api/calendar/events/{own}"),
            json!({"kind": "deploy"}),
        )
        .await;
    assert_eq!(r.status, 200, "{}", r.body);
    assert_eq!(
        delete_as(&td, &worker, &format!("/api/calendar/events/{own}"))
            .await
            .status,
        204
    );

    // The lead revokes; the worker reads only again.
    let g = td
        .req(
            hyper::Method::DELETE,
            "/api/projects/terra/calendar/grants/worker",
            None,
            Some(&lead),
        )
        .await;
    assert_eq!(g.body["calendar_write"], false, "{}", g.body);
    td.post_as(&worker, path, body("w2", t0))
        .await
        .err(403, "forbidden");

    // Assignees: agents of the project only.
    let r = td
        .put_as(
            &lead,
            &format!("/api/calendar/events/{by_lead}/assignees/{other}"),
            json!({}),
        )
        .await;
    r.err(422, "invalid");

    // Global events: a sysadmin or the operator writes; every project sees
    // them, marked global; a project user cannot change them.
    td.post_as(&lead, "/api/calendar/global/events", body("g", t0))
        .await
        .err(403, "forbidden");
    let g = td
        .post_as(&sys, "/api/calendar/global/events", body("host window", t0))
        .await;
    assert_eq!(g.status, 201, "{}", g.body);
    let gid = id(&g.body);
    assert_eq!(g.body["global"], true);
    assert!(g.body["project"].is_null());
    // Any agent is an assignee of a global event.
    let r = td
        .put_as(
            &sys,
            &format!("/api/calendar/events/{gid}/assignees/terra%2Fworker"),
            json!({}),
        )
        .await;
    assert_eq!(r.status, 200, "{}", r.body);
    td.patch_as(
        &lead,
        &format!("/api/calendar/events/{gid}"),
        json!({"title": "x"}),
    )
    .await
    .err(403, "forbidden");
    let from = stamp(t0 - Duration::hours(1));
    let view = td
        .get_as(
            &worker,
            &format!("/api/projects/terra/calendar?from={from}&days=2"),
        )
        .await;
    let occ = view.ok()["occurrences"].as_array().unwrap().clone();
    assert_eq!(occ.len(), 3, "{}", view.body);
    assert!(
        occ.iter()
            .any(|o| o["global"] == true && o["title"] == "host window")
    );
    assert_eq!(view.body["events"].as_array().unwrap().len(), 3);
    // A worker with no grant reads only: no write, no edit.
    assert_eq!(view.body["can_write"], false);
    assert!(
        view.body["events"]
            .as_array()
            .unwrap()
            .iter()
            .all(|e| e["can_edit"] == false)
    );
    let lv = td
        .get_as(
            &lead,
            &format!("/api/projects/terra/calendar?from={from}&days=2"),
        )
        .await;
    assert_eq!(lv.body["can_write"], true);
    for e in lv.body["events"].as_array().unwrap() {
        assert_eq!(e["can_edit"], e["global"] == false, "{e}");
    }
    let ops = td
        .get_as(
            &other,
            &format!("/api/projects/ops/calendar?from={from}&days=2"),
        )
        .await;
    assert_eq!(ops.ok()["occurrences"].as_array().unwrap().len(), 1);
    // The combined view: the operator sees all; an agent its own project.
    let all = td.get(&format!("/api/calendar?from={from}&days=2")).await;
    assert_eq!(all.ok()["occurrences"].as_array().unwrap().len(), 3);
    let mine = td
        .get_as(&other, &format!("/api/calendar?from={from}&days=2"))
        .await;
    assert_eq!(mine.ok()["occurrences"].as_array().unwrap().len(), 1);
    let only = td
        .get(&format!("/api/calendar/global?from={from}&days=2"))
        .await;
    assert_eq!(only.ok()["occurrences"].as_array().unwrap().len(), 1);

    // The audit rows.
    let audit = td.get("/api/audit?limit=200").await;
    let actions: Vec<String> = audit
        .ok()
        .as_array()
        .unwrap()
        .iter()
        .map(|a| a["action"].as_str().unwrap().to_owned())
        .collect();
    for a in [
        "calendar.create",
        "calendar.update",
        "calendar.delete",
        "calendar.grant",
        "calendar.revoke",
        "calendar.assign",
    ] {
        assert!(actions.iter().any(|x| x == a), "{a} in {actions:?}");
    }
}

#[tokio::test]
async fn recurrence_validation_skips_and_ranges() {
    let td = TestDaemon::start().await;
    setup(&td).await;
    let path = "/api/projects/terra/calendar/events";
    for (v, field) in [
        (
            json!({"title": "t", "start": "2027-03-01T09:00", "recurrence": "FREQ=YEARLY"}),
            "recurrence",
        ),
        (
            json!({"title": "t", "start": "2027-03-01T09:00",
                "recurrence": "FREQ=WEEKLY;BYDAY=1MO"}),
            "recurrence",
        ),
        (
            json!({"title": "t", "start": "2027-03-01T09:00",
                "recurrence": {"freq": "daily", "until": "2027-04-01", "count": 3}}),
            "recurrence",
        ),
        (
            json!({"title": "t", "start": "2027-03-01T09:00", "tz": "Mars/Base"}),
            "tz",
        ),
        (
            json!({"title": "t", "start": "2027-03-01T09:00", "kind": "party"}),
            "kind",
        ),
    ] {
        let r = td.post(path, v.clone()).await;
        r.err(422, "invalid");
        assert_eq!(
            r.body["error"]["details"]["field"], field,
            "{v}: {}",
            r.body
        );
    }
    // 09:00 London each Sunday, by RRULE: the UTC time moves at the DST
    // change of 2027-03-28.
    let e = td
        .post(
            path,
            json!({"title": "patch window", "kind": "window", "start": "2027-03-14T09:00",
                   "end": "2027-03-14T10:00", "tz": "Europe/London",
                   "recurrence": "RRULE:FREQ=WEEKLY;COUNT=4"}),
        )
        .await;
    assert_eq!(e.status, 201, "{}", e.body);
    assert_eq!(e.body["rrule"], "FREQ=WEEKLY;COUNT=4");
    assert_eq!(e.body["recurrence"]["freq"], "weekly");
    let eid = id(&e.body);
    let list = td
        .get("/api/projects/terra/calendar?from=2027-03-01&to=2027-05-01")
        .await;
    let starts: Vec<&str> = list.ok()["occurrences"]
        .as_array()
        .unwrap()
        .iter()
        .map(|o| o["start"].as_str().unwrap())
        .collect();
    assert_eq!(
        starts,
        [
            "2027-03-14T09:00:00.000Z",
            "2027-03-21T09:00:00.000Z",
            "2027-03-28T08:00:00.000Z",
            "2027-04-04T08:00:00.000Z"
        ]
    );
    // Skip one occurrence by its date; a date with none is refused.
    let r = td
        .post(
            &format!("/api/calendar/events/{eid}/skip"),
            json!({"occurrence": "2027-03-21"}),
        )
        .await;
    assert_eq!(r.status, 200, "{}", r.body);
    assert_eq!(r.body["exceptions"], json!(["2027-03-21T09:00:00.000Z"]));
    td.post(
        &format!("/api/calendar/events/{eid}/skip"),
        json!({"occurrence": "2027-03-22"}),
    )
    .await
    .err(422, "invalid");
    let list = td
        .get("/api/projects/terra/calendar?from=2027-03-01&to=2027-05-01")
        .await;
    assert_eq!(list.ok()["occurrences"].as_array().unwrap().len(), 3);
    let r = td
        .post(
            &format!("/api/calendar/events/{eid}/unskip"),
            json!({"occurrence": "2027-03-21T09:00:00Z"}),
        )
        .await;
    assert_eq!(r.body["exceptions"], json!([]), "{}", r.body);
    // An all-day freeze: the end is the last day.
    let f = td
        .post(
            path,
            json!({"title": "freeze", "kind": "freeze", "start": "2027-12-20",
                   "end": "2027-12-24", "tz": "Australia/Brisbane"}),
        )
        .await;
    assert_eq!(f.status, 201, "{}", f.body);
    assert_eq!(f.body["all_day"], true);
    assert_eq!(f.body["start_date"], "2027-12-20");
    assert_eq!(f.body["end_date"], "2027-12-24");
    assert_eq!(f.body["start"], "2027-12-19T14:00:00.000Z");
    // A bad window is a 400.
    td.get("/api/projects/terra/calendar?from=2027-01-01&to=2029-01-01")
        .await
        .err(400, "bad_request");
    td.get("/api/projects/terra/calendar?from=soon")
        .await
        .err(400, "bad_request");
}

#[tokio::test]
async fn watchers_get_the_changes_but_not_their_own() {
    let td = TestDaemon::start().await;
    let (lead, worker, helper, _other, sys) = setup(&td).await;
    let t0 = base();
    // The lead and the operator watch the project; the helper watches
    // the global calendar. The lead makes the worker watch.
    let r = td
        .post_empty_as(&lead, "/api/projects/terra/calendar/watches")
        .await;
    assert_eq!(r.status, 200, "{}", r.body);
    assert_eq!(r.body["watcher"], lead);
    td.post_empty("/api/projects/terra/calendar/watches")
        .await
        .ok();
    let r = td
        .post_as(
            &lead,
            "/api/projects/terra/calendar/watches",
            json!({"watcher": "worker"}),
        )
        .await;
    assert_eq!(r.body["watcher"], worker, "{}", r.body);
    // A worker cannot make another agent watch, nor watch for the operator.
    td.post_as(
        &worker,
        "/api/projects/terra/calendar/watches",
        json!({"watcher": "helper"}),
    )
    .await
    .err(403, "forbidden");
    td.post_as(
        &worker,
        "/api/projects/terra/calendar/watches",
        json!({"watcher": "operator"}),
    )
    .await
    .err(403, "forbidden");
    td.post_empty_as(&helper, "/api/calendar/global/watches")
        .await
        .ok();
    let w = td
        .get_as(&worker, "/api/projects/terra/calendar/watches")
        .await;
    assert_eq!(w.ok().as_array().unwrap().len(), 3, "{}", w.body);

    // The lead adds: the operator and the worker get mail, the lead not.
    let e = td
        .post_as(
            &lead,
            "/api/projects/terra/calendar/events",
            body("DB upgrade", t0),
        )
        .await;
    let eid = id(&e.body);
    assert_eq!(notices(&td, &worker, "added maintenance").await.len(), 1);
    assert_eq!(
        operator_notices(&td, "Calendar terra: added").await.len(),
        1
    );
    assert!(notices(&td, &lead, "Calendar terra").await.is_empty());
    assert!(notices(&td, &helper, "Calendar").await.is_empty());

    // The operator moves it: a diff for the lead and the worker.
    let later = t0 + Duration::hours(2);
    td.patch(
        &format!("/api/calendar/events/{eid}"),
        json!({"start": stamp(later), "title": "DB upgrade v2"}),
    )
    .await
    .ok();
    let m = notices(&td, &lead, "updated").await;
    assert_eq!(m.len(), 1);
    let full = td
        .get_as(
            &lead,
            &format!("/api/mail/{}", m[0]["id"].as_str().unwrap()),
        )
        .await;
    let text = full.ok()["body"].as_str().unwrap().to_owned();
    assert!(
        text.contains("- title: DB upgrade → DB upgrade v2"),
        "{text}"
    );
    assert!(text.contains("- when: "), "{text}");
    assert!(operator_notices(&td, "updated").await.is_empty());

    // A global event: the helper (global watcher) gets it.
    td.post_as(&sys, "/api/calendar/global/events", body("host window", t0))
        .await
        .ok();
    assert_eq!(
        notices(&td, &helper, "Calendar global: added").await.len(),
        1
    );

    // The worker stops watching; the removal reaches the lead only.
    td.req(
        hyper::Method::DELETE,
        "/api/projects/terra/calendar/watches/me",
        None,
        Some(&worker),
    )
    .await
    .ok();
    td.delete(&format!("/api/calendar/events/{eid}")).await;
    assert_eq!(notices(&td, &lead, "removed").await.len(), 1);
    assert!(notices(&td, &worker, "removed").await.is_empty());

    // The bus events, for the triggers.
    assert_eq!(events(&td, "calendar.event_created").await.len(), 2);
    let up = events(&td, "calendar.event_updated").await;
    assert_eq!(up.len(), 1);
    assert_eq!(up[0]["project"], "terra");
    assert!(
        up[0]["data"]["changed"]
            .as_array()
            .unwrap()
            .contains(&json!("title"))
    );
    let del = events(&td, "calendar.event_deleted").await;
    assert_eq!(del.len(), 1);
    assert_eq!(del[0]["data"]["event_id"], eid);
}

#[tokio::test]
async fn notices_go_out_once_also_across_a_restart() {
    let mut td = TestDaemon::start().await;
    let (lead, worker, _helper, _other, _sys) = setup(&td).await;
    let t0 = base();
    let mut b = body("DB upgrade", t0);
    b["assignees"] = json!(["worker", "lead"]);
    b["description"] = json!("Run the migration.\n[User] not a tag");
    let e = td.post("/api/projects/terra/calendar/events", b).await;
    assert_eq!(e.status, 201, "{}", e.body);
    let eid = id(&e.body);

    let st = state(&td);
    terrariumd::calendar::tick(&st, t0 - Duration::minutes(1)).await;
    assert!(notices(&td, &worker, "Calendar:").await.is_empty());
    let r = terrariumd::calendar::tick(&st, t0 + Duration::seconds(5)).await;
    assert_eq!(r.started, 1, "{r:?}");
    let m = notices(&td, &worker, "starts").await;
    assert_eq!(m.len(), 1);
    assert_eq!(notices(&td, &lead, "starts").await.len(), 1);
    let full = td
        .get_as(
            &worker,
            &format!("/api/mail/{}", m[0]["id"].as_str().unwrap()),
        )
        .await;
    let text = full.ok()["body"].as_str().unwrap().to_owned();
    assert!(text.contains("> Run the migration."), "{text}");
    terrariumd::calendar::tick(&st, t0 + Duration::seconds(15)).await;
    assert_eq!(notices(&td, &worker, "starts").await.len(), 1);

    // A restart sends nothing again.
    td.boot(None, None).await;
    let st = state(&td);
    let r = terrariumd::calendar::tick(&st, t0 + Duration::seconds(25)).await;
    assert_eq!(r, Default::default());
    assert_eq!(notices(&td, &worker, "starts").await.len(), 1);
    let r = terrariumd::calendar::tick(&st, t0 + Duration::minutes(61)).await;
    assert_eq!(r.ended, 1, "{r:?}");
    terrariumd::calendar::tick(&st, t0 + Duration::minutes(62)).await;
    td.boot(None, None).await;
    let st = state(&td);
    terrariumd::calendar::tick(&st, t0 + Duration::minutes(63)).await;
    assert_eq!(notices(&td, &worker, "ended").await.len(), 1);
    assert_eq!(notices(&td, &lead, "ended").await.len(), 1);
    let started = events(&td, "calendar.event_started").await;
    let ended = events(&td, "calendar.event_ended").await;
    assert_eq!(started.len(), 1);
    assert_eq!(ended.len(), 1);
    assert_eq!(started[0]["data"]["event_id"], eid);
    assert_eq!(started[0]["data"]["late"], false);
    let rows = td.get(&format!("/api/calendar/events/{eid}/notices")).await;
    let states: Vec<(String, String)> = rows
        .ok()
        .as_array()
        .unwrap()
        .iter()
        .map(|r| {
            (
                r["edge"].as_str().unwrap().into(),
                r["state"].as_str().unwrap().into(),
            )
        })
        .collect();
    assert_eq!(
        states,
        [
            ("start".into(), "sent".into()),
            ("end".into(), "sent".into())
        ]
    );
}

#[tokio::test]
async fn after_a_downtime_late_starts_and_missed_occurrences() {
    let td = TestDaemon::start().await;
    let (_lead, worker, _helper, _other, _sys) = setup(&td).await;
    let t0 = base();
    // Daily, 1 hour, from t0.
    let mut b = body("backup window", t0);
    b["recurrence"] = json!({"freq": "daily"});
    b["assignees"] = json!(["worker"]);
    let eid = id(&td.post("/api/projects/terra/calendar/events", b).await.body);
    let st = state(&td);
    terrariumd::calendar::tick(&st, t0 - Duration::minutes(1)).await;
    // The daemon is down for three days; it comes back 30 minutes into
    // the fourth occurrence.
    let back = t0 + Duration::days(3) + Duration::minutes(30);
    let r = terrariumd::calendar::tick(&st, back).await;
    assert_eq!(r.skipped, 3, "{r:?}");
    assert_eq!(r.late, 1, "{r:?}");
    let late = notices(&td, &worker, "late notice").await;
    assert_eq!(late.len(), 1);
    assert_eq!(notices(&td, &worker, "Calendar:").await.len(), 1);
    // The end notice comes once.
    terrariumd::calendar::tick(&st, back + Duration::minutes(31)).await;
    terrariumd::calendar::tick(&st, back + Duration::minutes(32)).await;
    assert_eq!(notices(&td, &worker, "ended").await.len(), 1);
    let rows = td.get(&format!("/api/calendar/events/{eid}/notices")).await;
    let skipped = rows
        .ok()
        .as_array()
        .unwrap()
        .iter()
        .filter(|r| r["state"] == "skipped")
        .count();
    assert_eq!(skipped, 6);
    let started = events(&td, "calendar.event_started").await;
    assert_eq!(started.len(), 1);
    assert_eq!(started[0]["data"]["late"], true);
}

/// Patch event `id` as the operator at the time `now`.
async fn edit(td: &TestDaemon, id: &str, patch: Value, now: DateTime<Utc>) {
    let id = id.to_owned();
    state(td)
        .db
        .write("operator", move |t| {
            use terrariumd::store::calendar as cal;
            let e = cal::get(t, &id)?;
            let input = cal::merged_input(&e, patch)?;
            cal::update(t, &id, &input, "operator", now).map(|_| ())
        })
        .await
        .unwrap();
}

#[tokio::test]
async fn an_edit_of_a_running_occurrence() {
    let td = TestDaemon::start().await;
    let (_lead, worker, _helper, _other, _sys) = setup(&td).await;
    let t0 = base();
    let mut b = body("deploy", t0);
    b["assignees"] = json!(["worker"]);
    let eid = id(&td.post("/api/projects/terra/calendar/events", b).await.body);
    let st = state(&td);
    terrariumd::calendar::tick(&st, t0 - Duration::minutes(1)).await;
    terrariumd::calendar::tick(&st, t0 + Duration::seconds(5)).await;
    assert_eq!(notices(&td, &worker, "starts").await.len(), 1);

    // A longer end keeps the open notice: no second start notice. (The
    // store call takes the fake clock; the API takes the real one.)
    edit(
        &td,
        &eid,
        json!({"end": stamp(t0 + Duration::hours(2))}),
        t0 + Duration::minutes(10),
    )
    .await;
    terrariumd::calendar::tick(&st, t0 + Duration::minutes(61)).await;
    assert!(notices(&td, &worker, "ended").await.is_empty());
    assert_eq!(notices(&td, &worker, "starts").await.len(), 1);
    terrariumd::calendar::tick(&st, t0 + Duration::minutes(121)).await;
    assert_eq!(notices(&td, &worker, "ended").await.len(), 1);

    // An edit that moves a running occurrence away ends it at once.
    let t2 = t0 + Duration::hours(3);
    let mut b = body("restart", t2);
    b["assignees"] = json!(["worker"]);
    let rid = id(&td.post("/api/projects/terra/calendar/events", b).await.body);
    terrariumd::calendar::tick(&st, t2 + Duration::seconds(1)).await;
    assert_eq!(notices(&td, &worker, "\"restart\" starts").await.len(), 1);
    edit(
        &td,
        &rid,
        json!({"start": stamp(t2 + Duration::days(1))}),
        t2 + Duration::minutes(5),
    )
    .await;
    let m = notices(&td, &worker, "\"restart\" ended").await;
    assert_eq!(m.len(), 1);
    assert!(
        m[0]["subject"].as_str().unwrap().contains("edit"),
        "{}",
        m[0]
    );
    terrariumd::calendar::tick(&st, t2 + Duration::minutes(61)).await;
    assert_eq!(notices(&td, &worker, "\"restart\" ended").await.len(), 1);

    // A delete of a running occurrence sends its end notice at once.
    let t1 = t0 + Duration::hours(5);
    let mut b = body("migration", t1);
    b["assignees"] = json!(["worker"]);
    let mid = id(&td.post("/api/projects/terra/calendar/events", b).await.body);
    terrariumd::calendar::tick(&st, t1 + Duration::seconds(1)).await;
    assert_eq!(notices(&td, &worker, "\"migration\" starts").await.len(), 1);
    let s = state(&td);
    let m2 = mid.clone();
    s.db.write("operator", move |t| {
        terrariumd::store::calendar::delete(t, &m2, "operator", t1 + Duration::minutes(10))
            .map(|_| ())
    })
    .await
    .unwrap();
    let m = notices(&td, &worker, "\"migration\" ended").await;
    assert_eq!(m.len(), 1);
    assert!(
        m[0]["subject"].as_str().unwrap().contains("removed"),
        "{}",
        m[0]
    );
}
