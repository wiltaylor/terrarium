//! Services C1 (TER-1264): the definition rules, the rights, the request
//! queue and its states, the payload store, the files, and the wait limit.
#![allow(clippy::unwrap_used, clippy::expect_used)]

use crate::common;

use common::TestDaemon;
use http_body_util::{BodyExt, Full};
use hyper::body::Bytes;
use hyper::{Method, Request};
use hyper_util::rt::TokioIo;
use serde_json::{Value, json};
use tokio::net::UnixStream;

/// A POST with a raw body and a content type, as agent `agent`:
/// `(status, the body as JSON)`.
async fn raw(td: &TestDaemon, path: &str, ct: &str, body: Vec<u8>, agent: &str) -> (u16, Value) {
    let stream = UnixStream::connect(&td.socket).await.unwrap();
    let (mut sender, conn) = hyper::client::conn::http1::handshake(TokioIo::new(stream))
        .await
        .unwrap();
    tokio::spawn(conn);
    let req = Request::builder()
        .method(Method::POST)
        .uri(path)
        .header("host", "localhost")
        .header("content-type", ct)
        .header("x-terrarium-agent", agent)
        .body(Full::new(Bytes::from(body)))
        .unwrap();
    let resp = sender.send_request(req).await.unwrap();
    let status = resp.status().as_u16();
    let bytes = resp.into_body().collect().await.unwrap().to_bytes();
    (
        status,
        serde_json::from_slice(&bytes).unwrap_or(Value::Null),
    )
}

/// Plenty of free space, whatever the disk of this machine has (the 20%
/// floor has its unit test in `store::services`).
fn plenty_of_space() {
    terrariumd::store::services::override_free_space(Some((900 << 30, 1000 << 30)));
}

/// Move request `id` along the state edges, as C2 will.
async fn move_request(td: &TestDaemon, id: &str, steps: &[(&str, &str)]) {
    use terrarium_core::service::State;
    let state = td.daemon.as_ref().unwrap().state.clone();
    for (from, to) in steps {
        let (id, from, to) = (
            id.to_owned(),
            State::parse(from).unwrap(),
            State::parse(to).unwrap(),
        );
        state
            .db
            .write("system", move |t| {
                terrariumd::store::services::transition(t, &id, from, to, Default::default())
            })
            .await
            .unwrap();
    }
}

/// Project `p` with a lead, two members, and targets; project `q` with a
/// member; a sysadmin in `ops`; a global target `box`.
async fn setup() -> TestDaemon {
    let td = TestDaemon::start().await;
    td.project("p").await;
    td.project("q").await;
    td.project("ops").await;
    td.agent("p", "lead", json!({"roles": ["project_admin"]}))
        .await;
    td.agent("p", "w1", json!({})).await;
    td.agent("p", "w2", json!({})).await;
    td.agent("q", "x1", json!({})).await;
    td.agent("ops", "root", json!({"roles": ["sysadmin"]}))
        .await;
    for t in [
        json!({"name": "box", "kind": "ssh", "address": "u@h"}),
        json!({"name": "p-box", "kind": "ssh", "project": "p", "address": "u@h"}),
        json!({"name": "p-gpu", "kind": "docker", "project": "p", "host": "helios", "image": "alpine", "ephemeral": true}),
        json!({"name": "p-dock", "kind": "docker", "project": "p", "host": "helios", "image": "alpine"}),
        json!({"name": "q-box", "kind": "ssh", "project": "q", "address": "u@h"}),
    ] {
        let r = td.post("/api/targets", t.clone()).await;
        assert_eq!(r.status, 201, "{t}: {}", r.body);
    }
    td
}

fn script(name: &str, project: Option<&str>, target: &str) -> Value {
    let mut v =
        json!({"name": name, "kind": "script", "target": target, "command": ["/opt/svc/run"]});
    if let Some(p) = project {
        v["project"] = p.into();
    }
    v
}

fn with(mut v: Value, extra: Value) -> Value {
    for (k, x) in extra.as_object().unwrap() {
        v[k] = x.clone();
    }
    v
}

