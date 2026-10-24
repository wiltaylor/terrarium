//! The real supervisor (`RunnerSupervisor`) with sandbox mode `none` and the
//! runner's fake TUI (`crates/terrarium-runner/tests/fake_tui/fake_tui.py`)
//! behind a test adapter: lifecycle, events over SSE, the PTY, push
//! delivery, the restart policy, a daemon restart with resume, the agent
//! RPC tunnel, and long-poll.
#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::collections::{BTreeMap, HashMap};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use futures_util::{SinkExt, StreamExt};
use http_body_util::{BodyExt, Full};
use hyper::body::Bytes;
use hyper::{Method, Request};
use hyper_util::rt::TokioIo;
use semver::{Version, VersionReq};
use serde_json::{Value, json};
use terrarium_core::{Adapter, Agent, Event, HarnessId, HarnessSessionId, HookPayload, LaunchSpec};
use terrarium_runner::DefaultReadyDetector;
use terrarium_runner::test_support::{RunnerProcessGuard, sweep_stale_runners_once};
use terrariumd::runner_supervisor::{
    AdapterEntry, RestartTuning, RunnerSupervisor, RunnerSupervisorConfig, RunnerTuning,
};
use terrariumd::sandbox::{SandboxConfig, SandboxMode};
use terrariumd::{Daemon, DaemonConfig};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::net::UnixStream;
use tokio::time::timeout;
use tokio_tungstenite::tungstenite::Message as WsMessage;
use tokio_tungstenite::tungstenite::client::IntoClientRequest;

const WAIT: Duration = Duration::from_secs(15);

fn fake_tui() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("../terrarium-runner/tests/fake_tui/fake_tui.py")
}

/// Runs the fake TUI. Its hook names are Claude Code's.
#[derive(Default)]
struct FakeAdapter {
    /// Extra env for the next launches. A test changes it between starts.
    env: Mutex<BTreeMap<String, String>>,
    /// `prompt_frozen_on_resume`, as Codex says.
    frozen: std::sync::atomic::AtomicBool,
    /// `exit_keys` for a graceful stop. Empty: kill.
    exit_keys: Mutex<Vec<u8>>,
}

impl FakeAdapter {
    fn set_env(&self, k: &str, v: &str) {
        self.env.lock().unwrap().insert(k.into(), v.into());
    }
    fn clear_env(&self) {
        self.env.lock().unwrap().clear();
    }
}

impl Adapter for FakeAdapter {
    fn id(&self) -> HarnessId {
        HarnessId::Claude
    }
    fn tested_version(&self) -> VersionReq {
        VersionReq::STAR
    }
    fn detect_version(&self) -> terrarium_core::Result<Version> {
        Ok(Version::new(1, 0, 0))
    }
    fn launch(&self, agent: &Agent, resume: Option<&HarnessSessionId>) -> LaunchSpec {
        let mut spec = LaunchSpec::new("python3", &agent.workspace)
            .arg(fake_tui().display().to_string())
            .env("FAKE_TUI_DELAY_MS", "150");
        spec = match resume {
            Some(id) => spec.arg("--resume").arg(id.0.as_str()),
            None => spec
                .arg("--session")
                .arg(format!("fake-{}", terrarium_core::SessionId::new())),
        };
        for (k, v) in self.env.lock().unwrap().iter() {
            spec = spec.env(k, v);
        }
        spec
    }
    fn prompt_frozen_on_resume(&self) -> bool {
        self.frozen.load(std::sync::atomic::Ordering::SeqCst)
    }
    fn exit_keys(&self) -> Vec<u8> {
        self.exit_keys.lock().unwrap().clone()
    }
    fn hook_event(&self, hook: &HookPayload) -> Vec<Event> {
        match hook.hook.as_str() {
            "UserPromptSubmit" => vec![Event::TurnStart {
                prompt: hook.str_field("prompt").map(str::to_owned),
            }],
            "Stop" => vec![Event::TurnEnd],
            "SessionStart" => vec![Event::SessionStarted {
                harness_session: HarnessSessionId(
                    hook.str_field("session_id").unwrap_or_default().to_owned(),
                ),
                transcript_path: None,
                source: hook.str_field("source").map(str::to_owned),
            }],
            _ => vec![],
        }
    }
}

/// A daemon in a temp dir. With TERRARIUM_TEST_RUNNER_MODE=process, the
/// runner processes live through the shutdown of the daemon: `_guard`
/// ends them when the test ends, also on a panic. It drops after the
/// daemon (the `Drop` of `Td`) and before `dir`.
struct Td {
    _guard: RunnerProcessGuard,
    dir: tempfile::TempDir,
    adapter: Arc<FakeAdapter>,
    sup: Arc<RunnerSupervisor>,
    daemon: Option<Daemon>,
    socket: PathBuf,
}

impl Td {
    async fn start() -> Self {
        Self::start_with(true).await
    }

