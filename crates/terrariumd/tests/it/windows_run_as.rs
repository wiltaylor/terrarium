//! Linux-only contract tests. No Windows logon implementation is needed.
#![allow(clippy::unwrap_used, clippy::expect_used)]

use crate::common::TestDaemon;
use futures_util::{SinkExt, StreamExt};
use serde_json::json;
use std::sync::{Arc, Mutex};
use std::time::Duration;
use base64::Engine;
use tracing_subscriber::util::SubscriberInitExt;
use terrarium_host::protocol::{DaemonFrame, HostFrame};
use tokio_tungstenite::tungstenite::{Message, client::IntoClientRequest};

const PASSWORD: &str = "win-session-unique-K9r3Z8p2Q6m4";
const ROTATED: &str = "win-rotated-unique-A7b3D9e5F2g8";

#[derive(Clone, Default)]
struct Trace(Arc<Mutex<Vec<u8>>>);

fn trace_capture() -> Trace {
    static TRACE: std::sync::OnceLock<Trace> = std::sync::OnceLock::new();
    TRACE.get_or_init(Trace::default).clone()
}

// Isolate the global subscriber and log bridge from the other integration
// tests' subscribers. Each child uses production logging for every spawned
// server, HTTP client, and WebSocket task, with deliberately hostile verbosity.
async fn trace_child(name: &str) -> bool {
    if std::env::var("TER968_TRACE_CHILD").as_deref() == Ok(name) {
        let writer = trace_capture();
        terrarium_host::logging::subscriber(move || writer.clone(), false).init();
        return false;
    }
    let output = tokio::process::Command::new(std::env::current_exe().unwrap())
        .args(["--exact", &format!("windows_run_as::{name}"), "--nocapture"])
        .env("TER968_TRACE_CHILD", name)
        .env("RUST_LOG", "trace,tungstenite::protocol=trace,tungstenite::protocol::frame=trace,hyper::proto=trace,axum::extract::ws=trace")
        .output().await.unwrap();
    assert!(output.status.success(), "TRACE child failed: {}", String::from_utf8_lossy(&output.stdout));
    true
}

fn password_representations(password: &str) -> Vec<Vec<u8>> {
    let lower = password.as_bytes().iter().map(|b| format!("{b:02x}")).collect::<String>();
    let upper = lower.to_uppercase();
    let spaced_lower = password.as_bytes().iter().map(|b| format!("{b:02x}")).collect::<Vec<_>>().join(" ");
    let spaced_upper = spaced_lower.to_uppercase();
    let base64 = base64::engine::general_purpose::STANDARD.encode(password);
    [password.to_owned(), lower, upper, spaced_lower, spaced_upper, base64]
        .into_iter().map(String::into_bytes).collect()
}

fn assert_no_password(bytes: &[u8]) {
    for password in [PASSWORD, ROTATED] {
        for representation in password_representations(password) {
            assert!(!bytes.windows(representation.len()).any(|w| w == representation),
                "password or reversible representation leaked");
        }
    }
}

impl std::io::Write for Trace {
    fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
        self.0.lock().unwrap().extend_from_slice(bytes);
        Ok(bytes.len())
    }
    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

struct Lab {
    td: TestDaemon,
    frames: tokio::sync::mpsc::UnboundedReceiver<DaemonFrame>,
    host: tokio::task::JoinHandle<()>,
    server: tokio::task::JoinHandle<()>,
    trace: Trace,
    captures: Vec<String>,
}

