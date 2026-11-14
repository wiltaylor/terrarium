//! Services C2 (TER-1265): a queued reservation per request, the run limit,
//! the target lease of the run, and the release at the end.
#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::time::Duration;

use chrono::Utc;
use serde_json::{Value, json};
use terrariumd::api::Shared;
use terrariumd::service_sched;
use terrariumd::store::reservations::{self as res_store, State as ResState};
use terrariumd::store::services::{self as store, RequestRow};

use crate::common::TestDaemon;

fn state_of(td: &TestDaemon) -> Shared {
    td.daemon.as_ref().unwrap().state.clone()
}

async fn request(state: &Shared, id: &str) -> RequestRow {
    let id = id.to_owned();
    state
        .db
        .read(move |c| store::get_request(c, &id))
        .await
        .unwrap()
}

async fn reservation(state: &Shared, id: &str) -> res_store::Reservation {
    let id = id.to_owned();
    state
        .db
        .read(move |c| res_store::get(c, &id))
        .await
        .unwrap()
}

async fn call(td: &TestDaemon, service: &str) -> String {
    let r = td
        .post_as(
            "p/lead",
            &format!("/api/services/{service}/requests"),
            json!({"request": {"n": 1}}),
        )
        .await;
    assert_eq!(r.status, 201, "{}", r.body);
    r.body["id"].as_str().unwrap().to_owned()
}

async fn define(td: &TestDaemon, body: Value) {
    let r = td.post("/api/services", body.clone()).await;
    assert_eq!(r.status, 201, "{body}: {}", r.body);
}

async fn tick(state: &Shared, at: chrono::DateTime<Utc>) -> service_sched::Tick {
    service_sched::tick(state, at).await.unwrap()
}

/// A GPU claim: the request gets one queued reservation held by the target
/// for the max run time and runs when it is active; a second request of
/// the same target waits in order (one reservation per holder and host);
/// a cancel gives the reservation back, and the next request then gets
/// its own; a run past the max run time ends as `timed_out` and gives its
/// reservation back.
#[tokio::test]
async fn a_claim_is_reserved_for_the_run_and_given_back() {
    let (td, server, client) = crate::reservations_api::setup().await;
    let state = state_of(&td);
    td.patch("/api/targets/p-box", json!({"ephemeral": true}))
        .await
        .ok();
    define(
        &td,
        json!({"name": "gen", "kind": "script", "target": "p-box", "project": "p",
               "command": ["/opt/gen"], "max_run_time": "2m", "concurrency": 2,
               "resources": {"gpus": 1, "gpu_exclusive": true}}),
    )
    .await;
    let a = call(&td, "gen").await;
    let b = call(&td, "gen").await;

    let t = tick(&state, Utc::now()).await;
    let ra = request(&state, &a).await;
    assert_eq!(ra.state.as_str(), "running", "{t:?} {ra:?}");
    let rid = ra.reservation_id.clone().unwrap();
    let res = reservation(&state, &rid).await;
    assert_eq!(res.state, ResState::Active);
    assert_eq!(
        res.holder,
        res_store::Holder::Target("p-box".into()),
        "N2: the target holds it"
    );
    assert_eq!(res.gpus.len(), 1);
    assert!(res.gpus[0].exclusive);
    // The reservation lasts the max run time, and the run ends with it.
    assert!(
        (res.duration_ms - 120_000).abs() < 5_000,
        "{}",
        res.duration_ms
    );
    assert_eq!(
        ra.run_until.as_deref(),
        Some(
            chrono::DateTime::<Utc>::from_timestamp_millis(res.until.unwrap())
                .unwrap()
                .to_rfc3339_opts(chrono::SecondsFormat::Millis, true)
                .as_str()
        )
    );
    // A2: the target holds one reservation on the host, so `b` waits.
    let rb = request(&state, &b).await;
    assert_eq!(rb.state.as_str(), "queued", "{rb:?}");
    assert!(rb.reservation_id.is_none());

    // A cancel gives the reservation back.
    let r = td
        .post_as(
            "p/lead",
            &format!("/api/service-requests/{a}/cancel"),
            json!({}),
        )
        .await;
    assert_eq!(r.status, 200, "{}", r.body);
    tick(&state, Utc::now()).await;
    let ra = request(&state, &a).await;
    assert!(ra.released_at.is_some(), "{ra:?}");
    let res = reservation(&state, &rid).await;
    assert_eq!(res.state, ResState::Retracted, "{res:?}");

    // `b` was refused with 409 and is tried again after the retry time.
    let later = Utc::now() + chrono::Duration::seconds(61);
    tick(&state, later).await;
    let rb = request(&state, &b).await;
    assert_eq!(rb.state.as_str(), "running", "{rb:?}");
    let rid_b = rb.reservation_id.clone().unwrap();
    assert_ne!(rid_b, rid);

    // Past the max run time: timed out, and the reservation is given back.
    let t = tick(&state, Utc::now() + chrono::Duration::minutes(5)).await;
    let rb = request(&state, &b).await;
    assert_eq!(rb.state.as_str(), "timed_out", "{t:?} {rb:?}");
    assert!(rb.error.unwrap().contains("max run time"));
    assert!(rb.released_at.is_some());
    let res = reservation(&state, &rid_b).await;
    assert_eq!(res.state, ResState::Retracted, "{res:?}");
    client.abort();
    server.abort();
}