#[tokio::test]
async fn a_definition_follows_the_rules() {
    let td = setup().await;
    let r = td
        .post(
            "/api/services",
            with(
                script("video-gen", Some("p"), "p-gpu"),
                json!({"max_run_time": "2h", "resources": {"gpus": 1, "gpu_exclusive": true}}),
            ),
        )
        .await;
    assert_eq!(r.status, 201, "{}", r.body);
    assert_eq!(r.body["scope"], "project");
    assert_eq!(r.body["source"], "api");
    for (body, code, says) in [
        // MEM-399: an exclusive GPU only on a docker target.
        (
            with(
                script("g2", Some("p"), "p-box"),
                json!({"resources": {"gpus": 1, "gpu_exclusive": true}}),
            ),
            422,
            "not a docker target",
        ),
        // The docker GPU rule of `reservations.rs`: ephemeral with an image.
        (
            with(
                script("g3", Some("p"), "p-dock"),
                json!({"resources": {"gpus": 1, "gpu_exclusive": true}}),
            ),
            422,
            "ephemeral",
        ),
        (
            script("t2", Some("p"), "q-box"),
            422,
            "not a target of project",
        ),
        (script("t3", None, "nope"), 422, "no target"),
        (
            with(script("t4", None, "box"), json!({"concurrency": 0})),
            422,
            "concurrency",
        ),
        (
            with(script("t5", None, "box"), json!({"shell": true})),
            400,
            "",
        ),
        (script("video-gen", Some("p"), "p-gpu"), 409, "exists"),
    ] {
        let r = td.post("/api/services", body.clone()).await;
        assert_eq!(r.status, code, "{body}: {}", r.body);
        assert!(r.body.to_string().contains(says), "{body}: {}", r.body);
    }
}

/// Design Q6 A: a sysadmin or the project lead makes a service; a member
/// does not; a lead makes no global service and none of another project.
#[tokio::test]
async fn who_defines_a_service() {
    let td = setup().await;
    let r = td
        .post_as("p/w1", "/api/services", script("a", Some("p"), "p-box"))
        .await;
    assert_eq!(r.status, 403, "{}", r.body);
    let r = td
        .post_as("p/lead", "/api/services", script("a", Some("p"), "p-box"))
        .await;
    assert_eq!(r.status, 201, "{}", r.body);
    let r = td
        .post_as("p/lead", "/api/services", script("b", None, "box"))
        .await;
    assert_eq!(r.status, 403, "{}", r.body);
    let r = td
        .post_as("p/lead", "/api/services", script("c", Some("q"), "q-box"))
        .await;
    assert_eq!(r.status, 403, "{}", r.body);
    let r = td
        .post_as("ops/root", "/api/services", script("b", None, "box"))
        .await;
    assert_eq!(r.status, 201, "{}", r.body);
    // The lead changes and removes its project's service.
    let r = td
        .patch_as("p/lead", "/api/services/a", json!({"concurrency": 2}))
        .await;
    assert_eq!(r.status, 200, "{}", r.body);
    assert_eq!(r.body["concurrency"], 2);
    let r = td
        .patch_as("p/lead", "/api/services/a", json!({"project": "q"}))
        .await;
    assert_eq!(r.status, 422, "{}", r.body);
    assert_eq!(td.delete_as("p/w1", "/api/services/a").await.status, 403);
    assert_eq!(td.delete_as("p/lead", "/api/services/a").await.status, 204);
}

