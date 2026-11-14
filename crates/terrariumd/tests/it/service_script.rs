//! Services C3 (TER-1266, design D4): the run of a script service request.
//! A real `terrarium_host::Client` runs the scripts on this machine; the
//! tests move a request to `running` as C2 does, then run a pass of
//! `terrariumd::service_script`.
#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::os::unix::fs::PermissionsExt;
use std::time::Duration;

use base64::Engine;
use serde_json::{Value, json};
use terrarium_core::protocol::HostCapabilities;
use terrarium_core::service::State;
use terrarium_host::{Client, ClientOptions};

use crate::common::TestDaemon;

/// The value of the global secret `tt`.
const TOKEN: &str = "sV3q8LmZ1xTr4KwB7nPdQs5Hz";

struct Lab {
    td: TestDaemon,
    scripts: tempfile::TempDir,
}

/// A daemon with host `lab1` online, the global host target `box`, the
/// project target `p-box` on it, and the global secret `tt`.
async fn lab() -> Lab {
    let td = TestDaemon::start().await;
    let state = td.daemon.as_ref().unwrap().state.clone();
    let (_task, addr) = terrariumd::hosts::serve(state, "127.0.0.1:0")
        .await
        .unwrap();
    let r = td.post("/api/hosts", json!({"name": "lab1"})).await;
    assert_eq!(r.status, 201, "{}", r.body);
    let token = r.body["token"].as_str().unwrap().to_owned();
    let mut o = ClientOptions::new(format!("ws://{addr}/hosts/connect"), &token, "lab1");
    o.capabilities = HostCapabilities {
        shell: Some("/bin/sh".into()),
        ..Default::default()
    };
    o.backoff_min = Duration::from_millis(50);
    o.backoff_max = Duration::from_millis(200);
    let scripts = tempfile::tempdir().unwrap();
    o.paths = terrarium_host::HostPaths {
        work_dir: td.state.with_file_name("work"),
        instances_dir: scripts.path().join("instances"),
        repos_dir: scripts.path().join("repos"),
    };
    tokio::spawn(Client::new(o).run());
    for _ in 0..300 {
        if td.get("/api/hosts/lab1").await.body["online"] == true {
            break;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    td.project("p").await;
    for t in [
        json!({"name": "box", "kind": "host", "host": "lab1"}),
        json!({"name": "p-box", "kind": "host", "host": "lab1", "project": "p"}),
    ] {
        let r = td.post("/api/targets", t.clone()).await;
        assert_eq!(r.status, 201, "{t}: {}", r.body);
    }
    td.put(
        "/api/secrets/global/tt",
        json!({"kind": "token", "fields": {"value": TOKEN}}),
    )
    .await
    .ok();
    terrariumd::store::services::override_free_space(Some((900 << 30, 1000 << 30)));
    Lab { td, scripts }
}

impl Lab {
    fn state(&self) -> terrariumd::api::Shared {
        self.td.daemon.as_ref().unwrap().state.clone()
    }

    /// A script in the scripts dir, mode 0755.
    fn script(&self, name: &str, body: &str) -> String {
        let p = self.scripts.path().join(name);
        std::fs::write(&p, format!("#!/bin/sh\n{body}\n")).unwrap();
        std::fs::set_permissions(&p, std::fs::Permissions::from_mode(0o755)).unwrap();
        p.display().to_string()
    }

    fn mark(&self, name: &str) -> String {
        self.scripts.path().join(name).display().to_string()
    }

    async fn service(&self, def: Value) {
        let r = self.td.post("/api/services", def).await;
        assert_eq!(r.status, 201, "{}", r.body);
    }

    /// Call `service` as the operator and move the request to `running`.
    async fn call_running(&self, service: &str, request: Value, payload: Option<&[u8]>) -> String {
        let mut body = json!({"request": request});
        if let Some(p) = payload {
            body["payload_base64"] = base64::engine::general_purpose::STANDARD.encode(p).into();
        }
        let r = self
            .td
            .post(&format!("/api/services/{service}/requests"), body)
            .await;
        assert_eq!(r.status, 201, "{}", r.body);
        let id = r.body["id"].as_str().unwrap().to_owned();
        for (from, to) in [
            (State::Queued, State::Waiting),
            (State::Waiting, State::Running),
        ] {
            let id = id.clone();
            self.state()
                .db
                .write("system", move |t| {
                    terrariumd::store::services::transition(t, &id, from, to, Default::default())
                })
                .await
                .unwrap();
        }
        id
    }

    async fn request(&self, id: &str) -> Value {
        let r = self.td.get(&format!("/api/service-requests/{id}")).await;
        assert_eq!(r.status, 200, "{}", r.body);
        r.body
    }

    /// Wait until request `id` has ended.
    async fn ended(&self, id: &str) -> Value {
        for _ in 0..1500 {
            let r = self.request(id).await;
            let st = State::parse(r["state"].as_str().unwrap()).unwrap();
            if st.is_final() && !terrariumd::service_script::is_live(id) {
                return r;
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
        panic!("request {id} did not end: {}", self.request(id).await);
    }
}

async fn wait_file(p: &str) -> String {
    for _ in 0..1500 {
        if let Ok(s) = std::fs::read_to_string(p)
            && s.ends_with('\n')
        {
            return s.trim().to_owned();
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    panic!("no {p}");
}

#[tokio::test]
async fn a_script_runs_with_its_files_and_returns_its_output() {
    let lab = lab().await;
    let seen = lab.mark("seen");
    let script = lab.script(
        "run",
        &format!(
            "set -e\n\
             [ \"$PWD\" = \"$TERRARIUM_SERVICE_DIR\" ]\n\
             [ \"$HF_TOKEN\" = \"{TOKEN}\" ]\n\
             cp \"$TERRARIUM_SERVICE_PAYLOAD\" \"$TERRARIUM_SERVICE_OUTPUT/copy.bin\"\n\
             mkdir \"$TERRARIUM_SERVICE_OUTPUT/sub\"\n\
             printf 'id=%s' \"$TERRARIUM_SERVICE_REQUEST_ID\" > \"$TERRARIUM_SERVICE_OUTPUT/sub/n.txt\"\n\
             ln -s /etc/passwd \"$TERRARIUM_SERVICE_OUTPUT/link\"\n\
             printf '{{\"ok\":true,\"request\":%s}}' \"$(cat \"$TERRARIUM_SERVICE_REQUEST\")\" > \"$TERRARIUM_SERVICE_RESPONSE\"\n\
             echo \"$TERRARIUM_SERVICE_DIR $(stat -c %a \"$TERRARIUM_SERVICE_DIR\")\" > {seen}"
        ),
    );
    lab.service(json!({"name": "echo", "kind": "script", "target": "box",
                       "command": [script], "env_secrets": {"HF_TOKEN": "tt"}}))
        .await;
    let payload = b"payload \x00\x01 bytes";
    let id = lab
        .call_running("echo", json!({"frames": 3}), Some(payload))
        .await;
    let state = lab.state();
    let t = terrariumd::service_script::tick(&state).await.unwrap();
    assert_eq!(t.started, vec![id.clone()]);
    // A second pass does not start it again.
    let t = terrariumd::service_script::tick(&state).await.unwrap();
    assert!(t.started.is_empty() && t.lost.is_empty(), "{t:?}");
    let r = lab.ended(&id).await;
    assert_eq!(r["state"], "done", "{r}");
    assert_eq!(r["exit_code"], 0, "{r}");
    assert_eq!(
        r["response"],
        json!({"ok": true, "request": {"frames": 3}}),
        "{r}"
    );
    let seen = wait_file(&seen).await;
    let (dir, mode) = seen.split_once(' ').unwrap();
    assert!(dir.contains(&format!("/terrarium-service-{id}-")), "{dir}");
    assert_eq!(mode, "700");
    // The directory on the target is gone.
    assert!(!std::path::Path::new(dir).exists(), "{dir}");
    // The response files: regular files and dirs; the link is left out.
    let resp = terrariumd::service_script::response_dir(&state.state_dir, &id);
    assert_eq!(std::fs::read(resp.join("copy.bin")).unwrap(), payload);
    assert_eq!(
        std::fs::read_to_string(resp.join("sub/n.txt")).unwrap(),
        format!("id={id}")
    );
    assert!(std::fs::symlink_metadata(resp.join("link")).is_err());
    // The response bytes count toward the caps with the payload.
    let n = state
        .db
        .read(|c| terrariumd::store::services::retained_bytes(c, Some("echo")))
        .await
        .unwrap();
    let files = (payload.len() + format!("id={id}").len()) as u64;
    assert_eq!(n, payload.len() as u64 + files);
    // The audit row has no secret.
    let audit = lab.td.get("/api/audit?action=service.run").await;
    let text = audit.body.to_string();
    assert!(text.contains(&id), "{text}");
    assert!(!text.contains(TOKEN), "{text}");
}

#[tokio::test]
async fn a_failing_script_fails_with_a_redacted_log_tail() {
    let lab = lab().await;
    let script = lab.script(
        "fail",
        "echo \"starting with $HF_TOKEN\"\necho boom >&2\nexit 3",
    );
    lab.service(json!({"name": "fail", "kind": "script", "target": "box",
                       "command": [script], "env_secrets": {"HF_TOKEN": "tt"}}))
        .await;
    let id = lab.call_running("fail", json!({}), None).await;
    terrariumd::service_script::tick(&lab.state())
        .await
        .unwrap();
    let r = lab.ended(&id).await;
    assert_eq!(r["state"], "failed", "{r}");
    assert_eq!(r["exit_code"], 3, "{r}");
    let e = r["error"].as_str().unwrap();
    assert!(e.contains("exited with code 3"), "{e}");
    assert!(e.contains("boom"), "{e}");
    assert!(e.contains("starting with [secret:"), "{e}");
    assert!(!e.contains(TOKEN), "{e}");
    // No response files for a failed run.
    let resp = terrariumd::service_script::response_dir(&lab.state().state_dir, &id);
    assert!(!resp.exists());
}

#[tokio::test]
async fn a_cancel_kills_the_run_and_removes_its_dir() {
    let lab = lab().await;
    let pid = lab.mark("pid");
    let script = lab.script(
        "slow",
        &format!("echo \"$$ $TERRARIUM_SERVICE_DIR\" > {pid}\nexec sleep 60"),
    );
    lab.service(json!({"name": "slow", "kind": "script", "target": "box", "command": [script]}))
        .await;
    let id = lab.call_running("slow", json!({}), None).await;
    terrariumd::service_script::tick(&lab.state())
        .await
        .unwrap();
    let line = wait_file(&pid).await;
    let (pid, dir) = line.split_once(' ').unwrap();
    let r = lab
        .td
        .post_empty(&format!("/api/service-requests/{id}/cancel"))
        .await;
    assert_eq!(r.status, 200, "{}", r.body);
    let r = lab.ended(&id).await;
    assert_eq!(r["state"], "cancelled", "{r}");
    crate::host_restart::wait_pid_gone(pid).await;
    assert!(!std::path::Path::new(dir).exists(), "{dir}");
}

#[tokio::test]
async fn a_run_lost_in_a_restart_fails_and_is_not_run_again() {
    let lab = lab().await;
    let ran = lab.mark("ran");
    let script = lab.script("once", &format!("echo ran >> {ran}"));
    lab.service(json!({"name": "once", "kind": "script", "target": "box", "command": [script]}))
        .await;
    let id = lab.call_running("once", json!({}), None).await;
    // As a daemon that died after it made the run directory.
    let (i, d) = (id.clone(), format!("/tmp/terrarium-service-{id}-gone"));
    lab.state()
        .db
        .write("system", move |t| {
            t.execute(
                "UPDATE service_requests SET script_dir = ?2 WHERE id = ?1",
                [&i, &d],
            )?;
            Ok(())
        })
        .await
        .unwrap();
    let t = terrariumd::service_script::tick(&lab.state())
        .await
        .unwrap();
    assert_eq!(t.lost, vec![id.clone()]);
    assert!(t.started.is_empty());
    let r = lab.ended(&id).await;
    assert_eq!(r["state"], "failed", "{r}");
    assert!(r["error"].as_str().unwrap().contains("restarted"), "{r}");
    assert!(!std::path::Path::new(&ran).exists());
}

#[tokio::test]
async fn a_project_service_needs_a_grant_for_a_global_secret() {
    let lab = lab().await;
    let ran = lab.mark("ran");
    let script = lab.script("p", &format!("echo ran >> {ran}"));
    lab.service(
        json!({"name": "psvc", "kind": "script", "target": "p-box", "project": "p",
                       "command": [script], "env_secrets": {"HF_TOKEN": "tt"}}),
    )
    .await;
    let id = lab.call_running("psvc", json!({}), None).await;
    terrariumd::service_script::tick(&lab.state())
        .await
        .unwrap();
    let r = lab.ended(&id).await;
    assert_eq!(r["state"], "failed", "{r}");
    assert!(r["error"].as_str().unwrap().contains("no grant"), "{r}");
    assert!(!r.to_string().contains(TOKEN));
    assert!(!std::path::Path::new(&ran).exists());
}
