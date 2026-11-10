//! The real supervisor (`RunnerSupervisor`) with sandbox mode `none` and the
//! runner's fake TUI (`crates/terrarium-runner/tests/fake_tui/fake_tui.py`)
//! behind a test adapter: lifecycle, events over SSE, the PTY, push
//! delivery, the restart policy, a daemon restart with resume, the agent
//! RPC tunnel, and long-poll.
#![allow(clippy::unwrap_used, clippy::expect_used)]

use crate::common;

use std::collections::{BTreeMap, HashMap};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use futures_util::{SinkExt, StreamExt};
use http_body_util::{BodyExt, Full};
use hyper::body::Bytes;
use hyper::{Method, Request};
use hyper_util::rt::TokioIo;
use semver::Version;
use serde_json::{Value, json};
use terrarium_core::{
    Adapter, Agent, AgentId, Event, HarnessId, HarnessSessionId, HookPayload, LaunchSpec,
};
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

/// The deadline of the waits for a state. Under load one start of the fake
/// TUI took 18 s (TER-117), so 15 s was too short. Only a failing test
/// waits it out.
const WAIT: Duration = Duration::from_secs(45);
/// The lead nudge rate limit in these tests. Long enough that the requests
/// a test makes inside one interval fit in it on a loaded machine. 4 s was
/// too short at a load of 70 to 100: the hub listener handles each card
/// event after a store read, and a move landed after the flush (TER-141).
const LEAD_NUDGE: Duration = Duration::from_secs(10);

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
    /// `exit_key_pause`: the pause before the CR of the exit keys.
    exit_pause: Mutex<Duration>,
    /// `waits_for_startup_hook`, as pi says: the agent stays `starting`
    /// until the `SessionStart` hook.
    startup_hook: std::sync::atomic::AtomicBool,
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
    fn exit_key_pause(&self) -> Duration {
        *self.exit_pause.lock().unwrap()
    }
    fn waits_for_startup_hook(&self) -> bool {
        self.startup_hook.load(std::sync::atomic::Ordering::SeqCst)
    }
    fn is_startup_hook(&self, hook: &HookPayload) -> bool {
        hook.hook == "SessionStart"
    }
    fn hook_event(&self, hook: &HookPayload) -> Vec<Event> {
        match hook.hook.as_str() {
            "UserPromptSubmit" => vec![Event::TurnStart {
                prompt: hook.str_field("prompt").map(str::to_owned),
            }],
            "Stop" => vec![Event::TurnEnd],
            "PreCompact" => vec![Event::CompactStarted {
                trigger: hook.str_field("trigger").map(str::to_owned),
            }],
            "PostCompact" => vec![Event::CompactFinished {
                ok: true,
                trigger: hook.str_field("trigger").map(str::to_owned),
                message: None,
            }],
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
        Self::start_full(insecure, FakeAdapter::default()).await
    }

    async fn start_full(insecure: bool, adapter: FakeAdapter) -> Self {
        let dir = tempfile::tempdir().unwrap();
        if let Some(bin) = process_mode_bin() {
            sweep_stale_runners_once(&bin);
        }
        let adapter = Arc::new(adapter);
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
        common::seed_database(&self.dir.path().join("state"));
        let daemon = terrariumd::start(DaemonConfig {
            socket: self.socket.clone(),
            listen: None,
            token: None,
            state_dir: self.dir.path().join("state"),
            config_dir: self.dir.path().join("config"),
            work_dir: self.dir.path().join("state").with_file_name("work"),
            max_targets_per_project: terrariumd::settings::DEFAULT_MAX_TARGETS_PER_PROJECT,
            repos_dir: std::path::PathBuf::from("/repos"),
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
        // The runner binary can be of another build: no upgrade restart.
        cfg.version = "0.0.0".into();
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
    cfg.lead_nudge_interval = LEAD_NUDGE;
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
async fn pending_runner_api_merges_queues_and_refuses_after_real_handoff() {
    use terrariumd::supervisor::Supervisor;
    let td = Td::start().await;
    let id = td.agent("demo", "bob", json!({})).await;
    td.post(&format!("/api/agents/{id}/start"), None).await;
    td.wait_status(&id, "idle").await;
    hold(&td, &id).await;
    td.post(
        &format!("/api/agents/{id}/send"),
        Some(json!({"text": "runner first"})),
    )
    .await;
    let agent = id.parse().unwrap();
    td.sup.reset_gate(agent, true);
    td.post(
        &format!("/api/agents/{id}/send"),
        Some(json!({"text": "slot second"})),
    )
    .await;
    let queued = td.get(&format!("/api/agents/{id}/queued")).await;
    let items = queued["items"].as_array().unwrap();
    assert_eq!(
        items
            .iter()
            .map(|t| t["source"].as_str().unwrap())
            .collect::<Vec<_>>(),
        ["runner", "supervisor"]
    );
    assert_eq!(items[0]["body"], "runner first");
    assert_eq!(items[1]["body"], "slot second");
    let runner_id = items[0]["id"].as_str().unwrap();
    let slot_id = items[1]["id"].as_str().unwrap();
    let path = format!("/api/agents/{id}/queued/{runner_id}?source=runner");
    assert_eq!(td.req(Method::DELETE, &path, None, None).await.0, 204);
    assert_eq!(td.req(Method::DELETE, &path, None, None).await.0, 404);
    // Opening the gate runs deliver_pending and transfers the slot turn to the real runner.
    td.sup.reset_gate(agent, false);
    let path = format!("/api/agents/{id}/queued/{slot_id}?source=supervisor");
    assert_eq!(td.req(Method::DELETE, &path, None, None).await.0, 404);
    let queued = td.get(&format!("/api/agents/{id}/queued")).await;
    let live_id = queued["items"][0]["id"].as_str().unwrap().to_owned();
    assert_eq!(queued["items"][0]["source"], "runner");
    end_hold(&td, &id).await;
    td.wait_status(&id, "idle").await;
    let start = Instant::now();
    while !prompts(&td, &id)
        .await
        .iter()
        .any(|t| t == "[User] slot second")
    {
        assert!(start.elapsed() < WAIT);
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    let path = format!("/api/agents/{id}/queued/{live_id}?source=runner");
    assert_eq!(td.req(Method::DELETE, &path, None, None).await.0, 404);
    assert!(
        !prompts(&td, &id)
            .await
            .iter()
            .any(|t| t == "[User] runner first")
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn idle_stop_refuses_a_turn_handed_over_after_the_idle_check() {
    use terrarium_core::AgentStatus;
    use terrariumd::supervisor::{Supervisor, SupervisorError};

    let td = Td::start().await;
    let id = td.agent("demo", "bob", json!({})).await;
    td.post(&format!("/api/agents/{id}/start"), None).await;
    td.wait_status(&id, "idle").await;
    let agent = id.parse().unwrap();
    assert_eq!(td.sup.status(agent).await, Some(AgentStatus::Idle));
    // No await for a working status: the reservation must protect even
    // the interval before the runner has taken the turn.
    td.sup
        .send_turn(agent, "do not stop this turn".into())
        .await
        .unwrap();
    assert!(matches!(
        td.sup.stop_idle(agent).await,
        Err(SupervisorError::NotReady(_))
    ));
    td.wait_events(&id, "turn_start", |e| !e.is_empty()).await;
    td.wait_status(&id, "idle").await;
    td.sup.stop_idle(agent).await.unwrap();
    td.wait_status(&id, "stopped").await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn idle_stop_refuses_a_pending_push_pointer() {
    use terrariumd::supervisor::{Supervisor, SupervisorError};

    let td = Td::start().await;
    let id = td.agent("demo", "bob", json!({})).await;
    td.post(&format!("/api/agents/{id}/start"), None).await;
    td.wait_status(&id, "idle").await;
    let agent = id.parse().unwrap();
    // Hold the pointer before typing, while the live runner is idle.
    td.sup.reset_gate(agent, true);
    td.sup.lease_granted(agent, "box".into(), "lease".into());
    assert!(matches!(
        td.sup.stop_idle(agent).await,
        Err(SupervisorError::NotReady(_))
    ));
    td.sup.stop(agent).await.unwrap();
    td.wait_status(&id, "stopped").await;
    assert!(td.sup.send_turn(agent, "too late".into()).await.is_err());
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn idle_stop_queues_a_turn_during_the_graceful_stop() {
    use terrariumd::supervisor::Supervisor;

    let td = Td::start().await;
    *td.adapter.exit_keys.lock().unwrap() = b"/exit 0\r".to_vec();
    *td.adapter.exit_pause.lock().unwrap() = WAIT;
    let id = td.agent("demo", "bob", json!({})).await;
    td.post(&format!("/api/agents/{id}/start"), None).await;
    td.wait_status(&id, "idle").await;
    let agent = id.parse().unwrap();
    let runner = td.sup.runner(agent).unwrap();
    td.daemon
        .as_ref()
        .unwrap()
        .state
        .lifecycle
        .expect_stop(agent, "idle");
    td.sup.stop_idle(agent).await.unwrap();
    let result = td
        .post(
            &format!("/api/agents/{id}/send"),
            Some(json!({"text": "after idle stop"})),
        )
        .await;
    assert_eq!(result["queued"], true, "{result}");
    // Finish the split exit keys only after the API has persisted the turn.
    runner.write(b"\r").await.unwrap();
    td.wait_events(&id, "turn_start", |events| {
        events
            .iter()
            .any(|e| e["data"]["prompt"] == "[User] after idle stop")
    })
    .await;
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
    assert_eq!(starts.last().unwrap()["data"]["prompt"], "[User] hello");

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

/// Any harness version starts: the fake reports 1.0.0, far from the pin,
/// and the start has no `warning` event. `GET /api/harnesses` shows the
/// version, with no range and no policy.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn any_harness_version_starts_with_no_warning() {
    let td = Td::start().await;
    let id = td.agent("demo", "old", json!({})).await;
    let (s, b) = td
        .req(Method::POST, &format!("/api/agents/{id}/start"), None, None)
        .await;
    assert_eq!(s, 202, "{b}");
    td.wait_status(&id, "idle").await;
    assert!(td.events(&id, "warning").await.is_empty());

    let r = td.get("/api/harnesses").await;
    assert!(r.get("policy").is_none(), "{r}");
    let claude = r["harnesses"]
        .as_array()
        .unwrap()
        .iter()
        .find(|h| h["harness"] == "claude")
        .unwrap()
        .clone();
    assert_eq!(claude["installed"], "1.0.0");
    assert_eq!(claude["base"], terrarium_core::harnesses::claude::VERSION);
    // `pinned` is a deprecated copy of `base`, not the runtime pin.
    assert_eq!(claude["pinned"], claude["base"]);
    assert!(claude.get("tested").is_none(), "{claude}");
    assert!(claude.get("in_range").is_none(), "{claude}");
    td.post(&format!("/api/agents/{id}/stop"), None).await;
}

/// F11: an unsafe entry in `private/` that the quarantine does not catch (a
/// directory with a record name) refuses the launch. The agent goes to
/// `error` with the path in the reason, and the restart policy does not try
/// again. A planted symlink at `private/` goes to quarantine, and the
/// launch goes on.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn unsafe_private_dir_is_an_error_without_a_restart_loop() {
    let td = Td::start().await;
    let id = td
        .agent("demo", "planted", json!({"restart": "always"}))
        .await;
    let agent_dir = td.dir.path().join("state/agents").join(&id);
    let private = agent_dir.join("private");

    // A symlink at `private/`: quarantine, then a clean launch.
    let elsewhere = td.dir.path().join("elsewhere");
    std::fs::create_dir_all(&elsewhere).unwrap();
    std::fs::create_dir_all(&agent_dir).unwrap();
    std::os::unix::fs::symlink(&elsewhere, &private).unwrap();
    td.post(&format!("/api/agents/{id}/start"), None).await;
    td.wait_status(&id, "idle").await;
    assert!(!private.is_symlink());
    assert!(std::fs::read_dir(&elsewhere).unwrap().next().is_none());
    td.post(&format!("/api/agents/{id}/stop"), None).await;
    td.wait_status(&id, "stopped").await;

    // A directory where a record goes: the start is refused.
    let bad = private.join("pty.log.1");
    std::fs::create_dir(&bad).unwrap();
    let (s, b) = td
        .req(Method::POST, &format!("/api/agents/{id}/start"), None, None)
        .await;
    assert_eq!(s, 422, "{b}");
    let bad_text = bad.display().to_string();
    assert!(
        b["error"]["message"].as_str().unwrap().contains(&bad_text),
        "{b}"
    );
    td.wait_status(&id, "error").await;
    let last = td.events(&id, "status").await.last().unwrap().clone();
    let reason = last["data"]["reason"].as_str().unwrap();
    assert!(reason.contains(&bad_text), "{last}");
    assert!(reason.contains("not a regular file"), "{last}");
    // No restart: the status stays `error`, with no `restarting`.
    tokio::time::sleep(Duration::from_millis(500)).await;
    assert_eq!(td.status(&id).await, "error");
    let n = td.events(&id, "status").await.len();

    // The same entry, planted while the harness runs: the relaunch after a
    // crash fails once, and the policy stops there.
    std::fs::remove_dir(&bad).unwrap();
    td.post(&format!("/api/agents/{id}/start"), None).await;
    td.wait_status(&id, "idle").await;
    std::fs::create_dir(&bad).unwrap();
    td.post(
        &format!("/api/agents/{id}/send"),
        Some(json!({"text": "/exit 1"})),
    )
    .await;
    let statuses = td
        .wait_events(&id, "status", |e| {
            e.len() > n && e.last().is_some_and(|x| x["data"]["status"] == "error")
        })
        .await;
    let last = statuses.last().unwrap();
    let reason = last["data"]["reason"].as_str().unwrap();
    assert!(reason.contains("restart failed"), "{last}");
    assert!(reason.contains(&bad_text), "{last}");
    tokio::time::sleep(Duration::from_millis(500)).await;
    let after = td.events(&id, "status").await;
    assert_eq!(after.len(), statuses.len(), "{after:#?}");
    assert_eq!(td.status(&id).await, "error");
    let restarts = after[n..]
        .iter()
        .filter(|x| x["data"]["status"] == "restarting")
        .count();
    assert_eq!(restarts, 1, "{after:#?}");
}

/// `DELETE /api/agents/{id}` stops a live agent and moves its dir to
/// `<state>/trash/<ulid>-<ts>/`, with the event log in it.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn delete_moves_the_agent_dir_to_the_trash() {
    let td = Td::start().await;
    let id = td.agent("demo", "gone", json!({})).await;
    td.post(&format!("/api/agents/{id}/start"), None).await;
    td.wait_status(&id, "idle").await;
    let state = td.dir.path().join("state");
    let agent_dir = state.join("agents").join(&id);
    assert!(agent_dir.join("private/events.jsonl").is_file());
    let ws = td.dir.path().join("work/demo/gone");
    std::fs::create_dir_all(ws.join("app")).unwrap();
    std::fs::write(ws.join("app/main.py"), "print(1)\n").unwrap();
    let (s, b) = td
        .req(Method::DELETE, &format!("/api/agents/{id}"), None, None)
        .await;
    assert_eq!(s, 409, "a live agent needs confirm: {b}");
    let (s, b) = td
        .req(
            Method::DELETE,
            &format!("/api/agents/{id}?confirm=true"),
            None,
            None,
        )
        .await;
    assert_eq!(s, 204, "{b}");
    assert!(!agent_dir.exists());
    let trash: Vec<PathBuf> = std::fs::read_dir(state.join("trash"))
        .unwrap()
        .map(|e| e.unwrap().path())
        .collect();
    assert_eq!(trash.len(), 1, "{trash:?}");
    let name = trash[0].file_name().unwrap().to_string_lossy().into_owned();
    assert!(name.starts_with(&format!("{id}-")), "{name}");
    assert!(trash[0].join("private/events.jsonl").is_file());
    // The workspace in the work root goes into the same trash entry.
    assert!(!ws.exists());
    assert!(trash[0].join("workspace/app/main.py").is_file());
    // The runner does not make the dir again after the move.
    tokio::time::sleep(Duration::from_millis(300)).await;
    assert!(!agent_dir.exists());
    assert!(!ws.exists());

    // `?keep_workspace=true` leaves the workspace in place.
    let kept = td.agent("demo", "kept", json!({})).await;
    let kws = td.dir.path().join("work/demo/kept");
    std::fs::create_dir_all(kws.join("app")).unwrap();
    let (s, b) = td
        .req(
            Method::DELETE,
            &format!("/api/agents/{kept}?keep_workspace=true"),
            None,
            None,
        )
        .await;
    assert_eq!(s, 204, "{b}");
    assert!(kws.join("app").is_dir());
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn project_inbox_mail_reaches_lead_as_a_turn() {
    let td = Td::start().await;
    td.agent("source", "worker", json!({})).await;
    let id = td
        .agent("destination", "lead", json!({"roles": ["project_admin"]}))
        .await;
    // A project inbox event must wake its stopped lead automatically.
    let (status, m) = td
        .req(
            Method::POST,
            "/api/mail",
            Some(
                json!({"to": "project:destination", "subject": "release", "body": "deploy notes"}),
            ),
            Some("source/worker"),
        )
        .await;
    assert_eq!(status, 201);
    assert_eq!(m["state"], "queued");
    let mid = m["id"].as_str().unwrap();
    let want = format!(
        "[Mailbox:source/worker] Subject: release (thread {}, message {mid})\n> deploy notes",
        m["thread_id"].as_str().unwrap()
    );
    td.wait_events(&id, "turn_start", |es| {
        es.iter().any(|e| e["data"]["prompt"] == want)
    })
    .await;
    td.wait_status(&id, "idle").await;
    assert_eq!(td.get(&format!("/api/mail/{mid}")).await["read_by"], id);
    // A live event must route to the same lead without rewriting the inbox address.
    let (status, live) = td
        .req(
            Method::POST,
            "/api/projects/destination/inbox",
            Some(json!({"body": "live notes"})),
            Some("source/worker"),
        )
        .await;
    assert_eq!(status, 201);
    let live_id = live["id"].as_str().unwrap();
    td.wait_events(&id, "turn_start", |es| {
        es.iter().any(|e| {
            e["data"]["prompt"]
                .as_str()
                .is_some_and(|p| p.contains(live_id) && p.contains("> live notes"))
        })
    })
    .await;
    assert_eq!(live["to"], "project:destination");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn queued_project_mail_survives_restart_and_wakes_stopped_lead() {
    let mut td = Td::start().await;
    td.agent("source", "worker", json!({})).await;
    let id = td
        .agent("destination", "lead", json!({"roles": ["project_admin"]}))
        .await;
    // Pause the stopped lead so the live event cannot deliver the message.
    td.post(&format!("/api/agents/{id}/pause"), None).await;
    assert_eq!(td.status(&id).await, "stopped");
    let (status, mail) = td
        .req(
            Method::POST,
            "/api/mail",
            Some(json!({"to": "project:destination", "body": "queued release"})),
            Some("source/worker"),
        )
        .await;
    assert_eq!(status, 201);
    assert_eq!(mail["state"], "queued");
    let key = id.clone();
    td.daemon
        .as_ref()
        .unwrap()
        .state
        .db
        .read(move |c| {
            assert_eq!(terrariumd::store::agents::queued_count(c, &key)?, 1);
            assert!(terrariumd::store::agents::with_queued_work(c)?.contains(&key));
            Ok(())
        })
        .await
        .unwrap();
    // Discard the supervisor's in-memory live pointer, then wake from stored work.
    td.restart_daemon().await;
    assert_eq!(td.status(&id).await, "stopped");
    td.post(&format!("/api/agents/{id}/unpause"), None).await;
    let mid = mail["id"].as_str().unwrap();
    td.wait_events(&id, "turn_start", |es| {
        es.iter().any(|e| {
            e["data"]["prompt"]
                .as_str()
                .is_some_and(|p| p.contains(mid) && p.contains("> queued release"))
        })
    })
    .await;
    td.wait_status(&id, "idle").await;
    assert_eq!(td.get(&format!("/api/mail/{mid}")).await["read_by"], id);
}

#[tokio::test]
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
    let mid = m["id"].as_str().unwrap().to_owned();
    // The turn is tagged, with the body quoted (PLAN §8.5).
    let want =
        format!("[Mailbox:operator] Subject:  (thread {thread}, message {mid})\n> secret body");
    let starts = td.wait_events(&id, "turn_start", |e| !e.is_empty()).await;
    assert_eq!(starts[0]["data"]["prompt"], want.as_str());
    td.wait_status(&id, "idle").await;
    // The turn held the whole body: the mail is read, by the agent.
    let msg = td.get(&format!("/api/mail/{mid}")).await;
    assert_eq!(msg["state"], "read");
    assert_eq!(msg["read_by"], id.as_str());

    // Working: the mail waits for the stop hook, and one pointer covers it
    // all, with a card that was assigned meanwhile. The turn stays open
    // until the test ends it: a turn of a set time could end between the
    // requests below on a loaded machine. A later pointer for the card
    // would still be by design, so the test reads the pushes in order
    // until the card block has arrived.
    hold(&td, &id).await;
    let a = td
        .post("/api/mail", Some(json!({"to": "demo/bob", "body": "one"})))
        .await;
    let t2 = a["thread_id"].as_str().unwrap().to_owned();
    let b = td
        .post(
            "/api/mail",
            Some(json!({"to": "demo/bob", "body": "two", "thread_id": t2})),
        )
        .await;
    let (ma, mb) = (a["id"].as_str().unwrap(), b["id"].as_str().unwrap());
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
    end_hold(&td, &id).await;
    // Wait with a deadline until the card block has been typed, in one push
    // or in a later one.
    let prompt = |x: &Value| x["data"]["prompt"].as_str().unwrap_or("").to_owned();
    let starts = td
        .wait_events(&id, "turn_start", |e| {
            e.iter().any(|x| prompt(x).contains(&card_id))
        })
        .await;
    // The pushes that carry the mails or the card, in the order they went
    // out. Joined, they are the three blocks, each once.
    let pushes: Vec<String> = starts
        .iter()
        .map(prompt)
        .filter(|p| p.contains(&t2) || p.contains(&card_id))
        .collect();
    let mail_a = format!("[Mailbox:operator] Subject:  (thread {t2}, message {ma})\n> one");
    let mail_b = format!("[Mailbox:operator] Subject:  (thread {t2}, message {mb})\n> two");
    let card_block = format!(
        "[System:card] Card {card_id} was assigned to you. Run `terrarium card show {card_id}`."
    );
    assert_eq!(
        pushes.join("\n\n"),
        format!("{mail_a}\n\n{mail_b}\n\n{card_block}"),
        "{starts:#?}"
    );
    // Both mails are in one push: the pending pointer covers them.
    assert!(
        pushes
            .iter()
            .any(|p| p.contains(&mail_a) && p.contains(&mail_b)),
        "one pointer for both messages: {pushes:#?}"
    );
}

/// A local repo under the repos dir is bound read-write into the sandbox
/// of each agent of its project, at the path of its URL. The agents of
/// another project do not get it, and a file URL outside the repos dir is
/// never bound. The launch also gives git an identity.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn local_repos_bind_into_the_sandbox_of_their_project_only() {
    let dir = tempfile::tempdir().unwrap();
    let repos = dir.path().join("repos");
    let outside = dir.path().join("outside");
    for d in [
        repos.join("app.git"),
        repos.join("lib.git"),
        outside.join("x.git"),
    ] {
        std::fs::create_dir_all(d).unwrap();
    }
    let adapter = Arc::new(FakeAdapter::default());
    let mut sup = supervisor(dir.path(), adapter.clone());
    // `supervisor` makes a sandbox-mode `none` config; this test needs the
    // bwrap argv, and starts no harness.
    let mut cfg = sup.config().clone();
    cfg.sandbox = SandboxConfig {
        bwrap: PathBuf::from("/usr/bin/bwrap"),
        repos_dir: repos.clone(),
        ..SandboxConfig::default()
    };
    sup = RunnerSupervisor::new(cfg);
    let mut td = Td {
        _guard: RunnerProcessGuard::new(dir.path().join("run")),
        socket: dir.path().join("api.sock"),
        sup: Arc::new(sup),
        dir,
        adapter,
        daemon: None,
    };
    td.boot(true).await;
    let w = td.agent("demo", "w", json!({})).await;
    let x = td.agent("other", "x", json!({})).await;
    let url = |p: PathBuf| format!("file://{}", p.display());
    for (project, u) in [
        ("demo", url(repos.join("app.git"))),
        ("demo", url(outside.join("x.git"))),
        ("other", url(repos.join("lib.git"))),
    ] {
        td.post(
            &format!("/api/projects/{project}/repos"),
            Some(json!({"url": u})),
        )
        .await;
    }
    let state = td.daemon.as_ref().unwrap().state.clone();
    let real = std::fs::canonicalize(&repos).unwrap();
    for (id, mine, theirs) in [(&w, "app.git", "lib.git"), (&x, "lib.git", "app.git")] {
        let key = id.clone();
        let agent = state
            .db
            .read(move |c| terrariumd::store::agents::get(c, &key))
            .await
            .unwrap()
            .agent;
        let spec = td.sup.launch_spec(&agent, None).await.unwrap();
        let a = &spec.args;
        let bind = |name: &str| {
            a.windows(3).any(|w| {
                w[0] == "--bind"
                    && w[1] == real.join(name).display().to_string()
                    && w[2] == repos.join(name).display().to_string()
            })
        };
        assert!(bind(mine), "{} has no bind of {mine}: {a:?}", agent.name);
        assert!(
            !bind(theirs),
            "{} has a bind of {theirs}: {a:?}",
            agent.name
        );
        let line = a.join(" ");
        assert!(!line.contains(&outside.display().to_string()), "{line}");
        assert_eq!(spec.env["GIT_AUTHOR_NAME"], "Terrarium");
        assert_eq!(spec.env["GIT_COMMITTER_EMAIL"], "terrarium@localhost");
    }
}

/// One API call through an agent's runner socket (the RPC tunnel).
async fn tunnel(sock: &Path, method: &str, path: &str, body: Value) -> (u64, Value) {
    let mut params = json!({"method": method, "path": path});
    if let Some((p, q)) = path.split_once('?') {
        params["path"] = p.into();
        let (k, v) = q.split_once('=').unwrap();
        params["query"] = json!({ k: v });
    }
    if !body.is_null() {
        params["body"] = body;
    }
    let r = rpc(sock, "http", params).await;
    (
        r["result"]["status"].as_u64().unwrap_or(0),
        r["result"]["body"].clone(),
    )
}

/// The API calls of the leader/worker flow, through the RPC tunnel, as the
/// CLI makes them (`leader_worker_flow_commands` in the CLI tests). A
/// `project_admin` makes, starts, stops, and removes a worker of its own
/// project, and assigns it a card. A `sysadmin` makes a project, attaches
/// a local repo, and makes and starts the project's lead.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn lead_and_sysadmin_manage_agents_through_the_tunnel() {
    let td = Td::start().await;
    let prompts = td.dir.path().join("prompts");
    std::fs::create_dir_all(&prompts).unwrap();
    let repo_prompts = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../defaults/prompts");
    for name in ["default", "project-lead", "system-lead"] {
        std::fs::copy(
            repo_prompts.join(format!("{name}.md")),
            prompts.join(format!("{name}.md")),
        )
        .unwrap();
    }
    let lead = td
        .agent("demo", "lead", json!({"roles": ["project_admin"]}))
        .await;
    let root = td
        .agent("ops", "root", json!({"roles": ["sysadmin"]}))
        .await;
    for id in [&lead, &root] {
        td.post(&format!("/api/agents/{id}/start"), None).await;
        td.wait_status(id, "idle").await;
    }
    let card = td
        .post(
            "/api/projects/demo/cards",
            Some(json!({"title": "T", "column": "ready"})),
        )
        .await;
    let card = card["id"].as_str().unwrap().to_owned();
    let ls = td.runner_socket(&lead);
    let call = |m: &'static str, p: String, b: Value| {
        let s = ls.clone();
        async move { tunnel(&s, m, &p, b).await }
    };

    // Project lead: a worker in its own project.
    let (s, b) = call(
        "POST",
        "/api/agents".into(),
        json!({"name": "w-1", "project": "demo", "harness": "claude",
               "model": "m", "prompt": "default"}),
    )
    .await;
    assert_eq!(s, 201, "{b}");
    let w = b["id"].as_str().unwrap().to_owned();
    let (s, b) = call(
        "POST",
        format!("/api/cards/{card}/assign"),
        json!({"assignee": "w-1"}),
    )
    .await;
    assert_eq!((s, b["assignee"].as_str()), (200, Some(w.as_str())), "{b}");
    // The assignment wakes the worker (PLAN §18): the lead does not start
    // it. The lead may pause and unpause it.
    td.wait_status(&w, "idle").await;
    let (s, b) = call("POST", "/api/agents/w-1/pause".into(), Value::Null).await;
    assert!((200..300).contains(&s), "{s} {b}");
    assert_eq!(b["paused"], true, "{b}");
    td.wait_status(&w, "stopped").await;
    let (s, b) = call("POST", "/api/agents/w-1/unpause".into(), Value::Null).await;
    assert!((200..300).contains(&s), "{s} {b}");
    assert_eq!(b["paused"], false, "{b}");
    let (s, b) = call("DELETE", "/api/agents/w-1?confirm=true".into(), Value::Null).await;
    assert_eq!(s, 204, "{b}");
    // Not in another project, and no project of its own.
    let (s, _) = call(
        "POST",
        "/api/agents".into(),
        json!({"name": "w-2", "project": "ops", "harness": "claude"}),
    )
    .await;
    assert_eq!(s, 403);
    let (s, _) = call("POST", "/api/projects".into(), json!({"name": "mine"})).await;
    assert_eq!(s, 403);

    // System lead: a project, its local repo, and its lead.
    let rs = td.runner_socket(&root);
    let (s, b) = tunnel(
        &rs,
        "POST",
        "/api/projects",
        json!({"name": "shop", "card_prefix": "SHP"}),
    )
    .await;
    assert_eq!(s, 201, "{b}");
    let (s, b) = tunnel(
        &rs,
        "POST",
        "/api/projects/shop/repos",
        json!({"url": "file:///repos/shop.git", "name": "app", "default_branch": "main"}),
    )
    .await;
    assert_eq!(s, 201, "{b}");
    let (s, b) = tunnel(
        &rs,
        "POST",
        "/api/agents",
        json!({"name": "lead", "project": "shop", "harness": "claude", "model": "m",
               "prompt": "project-lead", "roles": ["project_admin"]}),
    )
    .await;
    assert_eq!(s, 201, "{b}");
    assert_eq!(b["roles"], json!(["project_admin"]));
    let shop_lead = b["id"].as_str().unwrap().to_owned();
    let (s, b) = tunnel(&rs, "POST", "/api/agents/shop%2Flead/start", Value::Null).await;
    assert!((200..300).contains(&s), "{s} {b}");
    td.wait_status(&shop_lead, "idle").await;
    let prompt = std::fs::read_to_string(
        td.dir
            .path()
            .join("state/agents")
            .join(&shop_lead)
            .join("harness/prompt.md"),
    )
    .unwrap();
    assert!(
        prompt.contains("ops/leader-log"),
        "the project-lead template"
    );
}

/// The prompts that the push path typed into an agent.
async fn prompts(td: &Td, id: &str) -> Vec<String> {
    td.events(id, "turn_start")
        .await
        .iter()
        .map(|e| e["data"]["prompt"].as_str().unwrap_or("").to_owned())
        .collect()
}

/// Start a turn of agent `id` that stays open until [`end_hold`]: the fake
/// TUI's `/hold`. A test that needs work to land inside one turn holds it
/// open, so a slow machine cannot end the turn first (TER-117). Returns
/// when the agent is `working`.
async fn hold(td: &Td, id: &str) {
    td.post(
        &format!("/api/agents/{id}/send"),
        Some(json!({"text": "/hold"})),
    )
    .await;
    td.wait_status(id, "working").await;
}

/// End the open `/hold` turn of agent `id`: type one line into it.
async fn end_hold(td: &Td, id: &str) {
    td.post(
        &format!("/api/agents/{id}/send"),
        Some(json!({"text": "go", "raw": true})),
    )
    .await;
}

/// Lead nudges: a card with no assignee in `ready`, and a move to `review`
/// or `blocked`, type a pointer into each idle `project_admin` of the
/// project. The agent that made the change, the other members, and the
/// leads of other projects get none. Changes inside the rate limit go out
/// as one pointer.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn board_changes_nudge_the_project_leads() {
    let td = Td::start().await;
    // Not about the card reminder: it would add a turn after the idle.
    td.daemon
        .as_ref()
        .unwrap()
        .state
        .lifecycle
        .reminders
        .set_default(false);
    let lead = td
        .agent("demo", "lead", json!({"roles": ["project_admin"]}))
        .await;
    let bob = td.agent("demo", "bob", json!({})).await;
    let olead = td
        .agent("other", "olead", json!({"roles": ["project_admin"]}))
        .await;
    // Isolate the rate-limited nudge turns from immediate human-comment
    // mail, which would otherwise be the second turn before the interval.
    let (status, body) = td
        .req(
            Method::PATCH,
            "/api/projects/demo",
            Some(json!({"human_comment_notifications": false})),
            None,
        )
        .await;
    assert_eq!(status, 200, "{body}");
    for id in [&lead, &bob, &olead] {
        td.post(&format!("/api/agents/{id}/start"), None).await;
    }
    for id in [&lead, &bob, &olead] {
        td.wait_status(id, "idle").await;
    }

    // Bob claims his card, and the operator makes a second card, before
    // the first nudge: none of this nudges, and then only the two moves
    // below have to fit in one rate-limit interval on a slow machine.
    let as_bob = |path: String, body: Value| {
        let td = &td;
        async move {
            let (s, b) = td
                .req(Method::POST, &path, Some(body), Some("demo/bob"))
                .await;
            assert!((200..300).contains(&s), "POST {path}: {s} {b}");
        }
    };
    let cb = td
        .post("/api/projects/demo/cards", Some(json!({"title": "Bob's"})))
        .await;
    let cb = cb["id"].as_str().unwrap().to_owned();
    as_bob(format!("/api/cards/{cb}/claim"), json!({})).await;
    as_bob(
        format!("/api/cards/{cb}/move"),
        json!({"column": "in_progress"}),
    )
    .await;
    let c2 = td
        .post("/api/projects/demo/cards", Some(json!({"title": "Two"})))
        .await;
    let c2 = c2["id"].as_str().unwrap().to_owned();

    // A new card in ready, from the operator: one pointer at once. Its
    // flush starts the interval, so the interval starts after `first`.
    let first = Instant::now();
    let c1 = td
        .post(
            "/api/projects/demo/cards",
            Some(json!({"title": "Ship it\nnow", "column": "ready"})),
        )
        .await;
    let c1 = c1["id"].as_str().unwrap().to_owned();

    // Inside the rate limit: bob moves his card to review, and the
    // operator blocks another card. One pointer carries both, at the end
    // of the interval. The moves come at once, before the wait for the
    // first pointer: on a loaded machine that wait alone can last most of
    // an interval. The daemon handles the card events in order, so the
    // first flush always comes before these two.
    as_bob(format!("/api/cards/{cb}/move"), json!({"column": "review"})).await;
    td.post(
        &format!("/api/cards/{c2}/move"),
        Some(json!({"column": "blocked", "comment": "needs the API key"})),
    )
    .await;
    let starts = td.wait_events(&lead, "turn_start", |e| e.len() >= 2).await;
    assert_eq!(
        starts[0]["data"]["prompt"],
        format!(
            "[System:nudge] New card {c1} in ready: Ship it…. Run `terrarium card ls --unassigned --column ready`."
        )
        .as_str()
    );
    // The batch goes out one full interval after the first flush at the
    // earliest, and that flush came after `first`. Slow requests cannot make
    // this fail; a batch that does not wait fails it on a normal machine.
    assert!(
        first.elapsed() >= LEAD_NUDGE,
        "the batch waited for the interval: {:?}",
        first.elapsed()
    );
    assert_eq!(
        starts[1]["data"]["prompt"],
        format!(
            "[System:nudge] {cb} moved to review by demo/bob. Run `terrarium card show {cb}`.\n\n\
             [System:nudge] {c2} moved to blocked by operator: needs the API key. Run `terrarium card show {c2}`."
        )
        .as_str()
    );
    td.wait_status(&lead, "idle").await;

    // The lead's own change, a card in ready with an assignee, and a move
    // to another column nudge nobody.
    let (s, b) = td
        .req(
            Method::POST,
            &format!("/api/cards/{c2}/move"),
            Some(json!({"column": "review"})),
            Some("demo/lead"),
        )
        .await;
    assert_eq!(s, 200, "{b}");
    let c3 = td
        .post(
            "/api/projects/demo/cards",
            Some(json!({"title": "Three", "assignee": "demo/bob"})),
        )
        .await;
    let c3 = c3["id"].as_str().unwrap().to_owned();
    td.post(
        &format!("/api/cards/{c3}/move"),
        Some(json!({"column": "ready"})),
    )
    .await;
    td.post(
        &format!("/api/cards/{c3}/move"),
        Some(json!({"column": "in_progress"})),
    )
    .await;
    tokio::time::sleep(LEAD_NUDGE + Duration::from_millis(700)).await;
    assert_eq!(
        prompts(&td, &lead).await.len(),
        2,
        "{:#?}",
        prompts(&td, &lead).await
    );
    // Bob (a member) and the lead of another project got no nudge. Bob
    // got the pointer of the assignment only.
    let bob_prompts = prompts(&td, &bob).await;
    assert!(
        bob_prompts
            .iter()
            .all(|p| p.contains("was assigned to you")),
        "{bob_prompts:#?}"
    );
    assert!(prompts(&td, &olead).await.is_empty());
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
    // One harness session is one row: the two resumes count on it.
    let sessions = td.get(&format!("/api/agents/{id}/sessions")).await;
    assert_eq!(sessions.as_array().unwrap().len(), 1, "{sessions}");
    assert_eq!(sessions[0]["resumes"], 2, "{sessions}");
    assert_eq!(sessions[0]["outcome"], "error");

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
    // The resume reused the row of the harness session (PLAN §18).
    let sessions = td.get(&format!("/api/agents/{id}/sessions")).await;
    assert_eq!(sessions.as_array().unwrap().len(), 1, "{sessions}");
    assert_eq!(sessions[0]["harness_session_id"], first);
    assert_eq!(sessions[0]["resumes"], 1);
    assert_eq!(sessions[0]["current"], true);

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
                    .starts_with("[System:recap] Terrarium restarted you")
            })
        })
        .await;
    let recap = starts.last().unwrap()["data"]["prompt"]
        .as_str()
        .unwrap()
        .to_owned();
    // The old turn keeps its tag in the recap line.
    assert!(recap.contains("- turn: [User] remember me"), "{recap}");
    let started = td.events(&id, "session_started").await;
    assert_eq!(started.last().unwrap()["data"]["source"], "startup");
    assert_ne!(started.last().unwrap()["data"]["harness_session"], first);
    // A new harness session is a new row. The start reopened the old row
    // for the resume, so the failed attempt counts on it and ends it.
    let sessions = td.get(&format!("/api/agents/{id}/sessions")).await;
    assert_eq!(sessions.as_array().unwrap().len(), 2, "{sessions}");
    assert_eq!(
        sessions[0]["harness_session_id"],
        started.last().unwrap()["data"]["harness_session"]
    );
    assert_eq!(sessions[0]["resumes"], 0);
    assert_eq!(sessions[0]["current"], true);
    assert_eq!(sessions[1]["harness_session_id"], first);
    assert_eq!(sessions[1]["resumes"], 2);
    assert_eq!(sessions[1]["outcome"], "resume_failed");
    assert_eq!(sessions[1]["current"], false);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn agent_rpc_drain_retries_long_polls_without_journaling_them() {
    use terrarium_runner::RequestHandler;
    let td = Td::start_with(false).await;
    let bob = td.agent("demo", "bob", json!({})).await;
    let state = &td.daemon.as_ref().unwrap().state;
    let handler = terrariumd::agent_rpc::AgentRpc::new(Arc::downgrade(state));
    let ctx = serde_json::from_value(json!({"agent":bob, "name":"bob", "project":"demo",
        "harness":"claude", "status":"idle"}))
    .unwrap();
    let pending = handler.handle(
        &ctx,
        "http",
        json!({"method":"GET", "path":"/api/mail",
        "query":{"wait":30}, "request_id":"long-poll"}),
    );
    tokio::pin!(pending);
    tokio::select! {
        result = &mut pending => panic!("long poll answered early: {result:?}"),
        _ = tokio::time::sleep(Duration::from_millis(100)) => {}
    }
    let (drained, answer) = tokio::join!(
        terrariumd::agent_rpc::drain(state, Duration::from_secs(2)),
        pending
    );
    drained.unwrap();
    assert_eq!(answer.unwrap_err().code, "retry");
    let refused = handler
        .handle(
            &ctx,
            "http",
            json!({"method":"POST", "path":"/api/mail",
        "body":{"to":"demo/bob", "body":"not admitted"}, "request_id":"held"}),
        )
        .await;
    assert_eq!(refused.unwrap_err().code, "retry");
    let count: i64 = state
        .db
        .read(|c| Ok(c.query_row("SELECT count(*) FROM agent_rpc_answers", [], |r| r.get(0))?))
        .await
        .unwrap();
    assert_eq!(count, 0);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn runner_whoami_reads_role_changes_without_restart() {
    let td = Td::start_with(false).await;
    let bob = td.agent("demo", "bob", json!({})).await;
    td.post(&format!("/api/agents/{bob}/start"), None).await;
    td.wait_status(&bob, "idle").await;
    let sock = td.runner_socket(&bob);
    for roles in [json!([]), json!(["project_admin"]), json!([])] {
        let (status, body) = td
            .req(
                Method::PUT,
                &format!("/api/agents/{bob}/roles"),
                Some(roles.clone()),
                None,
            )
            .await;
        assert_eq!(status, 200, "{body}");
        let answer = rpc(&sock, "whoami", json!({})).await;
        assert_eq!(answer["result"]["roles"], roles, "{answer}");
        assert_eq!(answer["result"]["agent"], bob);
        assert_eq!(answer["result"]["name"], "bob");
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn agent_rpc_write_answers_survive_restart_without_duplicate_mail() {
    use sha2::{Digest, Sha256};
    let mut td = Td::start_with(false).await;
    let bob = td.agent("demo", "bob", json!({})).await;
    td.post(&format!("/api/agents/{bob}/start"), None).await;
    td.wait_status(&bob, "idle").await;
    let sock = td.runner_socket(&bob);
    let params = json!({"method":"POST", "path":"/api/mail", "request_id":"replay-mail",
        "body":{"to":"demo/bob", "body":"exactly one copy"}});
    let (a, b) = tokio::join!(
        rpc(&sock, "http", params.clone()),
        rpc(&sock, "http", params.clone())
    );
    assert_eq!(a["result"]["status"], 201, "{a}");
    assert_eq!(a, b);
    let mut changed = params.clone();
    changed["body"]["body"] = json!("different");
    assert_eq!(
        rpc(&sock, "http", changed).await["error"]["code"],
        "invalid_params"
    );
    td.restart_daemon().await;
    td.wait_status(&bob, "idle").await;
    assert_eq!(rpc(&sock, "http", params).await, a);
    let mail = rpc(&sock, "http", json!({"method":"GET", "path":"/api/mail"})).await;
    assert_eq!(
        mail["result"]["body"]
            .as_array()
            .unwrap()
            .iter()
            .filter(|m| m["body"] == "exactly one copy")
            .count(),
        1
    );

    // Simulate a crash after admitting a write but before persisting its answer.
    let pending = json!({"method":"POST", "path":"/api/mail", "request_id":"unresolved",
        "body":{"to":"demo/bob", "body":"must not run"}});
    let hash: String = Sha256::digest(serde_json::to_vec(&pending).unwrap())
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect();
    let key = bob.clone();
    td.daemon.as_ref().unwrap().state.db.write("test", move |t| {
        t.execute("INSERT INTO agent_rpc_answers(agent, request_id, fingerprint) VALUES (?1, 'unresolved', ?2)", rusqlite::params![key, hash])?;
        Ok(())
    }).await.unwrap();
    td.restart_daemon().await;
    td.wait_status(&bob, "idle").await;
    let answer = rpc(&sock, "http", pending).await;
    assert_eq!(answer["error"]["code"], "indeterminate", "{answer}");
    let mail = rpc(&sock, "http", json!({"method":"GET", "path":"/api/mail"})).await;
    assert!(
        !mail["result"]["body"]
            .as_array()
            .unwrap()
            .iter()
            .any(|m| m["body"] == "must not run")
    );
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

    // Long-poll: the request waits until new mail arrives. Its wait is
    // long, so an answer before the wait ends came from the mail, also on
    // a loaded machine.
    let after = mail[0]["id"].as_str().unwrap().to_owned();
    let s2 = sock.clone();
    let started = Instant::now();
    let waiter = tokio::spawn(async move {
        rpc(
            &s2,
            "http",
            json!({"method": "GET", "path": "/api/mail",
            "query": {"wait": 30, "cursor": after}}),
        )
        .await
    });
    tokio::time::sleep(Duration::from_millis(400)).await;
    assert!(!waiter.is_finished());
    td.post("/api/mail", Some(json!({"to": "demo/bob", "body": "late"})))
        .await;
    let r = timeout(WAIT, waiter).await.unwrap().unwrap();
    let took = started.elapsed();
    assert!(took < Duration::from_secs(30), "the wait ran out: {took:?}");
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
    common::seed_database(&dir.path().join("state"));
    let sup = Arc::new(supervisor(dir.path(), Arc::new(FakeAdapter::default())));
    let daemon = terrariumd::start(DaemonConfig {
        socket: dir.path().join("api.sock"),
        listen: None,
        token: None,
        state_dir: dir.path().join("state"),
        config_dir: dir.path().join("config"),
        work_dir: dir.path().join("state").with_file_name("work"),
        max_targets_per_project: terrariumd::settings::DEFAULT_MAX_TARGETS_PER_PROJECT,
        repos_dir: std::path::PathBuf::from("/repos"),
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
    assert_eq!(td.sup.waiting_turns(agent), ["[User] held turn"]);
    assert_eq!(td.status(&id).await, "blocked");

    // The release sends the turn, then the pointer.
    td.sup.hold_turns(agent, None).await.unwrap();
    let starts = td.wait_events(&id, "turn_start", |e| e.len() >= 2).await;
    assert_eq!(starts[0]["data"]["prompt"], "[User] held turn");
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

/// The text of each file under `dir`, recursively.
fn tree_text(dir: &Path) -> String {
    let mut out = String::new();
    if let Ok(rd) = std::fs::read_dir(dir) {
        for e in rd.flatten() {
            let p = e.path();
            if p.is_dir() {
                out.push_str(&tree_text(&p));
            } else if let Ok(b) = std::fs::read(&p) {
                out.push_str(&String::from_utf8_lossy(&b));
            }
        }
    }
    out
}

/// PLAN §16b.4: the env vars of the `read` grants, `GH_TOKEN` for a
/// `github_token`, the git helper for the granted hosts, and the
/// ssh-agent socket, which stops with the agent.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn secret_grants_reach_the_launch() {
    let td = Td::start().await;
    let id = td.agent("demo", "sec", json!({})).await;
    let key = ssh_key::PrivateKey::new(
        ssh_key::private::Ed25519Keypair::from_seed(&[5u8; 32]).into(),
        "t",
    )
    .unwrap()
    .to_openssh(ssh_key::LineEnding::LF)
    .unwrap()
    .to_string();
    for (path, body) in [
        (
            "/api/secrets/global/db",
            json!({"kind": "api_key", "fields": {"value": "postgres-GRANTED-1"}}),
        ),
        (
            "/api/secrets/global/gh",
            json!({"kind": "github_token",
                   "fields": {"value": "ghp_GRANTEDgh", "host": "git.example.org"}}),
        ),
        (
            "/api/secrets/demo/deploy",
            json!({"kind": "ssh_key", "fields": {"private_key": key}}),
        ),
    ] {
        let (s, b) = td.req(Method::PUT, path, Some(body), None).await;
        assert_eq!(s, 201, "{b}");
    }
    for (path, body) in [
        (
            "/api/secrets/global/db/grants",
            json!({"grantee": "agent:demo/sec", "level": "read", "env": "DB_URL"}),
        ),
        (
            "/api/secrets/global/gh/grants",
            json!({"grantee": "project:demo", "level": "read"}),
        ),
        (
            "/api/secrets/demo/deploy/grants",
            json!({"grantee": "agent:demo/sec"}),
        ),
    ] {
        td.post(path, Some(body)).await;
    }
    let state = td.daemon.as_ref().unwrap().state.clone();
    let key_id = id.clone();
    let view = state
        .db
        .read(move |c| terrariumd::store::agents::get(c, &key_id))
        .await
        .unwrap();
    let spec = td.sup.launch_spec(&view.agent, None).await.unwrap();
    // Secret manager review S1 and S6: the values never go into the launch
    // spec (so neither into `runner.json` nor into the env of `bwrap`);
    // `terrarium exec-env` gets them inside the sandbox
    // (`POST /api/agent/secrets/env`, tests/secret_grants.rs).
    assert!(!spec.env.contains_key("DB_URL"), "{:?}", spec.env.keys());
    assert!(!spec.env.contains_key("GH_TOKEN"), "{:?}", spec.env.keys());
    let all = format!("{spec:?}");
    assert!(!all.contains("postgres-GRANTED-1") && !all.contains("ghp_GRANTEDgh"));
    assert_eq!(spec.env["GIT_CONFIG_COUNT"], "1");
    assert_eq!(
        spec.env["GIT_CONFIG_KEY_0"],
        "credential.https://git.example.org.helper"
    );
    assert_eq!(spec.env["GIT_CONFIG_VALUE_0"], "!terrarium repo credential");
    let sock = td
        .dir
        .path()
        .join("run/ssh-agent")
        .join(&id)
        .join("agent.sock");
    assert_eq!(spec.env["SSH_AUTH_SOCK"], sock.display().to_string());
    let line = terrariumd::sandbox::command_line_hiding(&spec, &["DB_URL".to_owned()]);
    assert!(!line.contains("postgres-GRANTED-1"), "{line}");
    assert!(!line.contains("ghp_GRANTEDgh"), "{line}");

    // The start serves the socket; it lists the granted key.
    td.post(&format!("/api/agents/{id}/start"), None).await;
    td.wait_status(&id, "idle").await;
    let mut s = UnixStream::connect(&sock).await.unwrap();
    s.write_all(&[0, 0, 0, 1, 11]).await.unwrap();
    let mut len = [0u8; 4];
    tokio::io::AsyncReadExt::read_exact(&mut s, &mut len)
        .await
        .unwrap();
    let mut body = vec![0u8; u32::from_be_bytes(len) as usize];
    tokio::io::AsyncReadExt::read_exact(&mut s, &mut body)
        .await
        .unwrap();
    assert_eq!(body[0], 12);
    assert_eq!(&body[1..5], &[0, 0, 0, 1]);
    drop(s);
    // No value is in a file that the agent sees.
    let agent_dir = td.dir.path().join("state/agents").join(&id);
    let seen = tree_text(&agent_dir.join("harness")) + &tree_text(&td.dir.path().join("work"));
    for v in ["postgres-GRANTED-1", "ghp_GRANTEDgh", "PRIVATE KEY"] {
        assert!(!seen.contains(v), "a file holds {v}");
    }
    // No value is in any file of the agent dir: `private/runner.json` of a
    // runner process holds none (S6).
    let all = tree_text(&agent_dir);
    for v in ["postgres-GRANTED-1", "ghp_GRANTEDgh"] {
        assert!(!all.contains(v), "a file of the agent dir holds {v}");
    }
    // A stop removes the socket.
    td.post(&format!("/api/agents/{id}/stop"), None).await;
    td.wait_status(&id, "stopped").await;
    let start = Instant::now();
    while sock.exists() && start.elapsed() < WAIT {
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    assert!(!sock.exists());
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

    // PLAN §16b.4: the key reaches the harness only through `terrarium
    // exec-env` (tests/runner_process.rs has the full path). With no
    // `terrarium` CLI (`--cli`), the start fails with the reason, and the
    // key goes into no spec.
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
    let e = td
        .sup
        .launch_spec(&view.agent, None)
        .await
        .unwrap_err()
        .to_string();
    assert!(
        e.contains("ANTHROPIC_API_KEY") && e.contains("--cli"),
        "{e}"
    );
    assert!(!e.contains("sk-test-secret"), "{e}");
    let (s, b) = td
        .req(Method::POST, &format!("/api/agents/{id}/start"), None, None)
        .await;
    assert_eq!(s, 422, "{b}");
    assert!(b.to_string().contains("--cli"), "{b}");
    assert!(!b.to_string().contains("sk-test-secret"), "{b}");
    assert_eq!(td.status(&id).await, "stopped");

    // A harness that cannot spend the subscription: 422, and it stays stopped.
    for (name, sub, want) in [
        ("carl", "pionly", "cannot spend subscription `pionly`"),
        ("dora", "nosuch", "no such subscription"),
    ] {
        // The API refuses a missing subscription (#14): set it in the store,
        // as a file sync can, to test the start.
        let id = td.agent("demo", name, json!({})).await;
        set_subscription(&td, &id, sub).await;
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
    std::fs::write(prompts.join("t.md"), "v1 {{agent.name}}\n{{peers}}").unwrap();
    let id = td.agent("demo", "bob", json!({"prompt": "t"})).await;
    let carl = td.agent("demo", "carl", json!({})).await;
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
    // A peer status change alone (carl: stopped -> idle) is not in the
    // prompt: a resume.
    td.post(&format!("/api/agents/{carl}/start"), None).await;
    td.wait_status(&carl, "idle").await;
    let started = cycle(3).await;
    assert_eq!(started[2]["data"]["source"], "resume");
    assert_eq!(
        started[2]["data"]["harness_session"],
        started[0]["data"]["harness_session"]
    );
    // A changed prompt: a new session, with the reason.
    std::fs::write(prompts.join("t.md"), "v2 {{agent.name}}").unwrap();
    let started = cycle(4).await;
    assert_eq!(started[3]["data"]["source"], "startup");
    assert_ne!(
        started[3]["data"]["harness_session"],
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
    let started = cycle(5).await;
    assert_eq!(started[4]["data"]["source"], "resume");
    assert_eq!(
        started[4]["data"]["harness_session"],
        started[3]["data"]["harness_session"]
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
    let started = cycle(6).await;
    assert_eq!(started[5]["data"]["source"], "resume");
}

/// The agent's base system prompt goes first in `prompt.md`, rendered with
/// the same variables, then a blank line and the template. It is part of
/// the prompt hash: a change starts a new session of a harness that keeps
/// the prompt of a session (Codex), and a clear goes back to the template
/// alone.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn system_prompt_is_composed_and_in_the_prompt_hash() {
    let td = Td::start().await;
    td.adapter
        .frozen
        .store(true, std::sync::atomic::Ordering::SeqCst);
    let prompts = td.dir.path().join("prompts");
    std::fs::create_dir_all(&prompts).unwrap();
    std::fs::write(prompts.join("t.md"), "v1 {{agent.name}}").unwrap();
    let id = td
        .agent(
            "demo",
            "bob",
            json!({"prompt": "t", "system_prompt": "Base {{agent.name}} in {{project.name}}.\n"}),
        )
        .await;
    let prompt_file = td
        .dir
        .path()
        .join("state/agents")
        .join(&id)
        .join("harness/prompt.md");
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
    assert_eq!(
        std::fs::read_to_string(&prompt_file).unwrap(),
        "Base bob in demo.\n\nv1 bob"
    );
    // The same text: a resume.
    let started = cycle(2).await;
    assert_eq!(started[1]["data"]["source"], "resume");
    // A changed base prompt: a new session.
    let patch = async |v: Value| {
        td.req(
            Method::PATCH,
            &format!("/api/agents/{id}"),
            Some(json!({"system_prompt": v})),
            None,
        )
        .await
    };
    patch(json!("Base two.")).await;
    let started = cycle(3).await;
    assert_eq!(started[2]["data"]["source"], "startup");
    assert_eq!(
        std::fs::read_to_string(&prompt_file).unwrap(),
        "Base two.\n\nv1 bob"
    );
    // Cleared: the template alone, and a new session again.
    patch(Value::Null).await;
    let started = cycle(4).await;
    assert_eq!(started[3]["data"]["source"], "startup");
    assert_eq!(std::fs::read_to_string(&prompt_file).unwrap(), "v1 bob");
}

/// A harness that takes a changed prompt on a resume (Claude Code with
/// `--system-prompt-snapshot off`, pi): a changed template between a stop
/// and a start still resumes the same session. The launch has the new
/// prompt, `session-prompt.sha256` follows it, and a `system` event says
/// so. The same prompt again: no event.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn unfrozen_prompt_change_resumes_and_updates_the_hash() {
    use sha2::Digest;
    let td = Td::start().await;
    let prompts = td.dir.path().join("prompts");
    std::fs::create_dir_all(&prompts).unwrap();
    std::fs::write(prompts.join("t.md"), "v1 {{agent.name}}").unwrap();
    let id = td.agent("demo", "bob", json!({"prompt": "t"})).await;
    let agent_dir = td.dir.path().join("state/agents").join(&id);
    let hash_file = agent_dir.join("private/session-prompt.sha256");
    let prompt_hash = || {
        let text = std::fs::read_to_string(agent_dir.join("harness/prompt.md")).unwrap();
        sha2::Sha256::digest(text.as_bytes())
            .iter()
            .map(|b| format!("{b:02x}"))
            .collect::<String>()
    };
    let cycle = async |want: usize| {
        td.post(&format!("/api/agents/{id}/start"), None).await;
        td.wait_events(&id, "session_started", |e| e.len() == want)
            .await;
        td.wait_status(&id, "idle").await;
        td.post(&format!("/api/agents/{id}/stop"), None).await;
        td.wait_status(&id, "stopped").await;
        td.events(&id, "session_started").await
    };
    let updated = async || {
        td.events(&id, "system")
            .await
            .into_iter()
            .filter(|e| e["data"]["message"] == "prompt updated on resume")
            .count()
    };
    cycle(1).await;
    let v1 = std::fs::read_to_string(&hash_file).unwrap();
    assert_eq!(v1.trim(), prompt_hash());

    std::fs::write(prompts.join("t.md"), "v2 {{agent.name}}").unwrap();
    let started = cycle(2).await;
    assert_eq!(started[1]["data"]["source"], "resume");
    assert_eq!(
        started[1]["data"]["harness_session"],
        started[0]["data"]["harness_session"]
    );
    let prompt = std::fs::read_to_string(agent_dir.join("harness/prompt.md")).unwrap();
    assert!(prompt.starts_with("v2 bob"), "{prompt}");
    let v2 = std::fs::read_to_string(&hash_file).unwrap();
    assert_ne!(v2, v1);
    assert_eq!(v2.trim(), prompt_hash());
    assert_eq!(updated().await, 1);
    let statuses = td.events(&id, "status").await;
    assert!(
        !statuses
            .iter()
            .any(|e| e["data"]["reason"] == "new session: the prompt changed"),
        "{statuses:#?}"
    );

    // The same prompt: a resume, and no new event.
    let started = cycle(3).await;
    assert_eq!(started[2]["data"]["source"], "resume");
    assert_eq!(updated().await, 1);
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

/// A harness that submits a slash command only on a CR that comes after a
/// pause (as Codex with `/exit`): the daemon's stop types the text, waits
/// the adapter's `exit_key_pause`, then types the CR. The harness exits on
/// its keys (code 7), not on the kill.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn stop_types_the_exit_cr_after_the_exit_key_pause() {
    let td = Td::start().await;
    td.adapter.set_env("FAKE_TUI_SLASH_NEEDS_PAUSE", "1");
    *td.adapter.exit_keys.lock().unwrap() = b"/exit 7\r".to_vec();
    *td.adapter.exit_pause.lock().unwrap() = Duration::from_millis(300);
    let id = td.agent("demo", "bob", json!({})).await;
    td.post(&format!("/api/agents/{id}/start"), None).await;
    td.wait_status(&id, "idle").await;
    td.post(&format!("/api/agents/{id}/stop"), None).await;
    td.wait_status(&id, "stopped").await;
    let exits = td.wait_events(&id, "exit", |e| !e.is_empty()).await;
    assert_eq!(exits[0]["data"]["code"], 7, "{exits:#?}");
}

/// An agent in `error` (here: a non-zero exit with restart `never`) can be
/// stopped: the stop clears it to `stopped` with the reason "cleared by
/// operator" and a status event. A second stop is `not_running`. A
/// restart of an agent in `error` launches it again.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn stop_and_restart_clear_an_agent_in_error() {
    let td = Td::start().await;
    let id = td.agent("demo", "bad", json!({"restart": "never"})).await;
    let to_error = || async {
        td.post(&format!("/api/agents/{id}/start"), None).await;
        td.wait_status(&id, "idle").await;
        td.post(
            &format!("/api/agents/{id}/send"),
            Some(json!({"text": "/exit 1"})),
        )
        .await;
        td.wait_status(&id, "error").await;
    };
    to_error().await;

    let (s, b) = td
        .req(Method::POST, &format!("/api/agents/{id}/stop"), None, None)
        .await;
    assert_eq!(s, 202, "{b}");
    assert_eq!(b["status"], "stopped", "{b}");
    assert_eq!(td.status(&id).await, "stopped");
    let last = td.events(&id, "status").await.last().unwrap().clone();
    assert_eq!(last["data"]["status"], "stopped", "{last}");
    assert_eq!(last["data"]["from"], "error", "{last}");
    assert_eq!(last["data"]["reason"], "cleared by operator", "{last}");
    let (s, b) = td
        .req(Method::POST, &format!("/api/agents/{id}/stop"), None, None)
        .await;
    assert_eq!(s, 409, "{b}");
    assert_eq!(b["error"]["code"], "not_running", "{b}");

    // Restart from `error`: a new launch.
    to_error().await;
    let (s, b) = td
        .req(
            Method::POST,
            &format!("/api/agents/{id}/restart"),
            None,
            None,
        )
        .await;
    assert_eq!(s, 202, "{b}");
    td.wait_status(&id, "idle").await;
    td.post(&format!("/api/agents/{id}/stop"), None).await;
    td.wait_status(&id, "stopped").await;
}

/// pi uses API keys only (PLAN §16.3): a plan login on a pi agent, its
/// pool, or a pi template is `422`, and a start on one fails with the same
/// words. An API key is fine.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn pi_uses_api_keys_only() {
    const WORDS: &str =
        "pi uses API keys only; use Claude Code for a Claude plan and Codex for a ChatGPT plan";
    let td = Td::start().await;
    let refused = |s: u16, b: &Value| {
        assert_eq!(s, 422, "{b}");
        let msg = b["error"]["message"].as_str().unwrap();
        assert!(msg.contains(WORDS), "{msg}");
    };
    for (name, provider, plan) in [
        ("max", "anthropic", "Max 20x"),
        ("chatgpt", "openai", "ChatGPT Pro"),
        ("key", "anthropic", "API key"),
    ] {
        td.post(
            "/api/subscriptions",
            Some(json!({"name": name, "provider": provider, "plan": plan,
                        "harnesses": ["claude", "codex", "pi"]})),
        )
        .await;
    }
    // A plan never lists pi.
    let max = td.get("/api/subscriptions/max").await;
    assert_eq!(max["harnesses"], json!(["claude", "codex"]), "{max}");
    let key = td.get("/api/subscriptions/key").await;
    assert_eq!(key["harnesses"], json!(["claude", "codex", "pi"]), "{key}");

    let _ = td
        .req(
            Method::POST,
            "/api/projects",
            Some(json!({"name": "demo"})),
            None,
        )
        .await;
    for sub in ["max", "chatgpt"] {
        let (s, b) = td
            .req(
                Method::POST,
                "/api/agents",
                Some(json!({"name": "p0", "project": "demo", "harness": "pi",
                            "subscription": sub})),
                None,
            )
            .await;
        refused(s, &b);
    }
    // An API key is fine; a change to a plan, or a plan in the pool, is not.
    let id = td
        .agent(
            "demo",
            "p1",
            json!({"harness": "pi", "subscription": "key", "model": "m"}),
        )
        .await;
    for patch in [
        json!({"subscription": "max"}),
        json!({"subscription_pool": ["key", "max"]}),
    ] {
        let (s, b) = td
            .req(
                Method::PATCH,
                &format!("/api/agents/{id}"),
                Some(patch),
                None,
            )
            .await;
        refused(s, &b);
    }
    let a = td.get(&format!("/api/agents/{id}")).await;
    assert_eq!(a["subscription"], "key", "{a}");
    // A Claude agent on the plan may move to pi only with an API key.
    let c = td.agent("demo", "c1", json!({"subscription": "max"})).await;
    let (s, b) = td
        .req(
            Method::PATCH,
            &format!("/api/agents/{c}"),
            Some(json!({"harness": "pi"})),
            None,
        )
        .await;
    refused(s, &b);
    // A template.
    let (s, b) = td
        .req(
            Method::POST,
            "/api/templates",
            Some(json!({"name": "pi-plan", "spec": {"harness": "pi", "subscription": "max"}})),
            None,
        )
        .await;
    refused(s, &b);
    // An agent named a subscription that a plan later took: the start
    // fails with the same words.
    let late = td.agent("demo", "p2", json!({"harness": "pi"})).await;
    // The API refuses a missing subscription (#14): set it in the store.
    set_subscription(&td, &late, "later").await;
    td.post(
        "/api/subscriptions",
        Some(
            json!({"name": "later", "provider": "anthropic", "plan": "Pro",
                    "harnesses": ["claude", "pi"]}),
        ),
    )
    .await;
    let (s, b) = td
        .req(
            Method::POST,
            &format!("/api/agents/{late}/start"),
            None,
            None,
        )
        .await;
    refused(s, &b);
}

/// The loop guard (PLAN §12): the 7th agent-to-agent message on a thread
/// with no operator or project-inbox message types no pointer. The operator
/// gets one notice on the thread, and the thread is `paused`. An operator
/// message on the thread resets the count.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn mail_loop_guard_pauses_a_chatty_thread() {
    let td = Td::start().await;
    let alice = td.agent("demo", "alice", json!({})).await;
    let bob = td.agent("demo", "bob", json!({})).await;
    td.post(&format!("/api/agents/{bob}/start"), None).await;
    td.wait_status(&bob, "idle").await;

    let send = |body: String, thread: Option<String>| {
        let td = &td;
        let alice = alice.clone();
        async move {
            let mut m = json!({"to": "demo/bob", "body": body});
            if let Some(t) = thread {
                m["thread_id"] = json!(t);
            }
            let (s, b) = td
                .req(Method::POST, "/api/mail", Some(m), Some(&alice))
                .await;
            assert_eq!(s, 201, "{b}");
            b
        }
    };
    let pointers = |thread: String| {
        let td = &td;
        let bob = bob.clone();
        async move {
            td.events(&bob, "turn_start")
                .await
                .iter()
                .filter(|e| e["data"]["prompt"].as_str().unwrap_or("").contains(&thread))
                .count()
        }
    };

    // Six agent messages: six pointers, one after the other.
    let first = send("one".into(), None).await;
    assert_eq!(first["held"], false, "{first}");
    assert!(first.get("held_reason").is_none(), "{first}");
    let thread = first["thread_id"].as_str().unwrap().to_owned();
    for i in 1..=6 {
        if i > 1 {
            send(format!("msg {i}"), Some(thread.clone())).await;
        }
        let start = Instant::now();
        while pointers(thread.clone()).await < i {
            assert!(start.elapsed() < WAIT, "no pointer for message {i}");
            tokio::time::sleep(Duration::from_millis(30)).await;
        }
        td.wait_status(&bob, "idle").await;
    }

    // The 7th is held: no pointer, a notice to the operator, and the thread
    // is paused.
    let held = send("msg 7".into(), Some(thread.clone())).await;
    assert_eq!(held["state"], "delivered", "{held}");
    assert_eq!(held["thread_paused"], true, "{held}");
    // The row records the hold (migration 16), also on a later read.
    assert_eq!(held["held"], true, "{held}");
    let reason = held["held_reason"].as_str().unwrap_or_default();
    assert!(
        reason.starts_with("loop guard: message 7 of agent-to-agent mail"),
        "{held}"
    );
    let stored = td
        .get(&format!("/api/mail/{}", held["id"].as_str().unwrap()))
        .await;
    assert_eq!(stored["held"], true, "{stored}");
    assert_eq!(stored["held_reason"], held["held_reason"], "{stored}");
    tokio::time::sleep(Duration::from_millis(800)).await;
    assert_eq!(pointers(thread.clone()).await, 6, "the 7th typed a pointer");
    // The `mail` event of the 7th names the recipient (it shows in bob's
    // events), and carries `held`; the push listener skips it.
    let held_id = held["id"].as_str().unwrap();
    let mail_events = td.events(&bob, "mail").await;
    let ev = mail_events
        .iter()
        .find(|e| e["data"]["message_id"] == held_id)
        .unwrap_or_else(|| panic!("no mail event for the 7th in bob's events: {mail_events:#?}"));
    assert_eq!(ev["agent_id"].as_str(), Some(bob.as_str()), "{ev}");
    assert_eq!(ev["data"]["held"], true, "{ev}");
    let notice = td
        .get(&format!("/api/mail?to=operator&thread={thread}"))
        .await;
    let notice = notice.as_array().unwrap();
    assert_eq!(notice.len(), 1, "{notice:#?}");
    assert_eq!(notice[0]["from"], "system");
    assert_eq!(
        notice[0]["body"]
            .as_str()
            .unwrap()
            .split(". ")
            .next()
            .unwrap(),
        format!("thread {thread} needs a human: 7 agent replies without operator input")
    );
    // An 8th agent message: still held, and no second notice.
    send("msg 8".into(), Some(thread.clone())).await;
    let notices = td
        .get(&format!("/api/mail?to=operator&thread={thread}"))
        .await;
    let from_system = notices
        .as_array()
        .unwrap()
        .iter()
        .filter(|m| m["from"] == "system")
        .count();
    assert_eq!(from_system, 1);
    assert_eq!(pointers(thread.clone()).await, 6);

    // The operator answers on the thread: that types a pointer, and the
    // next agent message types one again.
    td.wait_status(&bob, "idle").await;
    td.post(
        "/api/mail",
        Some(json!({"to": "demo/bob", "body": "stop here", "thread_id": thread})),
    )
    .await;
    let start = Instant::now();
    while pointers(thread.clone()).await < 7 {
        assert!(start.elapsed() < WAIT, "no pointer for the operator mail");
        tokio::time::sleep(Duration::from_millis(30)).await;
    }
    td.wait_status(&bob, "idle").await;
    let after = send("msg 9".into(), Some(thread.clone())).await;
    assert_eq!(after["state"], "queued", "{after}");
    assert_eq!(after["held"], false, "{after}");
    assert!(after.get("thread_paused").is_none(), "{after}");
    let start = Instant::now();
    while pointers(thread.clone()).await < 8 {
        assert!(start.elapsed() < WAIT, "no pointer after the reset");
        tokio::time::sleep(Duration::from_millis(30)).await;
    }
}

/// `agent stop`, then `agent rm --confirm` at once, as a lead did with a
/// Codex worker: the delete stops again while the first stop waits its
/// exit-key pause. The harness gets the exit text once and the CR once,
/// in a write of its own (the fake TUI logs each read of its terminal).
/// Before the fix, both stops typed the text: `/exit 7/exit 7`.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_second_stop_in_the_exit_key_pause_types_no_more_keys() {
    let td = Td::start().await;
    let log = td.dir.path().join("input.log");
    td.adapter.set_env("FAKE_TUI_SLASH_NEEDS_PAUSE", "1");
    td.adapter
        .set_env("FAKE_TUI_INPUT_LOG", log.to_str().unwrap());
    *td.adapter.exit_keys.lock().unwrap() = b"/exit 7\r".to_vec();
    *td.adapter.exit_pause.lock().unwrap() = Duration::from_millis(300);
    let id = td.agent("demo", "bob", json!({})).await;
    td.post(&format!("/api/agents/{id}/start"), None).await;
    td.wait_status(&id, "idle").await;
    td.post(&format!("/api/agents/{id}/stop"), None).await;
    td.post(&format!("/api/agents/{id}/stop"), None).await;
    let (s, b) = td
        .req(
            Method::DELETE,
            &format!("/api/agents/{id}?confirm=true"),
            None,
            None,
        )
        .await;
    assert_eq!(s, 204, "{b}");
    let reads: Vec<Vec<u8>> = std::fs::read_to_string(&log)
        .unwrap()
        .lines()
        .filter(|l| !l.is_empty())
        .map(|l| {
            (0..l.len())
                .step_by(2)
                .map(|i| u8::from_str_radix(&l[i..i + 2], 16).unwrap())
                .collect()
        })
        .collect();
    let all: Vec<u8> = reads.concat();
    assert_eq!(
        String::from_utf8_lossy(&all),
        "/exit 7\r",
        "the terminal reads: {reads:?}"
    );
    assert_eq!(reads.last().unwrap(), b"\r", "{reads:?}");
}

/// A card assigned to an agent that does not run: the pointer waits, and
/// the next start types it once, at the first idle. A card that went to
/// another agent before the start types nothing.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_card_assigned_while_stopped_points_at_the_next_start() {
    let td = Td::start().await;
    let id = td.agent("demo", "bob", json!({})).await;
    td.agent("demo", "eve", json!({})).await;
    // Paused: the assignments park the pointer and do not wake bob (PLAN
    // §18). The unpause is the next start.
    td.post(&format!("/api/agents/{id}/pause"), None).await;
    let mk = |title: &'static str| {
        let td = &td;
        async move {
            let c = td
                .post("/api/projects/demo/cards", Some(json!({"title": title})))
                .await;
            c["id"].as_str().unwrap().to_owned()
        }
    };
    let c1 = mk("Kept").await;
    let c2 = mk("Moved away").await;
    for c in [&c1, &c2] {
        td.post(
            &format!("/api/cards/{c}/assign"),
            Some(json!({"assignee": "demo/bob"})),
        )
        .await;
    }
    td.post(
        &format!("/api/cards/{c2}/assign"),
        Some(json!({"assignee": "demo/eve"})),
    )
    .await;
    td.post(&format!("/api/agents/{id}/unpause"), None).await;
    let starts = td.wait_events(&id, "turn_start", |e| !e.is_empty()).await;
    assert_eq!(
        starts[0]["data"]["prompt"],
        format!("[System:card] Card {c1} was assigned to you. Run `terrarium card show {c1}`.")
            .as_str()
    );
    td.wait_status(&id, "idle").await;
    // A restart types it no more.
    td.post(&format!("/api/agents/{id}/restart"), None).await;
    td.wait_events(&id, "session_started", |e| e.len() == 2)
        .await;
    td.wait_status(&id, "idle").await;
    tokio::time::sleep(Duration::from_millis(500)).await;
    let all = prompts(&td, &id).await;
    assert_eq!(all.len(), 1, "{all:#?}");
}

/// A parked pointer lives through a daemon restart: the next start types
/// it once. A card pointer that no longer applies after the restart (the
/// card went to another agent) is dropped, and its stored row with it.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_parked_pointer_survives_a_daemon_restart() {
    let mut td = Td::start().await;
    let id = td.agent("demo", "bob", json!({})).await;
    td.agent("demo", "eve", json!({})).await;
    // Paused, so the assignments park the pointer and do not wake bob.
    td.post(&format!("/api/agents/{id}/pause"), None).await;
    let mut cards = Vec::new();
    for title in ["Kept", "Moved away"] {
        let c = td
            .post("/api/projects/demo/cards", Some(json!({"title": title})))
            .await;
        cards.push(c["id"].as_str().unwrap().to_owned());
    }
    let (c1, c2) = (&cards[0], &cards[1]);
    for c in [c1, c2] {
        td.post(
            &format!("/api/cards/{c}/assign"),
            Some(json!({"assignee": "demo/bob"})),
        )
        .await;
    }
    td.restart_daemon().await;
    // Stale after the restart: the card goes to another agent.
    td.post(
        &format!("/api/cards/{c2}/assign"),
        Some(json!({"assignee": "demo/eve"})),
    )
    .await;
    td.post(&format!("/api/agents/{id}/unpause"), None).await;
    let starts = td.wait_events(&id, "turn_start", |e| !e.is_empty()).await;
    assert_eq!(
        starts[0]["data"]["prompt"],
        format!("[System:card] Card {c1} was assigned to you. Run `terrarium card show {c1}`.")
            .as_str()
    );
    td.wait_status(&id, "idle").await;
    // Typed and dropped pointers are gone from the store: a stop, a daemon
    // restart, and a start type nothing.
    td.post(&format!("/api/agents/{id}/stop"), None).await;
    td.wait_status(&id, "stopped").await;
    td.restart_daemon().await;
    td.post(&format!("/api/agents/{id}/start"), None).await;
    td.wait_events(&id, "session_started", |e| e.len() == 2)
        .await;
    td.wait_status(&id, "idle").await;
    tokio::time::sleep(Duration::from_millis(500)).await;
    let all = prompts(&td, &id).await;
    assert_eq!(all.len(), 1, "{all:#?}");
}

/// The "Card ... was assigned to you" pointer goes out only when the
/// assignee has not claimed, moved, or commented on the card since the
/// assignment (a worker that found the card with `card ls --mine` in an
/// earlier turn). A later assignment to the agent again counts anew.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn the_assign_pointer_skips_a_card_that_the_assignee_took_up() {
    let td = Td::start().await;
    // Not about the card reminder: it would add a turn after the idle.
    td.daemon
        .as_ref()
        .unwrap()
        .state
        .lifecycle
        .reminders
        .set_default(false);
    let id = td.agent("demo", "bob", json!({})).await;
    td.agent("demo", "eve", json!({})).await;
    let mk = |title: &'static str| {
        let td = &td;
        async move {
            let c = td
                .post("/api/projects/demo/cards", Some(json!({"title": title})))
                .await;
            c["id"].as_str().unwrap().to_owned()
        }
    };
    let assign = |c: String, who: &'static str| {
        let td = &td;
        async move {
            td.post(
                &format!("/api/cards/{c}/assign"),
                Some(json!({"assignee": who})),
            )
            .await;
        }
    };
    let as_bob = |path: String, body: Option<Value>| {
        let td = &td;
        async move {
            let (s, b) = td.req(Method::POST, &path, body, Some("demo/bob")).await;
            assert!((200..300).contains(&s), "POST {path}: {s} {b}");
        }
    };
    // Paused: the assignments park the pointers and do not wake bob.
    td.post(&format!("/api/agents/{id}/pause"), None).await;
    let untouched = mk("Untouched").await;
    let claimed = mk("Claimed").await;
    let commented = mk("Commented").await;
    let again = mk("Assigned again").await;
    for c in [&untouched, &claimed, &commented, &again] {
        assign(c.clone(), "demo/bob").await;
    }
    as_bob(format!("/api/cards/{claimed}/claim"), None).await;
    as_bob(
        format!("/api/cards/{commented}/comments"),
        Some(json!({"body": "on it"})),
    )
    .await;
    as_bob(
        format!("/api/cards/{again}/comments"),
        Some(json!({"body": "a first look"})),
    )
    .await;
    assign(again.clone(), "demo/eve").await;
    assign(again.clone(), "demo/bob").await;
    td.post(&format!("/api/agents/{id}/unpause"), None).await;
    let starts = td.wait_events(&id, "turn_start", |e| !e.is_empty()).await;
    assert_eq!(
        starts[0]["data"]["prompt"],
        format!(
            "[System:card] Card {untouched} was assigned to you. Run `terrarium card show {untouched}`.\n\n\
             [System:card] Card {again} was assigned to you. Run `terrarium card show {again}`."
        )
        .as_str()
    );
    td.wait_status(&id, "idle").await;
    tokio::time::sleep(Duration::from_millis(300)).await;
    assert_eq!(prompts(&td, &id).await.len(), 1);
}

