//! The supervisor in runner process mode (`RunnerMode::Process`): each
//! agent runs in a `terrarium-runner` process with the runner's fake TUI.
//! The tests drive the agents through the API: the lifecycle, the PTY, a
//! daemon restart that the agent lives through (PLAN §18), a runner that
//! ended while the daemon was down, and the ephemeral targets of the
//! agent's policy (PLAN §19).
//!
//! The tests need the `terrarium-runner` binary. `cargo test --workspace`
//! builds it; with `-p terrariumd`, the first test builds it.
#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::{Arc, OnceLock};
use std::time::{Duration, Instant};

use futures_util::{SinkExt, StreamExt};
use http_body_util::{BodyExt, Full};
use hyper::body::Bytes;
use hyper::{Method, Request};
use hyper_util::rt::TokioIo;
use semver::{Version, VersionReq};
use serde_json::{Value, json};
use terrarium_core::protocol::{HostCapabilities, ToolCapability};
use terrarium_core::{Adapter, Agent, AgentId, Event, HarnessId, HarnessSessionId, HookPayload};
use terrarium_core::{LaunchSpec, SessionId};
use terrarium_runner::test_support::{RunnerProcessGuard, sweep_stale_runners_once};
use terrarium_runner::{AdapterKind, ReadyDetectorSpec, live_runner_pid};
use terrariumd::runner_supervisor::{
    AdapterEntry, RestartTuning, RunnerMode, RunnerSupervisor, RunnerSupervisorConfig, RunnerTuning,
};
use terrariumd::sandbox::{SandboxConfig, SandboxMode};
use terrariumd::{Daemon, DaemonConfig};
use tokio::io::AsyncWriteExt;
use tokio::net::UnixStream;
use tokio::time::timeout;
use tokio_tungstenite::tungstenite::Message as WsMessage;
use tokio_tungstenite::tungstenite::client::IntoClientRequest;

const WAIT: Duration = Duration::from_secs(20);

fn fake_tui() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("../terrarium-runner/tests/fake_tui/fake_tui.py")
}

/// The `terrarium-runner` binary next to the test binaries. A missing one
/// is built once.
fn runner_bin() -> PathBuf {
    static BIN: OnceLock<PathBuf> = OnceLock::new();
    BIN.get_or_init(|| {
        let exe = std::env::current_exe().unwrap();
        // target/debug/deps/<test> -> target/debug
        let dir = exe.parent().and_then(Path::parent).unwrap();
        let bin = dir.join("terrarium-runner");
        if !bin.is_file() {
            let cargo = std::env::var("CARGO").unwrap_or_else(|_| "cargo".into());
            let status = std::process::Command::new(cargo)
                .args([
                    "build",
                    "-q",
                    "-p",
                    "terrarium-runner",
                    "--bin",
                    "terrarium-runner",
                ])
                .current_dir(env!("CARGO_MANIFEST_DIR"))
                .status()
                .unwrap();
            assert!(status.success(), "cargo build -p terrarium-runner failed");
        }
        assert!(bin.is_file(), "no runner binary at {}", bin.display());
        bin
    })
    .clone()
}

/// The daemon side of the fake TUI: only the launch spec. The runner
/// process maps the hooks with its own fake adapter (`AdapterKind::Fake`).
struct FakeLaunch;

impl Adapter for FakeLaunch {
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
        let spec = LaunchSpec::new("python3", &agent.workspace)
            .arg(fake_tui().display().to_string())
            .env("FAKE_TUI_DELAY_MS", "150");
        match resume {
            Some(id) => spec.arg("--resume").arg(id.0.as_str()),
            None => spec
                .arg("--session")
                .arg(format!("fake-{}", SessionId::new())),
        }
    }
    fn hook_event(&self, _hook: &HookPayload) -> Vec<Event> {
        Vec::new()
    }
}