/// A claim that does not fit waits in the reservation queue (`waiting`);
/// the wait limit of the C1 sweep ends it, and the scheduler cancels its
/// queued reservation (C1 note N1).
#[tokio::test]
async fn a_request_that_waits_too_long_gives_its_queued_reservation_back() {
    let (td, server, client) = crate::reservations_api::setup().await;
    let state = state_of(&td);
    // Another target holds both GPUs.
    td.patch("/api/targets/p-box2", json!({"ephemeral": true}))
        .await
        .ok();
    let held = td
        .post(
            "/api/hosts/h/reservations",
            json!({"target": "p-box2", "until": "30m",
                   "gpus": [{"uuid": "GPU-abcd", "exclusive": true},
                            {"uuid": "GPU-1234", "exclusive": true}]}),
        )
        .await;
    assert_eq!(held.ok()["state"], "active", "{}", held.body);
    td.patch("/api/targets/p-box", json!({"ephemeral": true}))
        .await
        .ok();
    define(
        &td,
        json!({"name": "gen", "kind": "script", "target": "p-box", "project": "p",
               "command": ["/opt/gen"], "wait_limit": "5m",
               "resources": {"gpus": 1, "gpu_exclusive": true}}),
    )
    .await;
    let a = call(&td, "gen").await;
    tick(&state, Utc::now()).await;
    let ra = request(&state, &a).await;
    assert_eq!(ra.state.as_str(), "waiting", "{ra:?}");
    let rid = ra.reservation_id.clone().unwrap();
    let res = reservation(&state, &rid).await;
    assert_eq!(res.state, ResState::Queued);
    // The queue timeout of the reservation is the time left to the wait limit.
    let wait_until = chrono::DateTime::parse_from_rfc3339(&ra.wait_until)
        .unwrap()
        .timestamp_millis();
    assert!(
        (res.queue_until.unwrap() - wait_until).abs() < 5_000,
        "{res:?}"
    );

    terrariumd::service_sweep::sweep(&state, Utc::now() + chrono::Duration::minutes(6))
        .await
        .unwrap();
    assert_eq!(request(&state, &a).await.state.as_str(), "timed_out");
    tick(&state, Utc::now()).await;
    assert!(request(&state, &a).await.released_at.is_some());
    let res = reservation(&state, &rid).await;
    assert_eq!(res.state, ResState::Cancelled, "{res:?}");
    client.abort();
    server.abort();
}

