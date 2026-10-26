//! Event sources and ingest (docs/api.md §20): the API, the rights per
//! role, the secret shown only at create and rotate, the ingest listener
//! end to end (signatures, dedupe, ping, disabled and unknown sources, the
//! body limit), and `POST /api/events`.
#![allow(clippy::unwrap_used, clippy::expect_used)]

mod common;

use axum::body::Body;
use common::TestDaemon;
use http_body_util::BodyExt;
use hyper::{Request, StatusCode};
use serde_json::{Value, json};
use tower::ServiceExt;

fn id(v: &Value) -> String {
    v["id"].as_str().unwrap().to_owned()
}

fn fixture(rel: &str) -> Vec<u8> {
    let path = format!(
        "{}/../terrarium-core/tests/fixtures/ci/{rel}",
        env!("CARGO_MANIFEST_DIR")
    );
    std::fs::read(path).unwrap()
}

/// One request to the ingest router.
async fn hook(
    td: &TestDaemon,
    path: &str,
    headers: &[(&str, String)],
    body: Vec<u8>,
) -> (StatusCode, Value) {
    let state = td.daemon.as_ref().unwrap().state.clone();
    let mut req = Request::builder()
        .method("POST")
        .uri(path)
        .header("content-type", "application/json");
    for (k, v) in headers {
        req = req.header(*k, v);
    }
    let resp = terrariumd::ingest::router(state)
        .oneshot(req.body(Body::from(body)).unwrap())
        .await
        .unwrap();
    let status = resp.status();
    let raw = resp.into_body().collect().await.unwrap().to_bytes();
    let body = serde_json::from_slice(&raw)
        .unwrap_or_else(|_| Value::String(String::from_utf8_lossy(&raw).into()));
    (status, body)
}

fn github(event: &str, delivery: &str, secret: &str, body: &[u8]) -> Vec<(&'static str, String)> {
    vec![
        ("x-github-event", event.to_owned()),
        ("x-github-delivery", delivery.to_owned()),
        (
            "x-hub-signature-256",
            format!("sha256={}", terrariumd::ingest::sign(secret, body)),
        ),
    ]
}

/// A project `terra` with a lead (project_admin) and a worker, and a
/// project `ops` with an agent.
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

