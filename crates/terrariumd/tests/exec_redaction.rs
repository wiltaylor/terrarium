//! The redaction of exec output (TER-33, PLAN §16b.6): a real
//! `terrarium_host::Client` runs the commands on this machine, and the
//! output of an exec session is redacted as a stream for the caller, over
//! `/output` and the WebSocket. A file copy is not changed.
#![allow(clippy::unwrap_used, clippy::expect_used)]

use crate::common;

use std::os::unix::fs::PermissionsExt;
use std::time::Duration;

use base64::Engine;
use common::TestDaemon;
use serde_json::{Value, json};
use terrarium_core::protocol::HostCapabilities;
use terrarium_host::{Client, ClientOptions};

const B64: base64::engine::GeneralPurpose = base64::engine::general_purpose::STANDARD;
/// The global secret `gh`.
const GLOBAL: &str = "ghp_R4nd0mT0k3nV4lu3Xy9Qz7Lm2Pd";
/// The secret `deploy` of project `p`.
const PROJECT: &str = "dpl_8sK2mQ7vX4tR9wL3nB6yH1cJ5fZ0";

struct Lab {
    td: TestDaemon,
    scripts: tempfile::TempDir,
    w1: String,
}

/// A daemon with the host agent `lab1` online, the host target `box`, the
/// global secret `gh`, the secret `deploy` of `p`, and agent `p/w1` with
/// `box` in its policy.
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
    td.post(
        "/api/targets",
        json!({"name": "box", "kind": "host", "host": "lab1"}),
    )
    .await
    .ok();
    td.project("p").await;
    let w1 = td
        .agent("p", "w1", json!({"policy": {"targets": ["box"]}}))
        .await["id"]
        .as_str()
        .unwrap()
        .to_owned();
    td.post(
        "/api/secrets",
        json!({"name": "gh", "kind": "token", "fields": {"value": GLOBAL}}),
    )
    .await
    .ok();
    td.post(
        "/api/secrets",
        json!({"scope": "p", "name": "deploy", "kind": "api_key", "fields": {"value": PROJECT}}),
    )
    .await
    .ok();
    Lab { td, scripts, w1 }
}

impl Lab {
    /// A script in the scripts dir, mode 0755.
    fn script(&self, name: &str, body: &str) -> String {
        let p = self.scripts.path().join(name);
        std::fs::write(&p, format!("#!/bin/sh\n{body}\n")).unwrap();
        std::fs::set_permissions(&p, std::fs::Permissions::from_mode(0o755)).unwrap();
        p.display().to_string()
    }

    /// Start `cmd` on `box` as `agent` (`None`: the operator).
    async fn exec(&self, agent: Option<&str>, body: Value) -> String {
        let r = self
            .td
            .req(
                hyper::Method::POST,
                "/api/targets/box/exec",
                Some(body),
                agent,
            )
            .await;
        assert_eq!(r.status, 201, "{}", r.body);
        r.body["session_id"].as_str().unwrap().to_owned()
    }
}

/// Read session `sid` to its end as `agent`: stdout, stderr.
async fn drain(td: &TestDaemon, agent: Option<&str>, sid: &str) -> (String, String) {
    let (mut out, mut err) = (Vec::new(), Vec::new());
    let mut since = 0u64;
    for _ in 0..300 {
        let path = format!("/api/targets/box/sessions/{sid}/output?since={since}&wait=5");
        let r = td.req(hyper::Method::GET, &path, None, agent).await;
        let o = r.ok().clone();
        for c in o["chunks"].as_array().unwrap() {
            let bytes = B64.decode(c["data"].as_str().unwrap()).unwrap();
            match c["stream"].as_str().unwrap() {
                "stderr" => err.extend(bytes),
                _ => out.extend(bytes),
            }
        }
        since = o["next"].as_u64().unwrap();
        if o["done"] == true {
            return (
                String::from_utf8_lossy(&out).into_owned(),
                String::from_utf8_lossy(&err).into_owned(),
            );
        }
    }
    panic!("session {sid} never ended");
}

/// A script that prints a secret in two writes (a pause between them, so
/// they arrive as two output chunks).
fn split_print(var: &str, value: &str) -> String {
    let (a, b) = value.split_at(value.len() / 2);
    format!("printf '{var}={a}'; sleep 0.1; printf '{b}\\n'; printf 'err {value}\\n' >&2")
}

#[tokio::test]
async fn agent_exec_overrides_git_identity_and_supplies_provenance() {
    let lab = lab().await;
    let sid = lab.exec(Some(&lab.w1), json!({
        "cmd": ["sh", "-c", "printf '%s\\n' \"$GIT_AUTHOR_NAME\" \"$GIT_AUTHOR_EMAIL\" \"$GIT_COMMITTER_NAME\" \"$GIT_COMMITTER_EMAIL\"; printf '%s' \"$TERRARIUM_GIT_TRAILERS\"; test -z \"${TERRARIUM_GIT_TRAILERS_FILE-}\""],
        "env": {"GIT_AUTHOR_NAME": "wrong", "GIT_AUTHOR_EMAIL": "wrong", "GIT_COMMITTER_NAME": "wrong", "GIT_COMMITTER_EMAIL": "wrong", "TERRARIUM_GIT_TRAILERS_FILE": "/wrong"}
    })).await;
    let (out, err) = drain(&lab.td, Some(&lab.w1), &sid).await;
    assert!(err.is_empty(), "{err}");
    assert!(
        out.starts_with("Terrarium\nterrarium@localhost\nTerrarium\nterrarium@localhost\n"),
        "{out}"
    );
    assert!(out.contains("Terrarium-Agent: p/w1"), "{out}");
    assert!(!out.contains("wrong"), "{out}");
}