/// A lead nudge that no longer applies when the lead is idle again goes
/// out no more: a card in `ready` that got an assignee while the lead
/// worked. A nudge that still applies goes out.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_stale_lead_nudge_is_dropped() {
    let td = Td::start().await;
    let lead = td
        .agent("demo", "lead", json!({"roles": ["project_admin"]}))
        .await;
    td.agent("demo", "w", json!({})).await;
    td.post(&format!("/api/agents/{lead}/start"), None).await;
    td.wait_status(&lead, "idle").await;
    // The turn stays open until the cards and the assignment are in: a
    // fixed work time ended before the assignment under load.
    hold(&td, &lead).await;
    let mk = |title: &'static str| {
        let td = &td;
        async move {
            let c = td
                .post(
                    "/api/projects/demo/cards",
                    Some(json!({"title": title, "column": "ready"})),
                )
                .await;
            c["id"].as_str().unwrap().to_owned()
        }
    };
    let a = mk("Taken").await;
    let b = mk("Open").await;
    td.post(
        &format!("/api/cards/{a}/assign"),
        Some(json!({"assignee": "demo/w"})),
    )
    .await;
    end_hold(&td, &lead).await;
    let want = format!(
        "[System:nudge] New card {b} in ready: Open. Run `terrarium card ls --unassigned --column ready`."
    );
    let starts = td
        .wait_events(&lead, "turn_start", |e| {
            e.iter().any(|x| x["data"]["prompt"] == want.as_str())
        })
        .await;
    assert_eq!(starts.len(), 2, "{starts:#?}");
    tokio::time::sleep(LEAD_NUDGE + Duration::from_millis(500)).await;
    let all = prompts(&td, &lead).await;
    assert_eq!(all, vec!["[User] /hold".to_owned(), want], "{all:#?}");
}

