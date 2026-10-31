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
use semver::Version;
use serde_json::{Value, json};
use terrarium_core::protocol::{HostCapabilities, ToolCapability};
use terrarium_core::{Adapter, Agent, AgentId, Event, HarnessId, HarnessSessionId, HookPayload};
use terrarium_core::{LaunchSpec, SessionId};
use terrarium_runner::test_support::{RunnerProcessGuard, sweep_stale_runners_once};
use terrarium_runner::{AdapterKind, ReadyDetectorSpec, live_runner_pid};
use terrariumd::harness_tools::{ToolsConfig, compiled_pins, write_fake_npm};
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
struct FakeLaunch {
    /// Launch `claude <fake_tui.py> ...`, so the program comes from the
    /// harness version dir of the tools dir ([`tui_wrapper`]); else
    /// `python3 <fake_tui.py> ...`.
    pinned: bool,
    /// Launch this script instead of `python3`: it writes its env to a
    /// file and runs `python3 "$@"` ([`env_wrapper`]).
    wrapper: Option<PathBuf>,
}

impl Adapter for FakeLaunch {
    fn id(&self) -> HarnessId {
        HarnessId::Claude
    }
    fn detect_version(&self) -> terrarium_core::Result<Version> {
        Ok(Version::new(1, 0, 0))
    }
    fn launch(&self, agent: &Agent, resume: Option<&HarnessSessionId>) -> LaunchSpec {
        let program = match &self.wrapper {
            Some(w) => w.display().to_string(),
            None if self.pinned => "claude".to_owned(),
            None => "python3".to_owned(),
        };
        let spec = LaunchSpec::new(program, &agent.workspace)
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
    /// The fake TUI exits 0 on `/quit`. The supervisor's stop reads the
    /// exit keys from the daemon-side adapter, as for Claude's `/exit`.
    fn exit_keys(&self) -> Vec<u8> {
        b"/quit\r".to_vec()
    }
}

/// Make the harness command `bin` a wrapper that runs the fake TUI:
/// `bin <fake_tui.py> ...` is `python3 <fake_tui.py> ...`.
fn tui_wrapper(bin: &Path) {
    use std::os::unix::fs::PermissionsExt;
    let _ = std::fs::remove_file(bin);
    std::fs::write(bin, "#!/bin/sh\nexec python3 \"$@\"\n").unwrap();
    std::fs::set_permissions(bin, std::fs::Permissions::from_mode(0o755)).unwrap();
}

/// The tools dir of a [`Td`] with harness version dirs, and its fake npm.
fn tools_config(root: &Path) -> ToolsConfig {
    std::fs::create_dir_all(root.join("copilot")).unwrap();
    let mut cfg = ToolsConfig::new(root.join("tools"), compiled_pins(), root.join("copilot"));
    cfg.npm = write_fake_npm(root);
    cfg
}

/// The `terrarium` CLI binary next to the test binaries. A missing one is
/// built once.
fn cli_bin() -> PathBuf {
    static BIN: OnceLock<PathBuf> = OnceLock::new();
    BIN.get_or_init(|| {
        let exe = std::env::current_exe().unwrap();
        let dir = exe.parent().and_then(Path::parent).unwrap();
        let bin = dir.join("terrarium");
        if !bin.is_file() {
            let cargo = std::env::var("CARGO").unwrap_or_else(|_| "cargo".into());
            let status = std::process::Command::new(cargo)
                .args(["build", "-q", "-p", "terrarium", "--bin", "terrarium"])
                .current_dir(env!("CARGO_MANIFEST_DIR"))
                .status()
                .unwrap();
            assert!(status.success(), "cargo build -p terrarium failed");
        }
        assert!(bin.is_file(), "no CLI binary at {}", bin.display());
        bin
    })
    .clone()
}

/// A harness command that writes its env to `<seen>/<agent ulid>.env` and
/// then runs `python3 "$@"` (the fake TUI).
fn env_wrapper(root: &Path) -> PathBuf {
    use std::os::unix::fs::PermissionsExt;
    let seen = root.join("seen");
    std::fs::create_dir_all(&seen).unwrap();
    let bin = root.join("env-harness");
    std::fs::write(
        &bin,
        format!(
            "#!/bin/sh\nenv > '{}/'\"$TERRARIUM_AGENT\".env\nexec python3 \"$@\"\n",
            seen.display()
        ),
    )
    .unwrap();
    std::fs::set_permissions(&bin, std::fs::Permissions::from_mode(0o755)).unwrap();
    bin
}

fn supervisor(root: &Path, tools: Option<&Path>, cli: bool) -> Arc<RunnerSupervisor> {
    let mut homes = HashMap::new();
    for h in [
        HarnessId::Claude,
        HarnessId::Pi,
        HarnessId::Codex,
        HarnessId::Copilot,
    ] {
        let home = root.join("auth").join(h.as_str());
        std::fs::create_dir_all(&home).unwrap();
        homes.insert(h, home);
    }
    let mut cfg = RunnerSupervisorConfig::new(
        root.join("state"),
        root.join("run"),
        root.join("prompts"),
        SandboxConfig {
            mode: SandboxMode::None,
            tools_dir: tools.map(Path::to_path_buf),
            cli: cli.then(cli_bin),
            ..SandboxConfig::default()
        },
        homes,
    );
    let mut entry = AdapterEntry::new(Arc::new(FakeLaunch {
        pinned: tools.is_some(),
        wrapper: cli.then(|| env_wrapper(root)),
    }));
    entry.runner_adapter = AdapterKind::Fake;
    entry.ready_spec = Some(ReadyDetectorSpec {
        quiet_ms: 100,
        pattern: None,
    });
    cfg.adapters.clear();
    // The other harnesses too, so their credential paths run.
    for h in [HarnessId::Pi, HarnessId::Codex, HarnessId::Copilot] {
        cfg.adapters.insert(h, entry.clone());
    }
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
    /// A tools dir with harness version dirs ([`Td::start_with_tools`]).
    tools: Option<ToolsConfig>,
    /// The sandbox binds the real `terrarium` CLI, and the harness writes
    /// its env ([`Td::start_with_cli`]).
    cli: bool,
}

impl Td {
    async fn start() -> Self {
        Self::start_with(false).await
    }