    async fn start_with(insecure: bool) -> Self {
        let dir = tempfile::tempdir().unwrap();
        if let Some(bin) = process_mode_bin() {
            sweep_stale_runners_once(&bin);
        }
        let adapter = Arc::new(FakeAdapter::default());
        let mut td = Self {
            _guard: RunnerProcessGuard::new(dir.path().join("run")),
            socket: dir.path().join("api.sock"),
            sup: Arc::new(supervisor(dir.path(), adapter.clone())),
            dir,
            adapter,
            daemon: None,
        };
        td.boot(insecure).await;
        td
    }

    async fn boot(&mut self, insecure: bool) {
        let daemon = terrariumd::start(DaemonConfig {
            socket: self.socket.clone(),
            listen: None,
            token: None,
            state_dir: self.dir.path().join("state"),
            config_dir: self.dir.path().join("config"),
            supervisor: self.sup.clone(),
            insecure_agent_header: insecure,
            artifact_listen: None,
            web: Default::default(),
        })
        .await
        .unwrap();
        self.daemon = Some(daemon);
    }

    /// Stop this daemon (its harnesses die with it, the stored status
    /// stays), then start a new daemon with a new supervisor on the same
    /// state dir.
    async fn restart_daemon(&mut self) {
        if let Some(d) = self.daemon.take() {
            d.shutdown();
        }
        tokio::time::sleep(Duration::from_millis(300)).await;
        self.sup = Arc::new(supervisor(self.dir.path(), self.adapter.clone()));
        self.boot(true).await;
    }

    async fn req(
        &self,
        method: Method,
        path: &str,
        body: Option<Value>,
        agent: Option<&str>,
    ) -> (u16, Value) {
        let stream = UnixStream::connect(&self.socket).await.unwrap();
        let (mut sender, conn) = hyper::client::conn::http1::handshake(TokioIo::new(stream))
            .await
            .unwrap();
        tokio::spawn(conn);
        let mut req = Request::builder()
            .method(method)
            .uri(path)
            .header("host", "localhost");
        if let Some(a) = agent {
            req = req.header("x-terrarium-agent", a);
        }
        let bytes = match body {
            Some(b) => {
                req = req.header("content-type", "application/json");
                Bytes::from(b.to_string())
            }
            None => Bytes::new(),
        };
        let resp = sender
            .send_request(req.body(Full::new(bytes)).unwrap())
            .await
            .unwrap();
        let status = resp.status().as_u16();
        let raw = resp.into_body().collect().await.unwrap().to_bytes();
        let body = serde_json::from_slice(&raw).unwrap_or(Value::Null);
        (status, body)
    }

    async fn get(&self, path: &str) -> Value {
        let (s, b) = self.req(Method::GET, path, None, None).await;
        assert!((200..300).contains(&s), "GET {path}: {s} {b}");
        b
    }

    async fn post(&self, path: &str, body: Option<Value>) -> Value {
        let (s, b) = self.req(Method::POST, path, body, None).await;
        assert!((200..300).contains(&s), "POST {path}: {s} {b}");
        b
    }

    /// A project and one agent in it.
    async fn agent(&self, project: &str, name: &str, extra: Value) -> String {
        let _ = self
            .req(
                Method::POST,
                "/api/projects",
                Some(json!({"name": project})),
                None,
            )
            .await;
        let mut def = json!({"name": name, "project": project, "harness": "claude"});
        for (k, v) in extra.as_object().unwrap() {
            def[k] = v.clone();
        }
        let a = self.post("/api/agents", Some(def)).await;
        a["id"].as_str().unwrap().to_owned()
    }

    async fn status(&self, id: &str) -> String {
        self.get(&format!("/api/agents/{id}")).await["status"]
            .as_str()
            .unwrap()
            .to_owned()
    }