impl Lab {
    async fn new(os: &str, capability: bool) -> Self {
        let trace = trace_capture();
        let td = TestDaemon::start().await;
        let state = td.daemon.as_ref().unwrap().state.clone();
        let (server, addr) = terrariumd::hosts::serve(state, "127.0.0.1:0")
            .await
            .unwrap();
        let registered = td.post("/api/hosts", json!({"name": "win"})).await;
        registered.ok();
        let token = registered.body["token"].as_str().unwrap();
        let mut request = format!("ws://{addr}/hosts/connect")
            .into_client_request()
            .unwrap();
        request
            .headers_mut()
            .insert("authorization", format!("Bearer {token}").parse().unwrap());
        let (mut socket, _) = tokio_tungstenite::connect_async(request).await.unwrap();
        socket
            .send(Message::Text(
                json!({
                    "type": "hello", "protocol": 1, "host": "win", "version": "test",
                    "os": os, "arch": "x86_64", "max_sessions": 8,
                    "capabilities": {"windows_run_as": capability, "no_workspaces": true}
                })
                .to_string()
                .into(),
            ))
            .await
            .unwrap();
        let welcome = tokio::time::timeout(Duration::from_secs(30), socket.next())
            .await
            .unwrap()
            .unwrap()
            .unwrap();
        assert!(matches!(welcome, Message::Text(_)));
        let (tx, frames) = tokio::sync::mpsc::unbounded_channel();
        let host = tokio::spawn(async move {
            while let Some(Ok(message)) = socket.next().await {
                let Message::Text(text) = message else {
                    continue;
                };
                let frame: DaemonFrame = serde_json::from_str(&text).unwrap();
                if let DaemonFrame::Ping { nonce } = &frame {
                    socket
                        .send(Message::Text(
                            serde_json::to_string(&HostFrame::Pong { nonce: *nonce })
                                .unwrap()
                                .into(),
                        ))
                        .await
                        .unwrap();
                }
                let session = match &frame {
                    DaemonFrame::Exec(req) => Some(req.session),
                    DaemonFrame::CpPush(req) => Some(req.session),
                    DaemonFrame::CpPull(req) => Some(req.session),
                    _ => None,
                };
                if let Some(session) = session {
                    let started = json!({"type": "started", "session": session, "pid": 42});
                    socket
                        .send(Message::Text(started.to_string().into()))
                        .await
                        .unwrap();
                    // A logon failure must use a static message, never the credentials.
                    let ended = json!({"type": "error", "session": session,
                            "code": "bad_request", "message": "logon refused"});
                    socket
                        .send(Message::Text(ended.to_string().into()))
                        .await
                        .unwrap();
                }
                tx.send(frame).unwrap();
            }
        });
        tokio::time::timeout(Duration::from_secs(30), async {
            loop {
                let state = td.daemon.as_ref().unwrap().state.clone();
                if !state.executor.resource_pending("win") {
                    break;
                }
                tokio::time::sleep(Duration::from_millis(20)).await;
            }
        })
        .await
        .unwrap();
        Self {
            td,
            frames,
            host,
            server,
            trace,
            captures: vec![],
        }
    }

    async fn secret(&mut self, value: &str) {
        let mut body = json!({"kind": "login",
            "fields": {"username": "ignored", "password": value}});
        if value != ROTATED {
            body["name"] = json!("win-login");
        }
        let r = self
            .td
            .req(
                if value == ROTATED {
                    hyper::Method::PUT
                } else {
                    hyper::Method::POST
                },
                if value == ROTATED {
                    "/api/secrets/global/win-login"
                } else {
                    "/api/secrets"
                },
                Some(body),
                None,
            )
            .await;
        r.ok();
        self.captures.push(r.body.to_string());
    }

    async fn target(&mut self, secret: &str) {
        let r = self
            .td
            .post(
                "/api/targets",
                json!({"name": "box", "kind": "host", "host": "win",
            "windows_run_as": {"account": "DOMAIN\\worker", "password_secret": secret}}),
            )
            .await;
        r.ok();
        self.captures.push(r.body.to_string());
    }

    async fn exec(&mut self) -> crate::common::Resp {
        let r = self
            .td
            .post("/api/targets/box/exec", json!({"cmd": ["whoami"]}))
            .await;
        self.captures.push(r.body.to_string());
        r
    }

    async fn next_exec(&mut self) -> DaemonFrame {
        tokio::time::timeout(Duration::from_secs(30), async {
            loop {
                let frame = self.frames.recv().await.unwrap();
                if matches!(
                    frame,
                    DaemonFrame::Exec(_) | DaemonFrame::CpPush(_) | DaemonFrame::CpPull(_)
                ) {
                    return frame;
                }
            }
        })
        .await
        .unwrap()
    }

    async fn scan(&mut self) {
        for path in [
            "/api/targets/box",
            "/api/hosts/win",
            "/api/audit",
            "/api/events",
        ] {
            self.captures.push(self.td.get(path).await.body.to_string());
        }
        self.captures
            .push(String::from_utf8_lossy(&self.trace.0.lock().unwrap()).into_owned());
        assert!(self.captures.last().unwrap().contains("host transport frame send"),
            "TRACE capture did not observe the actual transport task");
        for capture in &self.captures {
            assert_no_password(capture.as_bytes());
        }
        // Includes exec replay records, logs and any transcripts, not just API views.
        scan_files(&self.td.state);
    }
}

impl Drop for Lab {
    fn drop(&mut self) {
        self.host.abort();
        self.server.abort();
    }
}

fn scan_files(path: &std::path::Path) {
    if path.is_dir() {
        for entry in std::fs::read_dir(path).unwrap() {
            scan_files(&entry.unwrap().path());
        }
    } else if path.is_file() {
        let bytes = std::fs::read(path).unwrap();
        assert_no_password(&bytes);
    }
}

