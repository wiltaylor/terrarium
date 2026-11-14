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