/// The callers list, what an agent sees, and the request rights.
#[tokio::test]
async fn who_calls_and_who_sees_a_request() {
    let td = setup().await;
    td.post("/api/services", script("own", Some("p"), "p-box"))
        .await
        .ok();
    td.post(
        "/api/services",
        with(
            script("shared", None, "box"),
            json!({"callers": ["agent:q/x1"]}),
        ),
    )
    .await
    .ok();
    td.post("/api/services", script("closed", None, "box"))
        .await
        .ok();
    // An empty callers list: the project's agents.
    let r = td
        .post_as(
            "p/w1",
            "/api/services/own/requests",
            json!({"request": {"n": 1}}),
        )
        .await;
    assert_eq!(r.status, 201, "{}", r.body);
    let id = r.body["id"].as_str().unwrap().to_owned();
    assert_eq!(r.body["state"], "queued");
    assert_eq!(r.body["position"], 1);
    assert_eq!(r.body["status_url"], format!("/api/service-requests/{id}"));
    td.post_as("q/x1", "/api/services/own/requests", json!({}))
        .await
        .err(404, "not_found");
    // A named caller of a global service; others do not see it.
    td.post_as("q/x1", "/api/services/shared/requests", json!({}))
        .await
        .ok();
    td.post_as("p/w1", "/api/services/shared/requests", json!({}))
        .await
        .err(404, "not_found");
    td.post_as("p/w1", "/api/services/closed/requests", json!({}))
        .await
        .err(404, "not_found");
    let names = |v: &Value| -> Vec<String> {
        v.as_array()
            .unwrap()
            .iter()
            .map(|s| s["name"].as_str().unwrap().to_owned())
            .collect()
    };
    assert_eq!(
        names(td.get_as("q/x1", "/api/services").await.ok()),
        ["shared"]
    );
    assert_eq!(
        names(td.get_as("p/w2", "/api/services").await.ok()),
        ["own"]
    );
    assert_eq!(names(td.get("/api/services").await.ok()).len(), 3);
    // The request: its caller and the lead see it; another member does not.
    let path = format!("/api/service-requests/{id}");
    assert_eq!(td.get_as("p/w1", &path).await.ok()["caller"], "agent:p/w1");
    td.get_as("p/lead", &path).await.ok();
    td.get_as("p/w2", &path).await.err(404, "not_found");
    // The list: the lead sees all, a member its own.
    td.post_as("p/w2", "/api/services/own/requests", json!({}))
        .await
        .ok();
    let all = td.get_as("p/lead", "/api/services/own/requests").await;
    assert_eq!(all.ok().as_array().unwrap().len(), 2);
    let mine = td.get_as("p/w2", "/api/services/own/requests").await;
    assert_eq!(mine.ok().as_array().unwrap().len(), 1);
    // The second request waits behind the first.
    let second = &mine.ok()[0];
    assert_eq!(second["position"], 2, "{second}");
    // Cancel: another member may not; the caller may; a second cancel is 409.
    td.post_as("p/w2", &format!("{path}/cancel"), json!({}))
        .await
        .err(404, "not_found");
    let r = td
        .post_as("p/w1", &format!("{path}/cancel"), json!({}))
        .await;
    assert_eq!(r.ok()["state"], "cancelled");
    let r = td
        .post_as("p/w1", &format!("{path}/cancel"), json!({}))
        .await;
    assert_eq!(r.status, 409, "{}", r.body);
    // A service with an open request is not removed.
    let r = td.delete("/api/services/own").await;
    assert_eq!(r.status, 409, "{}", r.body);
}

/// The payload store: the bytes, the hash, the size limit per payload and
/// per service, and the download.
#[tokio::test]
async fn payloads_are_stored_and_limited() {
    plenty_of_space();
    let td = setup().await;
    td.post(
        "/api/services",
        with(
            script("small", Some("p"), "p-box"),
            json!({"max_payload": "8", "max_retained": "12"}),
        ),
    )
    .await
    .ok();
    let call = |b64: &str| json!({"request": {"x": 1}, "payload_base64": b64});
    // "BYTES" (5 bytes).
    let r = td
        .post_as("p/w1", "/api/services/small/requests", call("QllURVM="))
        .await;
    assert_eq!(r.status, 201, "{}", r.body);
    let id = r.body["id"].as_str().unwrap().to_owned();
    let row = td
        .get_as("p/w1", &format!("/api/service-requests/{id}"))
        .await;
    assert_eq!(row.ok()["payload_size"], 5);
    assert_eq!(
        row.ok()["payload_sha256"],
        "7beb23b48f4fe9109f7cf593f427eec383dee0c07cea52c37a2feee28c3a8c5f"
    );
    let file = td.state.join("services/payloads").join(&id).join("request");
    assert_eq!(std::fs::read(&file).unwrap(), b"BYTES");
    let dl = td
        .get_as("p/w1", &format!("/api/service-requests/{id}/payload"))
        .await;
    assert_eq!(dl.status, 200);
    assert_eq!(dl.body, Value::String("BYTES".into()));
    // Over the payload limit: 9 bytes ("123456789").
    let r = td
        .post_as("p/w1", "/api/services/small/requests", call("MTIzNDU2Nzg5"))
        .await;
    assert_eq!(r.status, 413, "{}", r.body);
    // Over the retained limit: 5 + 8 > 12 ("12345678").
    let r = td
        .post_as("p/w1", "/api/services/small/requests", call("MTIzNDU2Nzg="))
        .await;
    assert_eq!(r.status, 503, "{}", r.body);
    assert!(r.body.to_string().contains("payload bytes"), "{}", r.body);
    // A call without a payload has no download.
    let r = td
        .post_as("p/w1", "/api/services/small/requests", json!({}))
        .await;
    let id2 = r.ok()["id"].as_str().unwrap().to_owned();
    td.get_as("p/w1", &format!("/api/service-requests/{id2}/payload"))
        .await
        .err(404, "not_found");
}