/// TER-33: a secret split across two output chunks is redacted, for the
/// caller's view: the agent sees its project's name and `[secret]` for a
/// global secret; the operator sees each name.
#[tokio::test]
async fn exec_output_is_redacted_across_chunks_for_the_caller() {
    let lab = lab().await;
    let s = lab.script(
        "leak.sh",
        &format!(
            "{}\n{}",
            split_print("deploy", PROJECT),
            split_print("gh", GLOBAL)
        ),
    );
    let sid = lab.exec(Some(&lab.w1), json!({"cmd": [s]})).await;
    let (out, err) = drain(&lab.td, Some(&lab.w1), &sid).await;
    assert!(
        !out.contains(PROJECT) && !err.contains(PROJECT),
        "{out} {err}"
    );
    assert!(
        !out.contains(GLOBAL) && !err.contains(GLOBAL),
        "{out} {err}"
    );
    assert!(!out.contains(&PROJECT[..10]), "{out}");
    assert_eq!(out, "deploy=[secret:p/deploy]\ngh=[secret]\n", "{out}");
    assert_eq!(err, "err [secret:p/deploy]\nerr [secret]\n", "{err}");

    let sid = lab.exec(None, json!({"cmd": [s]})).await;
    let (out, _) = drain(&lab.td, None, &sid).await;
    assert_eq!(out, "deploy=[secret:p/deploy]\ngh=[secret:gh]\n", "{out}");
}

/// The WebSocket of a session reads the same redacted bytes.
#[tokio::test]
async fn the_websocket_stream_is_redacted() {
    use futures_util::StreamExt;
    use tokio_tungstenite::tungstenite::Message;
    let lab = lab().await;
    let s = lab.script(
        "ws.sh",
        &format!("sleep 0.5; {}", split_print("gh", GLOBAL)),
    );
    let sid = lab.exec(None, json!({"cmd": [s]})).await;
    let mut ws = lab
        .td
        .ws(&format!("/api/targets/box/sessions/{sid}/ws"), None)
        .await
        .unwrap();
    let mut seen = Vec::new();
    while let Ok(Some(Ok(m))) = tokio::time::timeout(Duration::from_secs(5), ws.next()).await {
        match m {
            Message::Binary(b) => seen.extend_from_slice(&b),
            Message::Close(_) => break,
            _ => {}
        }
        let text = String::from_utf8_lossy(&seen);
        if text.contains("err ") && text.contains('\n') && text.matches('\n').count() >= 2 {
            break;
        }
    }
    let text = String::from_utf8_lossy(&seen).into_owned();
    assert!(!text.contains(GLOBAL), "{text}");
    assert!(text.contains("gh=[secret:gh]"), "{text}");
}

/// The held end of the output goes out after a short idle time: a line
/// with no newline shows while the command still runs.
#[tokio::test]
async fn held_output_shows_while_the_command_runs() {
    let lab = lab().await;
    let s = lab.script("prompt.sh", "printf 'Password: '; sleep 3; echo done");
    let sid = lab.exec(None, json!({"cmd": [s]})).await;
    let path = format!("/api/targets/box/sessions/{sid}/output?since=0&wait=2");
    let r = lab.td.get(&path).await;
    let o = r.ok().clone();
    assert_eq!(o["done"], false, "{o}");
    let text: String = o["chunks"]
        .as_array()
        .unwrap()
        .iter()
        .map(|c| {
            String::from_utf8_lossy(&B64.decode(c["data"].as_str().unwrap()).unwrap()).into_owned()
        })
        .collect();
    assert_eq!(text, "Password: ", "{o}");
    let (rest, _) = drain(&lab.td, None, &sid).await;
    assert!(rest.ends_with("done\n"), "{rest}");
}

/// A file copy is not redacted: the bytes and the checksum stay.
#[tokio::test]
async fn a_file_pull_is_not_changed() {
    let lab = lab().await;
    let file = lab.scripts.path().join("keys.txt");
    std::fs::write(&file, format!("token={GLOBAL}\n")).unwrap();
    let r = lab
        .td
        .post(
            "/api/targets/box/cp",
            json!({"direction": "pull", "path": file.display().to_string()}),
        )
        .await;
    assert_eq!(r.status, 201, "{}", r.body);
    let sid = r.body["session_id"].as_str().unwrap().to_owned();
    let mut bytes = Vec::new();
    let mut since = 0u64;
    let mut end = Value::Null;
    for _ in 0..100 {
        let path = format!("/api/targets/box/sessions/{sid}/output?since={since}&wait=5");
        let o = lab.td.get(&path).await.ok().clone();
        for c in o["chunks"].as_array().unwrap() {
            bytes.extend(B64.decode(c["data"].as_str().unwrap()).unwrap());
        }
        since = o["next"].as_u64().unwrap();
        if o["done"] == true {
            end = o["end"].clone();
            break;
        }
    }
    assert_eq!(String::from_utf8_lossy(&bytes), format!("token={GLOBAL}\n"));
    assert_eq!(end["type"], "cp", "{end}");
}