#[tokio::test]
async fn password_reaches_only_the_session_start_and_is_resolved_again_after_rotation() {
    if trace_child("password_reaches_only_the_session_start_and_is_resolved_again_after_rotation").await { return }
    let mut lab = Lab::new("windows", true).await;
    lab.secret(PASSWORD).await;
    lab.target("win-login").await;
    for password in [PASSWORD, ROTATED] {
        if password == ROTATED {
            lab.secret(ROTATED).await;
        }
        let response = lab.exec().await;
        response.ok();
        let sid = response.body["session_id"].as_str().unwrap();
        let frame = lab.next_exec().await;
        lab.captures.push(format!("{frame:?}"));
        let DaemonFrame::Exec(req) = frame else {
            unreachable!()
        };
        let credentials = req.windows_run_as.unwrap();
        assert_eq!(credentials.account, "DOMAIN\\worker");
        assert!(credentials.password == password, "wrong session password");
        // The error from the fake host is observable, without credentials.
        let output = lab
            .td
            .get(&format!("/api/targets/box/sessions/{sid}/output?wait=5"))
            .await;
        output.ok();
        lab.captures.push(output.body.to_string());
    }
    lab.scan().await;
}

#[tokio::test]
async fn transfer_sessions_receive_the_same_one_session_credentials() {
    if trace_child("transfer_sessions_receive_the_same_one_session_credentials").await { return }
    let mut lab = Lab::new("windows", true).await;
    lab.secret(PASSWORD).await;
    lab.target("win-login").await;
    for direction in ["push", "pull"] {
        let response = lab
            .td
            .post(
                "/api/targets/box/cp",
                json!({"direction": direction, "path": "/test-file"}),
            )
            .await;
        response.ok();
        lab.captures.push(response.body.to_string());
        let frame = lab.next_exec().await;
        lab.captures.push(format!("{frame:?}"));
        let credentials = match frame {
            DaemonFrame::CpPush(req) => req.windows_run_as,
            DaemonFrame::CpPull(req) => req.windows_run_as,
            _ => panic!("expected a transfer session"),
        }
        .unwrap();
        assert!(credentials.password == PASSWORD);
        assert_eq!(credentials.account, "DOMAIN\\worker");
    }
    lab.scan().await;
}

#[tokio::test]
async fn unsupported_hosts_never_receive_a_password_even_if_they_claim_the_capability() {
    if trace_child("unsupported_hosts_never_receive_a_password_even_if_they_claim_the_capability").await { return }
    for (os, capability) in [("linux", true), ("windows", false), ("linux", false)] {
        let mut lab = Lab::new(os, capability).await;
        lab.secret(PASSWORD).await;
        lab.target("win-login").await;
        let response = lab.exec().await;
        assert_eq!(response.status, 422);
        assert!(
            response
                .body
                .to_string()
                .contains("windows_run_as capability")
        );
        assert!(
            tokio::time::timeout(Duration::from_millis(100), lab.next_exec())
                .await
                .is_err()
        );
        lab.scan().await;
    }
}

#[tokio::test]
async fn missing_and_ungranted_passwords_refuse_before_sending() {
    if trace_child("missing_and_ungranted_passwords_refuse_before_sending").await { return }
    let mut lab = Lab::new("windows", true).await;
    lab.target("win-login").await;
    assert_eq!(lab.exec().await.status, 422);
    lab.secret(PASSWORD).await;
    lab.td.project("p").await;
    let state = lab.td.daemon.as_ref().unwrap().state.clone();
    // Model a project-owned definition without an admin's authorization.
    state
        .db
        .write("operator", |t| {
            let mut def = terrariumd::store::targets::get(t, "box")?.def;
            def.project = Some("p".into());
            t.execute(
                "UPDATE targets SET def = ?1, credential_admin = 0 WHERE name = 'box'",
                [serde_json::to_string(&def)?],
            )?;
            Ok(())
        })
        .await
        .unwrap();
    assert_eq!(lab.exec().await.status, 403);
    assert!(
        tokio::time::timeout(Duration::from_millis(100), lab.next_exec())
            .await
            .is_err()
    );
    lab.scan().await;
}

#[test]
fn target_validation_accepts_only_host_accounts_with_both_fields() {
    for account in ["worker", "DOMAIN\\worker"] {
        let def = serde_json::from_value(json!({"name": "box", "kind": "host", "host": "win",
            "windows_run_as": {"account": account, "password_secret": "login"}}))
        .unwrap();
        terrariumd::store::targets::validate(&def).unwrap();
    }
    for account in ["", "\\worker", "DOMAIN\\", "a\\b\\c", "a\nb"] {
        let def = serde_json::from_value(json!({"name": "box", "kind": "host", "host": "win",
            "windows_run_as": {"account": account, "password_secret": "login"}}))
        .unwrap();
        assert!(terrariumd::store::targets::validate(&def).is_err());
    }
    let def = serde_json::from_value(
        json!({"name": "box", "kind": "docker", "host": "win", "image": "alpine",
        "windows_run_as": {"account": "worker", "password_secret": "login"}}),
    )
    .unwrap();
    assert!(terrariumd::store::targets::validate(&def).is_err());
    assert!(
        serde_json::from_value::<terrarium_core::config::TargetDef>(json!({"kind": "host",
        "windows_run_as": {"account": "worker"}}))
        .is_err()
    );
}