/// No claim: the request runs at once, with `run_until`. The lease of the
/// run goes to the agent that runs it until `run_until`, and the end of the
/// request revokes it. An open request of a service that is gone fails.
#[tokio::test]
async fn a_run_without_a_claim_gets_its_lease_and_gives_it_back() {
    let td = TestDaemon::start().await;
    let state = state_of(&td);
    td.project("p").await;
    td.agent("p", "lead", json!({"roles": ["project_admin"]}))
        .await;
    td.agent("p", "runner", json!({})).await;
    let r = td
        .post(
            "/api/targets",
            json!({"name": "p-ssh", "kind": "ssh", "project": "p", "address": "u@h"}),
        )
        .await;
    assert_eq!(r.status, 201, "{}", r.body);
    define(
        &td,
        json!({"name": "echo", "kind": "script", "target": "p-ssh", "project": "p",
               "command": ["/opt/echo"], "max_run_time": "10m"}),
    )
    .await;
    let a = call(&td, "echo").await;
    tick(&state, Utc::now()).await;
    let ra = request(&state, &a).await;
    assert_eq!(ra.state.as_str(), "running", "{ra:?}");
    assert!(ra.reservation_id.is_none());
    assert!(ra.run_until.is_some());

    let lease = service_sched::lease_run(&state, &a, "p/runner")
        .await
        .unwrap();
    assert_eq!(lease.lease.target, "p-ssh");
    assert_eq!(lease.lease.expires_at, ra.run_until);
    let again = service_sched::lease_run(&state, &a, "p/runner").await;
    assert_eq!(again.unwrap_err().code, terrariumd::error::Code::Conflict);

    // The run ends (C3 or C4): the release revokes the lease.
    let id = a.clone();
    state
        .db
        .write("system", move |t| {
            store::transition(
                t,
                &id,
                terrarium_core::service::State::Running,
                terrarium_core::service::State::Done,
                Default::default(),
            )
        })
        .await
        .unwrap();
    tick(&state, Utc::now()).await;
    assert!(request(&state, &a).await.released_at.is_some());
    let lid = lease.lease.id.clone();
    let l = state
        .db
        .read(move |c| terrariumd::store::leases::get(c, &lid))
        .await
        .unwrap();
    assert_eq!(l.state, "revoked", "{l:?}");

    // N3: the service went while a request was open.
    let b = call(&td, "echo").await;
    state
        .db
        .write("system", |t| {
            t.execute("DELETE FROM services WHERE name = 'echo'", [])?;
            Ok(())
        })
        .await
        .unwrap();
    tick(&state, Utc::now()).await;
    let rb = request(&state, &b).await;
    assert_eq!(rb.state.as_str(), "failed", "{rb:?}");
    assert!(rb.error.unwrap().contains("removed"));
    // The scheduler is idle with nothing open.
    tokio::time::sleep(Duration::from_millis(10)).await;
    assert_eq!(
        tick(&state, Utc::now()).await,
        service_sched::Tick::default()
    );
}

/// rev-1264b minor 1: a resource claim on a target that `reserve` refuses
/// (not host or docker) is refused when the service is defined.
#[tokio::test]
async fn a_claim_on_a_target_that_cannot_reserve_is_refused() {
    let td = TestDaemon::start().await;
    td.project("p").await;
    for t in [
        json!({"name": "p-ssh", "kind": "ssh", "project": "p", "address": "u@h"}),
        json!({"name": "p-gpu", "kind": "docker", "project": "p", "host": "helios",
               "image": "alpine", "ephemeral": true}),
    ] {
        let r = td.post("/api/targets", t.clone()).await;
        assert_eq!(r.status, 201, "{t}: {}", r.body);
    }
    for resources in [
        json!({"cpu_millis": 1000}),
        json!({"memory": "1GiB"}),
        json!({"gpus": 1, "gpu_vram": "4GiB"}),
    ] {
        let r = td
            .post(
                "/api/services",
                json!({"name": "s", "kind": "script", "target": "p-ssh", "project": "p",
                       "command": ["/opt/x"], "resources": resources}),
            )
            .await;
        assert_eq!(r.status, 422, "{resources}: {}", r.body);
        assert!(
            r.body.to_string().contains("needs a host or docker target"),
            "{}",
            r.body
        );
    }
    // No claim on the same target is fine, and so is a claim on docker.
    define(
        &td,
        json!({"name": "s", "kind": "script", "target": "p-ssh", "project": "p",
               "command": ["/opt/x"]}),
    )
    .await;
    define(
        &td,
        json!({"name": "g", "kind": "script", "target": "p-gpu", "project": "p",
               "command": ["/opt/x"], "resources": {"cpu_millis": 1000}}),
    )
    .await;
}