/// Lease pointers (docs/api.md §17.7): a grant types "You now have access
/// to target ..." into the idle agent, and a revoke types "Your access to
/// target ... ended.". A grant to a stopped agent waits for its next start.
/// An extend types nothing; a grant and a revoke within one turn type only
/// "ended".
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn lease_grant_and_revoke_push_a_pointer() {
    let td = Td::start().await;
    // Turns take a while, so a grant and a revoke fit into one.
    td.adapter.set_env("FAKE_TUI_DELAY_MS", "1000");
    let id = td.agent("demo", "bob", json!({})).await;
    td.post(
        "/api/targets",
        Some(json!({"name": "demo-box", "kind": "host", "host": "lab1", "project": "demo"})),
    )
    .await;
    // Stopped: the pointer is parked until the start.
    let g = td
        .post(
            "/api/targets/demo-box/leases",
            Some(json!({"agent": "demo/bob"})),
        )
        .await;
    assert_eq!(g["state"], "active", "{g}");
    td.post(&format!("/api/agents/{id}/start"), None).await;
    let granted = "[System:lease] You now have access to target demo-box (unknown) until revoked. Run `terrarium targets ls`.";
    let starts = td.wait_events(&id, "turn_start", |e| !e.is_empty()).await;
    assert_eq!(starts[0]["data"]["prompt"], granted);
    td.wait_status(&id, "idle").await;
    // Running and idle: the revoke types at once.
    let (s, b) = td
        .req(
            Method::DELETE,
            "/api/targets/demo-box/leases/demo%2Fbob",
            None,
            None,
        )
        .await;
    assert_eq!(s, 200, "{b}");
    let starts = td.wait_events(&id, "turn_start", |e| e.len() >= 2).await;
    assert_eq!(
        starts[1]["data"]["prompt"],
        "[System:lease] Your access to target demo-box ended."
    );
    td.wait_status(&id, "idle").await;
    // A grant with an expiry names the time.
    let g = td
        .post(
            "/api/targets/demo-box/leases",
            Some(json!({"agent": "demo/bob", "until": "1h", "exclusive": true})),
        )
        .await;
    let until = g["expires_at"].as_str().unwrap().to_owned();
    let starts = td.wait_events(&id, "turn_start", |e| e.len() >= 3).await;
    assert_eq!(
        starts[2]["data"]["prompt"],
        format!(
            "[System:lease] You now have access to target demo-box (unknown) until {until}. Run `terrarium targets ls`."
        )
        .as_str()
    );
    td.wait_status(&id, "idle").await;
    // An extend types no pointer.
    let (s, b) = td
        .req(
            Method::PATCH,
            "/api/targets/demo-box/leases/demo%2Fbob",
            Some(json!({"until": "2h"})),
            None,
        )
        .await;
    assert_eq!(s, 200, "{b}");
    // A grant and a revoke while the agent works: only "ended" goes out.
    td.req(
        Method::DELETE,
        "/api/targets/demo-box/leases/demo%2Fbob",
        None,
        None,
    )
    .await;
    td.wait_events(&id, "turn_start", |e| e.len() >= 4).await;
    td.wait_status(&id, "idle").await;
    // The turn stays open until both are in: under load a 1 s turn ended
    // between the grant and the revoke (TER-117).
    hold(&td, &id).await;
    td.post(
        "/api/targets/demo-box/leases",
        Some(json!({"agent": "demo/bob"})),
    )
    .await;
    td.req(
        Method::DELETE,
        "/api/targets/demo-box/leases/demo%2Fbob",
        None,
        None,
    )
    .await;
    end_hold(&td, &id).await;
    let starts = td.wait_events(&id, "turn_start", |e| e.len() >= 6).await;
    td.wait_status(&id, "idle").await;
    tokio::time::sleep(Duration::from_millis(500)).await;
    let all = prompts(&td, &id).await;
    assert_eq!(
        starts[5]["data"]["prompt"],
        "[System:lease] Your access to target demo-box ended."
    );
    assert_eq!(all.len(), 6, "{all:#?}");
}