/// `services/<name>.toml`: a reload loads the good files, reports the bad
/// ones, and keeps file services out of PATCH and DELETE.
#[tokio::test]
async fn service_files_load_on_reload() {
    let td = setup().await;
    let dir = td.config.join("services");
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::write(
        dir.join("echo.toml"),
        "kind = \"script\"\ntarget = \"p-box\"\nproject = \"p\"\ncommand = [\"/opt/echo\"]\nwait_limit = \"2h\"\n",
    )
    .unwrap();
    std::fs::write(
        dir.join("bad.toml"),
        "kind = \"script\"\ntarget = \"p-box\"\nproject = \"p\"\ncommand = [\"/opt/x\"]\n[resources]\ngpus = 1\ngpu_exclusive = true\n",
    )
    .unwrap();
    let r = td.post_empty("/api/reload").await;
    let errors = r.ok()["errors"].to_string();
    assert!(errors.contains("services/bad.toml"), "{errors}");
    let s = td.get("/api/services/echo").await.ok().clone();
    assert_eq!(s["source"], "file", "{s}");
    assert_eq!(s["wait_limit"], "2h", "{s}");
    let r = td
        .patch("/api/services/echo", json!({"description": "x"}))
        .await;
    assert_eq!(r.status, 409, "{}", r.body);
    std::fs::remove_file(dir.join("echo.toml")).unwrap();
    td.post_empty("/api/reload").await.ok();
    td.get("/api/services/echo").await.err(404, "not_found");
}

/// Design D3, Q5 A: a request that did not start within its wait limit
/// ends as `timed_out`, and its caller gets a mail.
#[tokio::test]
async fn a_request_past_its_wait_limit_times_out_with_a_mail() {
    let td = setup().await;
    td.post(
        "/api/services",
        with(
            script("slow", Some("p"), "p-box"),
            json!({"wait_limit": "1m"}),
        ),
    )
    .await
    .ok();
    let r = td
        .post_as("p/w1", "/api/services/slow/requests", json!({}))
        .await;
    let id = r.ok()["id"].as_str().unwrap().to_owned();
    let state = td.daemon.as_ref().unwrap().state.clone();
    let (n, _) = terrariumd::service_sweep::sweep(&state, chrono::Utc::now())
        .await
        .unwrap();
    assert_eq!(n, 0);
    let later = chrono::Utc::now() + chrono::Duration::minutes(2);
    let (n, _) = terrariumd::service_sweep::sweep(&state, later)
        .await
        .unwrap();
    assert_eq!(n, 1);
    let row = td
        .get_as("p/w1", &format!("/api/service-requests/{id}"))
        .await;
    assert_eq!(row.ok()["state"], "timed_out");
    assert!(row.ok()["ended_at"].is_string());
    let mail = td.mail_of("p/w1").await;
    assert!(
        mail.iter()
            .any(|m| m["subject"].as_str().unwrap_or("").contains(&id)),
        "{mail:?}"
    );
}