fn supervisor(root: &Path) -> Arc<RunnerSupervisor> {
    let home = root.join("auth/claude");
    std::fs::create_dir_all(&home).unwrap();
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
    let mut entry = AdapterEntry::new(Arc::new(FakeLaunch));
    entry.runner_adapter = AdapterKind::Fake;
    entry.ready_spec = Some(ReadyDetectorSpec {
        quiet_ms: 100,
        pattern: None,
    });
    cfg.adapters.clear();
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
    cfg.mode = RunnerMode::Process { bin: runner_bin() };
    Arc::new(RunnerSupervisor::new(cfg))
}

/// A daemon in a temp dir. `_guard` ends the runner processes of the dir
/// when the test ends, also on a panic. It drops after the daemon (the
/// `Drop` of `Td`) and before `dir`.
struct Td {
    _guard: RunnerProcessGuard,
    dir: tempfile::TempDir,
    daemon: Option<Daemon>,
    socket: PathBuf,
}

impl Td {
    async fn start() -> Self {
        // Unix socket paths must stay short.
        let dir = tempfile::Builder::new()
            .prefix("trp")
            .tempdir_in("/tmp")
            .unwrap();
        std::fs::create_dir_all(dir.path().join("config")).unwrap();
        sweep_stale_runners_once(&runner_bin());
        let mut td = Self {
            _guard: RunnerProcessGuard::new(dir.path().join("run")),
            socket: dir.path().join("api.sock"),
            dir,
            daemon: None,
        };
        td.boot().await;
        td
    }

    /// Start a daemon with a new supervisor on the same state dir.
    async fn boot(&mut self) {
        let daemon = terrariumd::start(DaemonConfig {
            socket: self.socket.clone(),
            listen: None,
            token: None,
            state_dir: self.dir.path().join("state"),
            config_dir: self.dir.path().join("config"),
            supervisor: supervisor(self.dir.path()),
            insecure_agent_header: true,
            artifact_listen: None,
        })
        .await
        .unwrap();
        self.daemon = Some(daemon);
    }

    /// Stop the daemon. The runner processes keep running.
    fn shutdown(&mut self) {
        if let Some(d) = self.daemon.take() {
            d.shutdown();
        }
    }

    fn run_dir(&self) -> PathBuf {
        self.dir.path().join("run")
    }

    fn agent_dir(&self, id: &str) -> PathBuf {
        self.dir.path().join("state/agents").join(id)
    }

    async fn req(&self, method: Method, path: &str, body: Option<Value>) -> (u16, Value) {
        let stream = UnixStream::connect(&self.socket).await.unwrap();
        let (mut sender, conn) = hyper::client::conn::http1::handshake(TokioIo::new(stream))
            .await
            .unwrap();
        tokio::spawn(conn);
        let mut req = Request::builder()
            .method(method)
            .uri(path)
            .header("host", "localhost");
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
        (status, serde_json::from_slice(&raw).unwrap_or(Value::Null))
    }

    async fn get(&self, path: &str) -> Value {
        let (s, b) = self.req(Method::GET, path, None).await;
        assert!((200..300).contains(&s), "GET {path}: {s} {b}");
        b
    }

    async fn post(&self, path: &str, body: Option<Value>) -> Value {
        let (s, b) = self.req(Method::POST, path, body).await;
        assert!((200..300).contains(&s), "POST {path}: {s} {b}");
        b
    }

    async fn agent(&self, name: &str, extra: Value) -> String {
        let _ = self
            .req(Method::POST, "/api/projects", Some(json!({"name": "demo"})))
            .await;
        let mut def = json!({"name": name, "project": "demo", "harness": "claude"});
        for (k, v) in extra.as_object().unwrap() {
            def[k] = v.clone();
        }
        self.post("/api/agents", Some(def)).await["id"]
            .as_str()
            .unwrap()
            .to_owned()
    }