#[tokio::test]
async fn sources_rights_and_the_secret() {
    let td = TestDaemon::start().await;
    let (lead, worker, other) = setup(&td).await;

    let r = td
        .post(
            "/api/event-sources",
            json!({"name": "gh", "project": "terra", "kind": "github"}),
        )
        .await;
    assert_eq!(r.status, 201, "{}", r.body);
    let s = r.body.clone();
    let secret = s["secret"].as_str().unwrap().to_owned();
    assert_eq!(secret.len(), 64);
    assert_eq!(s["hook_path"], "/hooks/terra/gh");
    assert_eq!(s["kind"], "github");
    assert_eq!(s["enabled"], true);
    assert_eq!(s["received"], 0);
    assert!(s["setup"].as_array().unwrap().len() > 2);
    td.post(
        "/api/event-sources",
        json!({"name": "gh", "project": "terra", "kind": "github"}),
    )
    .await
    .err(409, "conflict");
    td.post(
        "/api/event-sources",
        json!({"name": "Bad Name", "project": "terra", "kind": "github"}),
    )
    .await
    .err(422, "invalid");
    let r = td
        .post(
            "/api/event-sources",
            json!({"name": "x", "project": "terra", "kind": "gitlab"}),
        )
        .await;
    assert_eq!(r.status, 400, "{}", r.body);

    // The secret is in no other answer, the log, or the audit.
    for path in [
        "/api/event-sources".to_owned(),
        format!("/api/event-sources/{}", id(&s)),
        "/api/event-sources/terra%2Fgh".to_owned(),
        "/api/events".to_owned(),
        "/api/audit".to_owned(),
    ] {
        let r = td.get(&path).await;
        assert_eq!(r.status, 200, "{path}: {}", r.body);
        assert!(
            !r.body.to_string().contains(&secret),
            "{path} shows the secret"
        );
    }
    let shown = td.get("/api/event-sources/terra%2Fgh").await;
    assert!(shown.body.get("secret").is_none());
    assert_eq!(shown.body["id"], s["id"]);

    // Members read their project's sources, without the secret; another
    // project's agent may not.
    let r = td.get_as(&worker, "/api/event-sources").await;
    assert_eq!(r.ok().as_array().unwrap().len(), 1);
    assert!(!r.body.to_string().contains(&secret));
    td.get_as(&other, "/api/event-sources/terra%2Fgh")
        .await
        .err(403, "forbidden");
    assert!(
        td.get_as(&other, "/api/event-sources")
            .await
            .ok()
            .as_array()
            .unwrap()
            .is_empty()
    );

    // A member manages nothing; the project_admin manages its project's.
    td.post_as(
        &worker,
        "/api/event-sources",
        json!({"name": "fj", "kind": "forgejo"}),
    )
    .await
    .err(403, "forbidden");
    let r = td
        .post_as(
            &lead,
            "/api/event-sources",
            json!({"name": "fj", "kind": "forgejo"}),
        )
        .await;
    assert_eq!(r.status, 201, "{}", r.body);
    assert_eq!(r.body["project"], "terra");
    assert!(r.body["secret"].is_string());
    td.post_as(
        &lead,
        "/api/event-sources",
        json!({"name": "fj", "project": "ops", "kind": "forgejo"}),
    )
    .await
    .err(403, "forbidden");
    // A global source needs a sysadmin.
    td.post_as(
        &lead,
        "/api/event-sources",
        json!({"name": "all", "project": null, "kind": "generic"}),
    )
    .await
    .err(403, "forbidden");
    let r = td
        .post(
            "/api/event-sources",
            json!({"name": "all", "project": null, "kind": "generic"}),
        )
        .await;
    assert_eq!(r.status, 201, "{}", r.body);
    assert!(r.body["project"].is_null());
    assert_eq!(r.body["hook_path"], "/hooks/_/all");
    td.get_as(&lead, "/api/event-sources/_%2Fall")
        .await
        .err(403, "forbidden");

    // Patch, rotate, and delete.
    td.patch_as(
        &worker,
        "/api/event-sources/terra%2Fgh",
        json!({"enabled": false}),
    )
    .await
    .err(403, "forbidden");
    let r = td
        .patch_as(
            &lead,
            "/api/event-sources/terra%2Fgh",
            json!({"enabled": false, "name": "github"}),
        )
        .await;
    assert_eq!(r.ok()["enabled"], false);
    assert_eq!(r.body["name"], "github");
    assert!(r.body.get("secret").is_none());
    let r = td
        .post_empty_as(&lead, "/api/event-sources/terra%2Fgithub/rotate")
        .await;
    let rotated = r.ok()["secret"].as_str().unwrap().to_owned();
    assert_ne!(rotated, secret);
    let r = td.delete("/api/event-sources/terra%2Ffj").await;
    assert_eq!(r.status, 204);
    td.get("/api/event-sources/terra%2Ffj")
        .await
        .err(404, "not_found");

    // Each change is an event and an audit row.
    let events = td.get("/api/events?kind=event_source").await;
    let changes: Vec<&str> = events
        .ok()
        .as_array()
        .unwrap()
        .iter()
        .map(|e| e["data"]["change"].as_str().unwrap())
        .collect();
    assert_eq!(
        changes,
        [
            "created", "created", "created", "disabled", "rotated", "deleted"
        ]
    );
    let audit = td.get("/api/audit").await.ok().to_string();
    for action in [
        "event_source.create",
        "event_source.update",
        "event_source.rotate",
        "event_source.delete",
    ] {
        assert!(audit.contains(action), "{action}");
    }
}