/// Q2 A: the payload of an ended request is deleted 7 days after the
/// call; one at 6 days stays, and so does the payload of an open request.
/// The deleted bytes no longer count toward `max_retained`.
#[tokio::test]
async fn payloads_go_after_the_seven_day_retention() {
    plenty_of_space();
    let td = setup().await;
    td.post(
        "/api/services",
        with(
            script("keep", Some("p"), "p-box"),
            json!({"max_payload": "8", "max_retained": "10"}),
        ),
    )
    .await
    .ok();
    let call = |b64: &str| json!({"request": {}, "payload_base64": b64});
    // Two payloads of 5 bytes ("BYTES"): the service is full.
    let mut ids = Vec::new();
    for _ in 0..2 {
        let r = td
            .post_as("p/w1", "/api/services/keep/requests", call("QllURVM="))
            .await;
        ids.push(r.ok()["id"].as_str().unwrap().to_owned());
    }
    let (ended, open) = (ids[0].clone(), ids[1].clone());
    let r = td
        .post_as("p/w1", "/api/services/keep/requests", call("QllURVM="))
        .await;
    assert_eq!(r.status, 503, "{}", r.body);
    // One ends; the other runs (C2 will move it).
    td.post_as(
        "p/w1",
        &format!("/api/service-requests/{ended}/cancel"),
        json!({}),
    )
    .await
    .ok();
    move_request(&td, &open, &[("queued", "waiting"), ("waiting", "running")]).await;
    let dir = |id: &str| td.state.join("services/payloads").join(id);
    let state = td.daemon.as_ref().unwrap().state.clone();
    let now = chrono::Utc::now();
    // At 6 days both payloads stay.
    let (_, deleted) = terrariumd::service_sweep::sweep(&state, now + chrono::Duration::days(6))
        .await
        .unwrap();
    assert_eq!(deleted, 0);
    assert!(dir(&ended).join("request").exists());
    let row = td
        .get_as("p/w1", &format!("/api/service-requests/{ended}"))
        .await;
    assert!(row.ok()["payload_deleted_at"].is_null(), "{}", row.body);
    // At 8 days the ended one goes; the running one stays.
    let (_, deleted) = terrariumd::service_sweep::sweep(&state, now + chrono::Duration::days(8))
        .await
        .unwrap();
    assert_eq!(deleted, 1);
    assert!(!dir(&ended).exists());
    assert!(dir(&open).join("request").exists());
    let row = td
        .get_as("p/w1", &format!("/api/service-requests/{ended}"))
        .await;
    assert!(row.ok()["payload_deleted_at"].is_string(), "{}", row.body);
    let r = td
        .get_as("p/w1", &format!("/api/service-requests/{ended}/payload"))
        .await;
    assert_eq!(r.status, 404, "{}", r.body);
    assert!(r.body.to_string().contains("retention"), "{}", r.body);
    let row = td
        .get_as("p/w1", &format!("/api/service-requests/{open}"))
        .await;
    assert!(row.ok()["payload_deleted_at"].is_null(), "{}", row.body);
    // The 5 freed bytes fit again.
    let r = td
        .post_as("p/w1", "/api/services/keep/requests", call("QllURVM="))
        .await;
    assert_eq!(r.status, 201, "{}", r.body);
    // TER-1285: the ended row stays until 30 days after its end, then the
    // sweep deletes it. The running one stays at any age.
    terrariumd::service_sweep::sweep(&state, now + chrono::Duration::days(29))
        .await
        .unwrap();
    let row = td
        .get_as("p/w1", &format!("/api/service-requests/{ended}"))
        .await;
    assert_eq!(row.status, 200, "{}", row.body);
    terrariumd::service_sweep::sweep(&state, now + chrono::Duration::days(31))
        .await
        .unwrap();
    let row = td
        .get_as("p/w1", &format!("/api/service-requests/{ended}"))
        .await;
    assert_eq!(row.status, 404, "{}", row.body);
    let row = td
        .get_as("p/w1", &format!("/api/service-requests/{open}"))
        .await;
    assert_eq!(row.status, 200, "{}", row.body);
}