async fn reservation_rows(state: &Shared, request: &str) -> i64 {
    let like = format!("% request {request}");
    state
        .db
        .read(move |c| {
            Ok(c.query_row(
                "SELECT COUNT(*) FROM reservations WHERE json_extract(data, '$.reason') LIKE ?1",
                [like],
                |r| r.get(0),
            )?)
        })
        .await
        .unwrap()
}

/// rev-1265 major 2: a refusal that waiting does not change fails the
/// request at once with the reason and writes no reservation row; a busy
/// host (here: the target runs) waits without a row and is tried again.
#[tokio::test]
async fn a_final_refusal_fails_at_once_and_a_busy_one_waits_without_a_row() {
    let (td, server, client) = crate::reservations_api::setup().await;
    let state = state_of(&td);
    td.patch("/api/targets/p-box", json!({"ephemeral": true}))
        .await
        .ok();
    // The host has 4 CPUs at 50%: 2 cores reservable. And 2 GPUs.
    for (name, resources, says) in [
        (
            "big",
            json!({"cpu_millis": 4000}),
            "larger than the reservable capacity",
        ),
        (
            "many",
            json!({"gpus": 3, "gpu_exclusive": true}),
            "GPU(s), and the service claims 3",
        ),
    ] {
        define(
            &td,
            json!({"name": name, "kind": "script", "target": "p-box", "project": "p",
                   "command": ["/opt/x"], "resources": resources}),
        )
        .await;
        let id = call(&td, name).await;
        tick(&state, Utc::now()).await;
        let r = request(&state, &id).await;
        assert_eq!(r.state.as_str(), "failed", "{name}: {r:?}");
        assert!(r.error.as_deref().unwrap().contains(says), "{name}: {r:?}");
        assert_eq!(reservation_rows(&state, &id).await, 0, "{name}");
    }

    define(
        &td,
        json!({"name": "gen", "kind": "script", "target": "p-box", "project": "p",
               "command": ["/opt/gen"], "resources": {"gpus": 1, "gpu_exclusive": true}}),
    )
    .await;
    let set_state = |s: &'static str| {
        let state = state.clone();
        async move {
            state
                .db
                .write("system", move |t| {
                    t.execute("UPDATE targets SET state = ?1 WHERE name = 'p-box'", [s])?;
                    Ok(())
                })
                .await
                .unwrap();
        }
    };
    set_state("running").await;
    let a = call(&td, "gen").await;
    for _ in 0..3 {
        tick(&state, Utc::now()).await;
    }
    let ra = request(&state, &a).await;
    assert_eq!(ra.state.as_str(), "queued", "{ra:?}");
    assert_eq!(
        reservation_rows(&state, &a).await,
        0,
        "a busy wait makes no row"
    );
    set_state("stopped").await;
    tick(&state, Utc::now() + chrono::Duration::seconds(61)).await;
    let ra = request(&state, &a).await;
    assert_eq!(ra.state.as_str(), "running", "{ra:?}");
    assert_eq!(reservation_rows(&state, &a).await, 1);
    client.abort();
    server.abort();
}

/// rev-1265 major 2: a refusal of the reservation's own preflight (after
/// its row is written) fails the request: one row per request at most.
/// Here an exclusive GPU waits for a session on another container target.
#[tokio::test]
async fn a_preflight_refusal_fails_the_request_with_one_row() {
    let (td, server, client) = crate::reservations_api::setup().await;
    let state = state_of(&td);
    td.patch("/api/targets/p-box", json!({"ephemeral": true}))
        .await
        .ok();
    define(
        &td,
        json!({"name": "gen", "kind": "script", "target": "p-box", "project": "p",
               "command": ["/opt/gen"], "resources": {"gpus": 1, "gpu_exclusive": true}}),
    )
    .await;
    // A live session on p-box2, which an exclusive GPU cannot cover.
    let mut other = state
        .db
        .read(|c| Ok(terrariumd::store::targets::get(c, "p-box2")?.def))
        .await
        .unwrap();
    other.kind = terrarium_core::config::TargetKind::Host;
    let cmd = terrariumd::executor::RemoteCmd {
        argv: vec!["sh".into(), "-c".into(), "sleep 30".into()],
        ..Default::default()
    };
    let running = terrariumd::executor::start_cmd(&state, &other, &cmd).unwrap();
    let collected = tokio::spawn(running.collect(1024));
    let a = call(&td, "gen").await;
    tick(&state, Utc::now()).await;
    tick(&state, Utc::now() + chrono::Duration::seconds(61)).await;
    let ra = request(&state, &a).await;
    assert_eq!(ra.state.as_str(), "failed", "{ra:?}");
    assert!(
        ra.error
            .as_deref()
            .unwrap()
            .contains("reservation was refused"),
        "{ra:?}"
    );
    assert_eq!(reservation_rows(&state, &a).await, 1);
    state
        .executor
        .end_resource_sessions(&["p-box2".to_owned()], None)
        .await;
    let _ = tokio::time::timeout(Duration::from_secs(5), collected).await;
    client.abort();
    server.abort();
}