/// PLAN §11 "mode 1": an agent made before the work root has its files in
/// `<state>/agents/<ulid>/workspace`. The daemon start points the agent at
/// `<work>/<project>/<agent>`, and the next agent start moves the files
/// there once. The harness files stay in the agent dir. When the new place
/// has files already, the move is skipped and the old files stay.
#[tokio::test]
async fn an_old_workspace_moves_to_the_work_root_at_the_next_start() {
    let mut td = Td::start().await;
    let bob = td.agent("demo", "bob", json!({})).await;
    let amy = td.agent("demo", "amy", json!({})).await;
    let old = |id: &str| td.dir.path().join(format!("state/agents/{id}/workspace"));
    let (old_bob, old_amy) = (old(&bob), old(&amy));
    for (dir, text) in [(&old_bob, "bob's"), (&old_amy, "amy's")] {
        std::fs::create_dir_all(dir.join("src")).unwrap();
        std::fs::write(dir.join("src/notes.txt"), text).unwrap();
    }
    let new_bob = td.dir.path().join("work/demo/bob");
    let new_amy = td.dir.path().join("work/demo/amy");
    std::fs::create_dir_all(&new_amy).unwrap();
    std::fs::write(new_amy.join("already.txt"), "here").unwrap();
    // The rows of agents from before the work root.
    let state = td.daemon.as_ref().unwrap().state.clone();
    for (id, dir) in [(&bob, &old_bob), (&amy, &old_amy)] {
        let (i, o) = (id.clone(), dir.display().to_string());
        state
            .db
            .write("system", move |t| {
                t.execute(
                    "UPDATE agents SET workspace = ?2 WHERE id = ?1",
                    rusqlite::params![i, o],
                )?;
                Ok(())
            })
            .await
            .unwrap();
    }
    td.restart_daemon().await;
    let a = td.get(&format!("/api/agents/{bob}")).await;
    assert_eq!(a["workspace"], new_bob.display().to_string(), "{a}");

    td.post(&format!("/api/agents/{bob}/start"), None).await;
    td.wait_status(&bob, "idle").await;
    assert_eq!(
        std::fs::read_to_string(new_bob.join("src/notes.txt")).unwrap(),
        "bob's"
    );
    assert!(!old_bob.exists(), "the old workspace is gone");
    assert!(
        td.dir
            .path()
            .join(format!("state/agents/{bob}/harness/prompt.md"))
            .is_file(),
        "the harness files stay in the agent dir"
    );
    // The harness runs in the new place (sandbox mode `none`).
    let argv = std::fs::read_to_string(
        td.dir
            .path()
            .join(format!("state/agents/{bob}/private/runner-argv.json")),
    )
    .unwrap_or_default();
    assert!(
        argv.is_empty() || argv.contains(&new_bob.display().to_string()),
        "{argv}"
    );

    td.post(&format!("/api/agents/{amy}/start"), None).await;
    td.wait_status(&amy, "idle").await;
    assert_eq!(
        std::fs::read_to_string(new_amy.join("already.txt")).unwrap(),
        "here"
    );
    assert!(
        !new_amy.join("src").exists(),
        "no merge into a full workspace"
    );
    assert_eq!(
        std::fs::read_to_string(old_amy.join("src/notes.txt")).unwrap(),
        "amy's",
        "the old files stay"
    );
}