#[tokio::test]
async fn ingest_end_to_end() {
    let td = TestDaemon::start().await;
    setup(&td).await;
    let s = td
        .post(
            "/api/event-sources",
            json!({"name": "gh", "project": "terra", "kind": "github"}),
        )
        .await
        .ok()
        .clone();
    let secret = s["secret"].as_str().unwrap().to_owned();
    let body = fixture("github/workflow_run_failure.json");

    // A good delivery is one event, with the source as the actor.
    let (status, r) = hook(
        &td,
        "/hooks/terra/gh",
        &github("workflow_run", "d-1", &secret, &body),
        body.clone(),
    )
    .await;
    assert_eq!(status, 200, "{r}");
    assert_eq!(r["status"], "accepted");
    assert_eq!(r["kind"], "ci.run.failed");
    let seq = r["seq"].as_i64().unwrap();
    let events = td.get("/api/events?kind=ci.run.failed").await;
    let e = &events.ok()[0];
    assert_eq!(e["seq"], seq);
    assert_eq!(e["actor"], "source:terra/gh");
    assert_eq!(e["project"], "terra");
    assert_eq!(e["data"]["source"], "terra/gh");
    assert_eq!(e["data"]["delivery"], "d-1");
    assert_eq!(e["data"]["repo"], "octo-org/hello-world");
    assert_eq!(e["data"]["branch"], "feature/login");

    // The same delivery again: no second event.
    let (status, r) = hook(
        &td,
        "/hooks/terra/gh",
        &github("workflow_run", "d-1", &secret, &body),
        body.clone(),
    )
    .await;
    assert_eq!(
        (status, r["status"].as_str()),
        (StatusCode::OK, Some("duplicate"))
    );
    assert_eq!(
        td.get("/api/events?kind=ci.run.failed")
            .await
            .ok()
            .as_array()
            .unwrap()
            .len(),
        1
    );

    // A ping: pong, no event. An ignored webhook: no event.
    let ping = fixture("github/ping.json");
    let (status, r) = hook(
        &td,
        "/hooks/terra/gh",
        &github("ping", "d-2", &secret, &ping),
        ping,
    )
    .await;
    assert_eq!(
        (status, r["status"].as_str()),
        (StatusCode::OK, Some("pong"))
    );
    let queued = fixture("github/workflow_job_queued.json");
    let (_, r) = hook(
        &td,
        "/hooks/terra/gh",
        &github("workflow_job", "d-3", &secret, &queued),
        queued,
    )
    .await;
    assert_eq!(r["status"], "ignored");

    let shown = td.get("/api/event-sources/terra%2Fgh").await.ok().clone();
    assert_eq!(shown["received"], 1);
    assert!(shown["last_seen"].is_string());
    assert!(shown["last_error"].is_null());

    // Auth failures: 401 with no detail.
    let mut bad = github("workflow_run", "d-4", &secret, &body);
    bad[2].1 = format!("sha256={}", terrariumd::ingest::sign("wrong", &body));
    let no_sig = github("workflow_run", "d-5", &secret, &body)[..2].to_vec();
    for (path, headers) in [
        ("/hooks/terra/gh", bad),
        ("/hooks/terra/gh", no_sig),
        (
            "/hooks/terra/nope",
            github("workflow_run", "d-6", &secret, &body),
        ),
        // The right name and secret, in the wrong project.
        (
            "/hooks/ops/gh",
            github("workflow_run", "d-7", &secret, &body),
        ),
        ("/hooks/_/gh", github("workflow_run", "d-8", &secret, &body)),
    ] {
        let (status, r) = hook(&td, path, &headers, body.clone()).await;
        assert_eq!(status, 401, "{path}: {r}");
        assert_eq!(r["error"]["message"], "unauthorized", "{path}");
    }
    // The source keeps the reason for its admin (written in the
    // background).
    let mut last_error = Value::Null;
    for _ in 0..100 {
        last_error = td.get("/api/event-sources/terra%2Fgh").await.ok()["last_error"].clone();
        if last_error.is_string() {
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(10)).await;
    }
    assert!(
        last_error
            .as_str()
            .unwrap()
            .contains("bad or missing signature"),
        "{last_error}"
    );

    // A body over 1 MiB.
    let big = vec![b' '; terrariumd::ingest::MAX_BODY + 1];
    let (status, _) = hook(
        &td,
        "/hooks/terra/gh",
        &github("push", "d-9", &secret, &big),
        big,
    )
    .await;
    assert_eq!(status, 413);

    // The form content type: refused with a hint.
    let state = td.daemon.as_ref().unwrap().state.clone();
    let mut req = Request::builder()
        .method("POST")
        .uri("/hooks/terra/gh")
        .header("content-type", "application/x-www-form-urlencoded");
    for (k, v) in github("push", "d-10", &secret, &body) {
        req = req.header(k, v);
    }
    let resp = terrariumd::ingest::router(state)
        .oneshot(req.body(Body::from(body.clone())).unwrap())
        .await
        .unwrap();
    assert_eq!(resp.status(), 415);

    // A disabled source: 401, also with a good signature.
    td.patch("/api/event-sources/terra%2Fgh", json!({"enabled": false}))
        .await
        .ok();
    let (status, _) = hook(
        &td,
        "/hooks/terra/gh",
        &github("workflow_run", "d-11", &secret, &body),
        body.clone(),
    )
    .await;
    assert_eq!(status, 401);

    // A rotated secret: the old one stops working.
    td.patch("/api/event-sources/terra%2Fgh", json!({"enabled": true}))
        .await
        .ok();
    let new = td
        .post_empty("/api/event-sources/terra%2Fgh/rotate")
        .await
        .ok()["secret"]
        .as_str()
        .unwrap()
        .to_owned();
    let (status, _) = hook(
        &td,
        "/hooks/terra/gh",
        &github("workflow_run", "d-12", &secret, &body),
        body.clone(),
    )
    .await;
    assert_eq!(status, 401);
    let (status, _) = hook(
        &td,
        "/hooks/terra/gh",
        &github("workflow_run", "d-12", &new, &body),
        body.clone(),
    )
    .await;
    assert_eq!(status, 200);

    // The health check, and nothing else on the listener.
    let state = td.daemon.as_ref().unwrap().state.clone();
    let resp = terrariumd::ingest::router(state.clone())
        .oneshot(Request::get("/healthz").body(Body::empty()).unwrap())
        .await
        .unwrap();
    assert_eq!(resp.status(), 200);
    let resp = terrariumd::ingest::router(state)
        .oneshot(Request::get("/api/health").body(Body::empty()).unwrap())
        .await
        .unwrap();
    assert_eq!(resp.status(), 404);
}