/// rev-1265 minor 3: a reservation of the scheduler that no request holds
/// (the daemon stopped between the grant and its write) is given back.
#[tokio::test]
async fn an_orphan_reservation_is_given_back() {
    let (td, server, client) = crate::reservations_api::setup().await;
    let state = state_of(&td);
    let r = td
        .post(
            "/api/hosts/h/reservations",
            json!({"target": "p-box", "until": "30m", "amounts": {"cpu_millis": 1000},
                   "reason": "service `gen` request 01ORPHAN"}),
        )
        .await;
    let rid = r.ok()["id"].as_str().unwrap().to_owned();
    // A reservation of someone else is left alone.
    let mine = td
        .post(
            "/api/hosts/h/reservations",
            json!({"target": "p-box2", "until": "30m", "amounts": {"cpu_millis": 500},
                   "reason": "a lead's reservation"}),
        )
        .await;
    let mine = mine.ok()["id"].as_str().unwrap().to_owned();
    tick(&state, Utc::now()).await;
    assert_eq!(reservation(&state, &rid).await.state, ResState::Retracted);
    assert_eq!(reservation(&state, &mine).await.state, ResState::Active);
    client.abort();
    server.abort();
}

/// rev-1265 major 1 and minor 5: a claim on a docker target without an
/// image, and on a target without a host, is refused at define time.
#[tokio::test]
async fn a_claim_needs_an_image_target_with_a_host() {
    let td = TestDaemon::start().await;
    td.project("p").await;
    for t in [
        json!({"name": "p-cont", "kind": "docker", "project": "p", "host": "helios",
               "container": "c1"}),
        json!({"name": "p-nohost", "kind": "docker", "project": "p", "host": "helios",
               "image": "alpine", "ephemeral": true}),
    ] {
        let r = td.post("/api/targets", t.clone()).await;
        assert_eq!(r.status, 201, "{t}: {}", r.body);
    }
    // A host-less row cannot come from the API (a docker target needs a
    // host); an older row or a file might have one.
    let state = state_of(&td);
    state
        .db
        .write("system", |t| {
            t.execute(
                "UPDATE targets SET def = json_remove(def, '$.host') WHERE name = 'p-nohost'",
                [],
            )?;
            Ok(())
        })
        .await
        .unwrap();
    for (target, resources, says) in [
        (
            "p-cont",
            json!({"cpu_millis": 1000}),
            "managed image target",
        ),
        ("p-cont", json!({"memory": "1GiB"}), "managed image target"),
        ("p-nohost", json!({"cpu_millis": 1000}), "has no host"),
    ] {
        let r = td
            .post(
                "/api/services",
                json!({"name": "s", "kind": "script", "target": target, "project": "p",
                       "command": ["/opt/x"], "resources": resources}),
            )
            .await;
        assert_eq!(r.status, 422, "{target} {resources}: {}", r.body);
        assert!(r.body.to_string().contains(says), "{}", r.body);
    }
    // No claim on the image-less target is fine.
    define(
        &td,
        json!({"name": "s", "kind": "script", "target": "p-cont", "project": "p",
               "command": ["/opt/x"]}),
    )
    .await;
}