/// PLAN §16.3: a Claude agent on a plan account runs with
/// `CLAUDE_SECURESTORAGE_CONFIG_DIR` through its pointer, and a swap to
/// another account of its pool is live: the pointer moves, the harness
/// keeps running. A subscription outside the pool is refused.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_claude_swap_inside_the_pool_is_live() {
    let td = Td::start().await;
    let state = td.daemon.as_ref().unwrap().state.state_dir.clone();
    for n in ["max-a", "max-b", "max-c"] {
        td.post(
            "/api/subscriptions",
            Some(json!({"name": n, "provider": "anthropic", "harnesses": ["claude"]})),
        )
        .await;
        let login = format!(
            r#"{{"claudeAiOauth": {{"accessToken": "at-{n}", "refreshToken": "rt-{n}"}}}}"#
        );
        terrariumd::accounts::write_credential(&state, n, HarnessId::Claude, login.as_bytes())
            .unwrap();
    }
    let id = td
        .agent(
            "demo",
            "swapper",
            json!({"subscription": "max-a", "subscription_pool": ["max-a", "max-b"]}),
        )
        .await;
    td.post(&format!("/api/agents/{id}/start"), None).await;
    td.wait_status(&id, "idle").await;
    let agent_id: AgentId = id.parse().unwrap();
    let harness_dir = terrarium_runner::harness_dir(&state, agent_id);
    assert_eq!(
        terrariumd::accounts::pointed(&harness_dir),
        Some(terrariumd::accounts::store_dir(&state, "max-a"))
    );
    let agent: Agent = serde_json::from_value(td.get(&format!("/api/agents/{id}")).await).unwrap();
    let spec = td.sup.launch_spec(&agent, None).await.unwrap();
    assert_eq!(
        spec.env
            .get("CLAUDE_SECURESTORAGE_CONFIG_DIR")
            .map(PathBuf::from),
        Some(terrariumd::accounts::claude_cred_dir(&harness_dir)),
        "{:?}",
        spec.env.keys().collect::<Vec<_>>()
    );
    let sessions = td.events(&id, "session_started").await.len();

    let r = td
        .post(
            &format!("/api/agents/{id}/subscription"),
            Some(json!({"subscription": "max-b"})),
        )
        .await;
    assert_eq!(r["applied"], "live", "{r}");
    assert_eq!(r["from"], "max-a");
    assert_eq!(
        terrariumd::accounts::pointed(&harness_dir),
        Some(terrariumd::accounts::store_dir(&state, "max-b"))
    );
    assert_eq!(
        td.get(&format!("/api/agents/{id}")).await["subscription"],
        "max-b"
    );
    assert_eq!(td.status(&id).await, "idle", "no restart");
    assert_eq!(td.events(&id, "session_started").await.len(), sessions);
    let swapped = td.events(&id, "agent.swapped").await;
    assert_eq!(swapped.len(), 1, "{swapped:#?}");
    assert_eq!(swapped[0]["data"]["applied"], "live");

    let (s, b) = td
        .req(
            Method::POST,
            &format!("/api/agents/{id}/subscription"),
            Some(json!({"subscription": "max-c"})),
            None,
        )
        .await;
    assert_eq!(s, 422, "{b}");
    assert!(b.to_string().contains("not in the pool"), "{b}");
    let audit = td.get("/api/audit?action=agent.swap").await.to_string();
    assert!(audit.contains("max-b") && audit.contains("live"), "{audit}");
}

