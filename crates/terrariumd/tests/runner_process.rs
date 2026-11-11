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

use crate::common;

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

/// The fixed deadlines of this file. 20 s was too short at load 40 to 60
/// on 32 cores (TER-647). The state waits of [`Td`] count only time
/// without progress ([`common::Stall`]).
const WAIT: Duration = common::WAIT;

fn fake_tui() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("../terrarium-runner/tests/fake_tui/fake_tui.py")
}

/// The `terrarium-runner` binary next to the test binaries. A missing one
/// is built once.
fn runner_bin() -> PathBuf {
    // CI builds the pinned N-1 runner separately and drives it with this
    // daemon, rather than pretending that a current binary is an old one.
    if let Some(path) = std::env::var_os("TERRARIUM_TEST_OLD_RUNNER") {
        let path = PathBuf::from(path);
        assert!(path.is_file(), "old runner fixture is missing");
        return path;
    }
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
    /// The keys of the runner's fake adapter: Esc, then a CR. The daemon
    /// checks that the harness has interrupt keys; the runner types its
    /// own.
    fn interrupt_keys(&self) -> Vec<Vec<u8>> {
        vec![b"\x1b".to_vec(), b"\r".to_vec()]
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

/// The daemon build that the supervisor compares runners with, and its
/// upgrade limit ([`Td::upgrade`]).
#[derive(Clone, Default)]
struct Upgrade {
    version: Option<String>,
    limit: Option<usize>,
    /// Start the runner processes through [`gated_runner`].
    runner: Option<PathBuf>,
}

fn supervisor(
    root: &Path,
    tools: Option<&Path>,
    cli: bool,
    upgrade: &Upgrade,
) -> Arc<RunnerSupervisor> {
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
    cfg.mode = RunnerMode::Process {
        bin: upgrade.runner.clone().unwrap_or_else(runner_bin),
    };
    // A version with no ref schedules no upgrade restart. The runner binary
    // of the tests can be of another build than the test daemon: the
    // pinned N-1 runner, or a `terrarium-runner` that `runner_bin` finds
    // from an earlier build with another `TERRARIUM_GIT_REF`. Only the
    // upgrade test claims a build.
    cfg.version = upgrade.version.clone().unwrap_or_else(|| "0.0.0".into());
    if let Some(n) = upgrade.limit {
        cfg.upgrade_limit = n;
    }
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
    /// The next [`Td::boot`] claims this daemon build.
    upgrade: Upgrade,
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
            upgrade: Upgrade::default(),
        }
    }

    /// Start a daemon with a new supervisor on the same state dir.
    async fn boot(&mut self) {
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
            supervisor: supervisor(
                self.dir.path(),
                self.tools.as_ref().map(|t| t.dir.as_path()),
                self.cli,
                &self.upgrade,
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

    /// The newest event `seq` of the agent and its status: what moves
    /// while a start, a restart or a turn goes on.
    async fn progress(&self, id: &str) -> (Value, Value) {
        let newest = self
            .get(&format!("/api/agents/{id}/events?order=desc&limit=1"))
            .await;
        (newest[0]["seq"].clone(), self.status(id).await)
    }

    async fn wait_status(&self, id: &str, want: &str) {
        let mut stall = common::Stall::new(self.progress(id).await);
        loop {
            let s = self.status(id).await;
            if s == want {
                return;
            }
            if stall.due() {
                let mark = self.progress(id).await;
                assert!(
                    stall.moved(mark),
                    "agent never reached {want}; last {s} ({stall})"
                );
            }
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
        let mut stall = common::Stall::new(self.progress(id).await);
        loop {
            let ev = self.events(id, kind).await;
            if f(&ev) {
                return ev;
            }
            if stall.due() {
                let mark = self.progress(id).await;
                assert!(
                    stall.moved(mark),
                    "{kind} events never matched ({stall}): {ev:#?}"
                );
            }
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

/// Issue #12: the Stop of the dashboard through a runner process. The
/// fake TUI stops `/hold` on Esc with no Stop hook (as Claude Code), and
/// the settled screen ends the turn: `idle` with the reason `interrupted`.
/// The harness keeps running and takes the next turn.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn interrupt_stops_the_turn_and_keeps_the_harness() {
    let td = Td::start().await;
    let id = td.agent("bob", json!({})).await;
    td.post(&format!("/api/agents/{id}/start"), None).await;
    td.wait_status(&id, "idle").await;
    let pid = td.runner_pid(&id);
    let url = format!("/api/agents/{id}/interrupt");
    let (s, b) = td.req(Method::POST, &url, None).await;
    assert_eq!(s, 409, "{b}");
    assert_eq!(b["error"]["code"], "conflict", "{b}");

    td.post(
        &format!("/api/agents/{id}/send"),
        Some(json!({"text": "/hold"})),
    )
    .await;
    td.wait_status(&id, "working").await;
    let r = td.post(&url, None).await;
    assert_eq!(r["interrupted"], true, "{r}");
    td.wait_status(&id, "idle").await;
    let a = td.get(&format!("/api/agents/{id}")).await;
    assert_eq!(a["status_reason"], "interrupted", "{a}");
    assert!(td.events(&id, "turn_end").await.is_empty());
    let notices = td.events(&id, "system").await;
    assert!(
        notices.iter().any(|e| e["data"]["message"]
            .as_str()
            .is_some_and(|m| m.starts_with("interrupt"))),
        "{notices:#?}"
    );
    assert_eq!(td.runner_pid(&id), pid, "the same runner, no restart");
    td.turn(&id, "after").await;
    td.post(&format!("/api/agents/{id}/stop"), None).await;
    td.wait_status(&id, "stopped").await;
}

/// Actual CLI calls wait through a hold and reach the next daemon exactly once.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn agent_rpc_cli_calls_cross_a_graceful_restart() {
    hot_swap_probation(false).await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn agent_rpc_hot_swap_failed_health_restores_then_replays_once() {
    hot_swap_probation(true).await;
}

#[tokio::test]
async fn agent_rpc_health_reports_activating_until_admission_is_ready() {
    let mut td = Td::start().await;
    td.shutdown();
    let releases = td.dir.path().join("state/releases");
    std::fs::create_dir_all(&releases).unwrap();
    let marker = releases.join("probation");
    std::fs::write(&marker, "").unwrap();
    td.boot().await;
    let state = td.daemon.as_ref().unwrap().state.clone();
    // Current-thread runtime plus direct handler calls keep activation from
    // racing the assertion between marker removal and activation_ready.
    std::fs::remove_file(marker).unwrap();
    let response = terrariumd::api::system::health(axum::extract::State(state.clone())).await;
    let bytes = response.into_body().collect().await.unwrap().to_bytes();
    assert_eq!(
        serde_json::from_slice::<Value>(&bytes).unwrap()["status"],
        "activating"
    );
    state.agent_calls.activation_ready();
    let response = terrariumd::api::system::health(axum::extract::State(state)).await;
    let bytes = response.into_body().collect().await.unwrap().to_bytes();
    assert_eq!(
        serde_json::from_slice::<Value>(&bytes).unwrap()["status"],
        "ok"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn agent_rpc_activation_failure_keeps_writes_closed_and_can_restart() {
    let mut td = Td::start().await;
    let state = td.dir.path().join("state");
    let key = state.join("secrets.key");
    let original = std::fs::read(&key).unwrap();
    td.shutdown();
    std::fs::create_dir_all(state.join("releases")).unwrap();
    let marker = state.join("releases/probation");
    std::fs::write(&marker, "").unwrap();
    std::fs::write(&key, b"invalid test key").unwrap();
    td.boot().await;
    std::fs::remove_file(marker).unwrap();
    let error = timeout(
        WAIT,
        td.daemon.as_ref().unwrap().state.agent_calls.wait_failure(),
    )
    .await
    .unwrap();
    assert!(error.contains("secret store"));
    assert_eq!(
        td.req_as(None, Method::GET, "/api/health", None).await.0,
        503
    );
    assert_eq!(
        td.req_as(
            None,
            Method::POST,
            "/api/projects",
            Some(json!({"name":"not-admitted"}))
        )
        .await
        .0,
        503
    );
    td.shutdown();
    std::fs::write(key, original).unwrap();
    td.boot().await;
    assert_eq!(
        td.req_as(None, Method::GET, "/api/health", None).await.0,
        200
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn agent_rpc_activation_failure_exits_the_daemon_nonzero() {
    let dir = tempfile::tempdir().unwrap();
    let state = dir.path().join("state");
    std::fs::create_dir_all(state.join("releases")).unwrap();
    std::fs::create_dir_all(dir.path().join("config")).unwrap();
    let marker = state.join("releases/probation");
    std::fs::write(&marker, "").unwrap();
    std::fs::write(state.join("secrets.key"), b"invalid test key").unwrap();
    let socket = dir.path().join("api.sock");
    let mut child = tokio::process::Command::new(env!("CARGO_BIN_EXE_terrariumd"))
        .env_clear()
        .env("PATH", std::env::var("PATH").unwrap())
        .env("TERRARIUM_STATE_DIR", &state)
        .env("TERRARIUM_API_SOCKET", &socket)
        .env("TERRARIUM_CONFIG_DIR", dir.path().join("config"))
        .env("TERRARIUM_CONFIG_REPO", dir.path().join("config"))
        .env("TERRARIUM_RUNTIME_DIR", dir.path().join("run"))
        .env("TERRARIUM_HOST_LISTEN", "")
        .env("TERRARIUM_ARTIFACT_LISTEN", "")
        .env("TERRARIUM_RUNNER", runner_bin())
        .args([
            "--sandbox",
            "none",
            "--tools-dir",
            "",
            "--no-harness-install",
        ])
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::piped())
        .kill_on_drop(true)
        .spawn()
        .unwrap();
    timeout(WAIT, async {
        while !socket.exists() {
            assert!(
                child.try_wait().unwrap().is_none(),
                "daemon exited before probation health"
            );
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    })
    .await
    .unwrap();
    assert!(child.try_wait().unwrap().is_none());
    std::fs::remove_file(marker).unwrap();
    let output = timeout(WAIT, child.wait_with_output())
        .await
        .unwrap()
        .unwrap();
    assert!(!output.status.success());
    assert!(
        String::from_utf8_lossy(&output.stderr).contains("activation failed after health commit")
    );
}

async fn hot_swap_probation(rollback: bool) {
    let cli = cli_bin();
    let mut td = Td::start().await;
    let bob = td.agent("bob", json!({})).await;
    td.post(&format!("/api/agents/{bob}/start"), None).await;
    td.wait_status(&bob, "idle").await;
    let sock = td.run_dir().join(format!("agents/{bob}.sock"));
    let pid = td.runner_pid(&bob).unwrap();
    terrariumd::agent_rpc::drain(&td.daemon.as_ref().unwrap().state, Duration::from_secs(2))
        .await
        .unwrap();
    let cli_sock = sock.clone();
    let calls = tokio::spawn(async move {
        for n in 0..5 {
            let out = tokio::process::Command::new(&cli)
                .env_clear()
                .env("TERRARIUM_AGENT", "bob")
                .env("TERRARIUM_SOCKET", &cli_sock)
                .args([
                    "-p",
                    "demo",
                    "mail",
                    "send",
                    "demo/bob",
                    "-s",
                    "restart",
                    &format!("restart-message-{n}"),
                ])
                .kill_on_drop(true)
                .output()
                .await
                .unwrap();
            assert!(
                out.status.success(),
                "CLI failed: {}",
                String::from_utf8_lossy(&out.stderr)
            );
        }
    });
    tokio::time::sleep(Duration::from_millis(150)).await;
    assert!(
        !calls.is_finished(),
        "calls must wait for hello from the next daemon"
    );
    td.shutdown();
    let state = td.dir.path().join("state");
    let releases = state.join("releases");
    std::fs::create_dir_all(&releases).unwrap();
    let probation = releases.join("probation");
    std::fs::write(&probation, "held").unwrap();
    let helper = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../docker/hot-swap.py");
    let backup = state.join("test-backup");
    let control = |command: &str| {
        let out = std::process::Command::new("python3")
            .arg(&helper)
            .arg(command)
            .arg(&backup)
            .env("TERRARIUM_STATE_DIR", &state)
            .output()
            .unwrap();
        assert!(
            out.status.success(),
            "{}",
            String::from_utf8_lossy(&out.stderr)
        );
    };
    control("snapshot");
    let audit_count = || {
        rusqlite::Connection::open(state.join("terrarium.db"))
            .unwrap()
            .query_row("SELECT count(*) FROM audit", [], |r| r.get::<_, i64>(0))
            .unwrap()
    };
    let before_probation = audit_count();
    td.boot().await;
    tokio::time::sleep(Duration::from_millis(300)).await;
    let (health, _) = td.req_as(None, Method::GET, "/api/health", None).await;
    assert_eq!(
        health, 200,
        "health reads remain available during probation"
    );
    let write = json!({"name": "probation-dashboard"});
    let (status, response) = td
        .req_as(None, Method::POST, "/api/projects", Some(write.clone()))
        .await;
    assert_eq!(status, 503);
    assert_eq!(response["error"], "upgrade_probation");
    let (status, _) = td
        .req_as(None, Method::GET, "/api/projects/probation-dashboard", None)
        .await;
    assert_eq!(status, 404, "refused dashboard write has no effect to lose");
    assert_eq!(
        audit_count(),
        before_probation,
        "definition sync, reconciliation, jobs and background writers must not run during probation"
    );
    assert!(
        !calls.is_finished(),
        "healthy probation must not admit held calls"
    );
    if rollback {
        let conn = rusqlite::Connection::open(state.join("terrarium.db")).unwrap();
        conn.execute_batch("CREATE TABLE hot_swap_test_migration(value TEXT)")
            .unwrap();
        drop(conn);
        terrariumd::agent_rpc::drain(&td.daemon.as_ref().unwrap().state, Duration::from_secs(2))
            .await
            .unwrap();
        td.shutdown();
        control("restore-database");
        td.boot().await;
        let conn = rusqlite::Connection::open(state.join("terrarium.db")).unwrap();
        assert!(
            conn.prepare("SELECT * FROM hot_swap_test_migration")
                .is_err()
        );
        assert!(
            !calls.is_finished(),
            "restored daemon still awaits health commit"
        );
    }
    std::fs::remove_file(&probation).unwrap();
    timeout(WAIT, calls).await.unwrap().unwrap();
    let (status, _) = td
        .req_as(None, Method::POST, "/api/projects", Some(write))
        .await;
    assert_eq!(status, 201, "dashboard retry succeeds after commit");
    let (status, _) = td
        .req_as(None, Method::GET, "/api/projects/probation-dashboard", None)
        .await;
    assert_eq!(status, 200);
    assert_eq!(
        td.runner_pid(&bob),
        Some(pid),
        "runner survived the restart"
    );
    let (status, mail) = td
        .req_as(Some("demo/bob"), Method::GET, "/api/mail", None)
        .await;
    assert_eq!(status, 200);
    let messages = mail.as_array().unwrap();
    for n in 0..5 {
        assert_eq!(
            messages
                .iter()
                .filter(|m| m["body"] == format!("restart-message-{n}"))
                .count(),
            1
        );
    }
}

/// R7: compose the real runner/CLI tunnel, native host run recovery, and
/// deferred runner upgrade. Only the harness and daemon replacement are
/// fakes: no production daemon or container is touched.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn hot_swap_mid_turn_preserves_wait_and_target_run() {
    let mut td = Td::start().await;
    let state = td.daemon.as_ref().unwrap().state.clone();
    let (listener, addr) = terrariumd::hosts::serve(state.clone(), "127.0.0.1:0")
        .await
        .unwrap();
    let host_info = td.post("/api/hosts", Some(json!({"name":"lab1"}))).await;
    let mut opts = terrarium_host::ClientOptions::new(
        format!("ws://{addr}/hosts/connect"),
        host_info["token"].as_str().unwrap(),
        "lab1",
    );
    opts.capabilities.shell = Some("/bin/sh".into());
    opts.backoff_min = Duration::from_millis(50);
    opts.backoff_max = Duration::from_millis(200);
    opts.paths = terrarium_host::HostPaths {
        work_dir: td.dir.path().join("work"),
        instances_dir: td.dir.path().join("instances"),
        repos_dir: td.dir.path().join("repos"),
    };
    let host = tokio::spawn(terrarium_host::Client::new(opts).run());
    timeout(WAIT, async {
        while td.get("/api/hosts/lab1").await["online"] != true
            || state.executor.resource_pending("lab1")
        {
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    })
    .await
    .unwrap();
    td.post(
        "/api/targets",
        Some(json!({"name":"box", "kind":"host", "host":"lab1"})),
    )
    .await;
    let bob = td.agent("bob", json!({"policy":{"targets":["box"]}})).await;
    td.post(&format!("/api/agents/{bob}/start"), None).await;
    td.wait_status(&bob, "idle").await;
    td.post(
        &format!("/api/agents/{bob}/send"),
        Some(json!({"text":"/hold"})),
    )
    .await;
    td.wait_status(&bob, "working").await;
    let old_pid = td.runner_pid(&bob).unwrap();
    let mark = last_seq(&td, &bob).await;
    let sock = td.run_dir().join(format!("agents/{bob}.sock"));
    let cli = |args: Vec<String>| {
        let sock = sock.clone();
        async move {
            let out = tokio::process::Command::new(cli_bin())
                .env_clear()
                .env("TERRARIUM_AGENT", "bob")
                .env("TERRARIUM_SOCKET", sock)
                .args(["-p", "demo", "--json"])
                .args(args)
                .kill_on_drop(true)
                .output()
                .await
                .unwrap();
            assert!(
                out.status.success(),
                "CLI: {}",
                String::from_utf8_lossy(&out.stderr)
            );
            serde_json::from_slice::<Value>(&out.stdout).unwrap()
        }
    };
    let wait = cli(vec![
        "wait".into(),
        "time".into(),
        "--for".into(),
        "1h".into(),
    ])
    .await;
    let gate = td.dir.path().join("finish-run");
    // Release the native shell even when an assertion fails before completion.
    struct ReleaseRun(PathBuf);
    impl Drop for ReleaseRun {
        fn drop(&mut self) {
            let _ = std::fs::write(&self.0, "");
        }
    }
    let _release = ReleaseRun(gate.clone());
    let started = td.dir.path().join("run-started");
    let script = format!(
        "echo before; touch '{}'; while [ ! -e '{}' ]; do sleep 0.02; done; echo after",
        started.display(),
        gate.display()
    );
    let run = cli(vec![
        "run".into(),
        "box".into(),
        "--name".into(),
        "proof".into(),
        "--".into(),
        "sh".into(),
        "-c".into(),
        script,
    ])
    .await;
    let wid = wait["id"].as_str().unwrap();
    let rid = run["wait"]["id"].as_str().unwrap();
    timeout(WAIT, async {
        while !started.exists() {
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    })
    .await
    .unwrap();
    for id in [wid, rid] {
        assert_eq!(td.get(&format!("/api/waits/{id}")).await["state"], "open");
    }

    terrariumd::agent_rpc::drain(&state, Duration::from_secs(2))
        .await
        .unwrap();
    let held = tokio::spawn(cli(vec!["whoami".into()]));
    tokio::time::sleep(Duration::from_millis(100)).await;
    assert!(
        !held.is_finished(),
        "CLI waits rather than reporting a tunnel error"
    );
    listener.abort();
    td.shutdown();
    td.upgrade = Upgrade {
        version: Some("0.0.0+0000000".into()),
        limit: Some(3),
        runner: None,
    };
    td.boot().await;
    let (listener, _) =
        terrariumd::hosts::serve(td.daemon.as_ref().unwrap().state.clone(), &addr.to_string())
            .await
            .unwrap();
    timeout(WAIT, held).await.unwrap().unwrap();
    td.wait_events(&bob, "system", |events| {
        events.iter().any(|e| {
            e["data"]["message"]
                .as_str()
                .is_some_and(|m| m.starts_with("restart_scheduled (reason upgrade)"))
        })
    })
    .await;
    assert_eq!(
        td.runner_pid(&bob),
        Some(old_pid),
        "still in the original turn"
    );
    assert_eq!(td.events(&bob, "session_started").await.len(), 1);
    for id in [wid, rid] {
        assert_eq!(td.get(&format!("/api/waits/{id}")).await["state"], "open");
    }
    td.post(
        &format!("/api/agents/{bob}/send"),
        Some(json!({"text":"go", "raw":true})),
    )
    .await;
    let starts = td
        .wait_events(&bob, "session_started", |e| e.len() == 2)
        .await;
    let start = starts[1]["seq"].as_u64().unwrap();
    let ends = td.events(&bob, "turn_end").await;
    assert!(
        ends.iter()
            .filter_map(|e| e["seq"].as_u64())
            .any(|s| s > mark && s < start)
    );
    // Open waits make the resting status waiting, not idle.
    td.wait_status(&bob, "waiting").await;
    assert_ne!(td.runner_pid(&bob), Some(old_pid));
    assert!(td.events(&bob, "error").await.is_empty());
    std::fs::write(&gate, "").unwrap();
    let resolved = timeout(WAIT, async {
        loop {
            let w = td.get(&format!("/api/waits/{rid}")).await;
            if w["state"] == "resolved" {
                break w;
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    })
    .await
    .unwrap();
    assert_eq!(resolved["result"]["exit_code"], 0);
    assert_eq!(resolved["result"]["stdout_tail"], "before\nafter\n");
    assert_eq!(td.get(&format!("/api/waits/{wid}")).await["state"], "open");
    td.post(&format!("/api/agents/{bob}/stop"), None).await;
    td.wait_status(&bob, "stopped").await;
    host.abort();
    listener.abort();
}

/// A runner command that waits while the file `gate` exists, then runs
/// the real runner: a relaunch holds there.
fn gated_runner(dir: &Path, gate: &Path) -> PathBuf {
    use std::os::unix::fs::PermissionsExt;
    let path = dir.join("gated-runner");
    std::fs::write(
        &path,
        format!(
            "#!/bin/sh\nwhile [ -e '{}' ]; do sleep 0.02; done\nexec '{}' \"$@\"\n",
            gate.display(),
            runner_bin().display()
        ),
    )
    .unwrap();
    std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755)).unwrap();
    path
}

/// The status events of the daemon in store order: (agent, status, reason).
fn status_order(td: &Td) -> Vec<(String, String, String)> {
    let db = rusqlite::Connection::open(td.dir.path().join("state/terrarium.db")).unwrap();
    let mut q = db
        .prepare("SELECT agent_id, payload FROM events WHERE kind = 'status' ORDER BY seq")
        .unwrap();
    q.query_map([], |r| Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?)))
        .unwrap()
        .map(|r| {
            let (agent, payload) = r.unwrap();
            let v: Value = serde_json::from_str(&payload).unwrap_or(Value::Null);
            let d = v.get("data").unwrap_or(&v);
            let text = |k: &str| d[k].as_str().unwrap_or_default().to_owned();
            (agent, text("status"), text("reason"))
        })
        .collect()
}

/// The upgrade restarts in store order: an agent is in its upgrade from
/// its `restarting` (reason `upgrade`) to the `starting` of the new launch.
/// The most agents in their upgrade at one time.
fn most_upgrading(order: &[(String, String, String)]) -> usize {
    let mut now = std::collections::HashSet::new();
    let mut most = 0;
    for (agent, status, reason) in order {
        if status == "restarting" && reason == "upgrade" {
            now.insert(agent.clone());
            most = most.max(now.len());
        } else if status == "starting" {
            now.remove(agent);
        }
    }
    most
}

/// R4 (TER-262): after a hot swap, a runner of another build gets
/// `restart_scheduled` (reason `upgrade`). An agent out of its turn
/// restarts with resume, one in a turn when the turn ends. A runner
/// `blocked` on a question is in its turn: it keeps running until it
/// rests. Here the new daemon claims another build. With a limit of 1, each
/// next upgrade starts only after the earlier launch is in (M1).
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn an_older_runner_upgrades_after_its_turn() {
    let mut td = Td::start().await;
    let idle = [
        td.agent("calm", json!({})).await,
        td.agent("rest", json!({})).await,
        td.agent("more", json!({})).await,
    ];
    let busy = td.agent("busy", json!({})).await;
    let asks = td.agent("asks", json!({})).await;
    for id in idle.iter().chain([&busy, &asks]) {
        td.post(&format!("/api/agents/{id}/start"), None).await;
        td.wait_status(id, "idle").await;
    }
    td.post(
        &format!("/api/agents/{busy}/send"),
        Some(json!({"text": "/hold"})),
    )
    .await;
    td.wait_status(&busy, "working").await;
    td.post(
        &format!("/api/agents/{asks}/send"),
        Some(json!({"text": "/question"})),
    )
    .await;
    td.wait_status(&asks, "blocked").await;
    let pid = |td: &Td, id: &str| td.runner_pid(id).unwrap();
    let old: HashMap<String, u32> = idle
        .iter()
        .chain([&busy, &asks])
        .map(|id| (id.clone(), pid(&td, id)))
        .collect();

    td.shutdown();
    tokio::time::sleep(Duration::from_millis(300)).await;
    td.upgrade = Upgrade {
        version: Some("0.0.0+0000000".into()),
        limit: Some(1),
        runner: None,
    };
    td.boot().await;

    // Each agent out of a turn restarts with resume on a new runner.
    for id in &idle {
        let notices = td
            .wait_events(id, "system", |e| {
                e.iter().any(|n| {
                    n["data"]["message"]
                        .as_str()
                        .is_some_and(|m| m.starts_with("restart_scheduled (reason upgrade)"))
                })
            })
            .await;
        assert!(!notices.is_empty());
        td.wait_events(id, "session_started", |e| e.len() == 2)
            .await;
        td.wait_status(id, "idle").await;
        assert_ne!(pid(&td, id), old[id], "a new runner process");
        let statuses = td.events(id, "status").await;
        let reasons: Vec<&str> = statuses
            .iter()
            .filter_map(|s| s["data"]["reason"].as_str())
            .collect();
        assert!(reasons.contains(&"upgrade"), "{reasons:?}");
        assert!(
            reasons.iter().any(|r| r.starts_with("resuming session")),
            "{reasons:?}"
        );
    }
    let order = status_order(&td);
    assert_eq!(most_upgrading(&order), 1, "at most the limit: {order:#?}");

    // In a turn, and blocked on a question in a turn: no restart.
    for id in [&busy, &asks] {
        assert_eq!(td.runner_pid(id), Some(old[id]), "no restart in a turn");
        assert_eq!(td.events(id, "session_started").await.len(), 1);
    }
    assert_eq!(td.status(&busy).await, "working");
    assert_eq!(td.status(&asks).await, "blocked");
    for (id, line) in [(&busy, "go"), (&asks, "y")] {
        td.post(
            &format!("/api/agents/{id}/send"),
            Some(json!({"text": line, "raw": true})),
        )
        .await;
        let starts = td
            .wait_events(id, "session_started", |e| e.len() == 2)
            .await;
        let ends = td.events(id, "turn_end").await;
        assert!(
            ends.last().unwrap()["seq"].as_u64() < starts[1]["seq"].as_u64(),
            "the restart came after the turn: {ends:#?} {starts:#?}"
        );
        td.wait_status(id, "idle").await;
        assert_ne!(td.runner_pid(id), Some(old[id]), "a new runner process");
    }
    assert_eq!(most_upgrading(&status_order(&td)), 1);

    // The new runners are not upgraded again.
    tokio::time::sleep(Duration::from_millis(500)).await;
    for id in idle.iter().chain([&busy, &asks]) {
        assert_eq!(td.events(id, "session_started").await.len(), 2);
    }
    for id in idle.iter().chain([&busy, &asks]) {
        td.post(&format!("/api/agents/{id}/stop"), None).await;
        td.wait_status(id, "stopped").await;
    }
}

/// TER-524: a runner `blocked` on a question is in its turn. An account
/// swap that cannot be live waits for the answer and the end of the turn,
/// then restarts the harness with resume. A swap while the agent is
/// `blocked` does not restart at once, and a swap that waits from
/// `working` does not restart when the runner turns `blocked`.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn an_account_swap_waits_for_a_blocked_turn() {
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
            "asks",
            json!({"subscription": "max-x", "subscription_pool": ["max-x", "max-y"]}),
        )
        .await;
    td.post(&format!("/api/agents/{id}/start"), None).await;
    td.wait_status(&id, "idle").await;

    // Swap while blocked on a question: the restart waits.
    let old = td.runner_pid(&id).unwrap();
    td.post(
        &format!("/api/agents/{id}/send"),
        Some(json!({"text": "/question"})),
    )
    .await;
    td.wait_status(&id, "blocked").await;
    let mark = last_seq(&td, &id).await;
    swap_waits(&td, &id, "max-y", old, 1).await;
    td.post(
        &format!("/api/agents/{id}/send"),
        Some(json!({"text": "y", "raw": true})),
    )
    .await;
    restarted_after_turn(&td, &id, mark, 2, old).await;

    // Swap while working: the runner turns `blocked` in the same turn
    // (a question hook), and the restart still waits for its end.
    let old = td.runner_pid(&id).unwrap();
    td.post(
        &format!("/api/agents/{id}/send"),
        Some(json!({"text": "/hold"})),
    )
    .await;
    td.wait_status(&id, "working").await;
    let mark = last_seq(&td, &id).await;
    let r = td
        .post(
            &format!("/api/agents/{id}/subscription"),
            Some(json!({"subscription": "max-x"})),
        )
        .await;
    assert_eq!(r["applied"], "restart_scheduled", "{r}");
    let sock = td.run_dir().join(format!("agents/{id}.sock"));
    hook(&sock, "Notification", json!({"message": "Allow the tool?"})).await;
    td.wait_status(&id, "blocked").await;
    tokio::time::sleep(Duration::from_millis(500)).await;
    assert_eq!(td.runner_pid(&id), Some(old), "no restart at blocked");
    assert_eq!(td.events(&id, "session_started").await.len(), 2);
    assert_eq!(td.status(&id).await, "blocked");
    td.post(
        &format!("/api/agents/{id}/send"),
        Some(json!({"text": "go", "raw": true})),
    )
    .await;
    restarted_after_turn(&td, &id, mark, 3, old).await;

    // Queued work does not hold the swap back, and it goes to the new
    // launch: a mail that comes during the turn queues a pointer, and the
    // pointer turn starts only in the new session.
    let old = td.runner_pid(&id).unwrap();
    td.post(
        &format!("/api/agents/{id}/send"),
        Some(json!({"text": "/hold"})),
    )
    .await;
    td.wait_status(&id, "working").await;
    let mark = last_seq(&td, &id).await;
    let m = td
        .post(
            "/api/mail",
            Some(json!({"to": "demo/asks", "body": "mail during the turn"})),
        )
        .await;
    let thread = m["thread_id"].as_str().unwrap().to_owned();
    let r = td
        .post(
            &format!("/api/agents/{id}/subscription"),
            Some(json!({"subscription": "max-y"})),
        )
        .await;
    assert_eq!(r["applied"], "restart_scheduled", "{r}");
    td.post(
        &format!("/api/agents/{id}/send"),
        Some(json!({"text": "go", "raw": true})),
    )
    .await;
    restarted_after_turn(&td, &id, mark, 4, old).await;
    let start = td.events(&id, "session_started").await[3]["seq"]
        .as_u64()
        .unwrap();
    let pointed = |e: &Value| {
        e["data"]["prompt"]
            .as_str()
            .is_some_and(|p| p.contains(&thread))
    };
    let turns = td
        .wait_events(&id, "turn_start", |e| e.iter().any(pointed))
        .await;
    let seqs: Vec<u64> = turns
        .iter()
        .filter(|e| pointed(e))
        .filter_map(|e| e["seq"].as_u64())
        .collect();
    assert!(
        seqs.iter().all(|s| *s > start),
        "the pointer went to the old harness: {start} {seqs:?} {turns:#?}"
    );

    td.post(&format!("/api/agents/{id}/stop"), None).await;
    td.wait_status(&id, "stopped").await;
}

/// The `seq` of the newest status event of agent `id`.
async fn last_seq(td: &Td, id: &str) -> u64 {
    td.events(id, "status").await.last().unwrap()["seq"]
        .as_u64()
        .unwrap()
}

/// A swap of a `blocked` agent to `to` is scheduled, and for 500 ms the
/// runner `old` and its `sessions` session starts stay.
async fn swap_waits(td: &Td, id: &str, to: &str, old: u32, sessions: usize) {
    let r = td
        .post(
            &format!("/api/agents/{id}/subscription"),
            Some(json!({"subscription": to})),
        )
        .await;
    assert_eq!(r["applied"], "restart_scheduled", "{r}");
    tokio::time::sleep(Duration::from_millis(500)).await;
    assert_eq!(td.runner_pid(id), Some(old), "no restart in a turn");
    assert_eq!(td.events(id, "session_started").await.len(), sessions);
    assert_eq!(td.status(id).await, "blocked");
}

/// The harness started its session number `n` on a new runner, after a
/// `turn_end` of the turn that was open at `mark` (a `seq` from before the
/// swap call).
async fn restarted_after_turn(td: &Td, id: &str, mark: u64, n: usize, old: u32) {
    let starts = td
        .wait_events(id, "session_started", |e| e.len() == n)
        .await;
    let start = starts[n - 1]["seq"].as_u64().unwrap();
    let ends = td.events(id, "turn_end").await;
    assert!(
        ends.iter()
            .filter_map(|e| e["seq"].as_u64())
            .any(|s| s > mark && s < start),
        "no turn_end between the swap and the restart: {mark} {ends:#?} {starts:#?}"
    );
    td.wait_status(id, "idle").await;
    assert_ne!(td.runner_pid(id), Some(old), "a new runner process");
}

/// M2 (TER-262): a stop between the kill of the old runner and the launch
/// of the new one frees the upgrade slot: the next upgrade still runs.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_stop_during_an_upgrade_frees_its_slot() {
    let mut td = Td::start().await;
    let agents = [
        td.agent("first", json!({})).await,
        td.agent("second", json!({})).await,
    ];
    for id in &agents {
        td.post(&format!("/api/agents/{id}/start"), None).await;
        td.wait_status(id, "idle").await;
    }
    td.shutdown();
    tokio::time::sleep(Duration::from_millis(300)).await;
    // The relaunch of the upgrade holds until the gate goes.
    let gate = td.dir.path().join("relaunch-gate");
    std::fs::write(&gate, "").unwrap();
    td.upgrade = Upgrade {
        version: Some("0.0.0+0000000".into()),
        limit: Some(1),
        runner: Some(gated_runner(td.dir.path(), &gate)),
    };
    td.boot().await;

    // One agent holds the only slot: its old runner is gone, the new one
    // waits at the gate.
    let start = Instant::now();
    let (held, other) = loop {
        let mut restarting = Vec::new();
        for id in &agents {
            if td.status(id).await == "restarting" {
                restarting.push(id.clone());
            }
        }
        if let [one] = restarting.as_slice() {
            let other = agents.iter().find(|a| *a != one).unwrap().clone();
            break (one.clone(), other);
        }
        assert!(start.elapsed() < WAIT, "no upgrade restart");
        tokio::time::sleep(Duration::from_millis(20)).await;
    };
    td.post(&format!("/api/agents/{held}/stop"), None).await;
    td.wait_status(&held, "stopped").await;
    std::fs::remove_file(&gate).unwrap();

    // The other agent gets the slot.
    td.wait_events(&other, "session_started", |e| e.len() == 2)
        .await;
    td.wait_status(&other, "idle").await;
    let reasons: Vec<String> = td
        .events(&other, "status")
        .await
        .iter()
        .filter_map(|s| s["data"]["reason"].as_str().map(str::to_owned))
        .collect();
    assert!(reasons.iter().any(|r| r == "upgrade"), "{reasons:?}");
    td.wait_runner_gone(&held).await;
    td.post(&format!("/api/agents/{other}/stop"), None).await;
    td.wait_status(&other, "stopped").await;
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
    let (_task, addr) = terrariumd::hosts::serve(state.clone(), "127.0.0.1:0")
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
    // Online means the link is connected, not that reservation reconciliation
    // has opened admission. Starting earlier can correctly refuse target up.
    while state.executor.resource_pending("lab1") {
        assert!(start.elapsed() < WAIT, "host admission never became ready");
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
        /// More fields of the key body (`key_id` and `cloud` of a cloud key).
        extra: Value,
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
            extra: Value::Null,
            want: &[("ANTHROPIC_API_KEY", "sk-ant-E2E-anthropic-1")],
        },
        Case {
            agent: "claude-zai",
            harness: "claude",
            sub: json!({"name": "glm", "provider": "zai", "plan": "coding",
                        "harnesses": ["claude", "pi"]}),
            key: Some(("zai", "zai-E2E-glm-key-2")),
            account_token: None,
            extra: Value::Null,
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
            extra: Value::Null,
            want: &[("ZAI_API_KEY", "zai-E2E-pi-key-3")],
        },
        Case {
            agent: "pi-kimi",
            harness: "pi",
            sub: json!({"name": "kimi", "provider": "moonshot", "plan": "coding",
                        "harnesses": ["pi"]}),
            key: Some(("moonshot", "sk-E2E-moonshot-4")),
            account_token: None,
            extra: Value::Null,
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
            extra: Value::Null,
            want: &[("OPENROUTER_API_KEY", "sk-or-E2E-openrouter-5")],
        },
        Case {
            agent: "codex-key",
            harness: "codex",
            sub: json!({"name": "oa", "provider": "openai", "harnesses": ["codex"]}),
            key: Some(("openai", "sk-E2E-openai-6")),
            account_token: None,
            extra: Value::Null,
            want: &[("OPENAI_API_KEY", "sk-E2E-openai-6")],
        },
        Case {
            agent: "copilot-pat",
            harness: "copilot",
            sub: json!({"name": "pat", "provider": "github", "harnesses": ["copilot"]}),
            key: Some(("github", "github_pat_E2E_copilot_7")),
            account_token: None,
            extra: Value::Null,
            want: &[("COPILOT_GITHUB_TOKEN", "github_pat_E2E_copilot_7")],
        },
        Case {
            agent: "copilot-plan",
            harness: "copilot",
            sub: json!({"name": "ghplan", "provider": "github", "harnesses": ["copilot"]}),
            key: None,
            account_token: Some("gho_E2E_copilot_plan_8"),
            extra: Value::Null,
            want: &[("COPILOT_GITHUB_TOKEN", "gho_E2E_copilot_plan_8")],
        },
        // The cloud kinds (TER-630), Claude Code only. A mocked call: the
        // fake harness shows the env that Claude Code would get.
        Case {
            agent: "claude-bedrock",
            harness: "claude",
            sub: json!({"name": "aws", "provider": "bedrock",
                        "cloud": {"region": "us-east-1",
                                  "models": {"opus": "us.anthropic.claude-opus-5-5"}}}),
            key: Some(("bedrock", "E2E-aws-secret-access-key-9")),
            account_token: None,
            extra: json!({"key_id": "AKIAE2EACCESSKEYID10"}),
            want: &[
                ("CLAUDE_CODE_USE_BEDROCK", "1"),
                ("AWS_REGION", "us-east-1"),
                ("AWS_ACCESS_KEY_ID", "AKIAE2EACCESSKEYID10"),
                ("AWS_SECRET_ACCESS_KEY", "E2E-aws-secret-access-key-9"),
                (
                    "ANTHROPIC_DEFAULT_OPUS_MODEL",
                    "us.anthropic.claude-opus-5-5",
                ),
            ],
        },
        Case {
            agent: "claude-bedrock-key",
            harness: "claude",
            sub: json!({"name": "aws-key", "provider": "bedrock"}),
            key: Some(("bedrock", "E2E-bedrock-api-key-11")),
            account_token: None,
            extra: Value::Null,
            want: &[
                ("CLAUDE_CODE_USE_BEDROCK", "1"),
                ("AWS_BEARER_TOKEN_BEDROCK", "E2E-bedrock-api-key-11"),
            ],
        },
        // The GCP JSON is a 0600 file; its var has the path (checked below).
        Case {
            agent: "claude-gcp",
            harness: "claude",
            sub: json!({"name": "vertex", "provider": "gcp",
                        "cloud": {"project_id": "e2e-proj", "region": "global"}}),
            key: Some((
                "gcp",
                r#"{"type":"service_account","private_key_id":"E2E-GCP-KEY-ID-13","private_key":"E2E-GCP-PRIVATE-KEY-14"}"#,
            )),
            account_token: None,
            extra: Value::Null,
            want: &[
                ("CLAUDE_CODE_USE_VERTEX", "1"),
                ("CLOUD_ML_REGION", "global"),
                ("ANTHROPIC_VERTEX_PROJECT_ID", "e2e-proj"),
            ],
        },
        Case {
            agent: "claude-foundry",
            harness: "claude",
            sub: json!({"name": "azure", "provider": "foundry",
                        "cloud": {"resource": "e2e-res"}}),
            key: Some(("foundry", "E2E-foundry-api-key-12")),
            account_token: None,
            extra: Value::Null,
            want: &[
                ("CLAUDE_CODE_USE_FOUNDRY", "1"),
                ("ANTHROPIC_FOUNDRY_RESOURCE", "e2e-res"),
                ("ANTHROPIC_FOUNDRY_API_KEY", "E2E-foundry-api-key-12"),
            ],
        },
    ];

    let td = Td::start_with_cli().await;
    let state = td.dir.path().join("state");
    let mut values: Vec<String> = Vec::new();
    for c in &cases {
        let name = c.sub["name"].as_str().unwrap();
        td.post("/api/subscriptions", Some(c.sub.clone())).await;
        if let Some((provider, key)) = c.key {
            let mut body = json!({"provider": provider, "key": key});
            if let Value::Object(extra) = &c.extra {
                for (k, v) in extra {
                    body[k] = v.clone();
                    if let Some(s) = v.as_str() {
                        values.push(s.to_owned());
                    }
                }
            }
            td.post(&format!("/api/subscriptions/{name}/api-key"), Some(body))
                .await;
            values.push(key.to_owned());
        }
        if let Some(token) = c.account_token {
            terrariumd::accounts::write_credential(
                &state,
                name,
                HarnessId::Copilot,
                token.as_bytes(),
            )
            .unwrap();
            values.push(token.to_owned());
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
        // GCP: the var is the path of a 0600 file in the private runtime
        // dir with the JSON, and the JSON is in no var.
        let mut gcp_file = None;
        if c.agent == "claude-gcp" {
            use std::os::unix::fs::PermissionsExt;
            assert!(
                !seen.contains("E2E-GCP-PRIVATE-KEY-14"),
                "the JSON is in a var"
            );
            assert!(!seen.contains("E2E-GCP-KEY-ID-13"), "the JSON is in a var");
            let path = seen
                .lines()
                .find_map(|l| l.strip_prefix("GOOGLE_APPLICATION_CREDENTIALS="))
                .expect("no GOOGLE_APPLICATION_CREDENTIALS");
            let path = std::path::PathBuf::from(path);
            assert!(
                path.starts_with(td.dir.path().join("run").join("credentials")),
                "{}",
                path.display()
            );
            assert_eq!(std::fs::read_to_string(&path).unwrap(), c.key.unwrap().1);
            let mode = |p: &Path| std::fs::metadata(p).unwrap().permissions().mode() & 0o777;
            assert_eq!(mode(&path), 0o600);
            assert_eq!(mode(path.parent().unwrap()), 0o700);
            gcp_file = Some(path);
            // Not in runner.json, nor in a state-dir file (checked below).
            values.push("E2E-GCP-PRIVATE-KEY-14".to_owned());
            values.push("E2E-GCP-KEY-ID-13".to_owned());
        }
        // The runner spec of this launch is there, with no value.
        let spec = std::fs::read_to_string(td.private_dir(&id).join("runner.json")).unwrap();
        assert!(spec.contains("exec-env"), "{}: {spec}", c.agent);
        assert!(spec.contains("--require-credentials"), "{}", c.agent);
        for v in &values {
            assert!(
                !spec.contains(v.as_str()),
                "{}: runner.json holds a key",
                c.agent
            );
        }
        td.post(&format!("/api/agents/{id}/stop"), None).await;
        td.wait_status(&id, "stopped").await;
        td.wait_runner_gone(&id).await;
        // Sandbox mode `none`: the file goes when the harness exits.
        if let Some(path) = gcp_file {
            let gone = || !path.exists();
            for _ in 0..100 {
                if gone() {
                    break;
                }
                tokio::time::sleep(std::time::Duration::from_millis(50)).await;
            }
            assert!(gone(), "{} is still there", path.display());
        }
    }
    // No file of the state dir holds a value, except the credential
    // stores (`credentials/`).
    let all = tree_text(&state, &[state.join("credentials")]);
    assert!(!all.is_empty());
    for v in &values {
        assert!(
            !all.contains(v.as_str()),
            "a file of the state dir holds {v}"
        );
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