    async fn wait_status(&self, id: &str, want: &str) {
        let start = Instant::now();
        loop {
            let s = self.status(id).await;
            if s == want {
                return;
            }
            assert!(
                start.elapsed() < WAIT,
                "agent never reached {want}; last {s}"
            );
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    }

    async fn events(&self, id: &str, kind: &str) -> Vec<Value> {
        self.get(&format!("/api/agents/{id}/events?kind={kind}&limit=1000"))
            .await
            .as_array()
            .unwrap()
            .clone()
    }

    /// Wait until `f` holds for the events of one kind.
    async fn wait_events(&self, id: &str, kind: &str, f: impl Fn(&[Value]) -> bool) -> Vec<Value> {
        let start = Instant::now();
        loop {
            let ev = self.events(id, kind).await;
            if f(&ev) {
                return ev;
            }
            assert!(
                start.elapsed() < WAIT,
                "{kind} events never matched: {ev:#?}"
            );
            tokio::time::sleep(Duration::from_millis(30)).await;
        }
    }

    fn runner_socket(&self, id: &str) -> PathBuf {
        self.dir
            .path()
            .join("run/agents")
            .join(format!("{id}.sock"))
    }
}

impl Drop for Td {
    fn drop(&mut self) {
        if let Some(d) = self.daemon.take() {
            d.shutdown();
        }
    }
}

/// The runner binary when TERRARIUM_TEST_RUNNER_MODE=process.
fn process_mode_bin() -> Option<PathBuf> {
    (std::env::var("TERRARIUM_TEST_RUNNER_MODE").as_deref() == Ok("process"))
        .then(|| Path::new(env!("CARGO_MANIFEST_DIR")).join("../../target/debug/terrarium-runner"))
}

fn supervisor(root: &Path, adapter: Arc<FakeAdapter>) -> RunnerSupervisor {
    let home = root.join("auth/claude");
    std::fs::create_dir_all(&home).unwrap();
    std::fs::create_dir_all(root.join("config")).unwrap();
    let mut homes = HashMap::new();
    homes.insert(HarnessId::Claude, home);
    let mut cfg = RunnerSupervisorConfig::new(
        root.join("state"),
        root.join("run"),
        root.join("prompts"),
        SandboxConfig {
            mode: SandboxMode::None,
            ..SandboxConfig::default()
        },
        homes,
    );
    let mut entry = AdapterEntry::new(adapter);
    entry.ready_detector = Some(Arc::new(DefaultReadyDetector {
        quiet: Duration::from_millis(100),
        pattern: None,
    }));
    if let Some(bin) = process_mode_bin() {
        // A check of the runner processes: the runner maps the hooks with
        // its fake adapter. Some tests here expect in-process behaviour
        // (a daemon restart relaunches the harness).
        entry.runner_adapter = terrarium_runner::AdapterKind::Fake;
        entry.ready_spec = Some(terrarium_runner::ReadyDetectorSpec {
            quiet_ms: 100,
            pattern: None,
        });
        cfg.mode = terrariumd::runner_supervisor::RunnerMode::Process { bin };
    }
    cfg.adapters.clear();
    // Also as `pi`, so a pi agent reaches the subscription checks.
    cfg.adapters.insert(HarnessId::Pi, entry.clone());
    cfg.adapters.insert(HarnessId::Claude, entry);
    cfg.restart = RestartTuning {
        backoff_base: Duration::from_millis(50),
        backoff_max: Duration::from_millis(200),
        crash_window: Duration::from_secs(60),
        crash_limit: 2,
    };
    cfg.runner = RunnerTuning {
        tick: Duration::from_millis(20),
        submit_timeout: Duration::from_secs(3),
        ..RunnerTuning::default()
    };
    RunnerSupervisor::new(cfg)
}

/// Read Server-Sent Events from `path` until `done` holds for one event.
async fn sse_until(socket: &Path, path: &str, done: impl Fn(&Value) -> bool) -> Vec<Value> {
    let stream = UnixStream::connect(socket).await.unwrap();
    let (mut sender, conn) = hyper::client::conn::http1::handshake(TokioIo::new(stream))
        .await
        .unwrap();
    tokio::spawn(conn);
    let req = Request::builder()
        .uri(path)
        .header("host", "localhost")
        .header("accept", "text/event-stream")
        .body(Full::new(Bytes::new()))
        .unwrap();
    let mut body = sender.send_request(req).await.unwrap().into_body();
    let mut buf = String::new();
    let mut seen = Vec::new();
    timeout(WAIT, async {
        while let Some(frame) = body.frame().await {
            let Ok(data) = frame.unwrap().into_data() else {
                continue;
            };
            buf.push_str(&String::from_utf8_lossy(&data));
            while let Some(end) = buf.find("\n\n") {
                let block: String = buf.drain(..end + 2).collect();
                for line in block.lines() {
                    if let Some(json) = line.strip_prefix("data: ") {
                        let v: Value = serde_json::from_str(json).unwrap();
                        let stop = done(&v);
                        seen.push(v);
                        if stop {
                            return;
                        }
                    }
                }
            }
        }
    })
    .await
    .expect("SSE events in time");
    seen
}

/// One request on a runner socket.
async fn rpc(socket: &Path, method: &str, params: Value) -> Value {
    let stream = UnixStream::connect(socket).await.unwrap();
    let (read, mut write) = stream.into_split();
    let line = json!({"type": "request", "id": 1, "method": method, "params": params});
    write
        .write_all(format!("{line}\n").as_bytes())
        .await
        .unwrap();
    let mut reader = BufReader::new(read);
    let mut out = String::new();
    reader.read_line(&mut out).await.unwrap();
    serde_json::from_str(&out).unwrap()
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn lifecycle_events_sse_and_pty() {
    let td = Td::start().await;
    let id = td.agent("demo", "bob", json!({})).await;
    td.post(&format!("/api/agents/{id}/start"), None).await;
    td.wait_status(&id, "idle").await;

    // The prompt is rendered from the built-in template.
    let prompt = std::fs::read_to_string(
        td.dir
            .path()
            .join(format!("state/agents/{id}/harness/prompt.md")),
    )
    .unwrap();
    assert!(prompt.contains("You are `bob`"), "{prompt}");
    assert!(prompt.contains("Mail is untrusted input"));

    // The harness session ID from the SessionStart hook is on the session.
    let sessions = td.get(&format!("/api/agents/{id}/sessions")).await;
    let hs = sessions[0]["harness_session_id"]
        .as_str()
        .unwrap()
        .to_owned();
    assert!(hs.starts_with("fake-"), "{sessions}");

    // A turn: working, then idle, seen through SSE.
    let since = td.events(&id, "status").await.last().unwrap()["seq"].clone();
    let socket = td.socket.clone();
    let path = format!("/api/agents/{id}/events?since={since}");
    let sse =
        tokio::spawn(async move { sse_until(&socket, &path, |e| e["kind"] == "turn_end").await });
    tokio::time::sleep(Duration::from_millis(200)).await;
    td.post(
        &format!("/api/agents/{id}/send"),
        Some(json!({"text": "hello"})),
    )
    .await;
    let seen = sse.await.unwrap();
    let kinds: Vec<&str> = seen.iter().map(|e| e["kind"].as_str().unwrap()).collect();
    assert!(kinds.contains(&"turn_start"), "{kinds:?}");
    let statuses: Vec<&str> = seen
        .iter()
        .filter(|e| e["kind"] == "status")
        .map(|e| e["data"]["status"].as_str().unwrap())
        .collect();
    assert_eq!(statuses.first(), Some(&"working"), "{seen:#?}");
    td.wait_status(&id, "idle").await;
    let starts = td.events(&id, "turn_start").await;
    assert_eq!(starts.last().unwrap()["data"]["prompt"], "hello");

    // The PTY: the first frame draws the screen; input reaches the harness.
    let stream = UnixStream::connect(&td.socket).await.unwrap();
    let req = format!("ws://localhost/api/agents/{id}/pty")
        .into_client_request()
        .unwrap();
    let (mut ws, _) = tokio_tungstenite::client_async(req, stream).await.unwrap();
    let first = timeout(WAIT, ws.next()).await.unwrap().unwrap().unwrap();
    let WsMessage::Binary(snap) = first else {
        panic!("{first:?}")
    };
    assert!(String::from_utf8_lossy(&snap).contains("done: hello"));
    ws.send(WsMessage::Text(
        json!({"type": "input", "data": "typed\r"})
            .to_string()
            .into(),
    ))
    .await
    .unwrap();
    let mut out = String::new();
    timeout(WAIT, async {
        while !out.contains("done: typed") {
            if let Some(Ok(WsMessage::Binary(b))) = ws.next().await {
                out.push_str(&String::from_utf8_lossy(&b));
            }
        }
    })
    .await
    .expect("PTY output");

    td.post(&format!("/api/agents/{id}/stop"), None).await;
    td.wait_status(&id, "stopped").await;
    // The socket closes when the agent stops.
    let end = timeout(WAIT, async {
        loop {
            match ws.next().await {
                None | Some(Err(_)) | Some(Ok(WsMessage::Close(_))) => return,
                _ => {}
            }
        }
    })
    .await;
    assert!(end.is_ok());
    let sessions = td.get(&format!("/api/agents/{id}/sessions")).await;
    assert_eq!(sessions[0]["outcome"], "stopped");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn unregistered_harness_is_refused() {
    let td = Td::start().await;
    let id = td.agent("demo", "cx", json!({"harness": "codex"})).await;
    let (s, b) = td
        .req(Method::POST, &format!("/api/agents/{id}/start"), None, None)
        .await;
    assert_eq!(s, 422, "{b}");
    assert!(
        b["error"]["message"]
            .as_str()
            .unwrap()
            .contains("not registered"),
        "{b}"
    );
    assert_eq!(td.status(&id).await, "stopped");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn mail_and_cards_push_a_pointer() {
    let td = Td::start().await;
    let id = td.agent("demo", "bob", json!({})).await;
    td.post(&format!("/api/agents/{id}/start"), None).await;
    td.wait_status(&id, "idle").await;

    // Idle: one mail types the pointer at once.
    let m = td
        .post(
            "/api/mail",
            Some(json!({"to": "demo/bob", "body": "secret body"})),
        )
        .await;
    let thread = m["thread_id"].as_str().unwrap().to_owned();
    let want = format!(
        "New mail from operator (thread {thread}). Run `terrarium mail read --thread {thread}`."
    );
    let starts = td.wait_events(&id, "turn_start", |e| !e.is_empty()).await;
    assert_eq!(starts[0]["data"]["prompt"], want.as_str());
    assert!(
        !starts[0]["data"]["prompt"]
            .as_str()
            .unwrap()
            .contains("secret")
    );
    td.wait_status(&id, "idle").await;
    let msg = td
        .get(&format!("/api/mail/{}", m["id"].as_str().unwrap()))
        .await;
    assert_eq!(msg["state"], "delivered");

    // Working: the mail waits for the stop hook, and one pointer covers it
    // all, with a card that was assigned meanwhile.
    td.adapter.set_env("FAKE_TUI_DELAY_MS", "1000");
    td.post(&format!("/api/agents/{id}/restart"), None).await;
    td.wait_events(&id, "session_started", |e| e.len() == 2)
        .await;
    td.wait_status(&id, "idle").await;
    td.post(
        &format!("/api/agents/{id}/send"),
        Some(json!({"text": "long work"})),
    )
    .await;
    td.wait_status(&id, "working").await;
    let a = td
        .post("/api/mail", Some(json!({"to": "demo/bob", "body": "one"})))
        .await;
    let t2 = a["thread_id"].as_str().unwrap().to_owned();
    td.post(
        "/api/mail",
        Some(json!({"to": "demo/bob", "body": "two", "thread_id": t2})),
    )
    .await;
    let card = td
        .post(
            "/api/projects/demo/cards",
            Some(json!({"title": "Ship it"})),
        )
        .await;
    let card_id = card["id"].as_str().unwrap().to_owned();
    td.post(
        &format!("/api/cards/{card_id}/assign"),
        Some(json!({"assignee": "demo/bob"})),
    )
    .await;
    let starts = td
        .wait_events(&id, "turn_start", |e| {
            e.iter()
                .any(|x| x["data"]["prompt"].as_str().unwrap_or("").contains(&t2))
        })
        .await;
    let pointer = starts.last().unwrap()["data"]["prompt"]
        .as_str()
        .unwrap()
        .to_owned();
    assert_eq!(
        pointer,
        format!(
            "New mail from operator (thread {t2}). Run `terrarium mail read --thread {t2}`.\n\
             Card {card_id} was assigned to you. Run `terrarium card show {card_id}`."
        )
    );
    let n = starts
        .iter()
        .filter(|x| x["data"]["prompt"].as_str().unwrap_or("").contains(&t2))
        .count();
    assert_eq!(n, 1, "one pointer for both messages: {starts:#?}");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn restart_policy_backoff_and_crash_loop() {
    let td = Td::start().await;
    // on-failure (the default): a non-zero exit restarts with resume.
    let id = td.agent("demo", "crashy", json!({})).await;
    td.adapter.set_env("FAKE_TUI_EXIT_AFTER_MS", "400");
    td.adapter.set_env("FAKE_TUI_EXIT_CODE", "2");
    td.post(&format!("/api/agents/{id}/start"), None).await;
    let statuses = td
        .wait_events(&id, "status", |e| {
            e.iter().any(|x| x["data"]["status"] == "error")
        })
        .await;
    let seq: Vec<&str> = statuses
        .iter()
        .map(|x| x["data"]["status"].as_str().unwrap())
        .collect();
    assert_eq!(
        seq.iter().filter(|s| **s == "restarting").count(),
        2,
        "{seq:?}"
    );
    let last = statuses.last().unwrap();
    assert!(
        last["data"]["reason"]
            .as_str()
            .unwrap()
            .contains("crash loop"),
        "{last}"
    );
    // Each restart resumed the same harness session.
    let started = td.events(&id, "session_started").await;
    assert_eq!(started.len(), 3, "{started:#?}");
    assert_eq!(started[1]["data"]["source"], "resume");
    assert_eq!(
        started[1]["data"]["harness_session"],
        started[0]["data"]["harness_session"]
    );
    let sessions = td.get(&format!("/api/agents/{id}/sessions")).await;
    assert_eq!(sessions.as_array().unwrap().len(), 3, "{sessions}");
    assert_eq!(sessions[1]["outcome"], "crashed");

    // never: a non-zero exit is `error`; exit 0 is `stopped` with "exited".
    td.adapter.clear_env();
    let id = td.agent("demo", "once", json!({"restart": "never"})).await;
    td.post(&format!("/api/agents/{id}/start"), None).await;
    td.wait_status(&id, "idle").await;
    td.post(
        &format!("/api/agents/{id}/send"),
        Some(json!({"text": "/exit 0"})),
    )
    .await;
    td.wait_status(&id, "stopped").await;
    let last = td.events(&id, "status").await.last().unwrap().clone();
    assert_eq!(last["data"]["reason"], "exited");
    td.post(&format!("/api/agents/{id}/start"), None).await;
    td.wait_status(&id, "idle").await;
    td.post(
        &format!("/api/agents/{id}/send"),
        Some(json!({"text": "/exit 3"})),
    )
    .await;
    td.wait_status(&id, "error").await;
    let last = td.events(&id, "status").await.last().unwrap().clone();
    assert_eq!(last["data"]["reason"], "exited with code 3");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn daemon_restart_relaunches_with_resume() {
    let mut td = Td::start().await;
    let id = td.agent("demo", "bob", json!({})).await;
    td.post(&format!("/api/agents/{id}/start"), None).await;
    td.wait_status(&id, "idle").await;
    let first = td.events(&id, "session_started").await[0]["data"]["harness_session"].clone();

    td.restart_daemon().await;
    td.wait_events(&id, "session_started", |e| e.len() == 2)
        .await;
    td.wait_status(&id, "idle").await;
    let started = td.events(&id, "session_started").await;
    assert_eq!(started[1]["data"]["source"], "resume");
    assert_eq!(started[1]["data"]["harness_session"], first);
    let sessions = td.get(&format!("/api/agents/{id}/sessions")).await;
    assert_eq!(sessions[1]["outcome"], "daemon_restart");

    // A resume that fails starts a fresh session and types a recap turn.
    td.post(
        &format!("/api/agents/{id}/send"),
        Some(json!({"text": "remember me"})),
    )
    .await;
    td.wait_events(&id, "turn_end", |e| !e.is_empty()).await;
    td.adapter.set_env("FAKE_TUI_RESUME_FAIL", "1");
    td.restart_daemon().await;
    let starts = td
        .wait_events(&id, "turn_start", |e| {
            e.iter().any(|x| {
                x["data"]["prompt"]
                    .as_str()
                    .unwrap_or("")
                    .starts_with("Terrarium restarted you")
            })
        })
        .await;
    let recap = starts.last().unwrap()["data"]["prompt"]
        .as_str()
        .unwrap()
        .to_owned();
    assert!(recap.contains("- turn: remember me"), "{recap}");
    let started = td.events(&id, "session_started").await;
    assert_eq!(started.last().unwrap()["data"]["source"], "startup");
    assert_ne!(started.last().unwrap()["data"]["harness_session"], first);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn agent_rpc_tunnel_and_long_poll() {
    let td = Td::start_with(false).await;
    let bob = td.agent("demo", "bob", json!({})).await;
    let _alice = td.agent("demo", "alice", json!({})).await;
    td.agent("other", "carol", json!({})).await;
    td.post(&format!("/api/agents/{bob}/start"), None).await;
    td.wait_status(&bob, "idle").await;
    // Without --insecure-agent-header the header is refused.
    let (s, _) = td
        .req(Method::GET, "/api/mail", None, Some("demo/bob"))
        .await;
    assert_eq!(s, 401);

    td.post(
        "/api/mail",
        Some(json!({"to": "demo/alice", "body": "not for bob"})),
    )
    .await;
    td.post(
        "/api/mail",
        Some(json!({"to": "demo/bob", "body": "for bob"})),
    )
    .await;
    let sock = td.runner_socket(&bob);
    let who = rpc(&sock, "whoami", json!({})).await;
    assert_eq!(who["result"]["name"], "bob");

    // GET /api/mail as bob: only bob's mail.
    let r = rpc(&sock, "http", json!({"method": "GET", "path": "/api/mail"})).await;
    assert_eq!(r["result"]["status"], 200, "{r}");
    let mail = r["result"]["body"].as_array().unwrap();
    assert_eq!(mail.len(), 1, "{r}");
    assert_eq!(mail[0]["body"], "for bob");
    // Policy applies: another project is out of reach, mail to it too.
    let r = rpc(
        &sock,
        "http",
        json!({"method": "GET", "path": "/api/projects/other/board"}),
    )
    .await;
    assert_eq!(r["result"]["status"], 403, "{r}");
    let r = rpc(
        &sock,
        "http",
        json!({"method": "POST", "path": "/api/mail",
        "body": {"to": "other/carol", "body": "hi"}}),
    )
    .await;
    assert_eq!(r["result"]["status"], 403, "{r}");
    assert_eq!(r["result"]["body"]["error"]["code"], "forbidden");
    // Pagination headers come back.
    let r = rpc(
        &sock,
        "http",
        json!({"method": "GET", "path": "/api/agents", "query": {"limit": 1}}),
    )
    .await;
    assert!(r["result"]["headers"]["x-next-cursor"].is_string(), "{r}");

    // Long-poll: the request waits until new mail arrives.
    let after = mail[0]["id"].as_str().unwrap().to_owned();
    let s2 = sock.clone();
    let started = Instant::now();
    let waiter = tokio::spawn(async move {
        rpc(
            &s2,
            "http",
            json!({"method": "GET", "path": "/api/mail",
            "query": {"wait": 10, "cursor": after}}),
        )
        .await
    });
    tokio::time::sleep(Duration::from_millis(400)).await;
    assert!(!waiter.is_finished());
    td.post("/api/mail", Some(json!({"to": "demo/bob", "body": "late"})))
        .await;
    let r = timeout(WAIT, waiter).await.unwrap().unwrap();
    assert!(started.elapsed() < Duration::from_secs(5));
    let mail = r["result"]["body"].as_array().unwrap();
    assert_eq!(mail.len(), 1, "{r}");
    assert_eq!(mail[0]["body"], "late");

    // A timeout answers an empty list.
    let started = Instant::now();
    let r = rpc(
        &sock,
        "http",
        json!({"method": "GET", "path": "/api/projects/demo/cards",
        "query": {"assignee": "me", "wait": 1}}),
    )
    .await;
    assert_eq!(r["result"]["body"], json!([]));
    assert!(started.elapsed() >= Duration::from_millis(900));

    // Cards: the wait wakes on an assignment.
    let card = td
        .post("/api/projects/demo/cards", Some(json!({"title": "T"})))
        .await;
    let s3 = sock.clone();
    let waiter = tokio::spawn(async move {
        rpc(
            &s3,
            "http",
            json!({"method": "GET", "path": "/api/projects/demo/cards",
            "query": {"assignee": "me", "wait": 10}}),
        )
        .await
    });
    tokio::time::sleep(Duration::from_millis(300)).await;
    td.post(
        &format!("/api/cards/{}/assign", card["id"].as_str().unwrap()),
        Some(json!({"assignee": "demo/bob"})),
    )
    .await;
    let r = timeout(WAIT, waiter).await.unwrap().unwrap();
    assert_eq!(r["result"]["body"].as_array().unwrap().len(), 1, "{r}");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn artifact_origin_listens_on_its_own_port() {
    let dir = tempfile::tempdir().unwrap();
    let sup = Arc::new(supervisor(dir.path(), Arc::new(FakeAdapter::default())));
    let daemon = terrariumd::start(DaemonConfig {
        socket: dir.path().join("api.sock"),
        listen: None,
        token: None,
        state_dir: dir.path().join("state"),
        config_dir: dir.path().join("config"),
        supervisor: sup,
        insecure_agent_header: false,
        artifact_listen: Some("127.0.0.1:0".into()),
        web: Default::default(),
    })
    .await
    .unwrap();
    let addr = daemon.artifact_addr.expect("artifact origin");
    let get = async |path: &str| {
        let stream = tokio::net::TcpStream::connect(addr).await.unwrap();
        let (mut sender, conn) = hyper::client::conn::http1::handshake(TokioIo::new(stream))
            .await
            .unwrap();
        tokio::spawn(conn);
        let req = Request::builder()
            .uri(path)
            .header("host", "localhost")
            .body(Full::new(Bytes::new()))
            .unwrap();
        sender.send_request(req).await.unwrap().status().as_u16()
    };
    // The origin answers; only the serving routes are there.
    assert_ne!(get("/a/ART-404/").await, 200);
    assert_eq!(get("/api/health").await, 404);
    daemon.shutdown();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn quota_hold_blocks_and_release_delivers() {
    use terrariumd::supervisor::{Supervisor, TurnHold};
    let td = Td::start().await;
    let id = td.agent("demo", "bob", json!({})).await;
    let agent: terrarium_core::AgentId = id.parse().unwrap();
    td.post(&format!("/api/agents/{id}/start"), None).await;
    td.wait_status(&id, "idle").await;

    let until = chrono::DateTime::parse_from_rfc3339("2030-01-01T05:00:00Z")
        .unwrap()
        .with_timezone(&chrono::Utc);
    td.sup
        .hold_turns(
            agent,
            Some(TurnHold {
                until: Some(until),
                reason: "the daily budget is used up".into(),
            }),
        )
        .await
        .unwrap();
    td.wait_status(&id, "blocked").await;
    let last = td.events(&id, "status").await.last().unwrap().clone();
    let reason = last["data"]["reason"].as_str().unwrap();
    assert!(reason.contains("2030-01-01T05:00:00Z"), "{reason}");

    // An operator turn and a push pointer wait.
    td.post(
        &format!("/api/agents/{id}/send"),
        Some(json!({"text": "held turn"})),
    )
    .await;
    let m = td
        .post("/api/mail", Some(json!({"to": "demo/bob", "body": "x"})))
        .await;
    let thread = m["thread_id"].as_str().unwrap().to_owned();
    tokio::time::sleep(Duration::from_millis(600)).await;
    assert!(td.events(&id, "turn_start").await.is_empty());
    assert_eq!(td.sup.waiting_turns(agent), ["held turn"]);
    assert_eq!(td.status(&id).await, "blocked");

    // The release sends the turn, then the pointer.
    td.sup.hold_turns(agent, None).await.unwrap();
    let starts = td.wait_events(&id, "turn_start", |e| e.len() >= 2).await;
    assert_eq!(starts[0]["data"]["prompt"], "held turn");
    assert!(
        starts[1]["data"]["prompt"]
            .as_str()
            .unwrap()
            .contains(&thread),
        "{starts:#?}"
    );
    td.wait_status(&id, "idle").await;
    assert!(td.sup.waiting_turns(agent).is_empty());
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn subscription_gates_the_start_and_injects_its_key() {
    let td = Td::start().await;
    td.post(
        "/api/subscriptions",
        Some(json!({"name": "keyed", "provider": "anthropic", "harnesses": ["claude"]})),
    )
    .await;
    td.post(
        "/api/subscriptions/keyed/api-key",
        Some(json!({"provider": "anthropic", "key": "sk-test-secret"})),
    )
    .await;
    td.post(
        "/api/subscriptions",
        Some(json!({"name": "pionly", "provider": "anthropic", "harnesses": ["pi"]})),
    )
    .await;

    // The key goes into the launch env; the command line hides it.
    let id = td
        .agent("demo", "bob", json!({"subscription": "keyed"}))
        .await;
    let state = td.daemon.as_ref().unwrap().state.clone();
    let key = id.clone();
    let view = state
        .db
        .read(move |c| terrariumd::store::agents::get(c, &key))
        .await
        .unwrap();
    let spec = td.sup.launch_spec(&view.agent, None).await.unwrap();
    assert_eq!(spec.env["ANTHROPIC_API_KEY"], "sk-test-secret");
    let line = terrariumd::sandbox::command_line(&spec);
    assert!(!line.contains("sk-test-secret"), "{line}");
    td.post(&format!("/api/agents/{id}/start"), None).await;
    td.wait_status(&id, "idle").await;

    // A harness that cannot spend the subscription: 422, and it stays stopped.
    for (name, sub, want) in [
        ("carl", "pionly", "cannot spend subscription `pionly`"),
        ("dora", "nosuch", "no such subscription"),
    ] {
        let id = td.agent("demo", name, json!({"subscription": sub})).await;
        let (s, b) = td
            .req(Method::POST, &format!("/api/agents/{id}/start"), None, None)
            .await;
        assert_eq!(s, 422, "{b}");
        let msg = b["error"]["message"].as_str().unwrap();
        assert!(msg.contains(want), "{msg}");
        assert_eq!(td.status(&id).await, "stopped");
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn frozen_prompt_change_starts_a_new_session() {
    let td = Td::start().await;
    td.adapter
        .frozen
        .store(true, std::sync::atomic::Ordering::SeqCst);
    let prompts = td.dir.path().join("prompts");
    std::fs::create_dir_all(&prompts).unwrap();
    std::fs::write(prompts.join("t.md"), "v1 {{agent.name}}").unwrap();
    let id = td.agent("demo", "bob", json!({"prompt": "t"})).await;
    let cycle = async |want: usize| {
        td.post(&format!("/api/agents/{id}/start"), None).await;
        td.wait_events(&id, "session_started", |e| e.len() == want)
            .await;
        td.wait_status(&id, "idle").await;
        td.post(&format!("/api/agents/{id}/stop"), None).await;
        td.wait_status(&id, "stopped").await;
        td.events(&id, "session_started").await
    };
    cycle(1).await;
    // The same prompt: a resume.
    let started = cycle(2).await;
    assert_eq!(started[1]["data"]["source"], "resume");
    assert_eq!(
        started[1]["data"]["harness_session"],
        started[0]["data"]["harness_session"]
    );
    // A changed prompt: a new session, with the reason.
    std::fs::write(prompts.join("t.md"), "v2 {{agent.name}}").unwrap();
    let started = cycle(3).await;
    assert_eq!(started[2]["data"]["source"], "startup");
    assert_ne!(
        started[2]["data"]["harness_session"],
        started[0]["data"]["harness_session"]
    );
    let statuses = td.events(&id, "status").await;
    assert!(
        statuses
            .iter()
            .any(|e| e["data"]["reason"] == "new session: the prompt changed"),
        "{statuses:#?}"
    );
    // The next start resumes the new session.
    let started = cycle(4).await;
    assert_eq!(started[3]["data"]["source"], "resume");
    assert_eq!(
        started[3]["data"]["harness_session"],
        started[2]["data"]["harness_session"]
    );
    // The flag off: a changed prompt resumes.
    td.req(
        Method::PATCH,
        &format!("/api/agents/{id}"),
        Some(json!({"new_session_on_prompt_change": false})),
        None,
    )
    .await;
    std::fs::write(prompts.join("t.md"), "v3 {{agent.name}}").unwrap();
    let started = cycle(5).await;
    assert_eq!(started[4]["data"]["source"], "resume");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn stop_types_the_exit_keys_first() {
    let td = Td::start().await;
    *td.adapter.exit_keys.lock().unwrap() = b"/exit 7\r".to_vec();
    let id = td.agent("demo", "bob", json!({})).await;
    td.post(&format!("/api/agents/{id}/start"), None).await;
    td.wait_status(&id, "idle").await;
    td.post(&format!("/api/agents/{id}/stop"), None).await;
    td.wait_status(&id, "stopped").await;
    // The harness exited on its keys (code 7), not on a kill.
    let exits = td.wait_events(&id, "exit", |e| !e.is_empty()).await;
    assert_eq!(exits[0]["data"]["code"], 7, "{exits:#?}");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn pi_refuses_a_claude_plan() {
    let td = Td::start().await;
    td.post(
        "/api/subscriptions",
        Some(
            json!({"name": "max", "provider": "anthropic", "plan": "Max 20x",
                    "harnesses": ["claude", "pi"]}),
        ),
    )
    .await;
    let id = td
        .agent(
            "demo",
            "p1",
            json!({"harness": "pi", "subscription": "max"}),
        )
        .await;
    let (s, b) = td
        .req(Method::POST, &format!("/api/agents/{id}/start"), None, None)
        .await;
    assert_eq!(s, 422, "{b}");
    let msg = b["error"]["message"].as_str().unwrap();
    assert!(msg.contains("pi cannot spend a Claude plan"), "{msg}");
}
