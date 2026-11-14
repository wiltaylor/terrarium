//! Services C5 (TER-1268, design D6): the one result mail at the end, the
//! result fetch and its rights, and the retention from the request end.
#![allow(clippy::unwrap_used, clippy::expect_used)]

use crate::common;

use common::TestDaemon;
use serde_json::{Value, json};
use terrarium_core::service::State;
use terrariumd::store::services::{self as store, Outcome};

/// Project `p` with a lead and two members, a target, and service `gen`.
async fn setup() -> TestDaemon {
    let td = TestDaemon::start().await;
    td.project("p").await;
    td.agent("p", "lead", json!({"roles": ["project_admin"]}))
        .await;
    td.agent("p", "w1", json!({})).await;
    td.agent("p", "w2", json!({})).await;
    td.post(
        "/api/targets",
        json!({"name": "p-box", "kind": "ssh", "project": "p", "address": "u@h"}),
    )
    .await
    .ok();
    td.post(
        "/api/services",
        json!({"name": "gen", "kind": "script", "target": "p-box",
               "command": ["/opt/gen"], "project": "p"}),
    )
    .await
    .ok();
    td
}

fn shared(td: &TestDaemon) -> terrariumd::api::Shared {
    td.daemon.as_ref().unwrap().state.clone()
}

async fn call(td: &TestDaemon, who: &str) -> String {
    let r = td
        .post_as(who, "/api/services/gen/requests", json!({"request": {"n": 1}}))
        .await;
    r.ok()["id"].as_str().unwrap().to_owned()
}

/// Run request `id` to its end `to`, as C2-C4 will.
async fn end(td: &TestDaemon, id: &str, to: State, out: Outcome) {
    let state = shared(td);
    for (from, next) in [
        (State::Queued, State::Waiting),
        (State::Waiting, State::Running),
    ] {
        let id = id.to_owned();
        state
            .db
            .write("system", move |t| {
                store::transition(t, &id, from, next, Outcome::default())
            })
            .await
            .unwrap();
    }
    let id = id.to_owned();
    state
        .db
        .write("system", move |t| {
            store::transition(t, &id, State::Running, to, out)
        })
        .await
        .unwrap();
}

/// The result mails of `who` about request `id`.
async fn mails(td: &TestDaemon, who: &str, id: &str) -> Vec<Value> {
    td.mail_of(who)
        .await
        .into_iter()
        .filter(|m| m["subject"].as_str().unwrap_or("").contains(id))
        .collect()
}

async fn body_of(td: &TestDaemon, who: &str, m: &Value) -> String {
    if let Some(b) = m["body"].as_str() {
        return b.to_owned();
    }
    let id = m["id"].as_str().unwrap();
    let r = td.get_as(who, &format!("/api/mail/{id}")).await;
    r.ok()["body"].as_str().unwrap_or("").to_owned()
}

#[tokio::test]
async fn a_done_request_mails_once_and_its_result_is_fetched() {
    let td = setup().await;
    let id = call(&td, "p/w1").await;
    // Before the end there is no result.
    let r = td
        .get_as("p/w1", &format!("/api/service-requests/{id}/result"))
        .await;
    r.err(409, "conflict");
    // The run writes its files (C3, C4), then ends the request.
    let dir = store::response_dir(&td.state, &id);
    std::fs::create_dir_all(dir.join("sub")).unwrap();
    std::fs::write(dir.join("out.txt"), b"abc").unwrap();
    std::fs::write(dir.join("sub/more.txt"), b"hello").unwrap();
    std::fs::write(td.state.join("outside"), b"secret").unwrap();
    std::os::unix::fs::symlink(td.state.join("outside"), dir.join("link")).unwrap();
    end(
        &td,
        &id,
        State::Done,
        Outcome {
            response: Some(json!({"frames": 120})),
            exit_code: Some(0),
            ..Outcome::default()
        },
    )
    .await;
    let state = shared(&td);
    assert_eq!(
        terrariumd::service_result::send_due(&state).await.unwrap(),
        1
    );
    // Exactly one: the next sweep sends nothing more.
    terrariumd::service_sweep::sweep(&state, chrono::Utc::now())
        .await
        .unwrap();
    let got = mails(&td, "p/w1", &id).await;
    assert_eq!(got.len(), 1, "{got:?}");
    assert_eq!(
        got[0]["subject"],
        format!("Service gen: request {id} done")
    );
    let body = body_of(&td, "p/w1", &got[0]).await;
    assert!(
        body.contains(&format!("terrarium service result {id} --out <dir>")),
        "{body}"
    );
    assert!(body.contains("\"frames\": 120"), "{body}");
    assert!(body.contains("- Response files: 2 (8 bytes)"), "{body}");

    // The list: the two files with their sha256, not the link.
    let r = td
        .get_as("p/w1", &format!("/api/service-requests/{id}/result"))
        .await;
    let v = r.ok();
    assert_eq!(v["state"], "done");
    assert_eq!(v["exit_code"], 0);
    assert_eq!(v["response"], json!({"frames": 120}));
    assert_eq!(
        v["files"],
        json!([
            {"path": "out.txt", "size": 3,
             "sha256": "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad"},
            {"path": "sub/more.txt", "size": 5,
             "sha256": "2cf24dba5fb0a30e26e83b2ac5b9e29e1b161e5c1fa7425e73043362938b9824"}
        ])
    );
    // A file raw, and in JSON chunks.
    let r = td
        .get_as(
            "p/w1",
            &format!("/api/service-requests/{id}/result/sub/more.txt"),
        )
        .await;
    assert_eq!(r.ok(), &json!("hello"));
    assert_eq!(r.headers["content-type"], "application/octet-stream");
    let r = td
        .get_as(
            "p/w1",
            &format!("/api/service-requests/{id}/result/sub/more.txt?format=json&offset=1&length=2"),
        )
        .await;
    assert_eq!(
        r.ok(),
        &json!({"path": "sub/more.txt", "size": 5, "offset": 1, "length": 2,
                "eof": false, "data_base64": "ZWw="})
    );
    // No way out of the response directory.
    for bad in ["link", "..%2F..%2Foutside", "sub", "nope.txt", "sub/..%2Fout.txt"] {
        let r = td
            .get_as("p/w1", &format!("/api/service-requests/{id}/result/{bad}"))
            .await;
        assert_eq!(r.status, 404, "{bad}: {}", r.body);
    }
    // A caller reads only its own requests; the service's lead may too.
    for path in [
        format!("/api/service-requests/{id}/result"),
        format!("/api/service-requests/{id}/result/out.txt"),
    ] {
        td.get_as("p/w2", &path).await.err(404, "not_found");
        td.get_as("p/lead", &path).await.ok();
    }
}