/// PLAN §16.3: a swap that cannot be live (here: no account is bound)
/// waits for the end of the turn, then restarts the harness with resume.
/// On an idle agent it restarts at once.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_swap_by_restart_waits_for_the_end_of_the_turn() {
    let td = Td::start().await;
    for n in ["max-x", "max-y"] {
        td.post(
            "/api/subscriptions",
            Some(json!({"name": n, "provider": "anthropic", "harnesses": ["claude"]})),
        )
        .await;
    }
    let id = td
        .agent(
            "demo",
            "busy",
            json!({"subscription": "max-x", "subscription_pool": ["max-x", "max-y"]}),
        )
        .await;
    td.adapter.set_env("FAKE_TUI_DELAY_MS", "1500");
    td.post(&format!("/api/agents/{id}/start"), None).await;
    td.wait_status(&id, "idle").await;
    // The turn stays open until the swap is in: under load a 1.5 s turn
    // ended before a poll saw `working` (TER-117).
    hold(&td, &id).await;
    let r = td
        .post(
            &format!("/api/agents/{id}/subscription"),
            Some(json!({"subscription": "max-y"})),
        )
        .await;
    assert_eq!(r["applied"], "restart_scheduled", "{r}");
    assert_eq!(
        td.get(&format!("/api/agents/{id}")).await["subscription"],
        "max-y"
    );
    end_hold(&td, &id).await;
    // The turn ends first; then the harness starts again with resume.
    let starts = td
        .wait_events(&id, "session_started", |e| e.len() == 2)
        .await;
    let ends = td.events(&id, "turn_end").await;
    assert!(!ends.is_empty());
    assert!(
        ends[0]["seq"].as_u64() < starts[1]["seq"].as_u64(),
        "the restart came after the turn: {ends:#?} {starts:#?}"
    );
    let statuses = td.events(&id, "status").await;
    assert!(
        statuses.iter().any(|s| s["data"]["reason"]
            .as_str()
            .is_some_and(|r| r.starts_with("resuming session"))),
        "{statuses:#?}"
    );
    td.wait_status(&id, "idle").await;

    // Idle: the restart is at once.
    let r = td
        .post(
            &format!("/api/agents/{id}/subscription"),
            Some(json!({"subscription": "max-x"})),
        )
        .await;
    assert_eq!(r["applied"], "restarted", "{r}");
    td.wait_events(&id, "session_started", |e| e.len() == 3)
        .await;
}