#[tokio::test]
async fn ingest_forgejo_and_generic() {
    let td = TestDaemon::start().await;
    setup(&td).await;
    let fj = td
        .post(
            "/api/event-sources",
            json!({"name": "fj", "project": "terra", "kind": "forgejo"}),
        )
        .await
        .ok()["secret"]
        .as_str()
        .unwrap()
        .to_owned();
    let body = fixture("forgejo/workflow_job_failure.json");
    let headers = vec![
        ("x-forgejo-event", "workflow_job_failure".to_owned()),
        ("x-forgejo-event-type", "workflow_job_failure".to_owned()),
        ("x-forgejo-delivery", "f-1".to_owned()),
        ("x-forgejo-signature", terrariumd::ingest::sign(&fj, &body)),
    ];
    let (status, r) = hook(&td, "/hooks/terra/fj", &headers, body.clone()).await;
    assert_eq!(status, 200, "{r}");
    assert_eq!(r["kind"], "ci.job.failed");
    // The Gitea names of the headers work as well.
    let headers = vec![
        ("x-gitea-event", "push".to_owned()),
        ("x-gitea-delivery", "f-2".to_owned()),
        (
            "x-gitea-signature",
            terrariumd::ingest::sign(&fj, &fixture("forgejo/push.json")),
        ),
    ];
    let (_, r) = hook(
        &td,
        "/hooks/terra/fj",
        &headers,
        fixture("forgejo/push.json"),
    )
    .await;
    assert_eq!(r["kind"], "git.push");

    // A global generic source, with the bearer token.
    let g = td
        .post(
            "/api/event-sources",
            json!({"name": "nightly", "project": null, "kind": "generic"}),
        )
        .await
        .ok()["secret"]
        .as_str()
        .unwrap()
        .to_owned();
    let body = serde_json::to_vec(
        &json!({"kind": "ci.run.failed", "summary": "nightly failed",
                                          "data": {"branch": "main", "source": "fake"}}),
    )
    .unwrap();
    let auth = vec![("authorization", format!("Bearer {g}"))];
    let (status, r) = hook(&td, "/hooks/_/nightly", &auth, body).await;
    assert_eq!(status, 200, "{r}");
    let e = td.get("/api/events?kind=ci.run.failed").await.ok()[0].clone();
    assert_eq!(e["actor"], "source:_/nightly");
    assert!(e["project"].is_null());
    assert_eq!(
        e["data"]["source"], "_/nightly",
        "the source field is the daemon's"
    );
    assert_eq!(e["data"]["provider"], "generic");
    // A generic source cannot pose as the daemon.
    let body = serde_json::to_vec(&json!({"kind": "status"})).unwrap();
    let (status, r) = hook(&td, "/hooks/_/nightly", &auth, body).await;
    assert_eq!(status, 400, "{r}");
}