    /// A daemon whose agents start through the real `terrarium exec-env`
    /// (PLAN §16b.4), with a harness that writes its env to
    /// `<dir>/seen/<ulid>.env`.
    async fn start_with_cli() -> Self {
        let mut td = Self::new(None, true);
        td.boot().await;
        td
    }

    /// A daemon whose agents run the harness from the version dirs of a
    /// tools dir: the fake npm installs the pins, and the claude command
    /// of the pinned version is the fake TUI.
    async fn start_with_tools() -> Self {
        Self::start_with(true).await
    }

    async fn start_with(tools: bool) -> Self {
        // Unix socket paths must stay short.
        let dir = tempfile::Builder::new()
            .prefix("trp")
            .tempdir_in("/tmp")
            .unwrap();
        std::fs::create_dir_all(dir.path().join("config")).unwrap();
        sweep_stale_runners_once(&runner_bin());
        let tools = match tools {
            true => {
                let cfg = tools_config(dir.path());
                assert_eq!(cfg.install().await.failed(), 0);
                let pin = cfg.pin("claude").unwrap().version.clone();
                tui_wrapper(&cfg.version_dir("claude", &pin).join("bin/claude"));
                Some(cfg)
            }
            false => None,
        };
        let mut td = Self::with_dir(dir, tools, false);
        td.boot().await;
        td
    }

    fn new(tools: Option<ToolsConfig>, cli: bool) -> Self {
        let dir = tempfile::Builder::new()
            .prefix("trp")
            .tempdir_in("/tmp")
            .unwrap();
        std::fs::create_dir_all(dir.path().join("config")).unwrap();
        sweep_stale_runners_once(&runner_bin());
        Self::with_dir(dir, tools, cli)
    }