/// PLAN §18 with the real supervisor: work for a stopped agent wakes it
/// with resume of its harness session. The queued turn goes out first,
/// then the mail pointer. The idle stop then stops it with the reason
/// `idle`, and restart `always` does not start it again: an idle stop is
/// not a crash.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn work_wakes_a_stopped_agent_and_rest_stops_it() {
    use terrarium_core::config::IdleStop;
    let td = Td::start().await;
    let id = td.agent("demo", "bob", json!({"restart": "always"})).await;
    td.post(&format!("/api/agents/{id}/start"), None).await;
    td.wait_status(&id, "idle").await;
    let first = td.events(&id, "session_started").await[0]["data"]["harness_session"].clone();
    td.post(&format!("/api/agents/{id}/stop"), None).await;
    td.wait_status(&id, "stopped").await;

    let r = td
        .post(
            &format!("/api/agents/{id}/send"),
            Some(json!({"text": "queued work"})),
        )
        .await;
    assert_eq!(r["queued"], true, "{r}");
    let m = td
        .post(
            "/api/mail",
            Some(json!({"to": "demo/bob", "body": "and mail"})),
        )
        .await;
    let thread = m["thread_id"].as_str().unwrap().to_owned();
    let starts = td
        .wait_events(&id, "turn_start", |e| {
            e.iter()
                .any(|x| x["data"]["prompt"].as_str().unwrap_or("").contains(&thread))
        })
        .await;
    let prompts: Vec<&str> = starts
        .iter()
        .map(|x| x["data"]["prompt"].as_str().unwrap())
        .collect();
    assert_eq!(prompts[0], "[User] queued work", "{prompts:#?}");
    assert_eq!(prompts.len(), 2, "{prompts:#?}");
    let started = td.events(&id, "session_started").await;
    assert_eq!(started.last().unwrap()["data"]["harness_session"], first);
    let woken = td.events(&id, "agent.woken").await;
    assert_eq!(
        woken.len(),
        1,
        "one wake for the turn and the mail: {woken:#?}"
    );
    td.wait_status(&id, "idle").await;

    td.daemon
        .as_ref()
        .unwrap()
        .state
        .lifecycle
        .set_tuning(terrariumd::lifecycle::Tuning {
            idle_stop: IdleStop::After(Duration::from_millis(300)),
            tick: Duration::from_millis(50),
            ..terrariumd::lifecycle::Tuning::default()
        });
    td.wait_status(&id, "stopped").await;
    let last = td.events(&id, "status").await.last().unwrap().clone();
    assert_eq!(last["data"]["reason"], "idle", "{last}");
    tokio::time::sleep(Duration::from_secs(1)).await;
    assert_eq!(td.status(&id).await, "stopped");
    let after = td.events(&id, "status").await;
    assert!(
        !after.iter().any(|x| x["data"]["status"] == "restarting"),
        "{after:#?}"
    );
}

/// Issue #8: a message to a sleeping agent wakes it, and the dashboard's
/// Conversation tab reads the current session at once, before the harness
/// reports its session. That session ID still answers after the harness
/// reported it: the wake reopened the row of the resumed harness session,
/// so there is no short row that goes (the old cause of "agent `bob` has
/// no session `…`").
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_wake_keeps_the_session_id_that_a_reader_has() {
    let td = Td::start().await;
    let id = td.agent("demo", "bob", json!({})).await;
    td.post(&format!("/api/agents/{id}/start"), None).await;
    td.wait_status(&id, "idle").await;
    let sessions = td.get(&format!("/api/agents/{id}/sessions")).await;
    let first = sessions[0]["id"].as_str().unwrap().to_owned();
    td.post(&format!("/api/agents/{id}/stop"), None).await;
    td.wait_status(&id, "stopped").await;

    // The harness holds its SessionStart hook until the gate file exists.
    // As with pi, the startup ends at that hook: the queued turn waits for
    // it. A turn typed before the gate is dropped, and its retries can run
    // out while the reads below take long on a loaded machine.
    let gate = td.dir.path().join("startup-gate");
    td.adapter
        .set_env("FAKE_TUI_STARTUP_GATE", &gate.display().to_string());
    td.adapter
        .startup_hook
        .store(true, std::sync::atomic::Ordering::SeqCst);
    let r = td
        .post(
            &format!("/api/agents/{id}/send"),
            Some(json!({"text": "wake up"})),
        )
        .await;
    assert_eq!(r["queued"], true, "{r}");

    // The reader: the current session while the harness starts.
    let start = Instant::now();
    let seen = loop {
        let rows = td.get(&format!("/api/agents/{id}/sessions")).await;
        if let Some(cur) = rows
            .as_array()
            .unwrap()
            .iter()
            .find(|s| s["current"] == true)
        {
            break cur["id"].as_str().unwrap().to_owned();
        }
        assert!(start.elapsed() < WAIT, "no current session: {rows}");
        tokio::time::sleep(Duration::from_millis(20)).await;
    };
    assert_eq!(
        seen, first,
        "the wake reopens the row of its harness session"
    );
    let transcript = format!("/api/agents/{id}/sessions/{seen}/transcript");
    let (s, b) = td.req(Method::GET, &transcript, None, None).await;
    assert_eq!(s, 200, "{b}");

    std::fs::write(&gate, "").unwrap();
    let started = td
        .wait_events(&id, "session_started", |e| e.len() == 2)
        .await;
    assert_eq!(started[1]["data"]["source"], "resume");
    let (s, b) = td.req(Method::GET, &transcript, None, None).await;
    assert_eq!(s, 200, "the session the reader has still answers: {b}");
    let sessions = td.get(&format!("/api/agents/{id}/sessions")).await;
    assert_eq!(sessions.as_array().unwrap().len(), 1, "{sessions}");
    assert_eq!(sessions[0]["id"], first.as_str());
    assert_eq!(sessions[0]["resumes"], 1);
    assert_eq!(sessions[0]["current"], true);
    td.wait_events(&id, "turn_start", |e| {
        e.iter().any(|x| x["data"]["prompt"] == "[User] wake up")
    })
    .await;
}

/// A wake whose start fails puts the agent in `error`, and later work
/// does not wake it until the operator clears the error.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_failed_wake_is_an_error_and_stops_the_wakes() {
    let td = Td::start().await;
    // No adapter is registered for codex here: every start fails.
    let id = td.agent("demo", "cx", json!({"harness": "codex"})).await;
    td.post("/api/mail", Some(json!({"to": "demo/cx", "body": "one"})))
        .await;
    td.wait_status(&id, "error").await;
    let last = td.events(&id, "status").await.last().unwrap().clone();
    assert!(
        last["data"]["reason"]
            .as_str()
            .unwrap()
            .starts_with("wake failed: "),
        "{last}"
    );
    td.post("/api/mail", Some(json!({"to": "demo/cx", "body": "two"})))
        .await;
    tokio::time::sleep(Duration::from_millis(500)).await;
    assert_eq!(td.status(&id).await, "error");
    assert_eq!(td.events(&id, "agent.woken").await.len(), 1);
    assert_eq!(td.get(&format!("/api/agents/{id}")).await["queued"], 2);
}

/// TER-542: a wake with a queued turn whose start ends before the first
/// idle. The harness waits at its startup gate, so the supervisor keeps
/// the turn for the first idle; then the operator stops the agent. The
/// row of the turn stays in the store. Returns the agent ID, with the
/// harness env cleared for the next start.
async fn kept_queued_turn(td: &Td) -> String {
    let id = td.agent("demo", "bob", json!({})).await;
    td.post(&format!("/api/agents/{id}/start"), None).await;
    td.wait_status(&id, "idle").await;
    td.post(&format!("/api/agents/{id}/stop"), None).await;
    td.wait_status(&id, "stopped").await;

    let gate = td.dir.path().join("startup-gate");
    td.adapter
        .set_env("FAKE_TUI_STARTUP_GATE", &gate.display().to_string());
    td.adapter
        .startup_hook
        .store(true, std::sync::atomic::Ordering::SeqCst);
    let r = td
        .post(
            &format!("/api/agents/{id}/send"),
            Some(json!({"text": "wake up"})),
        )
        .await;
    assert_eq!(r["queued"], true, "{r}");
    let agent: AgentId = id.parse().unwrap();
    let start = Instant::now();
    while td.sup.kept_queued_turns(agent).len() != 1 {
        assert!(
            start.elapsed() < WAIT,
            "the supervisor never kept the turn: {:?}",
            td.sup.kept_queued_turns(agent)
        );
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    let a = td.get(&format!("/api/agents/{id}")).await;
    assert_eq!(a["queued"], 1, "the row stays while the turn is kept: {a}");
    let q = td.get(&format!("/api/agents/{id}/queued")).await;
    let items = q["items"].as_array().unwrap();
    assert_eq!(items.len(), 1, "the turn shows once: {q}");
    assert_eq!(items[0]["source"], "store", "{q}");

    td.post(&format!("/api/agents/{id}/stop"), None).await;
    td.wait_status(&id, "stopped").await;
    let a = td.get(&format!("/api/agents/{id}")).await;
    assert_eq!(a["queued"], 1, "the row stays after the stop: {a}");
    td.adapter.clear_env();
    td.adapter
        .startup_hook
        .store(false, std::sync::atomic::Ordering::SeqCst);
    id
}

/// The turn starts of the queued turn of [`kept_queued_turn`], once the
/// agent is idle after it.
async fn wake_up_turns(td: &Td, id: &str) -> usize {
    let is_it = |x: &Value| x["data"]["prompt"] == "[User] wake up";
    td.wait_events(id, "turn_start", |e| e.iter().any(is_it))
        .await;
    td.wait_status(id, "idle").await;
    // A second delivery, if any, has time to start.
    tokio::time::sleep(Duration::from_millis(800)).await;
    td.events(id, "turn_start")
        .await
        .iter()
        .filter(|x| is_it(x))
        .count()
}

/// TER-542: a daemon restart before the next launch does not lose the
/// kept turn: its row is still in the store, and the wake at the start of
/// the new daemon delivers it once.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_kept_queued_turn_survives_a_daemon_restart() {
    let mut td = Td::start().await;
    let id = kept_queued_turn(&td).await;
    td.restart_daemon().await;
    assert_eq!(wake_up_turns(&td, &id).await, 1);
    let q = td.get(&format!("/api/agents/{id}/queued")).await;
    assert!(q["items"].as_array().unwrap().is_empty(), "{q}");
}

/// TER-542: with no daemon restart, the next start keeps the turn, and
/// it goes out once at the first idle, with its row. The wake at that
/// idle finds the row gone or kept, and does not type the turn again.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_kept_queued_turn_goes_out_once_without_a_restart() {
    let td = Td::start().await;
    let id = kept_queued_turn(&td).await;
    td.post(&format!("/api/agents/{id}/start"), None).await;
    assert_eq!(wake_up_turns(&td, &id).await, 1);
    let q = td.get(&format!("/api/agents/{id}/queued")).await;
    assert!(q["items"].as_array().unwrap().is_empty(), "{q}");
    assert!(td.sup.kept_queued_turns(id.parse().unwrap()).is_empty());
}

/// TER-562: a wake with a queued turn while a quota hold lasts. The hold
/// keeps the turn, and its row stays in the store until the turn is typed
/// after the hold. Returns the agent ID and the store row of the turn.
async fn held_queued_turn(td: &Td) -> (String, i64) {
    use terrariumd::supervisor::{Supervisor, TurnHold};
    let id = td.agent("demo", "bob", json!({})).await;
    let agent: AgentId = id.parse().unwrap();
    td.post(&format!("/api/agents/{id}/start"), None).await;
    td.wait_status(&id, "idle").await;
    td.post(&format!("/api/agents/{id}/stop"), None).await;
    td.wait_status(&id, "stopped").await;
    td.sup
        .hold_turns(
            agent,
            Some(TurnHold {
                until: None,
                reason: "the daily budget is used up".into(),
            }),
        )
        .await
        .unwrap();
    let r = td
        .post(
            &format!("/api/agents/{id}/send"),
            Some(json!({"text": "wake up"})),
        )
        .await;
    assert_eq!(r["queued"], true, "{r}");
    let start = Instant::now();
    while td.sup.waiting_turns(agent) != ["[User] wake up"] {
        assert!(
            start.elapsed() < WAIT,
            "the hold never kept the turn: {:?}",
            td.sup.waiting_turns(agent)
        );
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    let a = td.get(&format!("/api/agents/{id}")).await;
    assert_eq!(a["queued"], 1, "the row stays while the hold keeps the turn: {a}");
    let q = td.get(&format!("/api/agents/{id}/queued")).await;
    let items = q["items"].as_array().unwrap();
    assert_eq!(items.len(), 1, "the turn shows once: {q}");
    assert_eq!(items[0]["source"], "store", "{q}");
    let row = items[0]["id"].as_i64().unwrap();
    (id, row)
}

/// TER-562: a daemon restart during a quota hold does not lose the held
/// store turn: the wake at the start of the new daemon delivers it once.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_held_queued_turn_survives_a_daemon_restart() {
    let mut td = Td::start().await;
    let (id, _) = held_queued_turn(&td).await;
    td.restart_daemon().await;
    assert_eq!(wake_up_turns(&td, &id).await, 1);
    let q = td.get(&format!("/api/agents/{id}/queued")).await;
    assert!(q["items"].as_array().unwrap().is_empty(), "{q}");
}

/// TER-562: the same row sent again during the hold is kept once, and the
/// end of the hold types it once and deletes its row.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_held_queued_turn_goes_out_once_at_the_end_of_the_hold() {
    use terrariumd::supervisor::Supervisor;
    let td = Td::start().await;
    let (id, row) = held_queued_turn(&td).await;
    let agent: AgentId = id.parse().unwrap();
    let again = td
        .sup
        .send_queued_turn(agent, row, "[User] wake up".into())
        .await
        .unwrap();
    assert!(again, "the hold keeps the row");
    assert_eq!(td.sup.waiting_turns(agent), ["[User] wake up"]);
    td.sup.hold_turns(agent, None).await.unwrap();
    assert_eq!(wake_up_turns(&td, &id).await, 1);
    let q = td.get(&format!("/api/agents/{id}/queued")).await;
    assert!(q["items"].as_array().unwrap().is_empty(), "{q}");
}

/// TER-562: a held store turn that the operator removes during the hold
/// does not go out at its end.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_removed_held_queued_turn_does_not_go_out() {
    use terrariumd::supervisor::Supervisor;
    let td = Td::start().await;
    let (id, row) = held_queued_turn(&td).await;
    let agent: AgentId = id.parse().unwrap();
    let path = format!("/api/agents/{id}/queued/{row}?source=store");
    assert_eq!(td.req(Method::DELETE, &path, None, None).await.0, 204);
    td.sup.hold_turns(agent, None).await.unwrap();
    td.wait_status(&id, "idle").await;
    tokio::time::sleep(Duration::from_millis(800)).await;
    let starts = td.events(&id, "turn_start").await;
    assert!(
        !starts.iter().any(|x| x["data"]["prompt"] == "[User] wake up"),
        "{starts:#?}"
    );
    assert!(td.sup.waiting_turns(agent).is_empty());
}