#[tokio::test]
async fn a_failure_mail_carries_the_log_tail() {
    let td = setup().await;
    let id = call(&td, "p/w1").await;
    end(
        &td,
        &id,
        State::Failed,
        Outcome {
            exit_code: Some(3),
            error: Some("the script exited with 3".into()),
            log_tail: Some("step 1\nTraceback: out of memory\n".into()),
            ..Outcome::default()
        },
    )
    .await;
    terrariumd::service_sweep::sweep(&shared(&td), chrono::Utc::now())
        .await
        .unwrap();
    let got = mails(&td, "p/w1", &id).await;
    assert_eq!(got.len(), 1, "{got:?}");
    assert_eq!(
        got[0]["subject"],
        format!("Service gen: request {id} failed")
    );
    let body = body_of(&td, "p/w1", &got[0]).await;
    assert!(body.contains("Traceback: out of memory"), "{body}");
    assert!(body.contains("- Exit code: 3"), "{body}");
    assert!(body.contains("It is not tried again"), "{body}");
    assert!(body.contains("terrarium service call gen"), "{body}");
    let r = td
        .get_as("p/w1", &format!("/api/service-requests/{id}/result"))
        .await;
    assert_eq!(r.ok()["log_tail"], "step 1\nTraceback: out of memory\n");
    assert_eq!(r.ok()["files"], json!([]));
}

#[tokio::test]
async fn a_cancel_mails_the_caller_only_when_another_cancels() {
    let td = setup().await;
    let own = call(&td, "p/w1").await;
    td.post_as(
        "p/w1",
        &format!("/api/service-requests/{own}/cancel"),
        json!({}),
    )
    .await
    .ok();
    let other = call(&td, "p/w1").await;
    td.post_as(
        "p/lead",
        &format!("/api/service-requests/{other}/cancel"),
        json!({}),
    )
    .await
    .ok();
    let state = shared(&td);
    assert_eq!(
        terrariumd::service_result::send_due(&state).await.unwrap(),
        0
    );
    assert!(mails(&td, "p/w1", &own).await.is_empty());
    let got = mails(&td, "p/w1", &other).await;
    assert_eq!(got.len(), 1, "{got:?}");
    let body = body_of(&td, "p/w1", &got[0]).await;
    assert!(body.contains("cancelled by agent:p/lead"), "{body}");
}

/// rev-1264b minor 2: the retention counts from the end of the request,
/// so a request that waited long keeps its response the full 7 days.
#[tokio::test]
async fn the_retention_counts_from_the_end() {
    let td = setup().await;
    let id = call(&td, "p/w1").await;
    let state = shared(&td);
    // The call was 10 days ago; it ends now.
    let old = (chrono::Utc::now() - chrono::Duration::days(10))
        .to_rfc3339_opts(chrono::SecondsFormat::Millis, true);
    let i = id.clone();
    state
        .db
        .write("system", move |t| {
            t.execute(
                "UPDATE service_requests SET created_at = ?2 WHERE id = ?1",
                rusqlite::params![i, old],
            )?;
            Ok(())
        })
        .await
        .unwrap();
    let dir = store::response_dir(&td.state, &id);
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::write(dir.join("out.txt"), b"abc").unwrap();
    end(&td, &id, State::Done, Outcome::default()).await;
    let now = chrono::Utc::now();
    let (_, deleted) = terrariumd::service_sweep::sweep(&state, now + chrono::Duration::days(1))
        .await
        .unwrap();
    assert_eq!(deleted, 0);
    assert!(dir.join("out.txt").exists());
    let (_, deleted) = terrariumd::service_sweep::sweep(&state, now + chrono::Duration::days(8))
        .await
        .unwrap();
    assert_eq!(deleted, 1);
    assert!(!dir.exists());
    let r = td
        .get_as("p/w1", &format!("/api/service-requests/{id}/result"))
        .await;
    assert_eq!(r.ok()["files"], json!([]));
    assert!(r.ok()["files_deleted_at"].is_string(), "{}", r.body);
    let r = td
        .get_as("p/w1", &format!("/api/service-requests/{id}/result/out.txt"))
        .await;
    assert_eq!(r.status, 404, "{}", r.body);
    assert!(r.body.to_string().contains("retention"), "{}", r.body);
}
