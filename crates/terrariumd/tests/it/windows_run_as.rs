//! Linux-only contract tests. No Windows logon implementation is needed.
#![allow(clippy::unwrap_used, clippy::expect_used)]

use crate::common::TestDaemon;
use futures_util::{SinkExt, StreamExt};
use serde_json::json;
use std::sync::{Arc, Mutex};
use std::time::Duration;
use terrarium_host::protocol::{DaemonFrame, HostFrame};
use tokio_tungstenite::tungstenite::{Message, client::IntoClientRequest};

const PASSWORD: &str = "win-session-unique-K9r3Z8p2Q6m4";
const ROTATED: &str = "win-rotated-unique-A7b3D9e5F2g8";

#[derive(Clone, Default)]
struct Trace(Arc<Mutex<Vec<u8>>>);

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
    _guard: tracing::subscriber::DefaultGuard,
    captures: Vec<String>,
}

impl Lab {
    async fn new(os: &str, capability: bool) -> Self {
        let trace = Trace::default();
        let writer = trace.clone();
        let guard = tracing::subscriber::set_default(
            tracing_subscriber::fmt()
                .with_max_level(tracing::Level::DEBUG)
                .with_ansi(false)
                .with_writer(move || writer.clone())
                .finish(),
        );
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
                match &frame {
                    DaemonFrame::Ping { nonce } => {
                        socket
                            .send(Message::Text(
                                serde_json::to_string(&HostFrame::Pong { nonce: *nonce })
                                    .unwrap()
                                    .into(),
                            ))
                            .await
                            .unwrap();
                    }
                    _ => {}
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
            _guard: guard,
            captures: vec![],
        }
    }

    async fn secret(&mut self, value: &str) {
        let r = self
            .td
            .post(
                "/api/secrets",
                json!({"name": "win-login", "kind": "login",
            "fields": {"username": "ignored", "password": value}}),
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
        for capture in &self.captures {
            for password in [PASSWORD, ROTATED] {
                assert!(
                    !capture.contains(password),
                    "password leaked in captured output"
                );
            }
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
        for password in [PASSWORD, ROTATED] {
            assert!(
                !bytes
                    .windows(password.len())
                    .any(|w| w == password.as_bytes()),
                "password leaked to disk"
            );
        }
    }
}

#[tokio::test]
async fn password_reaches_only_the_session_start_and_is_resolved_again_after_rotation() {
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