    async fn status(&self, id: &str) -> Value {
        self.get(&format!("/api/agents/{id}")).await["status"].clone()
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

    /// Send a turn and wait for its `turn_end` and the idle status.
    async fn turn(&self, id: &str, text: &str) {
        let before = self.events(id, "turn_end").await.len();
        self.post(
            &format!("/api/agents/{id}/send"),
            Some(json!({"text": text})),
        )
        .await;
        self.wait_events(id, "turn_end", |e| e.len() > before).await;
        self.wait_status(id, "idle").await;
    }

    fn runner_pid(&self, id: &str) -> Option<u32> {
        live_runner_pid(&self.run_dir(), id.parse::<AgentId>().unwrap())
    }

    async fn wait_runner_gone(&self, id: &str) {
        let start = Instant::now();
        while self.runner_pid(id).is_some() {
            assert!(start.elapsed() < WAIT, "the runner process did not exit");
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    }
}

/// The runner processes outlive the daemon: `_guard` ends them next.
impl Drop for Td {
    fn drop(&mut self) {
        self.shutdown();
    }
}

/// The prompts of the `turn_start` events, in order.
fn prompts(events: &[Value]) -> Vec<String> {
    events
        .iter()
        .map(|e| e["data"]["prompt"].as_str().unwrap_or("").to_owned())
        .collect()
}

/// Send one hook message to the runner socket, as `terrarium hook` does.
async fn hook(socket: &Path, name: &str, payload: Value) {
    let mut s = UnixStream::connect(socket).await.unwrap();
    let line = json!({"type": "hook", "hook": name, "payload": payload}).to_string() + "\n";
    s.write_all(line.as_bytes()).await.unwrap();
    s.shutdown().await.unwrap();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn lifecycle_through_the_api() {
    let td = Td::start().await;
    let id = td.agent("bob", json!({})).await;
    td.post(&format!("/api/agents/{id}/start"), None).await;
    td.wait_status(&id, "idle").await;

    // The harness runs in its own runner process, with the spec file.
    let pid = td.runner_pid(&id).expect("a live runner process");
    assert_ne!(pid, std::process::id());
    let spec: Value =
        serde_json::from_slice(&std::fs::read(td.agent_dir(&id).join("runner.json")).unwrap())
            .unwrap();
    assert_eq!(spec["adapter"], "fake");
    assert_eq!(spec["launch"]["program"], "python3");
    assert_eq!(spec["name"], "bob");
    let started = td.events(&id, "session_started").await;
    assert_eq!(started.len(), 1, "{started:#?}");

    // A turn, and the status.
    td.turn(&id, "hello").await;
    assert_eq!(prompts(&td.events(&id, "turn_start").await), ["hello"]);
    assert_eq!(td.status(&id).await, "idle");

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
    td.wait_status(&id, "idle").await;

    // Stop: the harness ends, then the runner process.
    td.post(&format!("/api/agents/{id}/stop"), None).await;
    td.wait_status(&id, "stopped").await;
    td.wait_runner_gone(&id).await;
    assert!(td.agent_dir(&id).join("runner-exit.json").is_file());
    let end = timeout(WAIT, async {
        loop {
            match ws.next().await {
                None | Some(Err(_)) | Some(Ok(WsMessage::Close(_))) => return,
                _ => {}
            }
        }
    })
    .await;
    assert!(end.is_ok(), "the PTY socket closes when the agent stops");
    let sessions = td.get(&format!("/api/agents/{id}/sessions")).await;
    assert_eq!(sessions[0]["outcome"], "stopped");

    // A start again launches a new runner process.
    td.post(&format!("/api/agents/{id}/start"), None).await;
    td.wait_status(&id, "idle").await;
    assert!(td.runner_pid(&id).is_some());
    td.post(&format!("/api/agents/{id}/stop"), None).await;
    td.wait_status(&id, "stopped").await;
}

/// PLAN §18: the daemon restarts, the agent does not. A new supervisor on
/// the same state dir connects to the live runner, gets the events of the
/// downtime once, and turns go through.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn the_agent_lives_through_a_daemon_restart() {
    let mut td = Td::start().await;
    let id = td.agent("bob", json!({})).await;
    td.post(&format!("/api/agents/{id}/start"), None).await;
    td.wait_status(&id, "idle").await;
    td.turn(&id, "one").await;
    let pid = td.runner_pid(&id).unwrap();

    td.shutdown();
    tokio::time::sleep(Duration::from_millis(300)).await;
    assert_eq!(
        td.runner_pid(&id),
        Some(pid),
        "the runner outlives the daemon"
    );

    // Hooks while no daemon is connected: the runner records them.
    let sock = td.run_dir().join(format!("agents/{id}.sock"));
    hook(&sock, "UserPromptSubmit", json!({"prompt": "while down"})).await;
    hook(&sock, "Stop", json!({})).await;
    tokio::time::sleep(Duration::from_millis(300)).await;

    td.boot().await;
    td.wait_events(&id, "turn_start", |e| e.len() == 2).await;
    td.wait_status(&id, "idle").await;
    assert_eq!(td.runner_pid(&id), Some(pid), "the same runner process");

    // A turn goes through the new daemon.
    td.turn(&id, "two").await;

    // No relaunch: one harness session, one daemon session.
    assert_eq!(td.events(&id, "session_started").await.len(), 1);
    let sessions = td.get(&format!("/api/agents/{id}/sessions")).await;
    assert_eq!(sessions.as_array().unwrap().len(), 1, "{sessions}");
    // Each runner event is in the store once.
    assert_eq!(
        prompts(&td.events(&id, "turn_start").await),
        ["one", "while down", "two"]
    );
    assert_eq!(td.events(&id, "turn_end").await.len(), 3);
    let statuses: Vec<String> = td
        .events(&id, "status")
        .await
        .iter()
        .map(|e| e["data"]["status"].as_str().unwrap().to_owned())
        .collect();
    let working = statuses.iter().filter(|s| *s == "working").count();
    assert_eq!(working, 3, "{statuses:?}");

    // The stored seq is the last line of events.jsonl.
    let lines = std::fs::read_to_string(td.agent_dir(&id).join("events.jsonl"))
        .unwrap()
        .lines()
        .count() as i64;
    let db = rusqlite::Connection::open(td.dir.path().join("state/terrarium.db")).unwrap();
    let seq: i64 = db
        .query_row("SELECT runner_seq FROM agents WHERE id = ?1", [&id], |r| {
            r.get(0)
        })
        .unwrap();
    assert_eq!(seq, lines);

    td.post(&format!("/api/agents/{id}/stop"), None).await;
    td.wait_status(&id, "stopped").await;
    td.wait_runner_gone(&id).await;
}

/// A harness that ends while the daemon is down leaves `runner-exit.json`.
/// The next daemon applies the restart policy: `never` and a kill give
/// `error`; `on-failure` launches the agent again with resume.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_harness_that_ended_while_the_daemon_was_down() {
    let mut td = Td::start().await;
    let never = td.agent("never", json!({"restart": "never"})).await;
    let again = td.agent("again", json!({})).await;
    for id in [&never, &again] {
        td.post(&format!("/api/agents/{id}/start"), None).await;
        td.wait_status(id, "idle").await;
    }
    td.shutdown();
    for id in [&never, &again] {
        let r = terrarium_runner::RemoteRunner::connect(&td.run_dir(), id.parse().unwrap())
            .await
            .unwrap();
        let harness = r.hello().harness_pid.unwrap();
        drop(r);
        let status = std::process::Command::new("kill")
            .args(["-KILL", &harness.to_string()])
            .status()
            .unwrap();
        assert!(status.success());
        td.wait_runner_gone(id).await;
        assert!(td.agent_dir(id).join("runner-exit.json").is_file());
    }

    td.boot().await;
    td.wait_status(&never, "error").await;
    let last = td.events(&never, "status").await.last().unwrap().clone();
    let reason = last["data"]["reason"].as_str().unwrap();
    assert!(reason.contains("while the daemon was down"), "{reason}");

    td.wait_events(&again, "session_started", |e| e.len() == 2)
        .await;
    td.wait_status(&again, "idle").await;
    let started = td.events(&again, "session_started").await;
    assert_eq!(started[1]["data"]["source"], "resume");
    assert!(td.runner_pid(&again).is_some());
    td.post(&format!("/api/agents/{again}/stop"), None).await;
    td.wait_status(&again, "stopped").await;
}

/// A fake `docker` that records each call, as in `tests/hosts.rs`. It
/// keeps one state file for each container name.
fn fake_docker(dir: &Path) -> PathBuf {
    let log = dir.join("calls.log");
    let body = format!(
        r#"#!/bin/sh
echo "docker $*" >> '{log}'
last=""; name=""; prev=""
for a in "$@"; do
  [ "$prev" = "--name" ] && name=$a
  prev=$a; last=$a
done
case "$1" in
  inspect) if [ -f '{d}'/"$last".state ]; then echo '[{{"Id":"abc","Name":"/'"$last"'","State":{{"Status":"running","Running":true}}}}]'; else echo '[]'; echo 'Error: No such object' >&2; exit 1; fi ;;
  run) touch '{d}'/"$name".state; echo abc123 ;;
  rm) rm -f '{d}'/"$last".state ;;