/// A call over HTTP as multipart: the `request` part and the `payload`
/// part (the file), stored as they came.
#[tokio::test]
async fn a_multipart_call_stores_the_payload() {
    plenty_of_space();
    let td = setup().await;
    td.post("/api/services", script("mp", Some("p"), "p-box"))
        .await
        .ok();
    let mut body =
        b"--XyZ\r\nContent-Disposition: form-data; name=\"request\"\r\n\r\n{\"seconds\": 5}\r\n\
--XyZ\r\nContent-Disposition: form-data; name=\"payload\"; filename=\"in.bin\"\r\n\
Content-Type: application/octet-stream\r\n\r\n"
            .to_vec();
    let data: Vec<u8> = (0..=255u8).collect();
    body.extend_from_slice(&data);
    body.extend_from_slice(b"\r\n--XyZ--\r\n");
    let (status, v) = raw(
        &td,
        "/api/services/mp/requests",
        "multipart/form-data; boundary=XyZ",
        body,
        "p/w1",
    )
    .await;
    assert_eq!(status, 201, "{v}");
    let id = v["id"].as_str().unwrap();
    let row = td
        .get_as("p/w1", &format!("/api/service-requests/{id}"))
        .await;
    assert_eq!(row.ok()["request"], json!({"seconds": 5}));
    assert_eq!(row.ok()["payload_size"], 256);
    let file = td.state.join("services/payloads").join(id).join("request");
    assert_eq!(std::fs::read(file).unwrap(), data);
    // An unknown part is refused.
    let (status, v) = raw(
        &td,
        "/api/services/mp/requests",
        "multipart/form-data; boundary=XyZ",
        b"--XyZ\r\nContent-Disposition: form-data; name=\"other\"\r\n\r\nx\r\n--XyZ--\r\n".to_vec(),
        "p/w1",
    )
    .await;
    assert_eq!(status, 400, "{v}");
}

/// A body over the service's limit is refused from its Content-Length,
/// before the body is read: the message names the exact length.
#[tokio::test]
async fn a_body_over_the_limit_is_413_before_it_is_read() {
    let td = setup().await;
    td.post(
        "/api/services",
        with(
            script("tiny", Some("p"), "p-box"),
            json!({"max_payload": "8"}),
        ),
    )
    .await
    .ok();
    let body = json!({"request": {"pad": "x".repeat(400 * 1024)}}).to_string();
    let len = body.len();
    let (status, v) = raw(
        &td,
        "/api/services/tiny/requests",
        "application/json",
        body.into_bytes(),
        "p/w1",
    )
    .await;
    assert_eq!(status, 413, "{v}");
    assert!(
        v["error"]["message"]
            .as_str()
            .unwrap()
            .contains(&format!("the call has {len} bytes")),
        "{v}"
    );
}

/// TER-1264 review finding 3: a reload does not remove a file service
/// with open requests. It stays as removed, takes no new call, and goes
/// at the first reload after its requests end.
#[tokio::test]
async fn a_reload_keeps_a_removed_file_service_while_requests_are_open() {
    let td = setup().await;
    let dir = td.config.join("services");
    std::fs::create_dir_all(&dir).unwrap();
    let file = dir.join("echo.toml");
    let text =
        "kind = \"script\"\ntarget = \"p-box\"\nproject = \"p\"\ncommand = [\"/opt/echo\"]\n";
    std::fs::write(&file, text).unwrap();
    td.post_empty("/api/reload").await.ok();
    let r = td
        .post_as("p/w1", "/api/services/echo/requests", json!({}))
        .await;
    let id = r.ok()["id"].as_str().unwrap().to_owned();
    std::fs::remove_file(&file).unwrap();
    let r = td.post_empty("/api/reload").await;
    let errors = r.ok()["errors"].to_string();
    assert!(errors.contains("kept as removed"), "{errors}");
    let s = td.get("/api/services/echo").await.ok().clone();
    assert!(s["removed_at"].is_string(), "{s}");
    let r = td
        .post_as("p/w1", "/api/services/echo/requests", json!({}))
        .await;
    assert_eq!(r.status, 409, "{}", r.body);
    // The file comes back: active again.
    std::fs::write(&file, text).unwrap();
    td.post_empty("/api/reload").await.ok();
    let s = td.get("/api/services/echo").await.ok().clone();
    assert!(s["removed_at"].is_null(), "{s}");
    // Gone again, and the request ends: the next reload removes it.
    std::fs::remove_file(&file).unwrap();
    td.post_empty("/api/reload").await.ok();
    td.post_as(
        "p/w1",
        &format!("/api/service-requests/{id}/cancel"),
        json!({}),
    )
    .await
    .ok();
    td.post_empty("/api/reload").await.ok();
    td.get("/api/services/echo").await.err(404, "not_found");
    // The request stays readable.
    td.get_as("p/w1", &format!("/api/service-requests/{id}"))
        .await
        .ok();
}