/// PLAN §18 "Auto compact" with the real supervisor and runner: a manual
/// compaction types `/compact` as a raw command, shows `working` with the
/// reason `compacting`, and holds a new turn until the PostCompact hook.
/// Then `compact_on_idle` compacts before the idle stop.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_compaction_holds_turns_until_its_end_hook() {
    use terrarium_core::config::{CompactMaxAge, IdleStop};
    let td = Td::start().await;
    let gate_dir = tempfile::tempdir().unwrap();
    let gate = gate_dir.path().join("compact.end");
    td.adapter
        .set_env("FAKE_TUI_COMPACT_GATE", gate.to_str().unwrap());
    td.adapter.set_env("FAKE_TUI_DELAY_MS", "600");
    let id = td.agent("demo", "comp", json!({})).await;
    td.post(&format!("/api/agents/{id}/start"), None).await;
    td.wait_status(&id, "idle").await;
    td.post(
        &format!("/api/agents/{id}/send"),
        Some(json!({"text": "first"})),
    )
    .await;
    td.wait_events(&id, "turn_end", |e| !e.is_empty()).await;
    td.wait_status(&id, "idle").await;

    td.post(&format!("/api/agents/{id}/compact"), None).await;
    // The gate keeps compaction open even if sending the next turn is slow.
    td.wait_events(&id, "status", |e| {
        e.iter()
            .any(|s| s["data"]["status"] == "working" && s["data"]["reason"] == "compacting")
    })
    .await;
    td.post(
        &format!("/api/agents/{id}/send"),
        Some(json!({"text": "second"})),
    )
    .await;
    // send has returned, so the supervisor has accepted the held turn.
    // Give an incorrectly delivered turn time to emit its start hook.
    tokio::time::sleep(Duration::from_millis(300)).await;
    assert_eq!(td.events(&id, "turn_start").await.len(), 1);
    assert!(td.events(&id, "compact_finished").await.is_empty());
    std::fs::write(&gate, "go").unwrap();
    let done = td
        .wait_events(&id, "agent.compacted", |e| !e.is_empty())
        .await;
    assert_eq!(done[0]["data"]["ok"], true, "{done:#?}");
    assert_eq!(done[0]["data"]["cause"], "manual");
    let starts = td.wait_events(&id, "turn_start", |e| e.len() == 2).await;
    let finished = td.events(&id, "compact_finished").await;
    assert_eq!(finished.len(), 1, "{finished:#?}");
    assert_eq!(td.events(&id, "compact_started").await.len(), 1);
    // The held turn started after the end of the compaction; `/compact`
    // was no turn.
    assert!(
        starts[1]["seq"].as_u64() > finished[0]["seq"].as_u64(),
        "{starts:#?} {finished:#?}"
    );
    assert!(
        !starts.iter().any(|s| s["data"]["prompt"]
            .as_str()
            .unwrap_or("")
            .contains("/compact")),
        "{starts:#?}"
    );
    td.wait_status(&id, "idle").await;

    // On idle: compact first, then the idle stop.
    td.daemon
        .as_ref()
        .unwrap()
        .state
        .lifecycle
        .set_tuning(terrariumd::lifecycle::Tuning {
            idle_stop: IdleStop::After(Duration::from_millis(300)),
            tick: Duration::from_millis(50),
            compact_on_idle: true,
            compact_max_age: CompactMaxAge::Off,
            compact_wait: Duration::from_secs(10),
        });
    td.wait_status(&id, "stopped").await;
    let done = td.events(&id, "agent.compacted").await;
    assert_eq!(done.len(), 2, "{done:#?}");
    assert_eq!(done[1]["data"]["cause"], "idle", "{done:#?}");
    let last = td.events(&id, "status").await.last().unwrap().clone();
    assert_eq!(last["data"]["reason"], "idle", "{last}");
}

/// A harness that also opens a turn for `/compact` (`FAKE_TUI_COMPACT=turn`):
/// its prompt and stop hooks are notices, not a turn. A harness that sends
/// no end (`silent`): the compaction ends at its time limit, not ok, and
/// the agent is idle again.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn compaction_turn_hooks_are_no_turn_and_a_silent_end_times_out() {
    use terrarium_core::config::{CompactMaxAge, IdleStop};
    let td = Td::start().await;
    td.adapter.set_env("FAKE_TUI_COMPACT", "turn");
    let id = td.agent("demo", "comp2", json!({})).await;
    td.post(&format!("/api/agents/{id}/start"), None).await;
    td.wait_status(&id, "idle").await;
    td.post(
        &format!("/api/agents/{id}/send"),
        Some(json!({"text": "one"})),
    )
    .await;
    td.wait_events(&id, "turn_end", |e| !e.is_empty()).await;
    td.wait_status(&id, "idle").await;
    td.post(&format!("/api/agents/{id}/compact"), None).await;
    td.wait_events(&id, "agent.compacted", |e| !e.is_empty())
        .await;
    td.wait_status(&id, "idle").await;
    assert_eq!(td.events(&id, "turn_start").await.len(), 1);
    let notices = td.events(&id, "system").await;
    assert!(
        notices.iter().any(|n| n["data"]["message"]
            .as_str()
            .is_some_and(|m| m.starts_with("compaction: the harness opened a turn"))),
        "{notices:#?}"
    );

    td.daemon
        .as_ref()
        .unwrap()
        .state
        .lifecycle
        .set_tuning(terrariumd::lifecycle::Tuning {
            idle_stop: IdleStop::Off,
            tick: Duration::from_millis(50),
            compact_on_idle: false,
            compact_max_age: CompactMaxAge::Off,
            compact_wait: Duration::from_millis(800),
        });
    td.adapter.set_env("FAKE_TUI_COMPACT", "silent");
    td.post(&format!("/api/agents/{id}/restart"), None).await;
    td.wait_events(&id, "session_started", |e| e.len() == 2)
        .await;
    td.wait_status(&id, "idle").await;
    td.post(&format!("/api/agents/{id}/compact"), None).await;
    let done = td
        .wait_events(&id, "agent.compacted", |e| e.len() == 2)
        .await;
    assert_eq!(done[1]["data"]["ok"], false, "{done:#?}");
    td.wait_status(&id, "idle").await;
    let a = td.get(&format!("/api/agents/{id}")).await;
    assert_eq!(a["status_reason"], "compaction failed", "{a}");
}

/// The stream limit of a self-hosted subscription (`stream_limit`): one
/// stream, two agents. The turn of the second agent waits (status reason
/// `stream_limit`) until the turn of the first ends, then goes out. A
/// stop frees the stream.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn selfhosted_stream_limit_queues_the_second_turn() {
    let td = Td::start().await;
    td.post(
        "/api/subscriptions",
        Some(
            json!({"name": "gpu", "provider": "selfhosted", "selfhosted": {
            "api": "anthropic", "base_url": "http://127.0.0.1:9", "models": ["m"],
            "max_concurrent": 1}}),
        ),
    )
    .await;
    let bob = td
        .agent("demo", "bob", json!({"subscription": "gpu"}))
        .await;
    let carol = td
        .agent("demo", "carol", json!({"subscription": "gpu"}))
        .await;
    for id in [&bob, &carol] {
        td.post(&format!("/api/agents/{id}/start"), None).await;
    }
    for id in [&bob, &carol] {
        td.wait_status(id, "idle").await;
    }
    // Bob's turn holds the stream until the test ends it: a turn of a set
    // time could end before Carol's send on a loaded machine, and her turn
    // then gets the free stream at once.
    hold(&td, &bob).await;
    td.wait_events(&bob, "turn_start", |e| !e.is_empty()).await;
    td.post(
        &format!("/api/agents/{carol}/send"),
        Some(json!({"text": "second"})),
    )
    .await;
    // Carol waits for the stream.
    let waiting = td
        .wait_events(&carol, "status", |e| {
            e.iter().any(|x| {
                x["data"]["reason"]
                    .as_str()
                    .is_some_and(|r| r.starts_with("stream_limit"))
            })
        })
        .await;
    assert!(!waiting.is_empty());
    let sub = td.get("/api/subscriptions/gpu").await;
    assert_eq!(sub["streams"], json!({"max": 1, "in_use": 1, "waiting": 1}));
    assert!(td.events(&carol, "turn_start").await.is_empty());
    // Bob's turn ends: Carol's turn goes out.
    end_hold(&td, &bob).await;
    let starts = td
        .wait_events(&carol, "turn_start", |e| !e.is_empty())
        .await;
    assert_eq!(starts[0]["data"]["prompt"], "[User] second");
    let bob_end = td.events(&bob, "turn_end").await;
    assert!(!bob_end.is_empty(), "bob's turn ended first");
    // A stop frees the stream: a new turn of Bob goes out.
    td.post(&format!("/api/agents/{carol}/stop"), None).await;
    td.wait_status(&carol, "stopped").await;
    td.post(
        &format!("/api/agents/{bob}/send"),
        Some(json!({"text": "third"})),
    )
    .await;
    td.wait_events(&bob, "turn_start", |e| e.len() >= 2).await;
    td.wait_status(&bob, "idle").await;
    let sub = td.get("/api/subscriptions/gpu").await;
    assert_eq!(sub["streams"]["waiting"], 0);
}

/// PLAN §18 "Compact and new session on request" with the real supervisor
/// and runner: a new session ends the harness on its exit keys and starts
/// it with no resume (a new harness session and a new `sessions` row), with
/// the status reason that names who asked. The recap turn and the `then`
/// turn go out in that order. A compaction with `then` types the prompt as
/// the next turn after the end of the compaction.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_requested_new_session_and_compaction_send_then() {
    let td = Td::start().await;
    *td.adapter.exit_keys.lock().unwrap() = b"/exit 7\r".to_vec();
    let id = td.agent("demo", "nina", json!({})).await;
    td.post(&format!("/api/agents/{id}/start"), None).await;
    td.wait_status(&id, "idle").await;
    td.post(
        &format!("/api/agents/{id}/send"),
        Some(json!({"text": "first"})),
    )
    .await;
    td.wait_events(&id, "turn_end", |e| !e.is_empty()).await;
    td.wait_status(&id, "idle").await;

    // A compaction with a prompt after it.
    let r = td
        .post(
            &format!("/api/agents/{id}/compact"),
            Some(json!({"then": "After the compaction."})),
        )
        .await;
    assert_eq!(r["reset"]["state"], "started", "{r}");
    let done = td
        .wait_events(&id, "agent.compacted", |e| !e.is_empty())
        .await;
    assert_eq!(done[0]["data"]["ok"], true, "{done:#?}");
    let starts = td.wait_events(&id, "turn_start", |e| e.len() == 2).await;
    assert_eq!(
        starts[1]["data"]["prompt"],
        "[System:reset requested by operator] After the compaction."
    );
    let finished = td.events(&id, "compact_finished").await;
    assert!(starts[1]["seq"].as_u64() > finished[0]["seq"].as_u64());
    td.wait_events(&id, "turn_end", |e| e.len() == 2).await;
    td.wait_status(&id, "idle").await;

    // A new session.
    let r = td
        .post(
            &format!("/api/agents/{id}/new-session"),
            Some(json!({"then": "Start the next card."})),
        )
        .await;
    assert_eq!(r["reset"]["state"], "started", "{r}");
    let started = td
        .wait_events(&id, "session_started", |e| e.len() == 2)
        .await;
    assert_eq!(started[1]["data"]["source"], "startup", "{started:#?}");
    assert_ne!(
        started[1]["data"]["harness_session"],
        started[0]["data"]["harness_session"]
    );
    // The old harness ended on its exit keys.
    let exits = td.wait_events(&id, "exit", |e| !e.is_empty()).await;
    assert_eq!(exits[0]["data"]["code"], 7, "{exits:#?}");
    let starts = td.wait_events(&id, "turn_start", |e| e.len() == 4).await;
    let recap = starts[2]["data"]["prompt"].as_str().unwrap();
    assert!(
        recap.starts_with(
            "[System:recap] Terrarium started you in a new session, as operator requested."
        ),
        "{starts:#?}"
    );
    assert_eq!(
        starts[3]["data"]["prompt"],
        "[System:reset requested by operator] Start the next card."
    );
    let statuses = td.events(&id, "status").await;
    assert!(
        statuses
            .iter()
            .any(|e| e["data"]["reason"] == "new session: requested by operator"),
        "{statuses:#?}"
    );
    let sessions = td.get(&format!("/api/agents/{id}/sessions")).await;
    let sessions = sessions.as_array().unwrap();
    assert_eq!(sessions.len(), 2, "{sessions:#?}");
    assert_eq!(sessions[1]["outcome"], "new_session");
    assert_eq!(
        sessions[0]["harness_session_id"],
        started[1]["data"]["harness_session"]
    );
}

/// The stream limit applies to the turns of a new session: with one
/// stream, the recap and the `then` turn of the second agent wait until
/// the turn of the first agent ends.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn the_then_turn_waits_for_a_free_stream() {
    let td = Td::start().await;
    // A turn of the fake TUI takes 1.5 s.
    td.adapter.set_env("FAKE_TUI_DELAY_MS", "1500");
    td.post(
        "/api/subscriptions",
        Some(
            json!({"name": "gpu", "provider": "selfhosted", "selfhosted": {
            "api": "anthropic", "base_url": "http://127.0.0.1:9", "models": ["m"],
            "max_concurrent": 1}}),
        ),
    )
    .await;
    let bob = td
        .agent("demo", "bob", json!({"subscription": "gpu"}))
        .await;
    let carol = td
        .agent("demo", "carol", json!({"subscription": "gpu"}))
        .await;
    for id in [&bob, &carol] {
        td.post(&format!("/api/agents/{id}/start"), None).await;
    }
    for id in [&bob, &carol] {
        td.wait_status(id, "idle").await;
    }
    // Bob's turn holds the one stream until the test ends it: under load
    // carol's new session took longer than a fixed 1.5 s turn, and she
    // never had to wait (TER-117).
    hold(&td, &bob).await;
    td.post(
        &format!("/api/agents/{carol}/new-session"),
        Some(json!({"then": "Go.", "recap": false})),
    )
    .await;
    td.wait_events(&carol, "session_started", |e| e.len() == 2)
        .await;
    td.wait_events(&carol, "status", |e| {
        e.iter().any(|x| {
            x["data"]["reason"]
                .as_str()
                .is_some_and(|r| r.starts_with("stream_limit"))
        })
    })
    .await;
    assert!(
        td.events(&carol, "turn_start").await.is_empty(),
        "carol's then turn waits while bob holds the stream"
    );
    end_hold(&td, &bob).await;
    let starts = td
        .wait_events(&carol, "turn_start", |e| !e.is_empty())
        .await;
    assert_eq!(
        starts[0]["data"]["prompt"],
        "[System:reset requested by operator] Go."
    );
    // Bob's turn ended (its hook can land a moment after carol's start).
    td.wait_events(&bob, "turn_end", |e| !e.is_empty()).await;
}

/// Set the subscription of agent `id` in the store, with no check: an
/// agent that names a subscription that does not exist (yet).
async fn set_subscription(td: &Td, id: &str, sub: &str) {
    let state = td.daemon.as_ref().unwrap().state.clone();
    let (id, sub) = (id.to_owned(), sub.to_owned());
    state
        .db
        .write("operator", move |t| {
            t.execute(
                "UPDATE agents SET subscription = ?2 WHERE id = ?1",
                [&id, &sub],
            )?;
            Ok(())
        })
        .await
        .unwrap();
}