    fn with_dir(dir: tempfile::TempDir, tools: Option<ToolsConfig>, cli: bool) -> Self {
        Self {
            _guard: RunnerProcessGuard::new(dir.path().join("run")),
            socket: dir.path().join("api.sock"),
            dir,
            daemon: None,
            tools,
            cli,
        }
    }

    /// Start a daemon with a new supervisor on the same state dir.
    async fn boot(&mut self) {
        let daemon = terrariumd::start(DaemonConfig {
            socket: self.socket.clone(),
            listen: None,
            token: None,
            state_dir: self.dir.path().join("state"),
            config_dir: self.dir.path().join("config"),
            work_dir: self.dir.path().join("state").with_file_name("work"),
            max_targets_per_project: terrariumd::settings::DEFAULT_MAX_TARGETS_PER_PROJECT,
            repos_dir: std::path::PathBuf::from("/repos"),
            supervisor: supervisor(
                self.dir.path(),
                self.tools.as_ref().map(|t| t.dir.as_path()),
                self.cli,
            ),
            insecure_agent_header: true,
            artifact_listen: None,
            web: Default::default(),
        })
        .await
        .unwrap();
        if let Some(t) = &self.tools {
            daemon.state.harness_tools.configure(t.clone());
        }
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

    /// `<state>/agents/<ulid>/private`: the runner's records.
    fn private_dir(&self, id: &str) -> PathBuf {
        self.dir
            .path()
            .join("state/agents")
            .join(id)
            .join("private")
    }

    async fn req(&self, method: Method, path: &str, body: Option<Value>) -> (u16, Value) {
        self.req_as(None, method, path, body).await
    }

    /// A request as the agent `as_agent` (`project/name`), through the
    /// test-only agent header.
    async fn req_as(
        &self,
        as_agent: Option<&str>,
        method: Method,
        path: &str,
        body: Option<Value>,
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
        if let Some(a) = as_agent {
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
        serde_json::from_slice(&std::fs::read(td.private_dir(&id).join("runner.json")).unwrap())
            .unwrap();
    assert_eq!(spec["adapter"], "fake");
    assert_eq!(spec["launch"]["program"], "python3");
    assert_eq!(spec["name"], "bob");
    let started = td.events(&id, "session_started").await;
    assert_eq!(started.len(), 1, "{started:#?}");
    // The runner's `stopped -> starting` ("spawned ...") comes after the
    // supervisor's own `starting`: a `system` notice, not `starting -> starting`.
    let statuses = td.events(&id, "status").await;
    assert!(
        statuses
            .iter()
            .all(|e| e["data"]["from"] != e["data"]["status"]),
        "{statuses:#?}"
    );
    let notices = td.events(&id, "system").await;
    assert!(
        notices.iter().any(|e| e["data"]["message"]
            .as_str()
            .is_some_and(|m| m.starts_with("spawned "))),
        "{notices:#?}"
    );

    // A turn, and the status.
    td.turn(&id, "hello").await;
    assert_eq!(
        prompts(&td.events(&id, "turn_start").await),
        ["[User] hello"]
    );
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
    assert!(td.private_dir(&id).join("runner-exit.json").is_file());
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
    // The stop was graceful: the idle harness got the exit keys and exited
    // 0. A kill gives SIGHUP (code 129) or the code 1 of a lost harness.
    let exits = td.wait_events(&id, "exit", |e| !e.is_empty()).await;
    assert_eq!(exits[0]["data"]["code"], 0, "{exits:#?}");

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
        ["[User] one", "while down", "[User] two"]
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
    let lines = std::fs::read_to_string(td.private_dir(&id).join("events.jsonl"))
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
        assert!(td.private_dir(id).join("runner-exit.json").is_file());
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

/// The container stops: `docker/entrypoint.sh` stops the daemon, then
/// sends SIGTERM to each runner. The runner stops the harness (a clean
/// exit on the exit keys, or a signal) and writes `"stopped_by":
/// "container"` into `runner-exit.json`. The next daemon launches the agent
/// again with resume, whatever the exit code and the restart policy. The
/// entrypoint no longer deletes the exit record for this.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_container_stop_relaunches_with_resume() {
    let mut td = Td::start().await;
    let plain = td.agent("plain", json!({})).await;
    let never = td.agent("never", json!({"restart": "never"})).await;
    for id in [&plain, &never] {
        td.post(&format!("/api/agents/{id}/start"), None).await;
        td.wait_status(id, "idle").await;
    }
    td.turn(&plain, "before the stop").await;
    td.shutdown();
    for id in [&plain, &never] {
        let pid = td.runner_pid(id).expect("a live runner");
        let status = std::process::Command::new("kill")
            .args(["-TERM", &pid.to_string()])
            .status()
            .unwrap();
        assert!(status.success());
        td.wait_runner_gone(id).await;
        let rec =
            terrarium_runner::read_exit_record(&td.dir.path().join("state"), id.parse().unwrap())
                .expect("runner-exit.json");
        assert_eq!(rec.stopped_by, Some(terrarium_runner::StoppedBy::Container));
    }

    td.boot().await;
    for id in [&plain, &never] {
        td.wait_events(id, "session_started", |e| e.len() == 2)
            .await;
        td.wait_status(id, "idle").await;
        let started = td.events(id, "session_started").await;
        assert_eq!(started[1]["data"]["source"], "resume", "{started:#?}");
        assert!(td.runner_pid(id).is_some());
    }
    // An operator stop stays a stop across a daemon restart: the record
    // says `operator`, and the next daemon does not launch it.
    td.post(&format!("/api/agents/{plain}/stop"), None).await;
    td.wait_status(&plain, "stopped").await;
    td.wait_runner_gone(&plain).await;
    let rec =
        terrarium_runner::read_exit_record(&td.dir.path().join("state"), plain.parse().unwrap())
            .unwrap();
    assert_eq!(rec.stopped_by, Some(terrarium_runner::StoppedBy::Operator));
    td.post(&format!("/api/agents/{never}/stop"), None).await;
    td.wait_status(&never, "stopped").await;
}

/// A stop that an agent (here a project lead) asks for: `runner-exit.json`
/// names that agent, `agent:<project>/<name>`, not `operator`. The next
/// daemon keeps it stopped, as for an operator stop.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn an_agent_stop_records_the_agent_in_stopped_by() {
    let mut td = Td::start().await;
    td.agent("lead", json!({"roles": ["project_admin"]})).await;
    let w = td.agent("w", json!({})).await;
    td.post(&format!("/api/agents/{w}/start"), None).await;
    td.wait_status(&w, "idle").await;
    let (s, b) = td
        .req_as(
            Some("demo/lead"),
            Method::POST,
            &format!("/api/agents/{w}/stop"),
            None,
        )
        .await;
    assert_eq!(s, 202, "{b}");
    td.wait_status(&w, "stopped").await;
    td.wait_runner_gone(&w).await;
    let rec = terrarium_runner::read_exit_record(&td.dir.path().join("state"), w.parse().unwrap())
        .expect("runner-exit.json");
    assert_eq!(
        rec.stopped_by,
        Some(terrarium_runner::StoppedBy::Actor("agent:demo/lead".into()))
    );
    let raw = std::fs::read_to_string(td.private_dir(&w).join("runner-exit.json")).unwrap();
    assert!(raw.contains(r#""stopped_by":"agent:demo/lead""#), "{raw}");

    td.post(&format!("/api/agents/{w}/start"), None).await;
    td.wait_status(&w, "idle").await;

    // The daemon was down when the harness ended on that stop: the next
    // daemon keeps it stopped and names the agent in the reason. (The
    // runner ends on a SIGTERM; the record then gets the agent stop.)
    td.shutdown();
    let pid = td.runner_pid(&w).expect("a live runner");
    let status = std::process::Command::new("kill")
        .args(["-TERM", &pid.to_string()])
        .status()
        .unwrap();
    assert!(status.success());
    td.wait_runner_gone(&w).await;
    std::fs::write(
        td.private_dir(&w).join("runner-exit.json"),
        r#"{"code":0,"stopped_by":"agent:demo/lead"}"#,
    )
    .unwrap();
    td.boot().await;
    let start = Instant::now();
    loop {
        let statuses = td.events(&w, "status").await;
        if statuses
            .iter()
            .any(|e| e["data"]["reason"] == "stopped by agent:demo/lead")
        {
            break;
        }
        assert!(start.elapsed() < WAIT, "{statuses:#?}");
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    assert_eq!(td.status(&w).await, "stopped");
    assert!(td.runner_pid(&w).is_none());
}

/// F1: a state dir from before the layout split. The daemon moves the
/// records into `private/` and the harness files into `harness/` once, and
/// the agent starts with its old events.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn an_old_agent_dir_moves_to_the_new_layout() {
    let mut td = Td::start().await;
    let id = td.agent("old", json!({})).await;
    td.post(&format!("/api/agents/{id}/start"), None).await;
    td.wait_status(&id, "idle").await;
    td.post(&format!("/api/agents/{id}/stop"), None).await;
    td.wait_status(&id, "stopped").await;
    td.wait_runner_gone(&id).await;
    td.shutdown();
    // Put the files back where the old layout had them.
    let top = td.dir.path().join("state/agents").join(&id);
    for name in [
        "events.jsonl",
        "runner.json",
        "runner-exit.json",
        "runner.log",
    ] {
        std::fs::rename(top.join("private").join(name), top.join(name)).unwrap();
    }
    std::fs::rename(top.join("harness/prompt.md"), top.join("prompt.md")).unwrap();
    let lines = std::fs::read_to_string(top.join("events.jsonl"))
        .unwrap()
        .lines()
        .count();
    assert!(lines > 0);

    td.boot().await;
    for name in [
        "events.jsonl",
        "runner.json",
        "runner-exit.json",
        "prompt.md",
    ] {
        assert!(!top.join(name).exists(), "{name} is still at the top");
    }
    assert!(top.join("private/events.jsonl").is_file());
    assert!(top.join("harness/prompt.md").is_file());
    td.post(&format!("/api/agents/{id}/start"), None).await;
    td.wait_status(&id, "idle").await;
    let now = std::fs::read_to_string(top.join("private/events.jsonl"))
        .unwrap()
        .lines()
        .count();
    assert!(now > lines, "the old events stay, the new ones follow");
    td.post(&format!("/api/agents/{id}/stop"), None).await;
    td.wait_status(&id, "stopped").await;
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
        ..Default::default()
    };
    o.backoff_min = Duration::from_millis(50);
    o.backoff_max = Duration::from_millis(200);
    o.docker_bin = docker.display().to_string();
    // The host agent's work_dir is the daemon's work root, as on one machine.
    o.paths = terrarium_host::HostPaths {
        work_dir: td.dir.path().join("work"),
        instances_dir: bins.path().join("instances"),
        repos_dir: bins.path().join("repos"),
    };
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

/// The claude row of `GET /api/harnesses`.
async fn claude_row(td: &Td) -> Value {
    let h = td.get("/api/harnesses").await;
    h["harnesses"]
        .as_array()
        .unwrap()
        .iter()
        .find(|r| r["harness"] == "claude")
        .unwrap()
        .clone()
}

/// A harness update while an agent runs (the harness version dirs,
/// `harness_tools`): the update goes ahead, the agent keeps the version it
/// runs, and the answer lists it. A restart moves it to the new version.
/// The GC keeps a version while an agent runs it, and removes it after.
/// A daemon restart reads the version of a live runner back.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn an_update_while_an_agent_runs_keeps_its_version_until_a_restart() {
    let mut td = Td::start_with_tools().await;
    let tools = td.tools.clone().unwrap();
    let pin = tools.pin("claude").unwrap().version.clone();
    let id = td.agent("w", json!({})).await;
    td.post(&format!("/api/agents/{id}/start"), None).await;
    td.wait_status(&id, "idle").await;
    td.turn(&id, "one").await;
    assert_eq!(
        td.get(&format!("/api/agents/{id}")).await["harness_version"],
        pin.as_str()
    );
    let row = claude_row(&td).await;
    assert_eq!(row["active"], pin.as_str());
    assert_eq!(
        row["agents"],
        json!([{"agent": "demo/w", "id": id, "version": pin}])
    );
    let rec = std::fs::read_to_string(td.private_dir(&id).join("harness-version.json")).unwrap();
    assert!(rec.contains(&pin), "{rec}");
    // The runner runs the command of the version dir, by its path.
    let spec: Value = serde_json::from_str(
        &std::fs::read_to_string(td.private_dir(&id).join("runner.json")).unwrap(),
    )
    .unwrap();
    let program = tools.version_dir("claude", &pin).join("bin/claude");
    assert_eq!(
        spec["launch"]["program"],
        program.display().to_string(),
        "{spec}"
    );

    // The update: 200, the agent stays on its version, and its dir stays.
    let r = td
        .post(
            "/api/harnesses/claude/update",
            Some(json!({"version": "2.1.999"})),
        )
        .await;
    assert_eq!(r["from"], pin.as_str());
    assert_eq!(r["to"], "2.1.999");
    assert_eq!(
        r["agents_on_old_version"],
        json!([{"agent": "demo/w", "id": id, "version": pin}])
    );
    assert!(r["note"].as_str().unwrap().contains("next restart"), "{r}");
    assert_eq!(r.get("removed"), None, "{r}");
    let old = tools.version_dir("claude", &pin);
    assert!(old.join("bin/claude").is_file());
    assert_eq!(
        td.get(&format!("/api/agents/{id}")).await["harness_version"],
        pin.as_str()
    );
    // The agent still works on its version.
    td.turn(&id, "two").await;

    // A restart moves it to the active version.
    tui_wrapper(&tools.version_dir("claude", "2.1.999").join("bin/claude"));
    td.post(&format!("/api/agents/{id}/restart"), None).await;
    let start = Instant::now();
    loop {
        let v = td.get(&format!("/api/agents/{id}")).await;
        if v["harness_version"] == "2.1.999" && v["status"] == "idle" {
            break;
        }
        assert!(
            start.elapsed() < WAIT,
            "the restart did not take 2.1.999: {v}"
        );
        tokio::time::sleep(Duration::from_millis(30)).await;
    }

    // Nothing runs the old version now, but it is the previous version
    // (the rollback target), so the GC keeps it.
    let r = td
        .post(
            "/api/harnesses/claude/update",
            Some(json!({"version": "2.1.999"})),
        )
        .await;
    assert_eq!(r["reused"], true);
    assert_eq!(r["agents_on_old_version"], json!([]));
    assert!(r.get("removed").is_none(), "{r}");
    assert!(old.exists());

    // A daemon restart: the runner lives on, and the new daemon reads its
    // version back.
    td.shutdown();
    td.boot().await;
    td.wait_status(&id, "idle").await;
    let row = claude_row(&td).await;
    assert_eq!(row["agents"][0]["version"], "2.1.999", "{row}");

    td.post(&format!("/api/agents/{id}/stop"), None).await;
    td.wait_status(&id, "stopped").await;
    td.wait_runner_gone(&id).await;
}

/// The bytes of each file under `dir`, recursively, as text, except the
/// paths that `skip` names.
fn tree_text(dir: &Path, skip: &[PathBuf]) -> String {
    let mut out = String::new();
    if let Ok(rd) = std::fs::read_dir(dir) {
        for e in rd.flatten() {
            let p = e.path();
            if skip.contains(&p) {
                continue;
            }
            let Ok(m) = std::fs::symlink_metadata(&p) else {
                continue;
            };
            if m.is_dir() {
                out.push_str(&tree_text(&p, skip));
            } else if m.is_file()
                && let Ok(b) = std::fs::read(&p)
            {
                out.push_str(&String::from_utf8_lossy(&b));
            }
        }
    }
    out
}

/// PLAN §16b.4, secret manager review §14: the credential env of each
/// provider kind reaches the harness through the real `terrarium exec-env`
/// (the runner socket, then `POST /api/agent/secrets/env`). No value is in
/// `private/runner.json`, in any other file of the agent dir, or in any
/// file of the state dir outside the credential stores (the store, the
/// events, the audit log).
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn the_harness_credentials_come_through_exec_env_only() {
    struct Case {
        agent: &'static str,
        harness: &'static str,
        sub: Value,
        /// The API key to store, if any (provider, key).
        key: Option<(&'static str, &'static str)>,
        /// A Copilot plan account token, if any.
        account_token: Option<&'static str>,
        /// The vars that the harness must see with the value.
        want: &'static [(&'static str, &'static str)],
    }
    let cases = [
        Case {
            agent: "claude-key",
            harness: "claude",
            sub: json!({"name": "ant", "provider": "anthropic", "harnesses": ["claude"]}),
            key: Some(("anthropic", "sk-ant-E2E-anthropic-1")),
            account_token: None,
            want: &[("ANTHROPIC_API_KEY", "sk-ant-E2E-anthropic-1")],
        },
        Case {
            agent: "claude-zai",
            harness: "claude",
            sub: json!({"name": "glm", "provider": "zai", "plan": "coding",
                        "harnesses": ["claude", "pi"]}),
            key: Some(("zai", "zai-E2E-glm-key-2")),
            account_token: None,
            want: &[
                ("ANTHROPIC_AUTH_TOKEN", "zai-E2E-glm-key-2"),
                ("ZAI_API_KEY", "zai-E2E-glm-key-2"),
                ("ANTHROPIC_BASE_URL", "https://api.z.ai/api/anthropic"),
            ],
        },
        Case {
            agent: "pi-zai",
            harness: "pi",
            sub: json!({"name": "glm-pi", "provider": "zai", "plan": "coding",
                        "harnesses": ["pi"]}),
            key: Some(("zai", "zai-E2E-pi-key-3")),
            account_token: None,
            want: &[("ZAI_API_KEY", "zai-E2E-pi-key-3")],
        },
        Case {
            agent: "pi-kimi",
            harness: "pi",
            sub: json!({"name": "kimi", "provider": "moonshot", "plan": "coding",
                        "harnesses": ["pi"]}),
            key: Some(("moonshot", "sk-E2E-moonshot-4")),
            account_token: None,
            want: &[
                ("MOONSHOT_API_KEY", "sk-E2E-moonshot-4"),
                ("KIMI_API_KEY", "sk-E2E-moonshot-4"),
            ],
        },
        Case {
            agent: "pi-or",
            harness: "pi",
            sub: json!({"name": "or", "provider": "openrouter", "harnesses": ["pi"]}),
            key: Some(("openrouter", "sk-or-E2E-openrouter-5")),
            account_token: None,
            want: &[("OPENROUTER_API_KEY", "sk-or-E2E-openrouter-5")],
        },
        Case {
            agent: "codex-key",
            harness: "codex",
            sub: json!({"name": "oa", "provider": "openai", "harnesses": ["codex"]}),
            key: Some(("openai", "sk-E2E-openai-6")),
            account_token: None,
            want: &[("OPENAI_API_KEY", "sk-E2E-openai-6")],
        },
        Case {
            agent: "copilot-pat",
            harness: "copilot",
            sub: json!({"name": "pat", "provider": "github", "harnesses": ["copilot"]}),
            key: Some(("github", "github_pat_E2E_copilot_7")),
            account_token: None,
            want: &[("COPILOT_GITHUB_TOKEN", "github_pat_E2E_copilot_7")],
        },
        Case {
            agent: "copilot-plan",
            harness: "copilot",
            sub: json!({"name": "ghplan", "provider": "github", "harnesses": ["copilot"]}),
            key: None,
            account_token: Some("gho_E2E_copilot_plan_8"),
            want: &[("COPILOT_GITHUB_TOKEN", "gho_E2E_copilot_plan_8")],
        },
    ];

    let td = Td::start_with_cli().await;
    let state = td.dir.path().join("state");
    let mut values = Vec::new();
    for c in &cases {
        let name = c.sub["name"].as_str().unwrap();
        td.post("/api/subscriptions", Some(c.sub.clone())).await;
        if let Some((provider, key)) = c.key {
            td.post(
                &format!("/api/subscriptions/{name}/api-key"),
                Some(json!({"provider": provider, "key": key})),
            )
            .await;
            values.push(key);
        }
        if let Some(token) = c.account_token {
            terrariumd::accounts::write_credential(
                &state,
                name,
                HarnessId::Copilot,
                token.as_bytes(),
            )
            .unwrap();
            values.push(token);
        }
        let id = td
            .agent(c.agent, json!({"harness": c.harness, "subscription": name}))
            .await;
        td.post(&format!("/api/agents/{id}/start"), None).await;
        td.wait_status(&id, "idle").await;
        let seen = std::fs::read_to_string(td.dir.path().join("seen").join(format!("{id}.env")))
            .unwrap_or_else(|e| panic!("{}: the harness wrote no env: {e}", c.agent));
        for (k, v) in c.want {
            assert!(
                seen.lines().any(|l| l == format!("{k}={v}")),
                "{}: the harness has no {k}",
                c.agent
            );
        }
        // The runner spec of this launch is there, with no value.
        let spec = std::fs::read_to_string(td.private_dir(&id).join("runner.json")).unwrap();
        assert!(spec.contains("exec-env"), "{}: {spec}", c.agent);
        assert!(spec.contains("--require-credentials"), "{}", c.agent);
        for v in &values {
            assert!(!spec.contains(v), "{}: runner.json holds a key", c.agent);
        }
        td.post(&format!("/api/agents/{id}/stop"), None).await;
        td.wait_status(&id, "stopped").await;
        td.wait_runner_gone(&id).await;
    }
    // No file of the state dir holds a value, except the credential
    // stores (`credentials/`).
    let all = tree_text(&state, &[state.join("credentials")]);
    assert!(!all.is_empty());
    for v in &values {
        assert!(!all.contains(v), "a file of the state dir holds {v}");
    }
}

/// PLAN §16b.4: when `terrarium exec-env --require-credentials` cannot
/// reach the daemon, it does not start the harness; it exits 78, and the
/// status reason says why.
#[test]
fn exec_env_without_the_daemon_does_not_start_a_harness_with_no_credentials() {
    let dir = tempfile::tempdir().unwrap();
    let o = std::process::Command::new(cli_bin())
        .env_clear()
        .env("PATH", std::env::var("PATH").unwrap_or_default())
        .env("TERRARIUM_AGENT", "01AGENT")
        .env("TERRARIUM_SOCKET", dir.path().join("gone.sock"))
        .args([
            "exec-env",
            "--require-credentials",
            "--",
            "sh",
            "-c",
            "echo started",
        ])
        .output()
        .unwrap();
    assert_eq!(
        o.status.code(),
        Some(terrarium_core::launch::EXEC_ENV_NO_CREDENTIALS)
    );
    assert!(!String::from_utf8_lossy(&o.stdout).contains("started"));
    let err = String::from_utf8_lossy(&o.stderr);
    assert!(err.contains("try 5 of 5"), "{err}");
    assert!(err.contains("does not start"), "{err}");
    let how = terrariumd::runner_supervisor::exit_how(&terrarium_runner::ExitStatus {
        code: 78,
        signal: None,
    });
    assert!(how.contains("harness credentials"), "{how}");
}