esac
exit 0
"#,
        log = log.display(),
        d = dir.display()
    );
    let p = dir.join("docker");
    std::fs::write(&p, body).unwrap();
    use std::os::unix::fs::PermissionsExt;
    std::fs::set_permissions(&p, std::fs::Permissions::from_mode(0o755)).unwrap();
    p
}

/// PLAN §19: an ephemeral target in the agent's policy comes up before the
/// harness starts and goes away when the agent stops. A `keep = true`
/// target stays.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn ephemeral_targets_follow_the_agent() {
    let td = Td::start().await;
    let state = td.daemon.as_ref().unwrap().state.clone();
    let (_task, addr) = terrariumd::hosts::serve(state, "127.0.0.1:0")
        .await
        .unwrap();
    let bins = tempfile::tempdir().unwrap();
    let docker = fake_docker(bins.path());
    let log = || std::fs::read_to_string(bins.path().join("calls.log")).unwrap_or_default();

    let (s, h) = td
        .req(Method::POST, "/api/hosts", Some(json!({"name": "lab1"})))
        .await;
    assert_eq!(s, 201, "{h}");
    let mut o = terrarium_host::ClientOptions::new(
        format!("ws://{addr}/hosts/connect"),
        h["token"].as_str().unwrap(),
        "lab1",
    );
    o.capabilities = HostCapabilities {
        docker: ToolCapability {
            present: true,
            version: Some("29.8.0".into()),
        },
        vmlab: ToolCapability::default(),
        shell: Some("/bin/sh".into()),
    };
    o.backoff_min = Duration::from_millis(50);
    o.backoff_max = Duration::from_millis(200);
    o.docker_bin = docker.display().to_string();
    let host = tokio::spawn(terrarium_host::Client::new(o).run());
    let start = Instant::now();
    while td.get("/api/hosts/lab1").await["online"] != true {
        assert!(start.elapsed() < WAIT, "the host never came online");
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    for (name, keep) in [("dbox", false), ("kbox", true)] {
        let (s, b) = td
            .req(
                Method::POST,
                "/api/targets",
                Some(json!({"name": name, "kind": "docker", "host": "lab1",
                            "image": "alpine:3", "ephemeral": true, "keep": keep})),
            )
            .await;
        assert_eq!(s, 201, "{b}");
    }

    let id = td
        .agent("worker", json!({"policy": {"targets": ["dbox", "kbox"]}}))
        .await;
    td.post(&format!("/api/agents/{id}/start"), None).await;
    td.wait_status(&id, "idle").await;
    let calls = log();
    assert_eq!(calls.matches("docker run").count(), 2, "{calls}");
    assert_eq!(td.get("/api/targets/dbox").await["state"], "running");

    td.post(&format!("/api/agents/{id}/stop"), None).await;
    td.wait_status(&id, "stopped").await;
    let start = Instant::now();
    while !log().contains("docker rm") {
        assert!(
            start.elapsed() < WAIT,
            "the target was not removed: {}",
            log()
        );
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    let start = Instant::now();
    while td.get("/api/targets/dbox").await["state"] != "absent" {
        assert!(start.elapsed() < WAIT, "dbox never became absent");
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    // Only `dbox` goes: `kbox` has keep = true.
    assert_eq!(log().matches("docker rm").count(), 1, "{}", log());
    assert_eq!(td.get("/api/targets/kbox").await["state"], "running");
    host.abort();
}
