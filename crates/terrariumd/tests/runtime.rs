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
/// The lead nudge rate limit in these tests.
const LEAD_NUDGE: Duration = Duration::from_secs(2);

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
    fn exit_key_pause(&self) -> Duration {
        *self.exit_pause.lock().unwrap()
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
    // The runner does not make the dir again after the move.
    tokio::time::sleep(Duration::from_millis(300)).await;
    assert!(!agent_dir.exists());
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
        assert_eq!(spec.env["GIT_AUTHOR_NAME"], agent.name);
        assert_eq!(
            spec.env["GIT_COMMITTER_EMAIL"],
            format!("{}.{}@terrarium.invalid", agent.name, agent.project)
        );
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
    let repo_prompts = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../prompts");
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
    let (s, b) = call("POST", "/api/agents/w-1/start".into(), Value::Null).await;
    assert!((200..300).contains(&s), "{s} {b}");
    td.wait_status(&w, "idle").await;
    let (s, b) = call("POST", "/api/agents/w-1/stop".into(), Value::Null).await;
    assert!((200..300).contains(&s), "{s} {b}");
    td.wait_status(&w, "stopped").await;
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

/// Lead nudges: a card with no assignee in `ready`, and a move to `review`
/// or `blocked`, type a pointer into each idle `project_admin` of the
/// project. The agent that made the change, the other members, and the
/// leads of other projects get none. Changes inside the rate limit go out
/// as one pointer.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn board_changes_nudge_the_project_leads() {
    let td = Td::start().await;
    let lead = td
        .agent("demo", "lead", json!({"roles": ["project_admin"]}))
        .await;
    let bob = td.agent("demo", "bob", json!({})).await;
    let olead = td
        .agent("other", "olead", json!({"roles": ["project_admin"]}))
        .await;
    for id in [&lead, &bob, &olead] {
        td.post(&format!("/api/agents/{id}/start"), None).await;
    }
    for id in [&lead, &bob, &olead] {
        td.wait_status(id, "idle").await;
    }

    // A new card in ready, from the operator: one pointer at once.
    let c1 = td
        .post(
            "/api/projects/demo/cards",
            Some(json!({"title": "Ship it\nnow", "column": "ready"})),
        )
        .await;
    let c1 = c1["id"].as_str().unwrap().to_owned();
    let starts = td.wait_events(&lead, "turn_start", |e| !e.is_empty()).await;
    assert_eq!(
        starts[0]["data"]["prompt"],
        format!(
            "New card {c1} in ready: Ship it…. Run `terrarium card ls --unassigned --column ready`."
        )
        .as_str()
    );

    // Inside the rate limit: bob moves his card to review, and the
    // operator blocks another card. One pointer carries both, at the end
    // of the interval.
    let as_bob = |path: String, body: Value| {
        let td = &td;
        async move {
            let (s, b) = td
                .req(Method::POST, &path, Some(body), Some("demo/bob"))
                .await;
            assert!((200..300).contains(&s), "POST {path}: {s} {b}");
        }
    };
    as_bob(format!("/api/cards/{c1}/claim"), json!({})).await;
    as_bob(
        format!("/api/cards/{c1}/move"),
        json!({"column": "in_progress"}),
    )
    .await;
    as_bob(format!("/api/cards/{c1}/move"), json!({"column": "review"})).await;
    let c2 = td
        .post("/api/projects/demo/cards", Some(json!({"title": "Two"})))
        .await;
    let c2 = c2["id"].as_str().unwrap().to_owned();
    td.post(
        &format!("/api/cards/{c2}/move"),
        Some(json!({"column": "blocked", "comment": "needs the API key"})),
    )
    .await;
    let batch = Instant::now();
    let starts = td.wait_events(&lead, "turn_start", |e| e.len() >= 2).await;
    assert!(
        batch.elapsed() > LEAD_NUDGE / 2,
        "the batch waited for the interval"
    );
    assert_eq!(
        starts[1]["data"]["prompt"],
        format!(
            "{c1} moved to review by demo/bob. Run `terrarium card show {c1}`.\n\
             {c2} moved to blocked by operator: needs the API key. Run `terrarium card show {c2}`."
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
    td.post(&format!("/api/agents/{id}/start"), None).await;
    let starts = td.wait_events(&id, "turn_start", |e| !e.is_empty()).await;
    assert_eq!(
        starts[0]["data"]["prompt"],
        format!("Card {c1} was assigned to you. Run `terrarium card show {c1}`.").as_str()
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

/// A lead nudge that no longer applies when the lead is idle again goes
/// out no more: a card in `ready` that got an assignee while the lead
/// worked. A nudge that still applies goes out.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_stale_lead_nudge_is_dropped() {
    let td = Td::start().await;
    td.adapter.set_env("FAKE_TUI_DELAY_MS", "3000");
    let lead = td
        .agent("demo", "lead", json!({"roles": ["project_admin"]}))
        .await;
    td.agent("demo", "w", json!({})).await;
    td.post(&format!("/api/agents/{lead}/start"), None).await;
    td.wait_status(&lead, "idle").await;
    td.post(
        &format!("/api/agents/{lead}/send"),
        Some(json!({"text": "long work"})),
    )
    .await;
    td.wait_status(&lead, "working").await;
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
    let want = format!(
        "New card {b} in ready: Open. Run `terrarium card ls --unassigned --column ready`."
    );
    let starts = td
        .wait_events(&lead, "turn_start", |e| {
            e.iter().any(|x| x["data"]["prompt"] == want.as_str())
        })
        .await;
    assert_eq!(starts.len(), 2, "{starts:#?}");
    tokio::time::sleep(LEAD_NUDGE + Duration::from_millis(500)).await;
    let all = prompts(&td, &lead).await;
    assert_eq!(all, vec!["long work".to_owned(), want], "{all:#?}");
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
    let granted = "You now have access to target demo-box (unknown) until revoked. Run `terrarium targets ls`.";
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
        "Your access to target demo-box ended."
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
            "You now have access to target demo-box (unknown) until {until}. Run `terrarium targets ls`."
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
    td.post(
        &format!("/api/agents/{id}/send"),
        Some(json!({"text": "busy"})),
    )
    .await;
    td.wait_status(&id, "working").await;
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
    let starts = td.wait_events(&id, "turn_start", |e| e.len() >= 6).await;
    td.wait_status(&id, "idle").await;
    tokio::time::sleep(Duration::from_millis(500)).await;
    let all = prompts(&td, &id).await;
    assert_eq!(
        starts[5]["data"]["prompt"],
        "Your access to target demo-box ended."
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