#[tokio::test]
async fn emit_rights() {
    let td = TestDaemon::start().await;
    let (_lead, worker, _other) = setup(&td).await;

    // The operator emits any kind, in any project or none.
    let r = td
        .post(
            "/api/events",
            json!({"kind": "maintenance", "summary": "db backup", "data": {"ok": true}}),
        )
        .await;
    assert_eq!(r.status, 201, "{}", r.body);
    assert_eq!(r.body["actor"], "operator");
    assert!(r.body["project"].is_null());
    let r = td
        .post(
            "/api/events",
            json!({"kind": "ci.run.failed", "project": "ops"}),
        )
        .await;
    assert_eq!(r.status, 201);
    assert_eq!(r.body["project"], "ops");
    assert_eq!(r.body["summary"], "ci.run.failed");
    td.post("/api/events", json!({"kind": "x", "project": "nope"}))
        .await
        .err(404, "not_found");
    td.post("/api/events", json!({"kind": "Bad Kind"}))
        .await
        .err(422, "invalid");
    td.post("/api/events", json!({"kind": "x", "data": [1]}))
        .await
        .err(422, "invalid");

    // An agent: its own project, and not the daemon's kinds.
    let r = td
        .post_as(
            &worker,
            "/api/events",
            json!({"kind": "ci.run.failed", "summary": "local\ntests failed", "data": {"branch": "main"}}),
        )
        .await;
    assert_eq!(r.status, 201, "{}", r.body);
    assert_eq!(r.body["project"], "terra");
    assert_eq!(r.body["actor"], format!("agent:{worker}"));
    assert_eq!(r.body["agent"], "worker");
    assert_eq!(r.body["summary"], "local tests failed");
    for kind in [
        "status",
        "mail",
        "card",
        "turn_start",
        "schedule",
        "memory.created",
    ] {
        td.post_as(&worker, "/api/events", json!({"kind": kind}))
            .await
            .err(403, "forbidden");
    }
    td.post_as(
        &worker,
        "/api/events",
        json!({"kind": "agent.note", "project": "ops"}),
    )
    .await
    .err(403, "forbidden");
    // The event reaches the stream.
    let page = td.get_as(&worker, "/api/events?kind=agent.note").await;
    assert!(page.ok().as_array().unwrap().is_empty());
    let r = td
        .post_as(&worker, "/api/events", json!({"kind": "agent.note"}))
        .await;
    let seq = r.ok()["seq"].clone();
    let page = td.get_as(&worker, "/api/events?kind=agent.note").await;
    assert_eq!(page.ok()[0]["seq"], seq);
}
