//! Linux-only contract tests. No Windows logon implementation is needed.
#![allow(clippy::unwrap_used, clippy::expect_used)]

use crate::common::TestDaemon;
use base64::Engine;
use futures_util::{SinkExt, StreamExt};
use serde_json::json;
use std::sync::{Arc, Mutex};
use std::time::Duration;
use terrarium_host::protocol::{DaemonFrame, HostFrame};
use tokio_tungstenite::tungstenite::{Message, client::IntoClientRequest};
use tracing_subscriber::util::SubscriberInitExt;

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
    assert!(
        output.status.success(),
        "TRACE child failed: {}\n{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    true
}

fn password_representations(password: &str) -> Vec<Vec<u8>> {
    let lower = password
        .as_bytes()
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect::<String>();
    let upper = lower.to_uppercase();
    let spaced_lower = password
        .as_bytes()
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect::<Vec<_>>()
        .join(" ");
    let spaced_upper = spaced_lower.to_uppercase();
    let base64 = base64::engine::general_purpose::STANDARD.encode(password);
    [
        password.to_owned(),
        lower,
        upper,
        spaced_lower,
        spaced_upper,
        base64,
    ]
    .into_iter()
    .map(String::into_bytes)
    .collect()
}

fn assert_no_password(bytes: &[u8]) {
    for password in [PASSWORD, ROTATED] {
        for representation in password_representations(password) {
            assert!(
                !bytes
                    .windows(representation.len())
                    .any(|w| w == representation),
                "password or reversible representation leaked"
            );
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
    addr: std::net::SocketAddr,
    token: String,
    resume_token: String,
    hold: Arc<std::sync::atomic::AtomicBool>,
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
        let mut token = registered.body["token"].as_str().unwrap().to_owned();
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
        let DaemonFrame::Welcome(welcome) =
            serde_json::from_str(welcome.to_text().unwrap()).unwrap()
        else {
            panic!("expected welcome")
        };
        if let Some(new_token) = welcome.token {
            token = new_token;
        }
        let resume_token = welcome.resume_token;
        let (tx, frames) = tokio::sync::mpsc::unbounded_channel();
        let hold = Arc::new(std::sync::atomic::AtomicBool::new(false));
        let hold_session = hold.clone();
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
                    if !hold_session.load(std::sync::atomic::Ordering::SeqCst) {
                        // A logon failure must use a static message, never the credentials.
                        let ended = json!({"type": "error", "session": session,
                            "code": "bad_request", "message": "logon refused"});
                        socket
                            .send(Message::Text(ended.to_string().into()))
                            .await
                            .unwrap();
                    }
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
            addr,
            token,
            resume_token,
            hold,
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

    async fn replacement(
        &mut self,
        os: &str,
        capability: bool,
        resume: serde_json::Value,
    ) -> (
        tokio_tungstenite::WebSocketStream<
            tokio_tungstenite::MaybeTlsStream<tokio::net::TcpStream>,
        >,
        terrarium_host::protocol::Welcome,
    ) {
        let mut request = format!("ws://{}/hosts/connect", self.addr)
            .into_client_request()
            .unwrap();
        request.headers_mut().insert(
            "authorization",
            format!("Bearer {}", self.token).parse().unwrap(),
        );
        let (mut socket, _) = tokio_tungstenite::connect_async(request).await.unwrap();
        socket
            .send(Message::Text(
                json!({"type":"hello", "protocol":1, "host":"win", "version":"test",
            "os":os, "arch":"x86_64", "max_sessions":8, "resume":resume,
            "capabilities":{"windows_run_as":capability,"no_workspaces":true}})
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
        let DaemonFrame::Welcome(welcome) =
            serde_json::from_str(welcome.to_text().unwrap()).unwrap()
        else {
            panic!("expected welcome")
        };
        self.captures.push(format!("{welcome:?}"));
        (socket, welcome)
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
        assert!(
            self.captures
                .last()
                .unwrap()
                .contains("host transport frame send"),
            "TRACE capture did not observe the actual transport task"
        );
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
    if trace_child("password_reaches_only_the_session_start_and_is_resolved_again_after_rotation")
        .await
    {
        return;
    }
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
    if trace_child("transfer_sessions_receive_the_same_one_session_credentials").await {
        return;
    }
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
    if trace_child("unsupported_hosts_never_receive_a_password_even_if_they_claim_the_capability")
        .await
    {
        return;
    }
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
    if trace_child("missing_and_ungranted_passwords_refuse_before_sending").await {
        return;
    }
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
    for kind in ["ssh", "exedev"] {
        let def = serde_json::from_value(
            json!({"name":"box", "kind":kind, "address":"example.invalid",
            "windows_run_as":{"account":"worker","password_secret":"login"}}),
        )
        .unwrap();
        assert!(
            terrariumd::store::targets::validate(&def).is_err(),
            "SSH spools must not accept run-as passwords"
        );
    }
    assert!(
        serde_json::from_value::<terrarium_core::config::TargetDef>(json!({"kind": "host",
        "windows_run_as": {"account": "worker"}}))
        .is_err()
    );
}

#[tokio::test]
async fn link_keys_and_cross_project_agents_cannot_start_run_as_sessions() {
    if trace_child("link_keys_and_cross_project_agents_cannot_start_run_as_sessions").await {
        return;
    }
    use terrariumd::api::{LinkAuth, TokenAuth, tcp_router_with};
    use tower::ServiceExt;
    let mut lab = Lab::new("windows", true).await;
    lab.secret(PASSWORD).await;
    lab.td.project("p").await;
    lab.td.project("q").await;
    let r = lab
        .td
        .post(
            "/api/targets",
            json!({"name":"box", "project":"p", "kind":"host", "host":"win",
        "windows_run_as":{"account":"worker", "password_secret":"win-login"}}),
        )
        .await;
    r.ok();
    lab.captures.push(r.body.to_string());
    let agent = lab
        .td
        .agent("q", "other", json!({"policy":{"targets":["p-box"]}}))
        .await;
    let aid = agent["id"].as_str().unwrap();
    let r = lab
        .td
        .post_as(aid, "/api/targets/p-box/exec", json!({"cmd":["whoami"]}))
        .await;
    assert_eq!(r.status, 403);
    lab.captures.push(r.body.to_string());
    let link = lab
        .td
        .post("/api/links", json!({"label":"home", "read_only":false}))
        .await
        .ok()
        .clone();
    let paired = lab
        .td
        .post(
            "/api/links/pair",
            json!({"code":link["code"],
        "home_instance_id":ulid::Ulid::new().to_string(), "home_name":"home"}),
        )
        .await
        .ok()
        .clone();
    let state = lab.td.daemon.as_ref().unwrap().state.clone();
    let app = tcp_router_with(
        state.clone(),
        TokenAuth::new("operator-test-token".into(), false).with_links(LinkAuth {
            db: state.db.clone(),
            runtime: state.links.clone(),
        }),
        false,
    );
    for (path, body) in [
        ("exec", json!({"cmd":["whoami"]})),
        ("cp", json!({"direction":"pull", "path":"/test-file"})),
    ] {
        let request = axum::http::Request::builder()
            .method("POST")
            .uri(format!("/api/targets/p-box/{path}"))
            .header(
                "authorization",
                format!("Bearer {}", paired["key"].as_str().unwrap()),
            )
            .header("content-type", "application/json")
            .body(axum::body::Body::from(body.to_string()))
            .unwrap();
        let response = app.clone().oneshot(request).await.unwrap();
        assert_eq!(response.status(), 403);
        let bytes = axum::body::to_bytes(response.into_body(), 1024 * 1024)
            .await
            .unwrap();
        lab.captures
            .push(String::from_utf8_lossy(&bytes).into_owned());
    }
    assert!(
        tokio::time::timeout(Duration::from_millis(100), lab.next_exec())
            .await
            .is_err()
    );
    lab.scan().await;
}

#[tokio::test]
async fn replacing_capability_after_password_resolution_refuses_admission() {
    if trace_child("replacing_capability_after_password_resolution_refuses_admission").await {
        return;
    }
    let mut lab = Lab::new("windows", true).await;
    lab.secret(PASSWORD).await;
    lab.target("win-login").await;
    let state = lab.td.daemon.as_ref().unwrap().state.clone();
    assert!(state.executor.hosts.supports_windows_run_as("win"));
    let secret =
        terrariumd::secrets::load_now(&state, terrariumd::secrets::Owner::Global, "win-login")
            .unwrap();
    let terrariumd::secrets::Secret::Login { password, .. } = secret else {
        panic!("login")
    };
    // Deterministically replace the connection in the resolution/admission gap.
    lab.host.abort();
    let (mut socket, _) = lab
        .replacement("linux", true, serde_json::Value::Null)
        .await;
    let result = state.executor.hosts.open("win", move |session| {
        serde_json::from_value(json!({"type":"exec", "session":session, "cmd":"whoami",
            "windows_run_as":{"account":"worker", "password":password.as_str()}}))
        .unwrap()
    });
    assert!(matches!(
        result,
        Err(terrariumd::hosts::HostError::WindowsRunAsUnsupported)
    ));
    assert!(
        tokio::time::timeout(Duration::from_millis(100), socket.next())
            .await
            .is_err(),
        "replacement host received a session"
    );
    lab.scan().await;
}

#[tokio::test]
async fn durable_run_resume_never_replays_credentials() {
    if trace_child("durable_run_resume_never_replays_credentials").await {
        return;
    }
    let mut lab = Lab::new("windows", true).await;
    lab.secret(PASSWORD).await;
    lab.target("win-login").await;
    lab.td.project("p").await;
    let agent = lab
        .td
        .agent("p", "worker", json!({"policy":{"targets":["box"]}}))
        .await;
    let aid = agent["id"].as_str().unwrap();
    lab.hold.store(true, std::sync::atomic::Ordering::SeqCst);
    let run = lab
        .td
        .post_as(
            aid,
            &format!("/api/agents/{aid}/run"),
            json!({"target":"box", "name":"logon", "cmd":["whoami"]}),
        )
        .await;
    run.ok();
    lab.captures.push(run.body.to_string());
    let wid = run.body["wait"]["id"].as_str().unwrap().to_owned();
    let frame = lab.next_exec().await;
    lab.captures.push(format!("{frame:?}"));
    let DaemonFrame::Exec(req) = frame else {
        panic!("exec")
    };
    let session = req.session;
    assert!(req.windows_run_as.unwrap().password == PASSWORD);
    // Read the live session before reconnect, including its durable wait/log.
    lab.captures.push(
        lab.td
            .get(&format!("/api/waits/{wid}"))
            .await
            .body
            .to_string(),
    );
    lab.captures.push(
        lab.td
            .get(&format!("/api/waits/{wid}/log"))
            .await
            .body
            .to_string(),
    );
    lab.scan().await;
    lab.host.abort();
    let offer = json!({"token":lab.resume_token, "sessions":[{"session":session, "sent":0}]});
    let (mut socket, welcome) = lab.replacement("windows", true, offer).await;
    assert!(
        welcome.resume.iter().any(|s| s.session == session),
        "session was not resumed"
    );
    // A resumed connection gets acknowledgements/control only, not another
    // session start and not the already-delivered password.
    socket
        .send(Message::Text(
            json!({"type":"exit", "session":session,
        "code":0, "signal":null, "reason":"exited"})
            .to_string()
            .into(),
        ))
        .await
        .unwrap();
    tokio::time::timeout(Duration::from_secs(30), async {
        loop {
            let wait = lab.td.get(&format!("/api/waits/{wid}")).await;
            lab.captures.push(wait.body.to_string());
            if wait.body["state"] == "resolved" {
                break;
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    })
    .await
    .unwrap();
    while let Ok(Some(Ok(message))) =
        tokio::time::timeout(Duration::from_millis(100), socket.next()).await
    {
        if let Message::Text(text) = message {
            assert_no_password(text.as_bytes());
            let frame: DaemonFrame = serde_json::from_str(&text).unwrap();
            // Reconnect reconciliation and run-kill cleanup may open their own
            // service sessions. Neither those nor resume may resend credentials
            // or start the original session again.
            match &frame {
                DaemonFrame::Exec(req) => {
                    assert_ne!(req.session, session);
                    assert!(req.windows_run_as.is_none());
                }
                DaemonFrame::CpPush(req) => {
                    assert_ne!(req.session, session);
                    assert!(req.windows_run_as.is_none());
                }
                DaemonFrame::CpPull(req) => {
                    assert_ne!(req.session, session);
                    assert!(req.windows_run_as.is_none());
                }
                _ => {}
            }
            lab.captures.push(format!("{frame:?}"));
        }
    }
    lab.captures.push(
        lab.td
            .get(&format!("/api/waits/{wid}/log"))
            .await
            .body
            .to_string(),
    );
    lab.scan().await;
}
