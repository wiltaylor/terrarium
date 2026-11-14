//! Services C1 (TER-1264): the definition rules, the rights, the request
//! queue and its states, the payload store, the files, and the wait limit.
#![allow(clippy::unwrap_used, clippy::expect_used)]

use crate::common;

use common::TestDaemon;
use serde_json::{Value, json};

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
        json!({"name": "p-gpu", "kind": "docker", "project": "p", "host": "helios", "image": "alpine"}),
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
    // Plenty of free space, whatever the disk of this machine has (the
    // 20% floor has its unit test in `store::services`).
    terrariumd::store::services::override_free_space(Some((900 << 30, 1000 << 30)));
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
    // Past the retention, an ended request's payload goes.
    let much_later = chrono::Utc::now() + chrono::Duration::days(8);
    terrariumd::service_sweep::sweep(&state, much_later)
        .await
        .unwrap();
}
