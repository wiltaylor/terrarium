//! Integration tests: the `terrarium-runner` process against the fake TUI,
//! driven through `RemoteRunner` over the control socket. Every wait is
//! for a condition, with the limits of `common`.

mod common;

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use common::{ms, until, wait};
use serde_json::{Value, json};
use terrarium_core::{AgentId, AgentStatus, Event, EventRecord, LaunchSpec};
use terrarium_runner::events::read_since;
use terrarium_runner::test_support::{RunnerProcessGuard, sweep_stale_runners_once};
use terrarium_runner::{
    AdapterKind, AttachEvent, BoxFuture, ConnectOptions, RemoteRunner, RequestContext,
    RequestHandler, RpcError, RunnerHandle, RunnerSpec, SpawnOptions, live_runner_pid,
    read_exit_status,
};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::net::UnixStream;
use tokio::sync::broadcast;
use tokio::time::timeout;

fn bin() -> PathBuf {
    PathBuf::from(env!("CARGO_BIN_EXE_terrarium-runner"))
}

fn fake_tui() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fake_tui/fake_tui.py")
}

/// Answers `echo` with its params, as the daemon answers `http`.
struct Echo;

impl RequestHandler for Echo {
    fn handle<'a>(
        &'a self,
        ctx: &'a RequestContext,
        method: &'a str,
        params: Value,
    ) -> BoxFuture<'a, Result<Value, RpcError>> {
        Box::pin(async move {
            match method {
                "echo" => Ok(json!({"agent": ctx.agent, "params": params})),
                _ => Err(RpcError::method_not_found(method)),
            }
        })
    }
}

/// One agent in a temp dir. `guard` ends the runner processes of the dir
/// when the test ends, also on a panic. It drops before `dir`.
struct Env {
    guard: RunnerProcessGuard,
    dir: tempfile::TempDir,
    agent: AgentId,
}

impl Env {
    fn new() -> Self {
        sweep_stale_runners_once(&bin());
        let dir = tempfile::tempdir().expect("tempdir");
        Self {
            guard: RunnerProcessGuard::new(dir.path().join("run")),
            dir,
            agent: AgentId::new(),
        }
    }
    fn state(&self) -> PathBuf {
        self.dir.path().join("state")
    }
    fn run(&self) -> PathBuf {
        self.dir.path().join("run")
    }
    fn spec(&self) -> RunnerSpec {
        self.spec_with_queue(300)
    }
    fn spec_with_queue(&self, queue_ms: u64) -> RunnerSpec {
        let launch = LaunchSpec::new("python3", self.dir.path())
            .arg(fake_tui().display().to_string())
            .env("FAKE_TUI_DELAY_MS", "200");
        let spec: RunnerSpec = serde_json::from_value(json!({
            "harness": "claude",
            "adapter": "fake",
            "launch": launch,
            "name": "tester",
            "project": "demo",
            "rows": 24,
            "cols": 80,
            "tick_ms": 20,
            "submit_timeout_ms": ms(10_000).as_millis() as u64,
            "ready_detector": {"quiet_ms": 100},
            "rpc_queue_timeout_ms": queue_ms,
        }))
        .expect("spec");
        assert_eq!(spec.adapter, AdapterKind::Fake);
        spec
    }
    fn options(&self, since: Option<u64>) -> ConnectOptions {
        ConnectOptions {
            handler: Arc::new(Echo),
            events_since: since,
            ..ConnectOptions::default()
        }
    }
    async fn spawn(&self) -> RemoteRunner {
        self.spawn_spec(self.spec()).await
    }
    async fn spawn_spec(&self, spec: RunnerSpec) -> RemoteRunner {
        let mut opts = SpawnOptions::new(self.agent, self.state(), self.run(), spec);
        opts.connect = self.options(Some(0));
        opts.start_timeout = wait();
        RemoteRunner::spawn_process(&bin(), opts)
            .await
            .expect("spawn the runner process")
    }
    async fn connect(&self, since: Option<u64>) -> RemoteRunner {
        self.connect_opts(self.options(since)).await
    }
    async fn connect_opts(&self, opts: ConnectOptions) -> RemoteRunner {
        RemoteRunner::connect_with(&self.run(), self.agent, opts)
            .await
            .expect("connect")
    }
    /// Start the binary as a child of the test, so that the test sees its
    /// exit code, and connect to it.
    async fn spawn_direct(
        &self,
        spec: RunnerSpec,
        opts: ConnectOptions,
    ) -> (tokio::process::Child, RemoteRunner) {
        let dir = self.state().join("agents").join(self.agent.to_string());
        std::fs::create_dir_all(&dir).expect("agent dir");
        let spec_path = dir.join("runner.json");
        spec.write(&spec_path).expect("spec");
        let child = tokio::process::Command::new(bin())
            .arg("--agent")
            .arg(self.agent.to_string())
            .arg("--state-dir")
            .arg(self.state())
            .arg("--runtime-dir")
            .arg(self.run())
            .arg("--spec")
            .arg(&spec_path)
            .arg("--linger-ms")
            .arg("200")
            .stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .kill_on_drop(true)
            .spawn()
            .expect("start the runner");
        if let Some(pid) = child.id() {
            self.guard.track(pid);
        }
        let control = terrarium_runner::control_socket_path(&self.run(), self.agent);
        until("the control socket", || control.exists()).await;
        let r = self.connect_opts(opts).await;
        (child, r)
    }
    /// Wait until `runner.log` holds `text`.
    async fn until_log(&self, text: &str) {
        let log = self
            .state()
            .join("agents")
            .join(self.agent.to_string())
            .join("runner.log");
        until(&format!("`{text}` in runner.log"), || {
            std::fs::read_to_string(&log)
                .unwrap_or_default()
                .contains(text)
        })
        .await;
    }
    /// Wait until the runner process is gone: no pid lock, no control socket.
    async fn until_gone(&self) {
        let control = terrarium_runner::control_socket_path(&self.run(), self.agent);
        until("the runner process exits", || {
            live_runner_pid(&self.run(), self.agent).is_none() && !control.exists()
        })
        .await;
    }
}

async fn until_status(
    events: &mut broadcast::Receiver<EventRecord>,
    to: AgentStatus,
) -> Vec<EventRecord> {
    let mut seen = Vec::new();
    timeout(wait(), async {
        loop {
            let rec = events.recv().await.expect("event");
            let done = matches!(rec.event, Event::StatusChanged { to: t, .. } if t == to);
            seen.push(rec);
            if done {
                return;
            }
        }
    })
    .await
    .unwrap_or_else(|_| panic!("no change to {to}; saw {seen:?}"));
    seen
}

/// One line to the hook socket; read one answer line when `answer`.
async fn hook_socket(path: &Path, msg: Value, answer: bool) -> Option<Value> {
    let mut s = UnixStream::connect(path).await.expect("hook socket");
    let mut line = serde_json::to_vec(&msg).expect("json");
    line.push(b'\n');
    s.write_all(&line).await.expect("write");
    if !answer {
        return None;
    }
    let mut reader = BufReader::new(s);
    let mut text = String::new();
    timeout(wait(), reader.read_line(&mut text))
        .await
        .expect("answer in time")
        .expect("read");
    Some(serde_json::from_str(&text).expect("answer json"))
}

fn seqs(path: &Path) -> Vec<u64> {
    read_since(path, 0)
        .expect("read events")
        .iter()
        .map(|r| r.seq.expect("seq"))
        .collect()
}

#[tokio::test]
async fn the_runner_process_survives_a_daemon_disconnect() {
    let env = Env::new();
    let first = env.spawn().await;
    let r: Arc<dyn RunnerHandle> = Arc::new(first.clone());
    assert!(first.hello().runner_pid > 0);
    assert!(first.hello().harness_pid.is_some());
    // `events_since: 0` replays the start, from before the connect.
    let mut events = r.subscribe_events();
    let start = until_status(&mut events, AgentStatus::Idle).await;
    assert!(matches!(
        start[0].event,
        Event::StatusChanged {
            to: AgentStatus::Starting,
            ..
        }
    ));
    assert_eq!(start[0].seq, Some(1));

    // Attach, then type a turn: starting -> idle -> working -> idle.
    let (snapshot, mut output) = r.attach().await.expect("attach");
    assert!(snapshot.text().contains("fake tui 1.0"));
    r.type_turn("hello".into()).wait().await.expect("delivered");
    until_status(&mut events, AgentStatus::Working).await;
    until_status(&mut events, AgentStatus::Idle).await;
    let mut seen = Vec::new();
    timeout(wait(), async {
        while !String::from_utf8_lossy(&seen).contains("done: hello") {
            match output.recv().await.expect("output") {
                AttachEvent::Output(chunk) => seen.extend_from_slice(&chunk),
                AttachEvent::Resynced(snap) => seen = snap.text().into_bytes(),
            }
        }
    })
    .await
    .expect("attach output shows the turn");
    assert!(
        r.screen()
            .await
            .expect("screen")
            .text()
            .contains("done: hello")
    );

    // The daemon goes away.
    let last = first.last_seq();
    let hooks = r.socket_path().to_path_buf();
    let events_path = r.events_path().to_path_buf();
    drop((r, events, output, first));

    // Hooks still reach the runner while no daemon is connected.
    hook_socket(
        &hooks,
        json!({"type":"hook","hook":"SessionStart","payload":{"session_id":"s-offline","source":"startup"}}),
        false,
    )
    .await;
    // The runner answers `whoami` itself.
    let who = hook_socket(
        &hooks,
        json!({"type":"request","id":1,"method":"whoami"}),
        true,
    )
    .await
    .expect("whoami");
    assert_eq!(who["result"]["name"], "tester");
    // A request for the daemon gets the tunnel error.
    let rpc = hook_socket(
        &hooks,
        json!({"type":"request","id":2,"method":"echo","params":{"x":1}}),
        true,
    )
    .await
    .expect("echo");
    assert_eq!(rpc["error"]["code"], "unavailable", "{rpc}");

    // A new daemon connects and replays from its last seq.
    until("the offline hook is in events.jsonl", || {
        std::fs::read_to_string(&events_path)
            .unwrap_or_default()
            .contains("s-offline")
    })
    .await;
    let second = env.connect(Some(last)).await;
    let hello = second.hello().clone();
    assert_eq!(hello.status, AgentStatus::Idle);
    assert_eq!(
        hello.session.as_ref().map(|s| s.0.as_str()),
        Some("s-offline")
    );
    assert!(hello.seq > last);
    let r: Arc<dyn RunnerHandle> = Arc::new(second.clone());
    let mut events = r.subscribe_events();
    let replayed = timeout(wait(), events.recv())
        .await
        .expect("replay")
        .expect("event");
    assert_eq!(replayed.seq, Some(last + 1), "no gap after the reconnect");
    assert!(matches!(replayed.event, Event::SessionStarted { .. }));

    // A turn through the second connection.
    r.type_turn("again".into()).wait().await.expect("delivered");
    until_status(&mut events, AgentStatus::Idle).await;

    // With a daemon connected, agent RPC goes through it.
    let rpc = hook_socket(
        &hooks,
        json!({"type":"request","id":3,"method":"echo","params":{"x":2}}),
        true,
    )
    .await
    .expect("echo");
    assert_eq!(rpc["result"]["params"]["x"], 2, "{rpc}");
    assert_eq!(rpc["result"]["agent"], env.agent.to_string());

    // The seqs in events.jsonl have no gap.
    let all = seqs(&events_path);
    assert_eq!(all, (1..=all.len() as u64).collect::<Vec<_>>());

    // Kill: the harness stops, and the runner process exits.
    r.kill().await.expect("kill");
    let status = timeout(wait(), r.wait()).await.expect("exit");
    assert_ne!(
        status.signal.as_deref(),
        Some(terrarium_runner::LOST_SIGNAL)
    );
    until_status(&mut events, AgentStatus::Stopped).await;
    env.until_gone().await;
    assert_eq!(read_exit_status(&env.state(), env.agent), Some(status));
    assert!(!hooks.exists(), "the hook socket is removed");
}

#[tokio::test]
async fn a_second_runner_for_the_agent_refuses_to_start() {
    let env = Env::new();
    let r = env.spawn().await;
    let spec = env
        .state()
        .join("agents")
        .join(env.agent.to_string())
        .join("runner.json");
    let out = tokio::process::Command::new(bin())
        .arg("--agent")
        .arg(env.agent.to_string())
        .arg("--state-dir")
        .arg(env.state())
        .arg("--runtime-dir")
        .arg(env.run())
        .arg("--spec")
        .arg(&spec)
        .output()
        .await
        .expect("run");
    assert_eq!(
        out.status.code(),
        Some(3),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    assert!(String::from_utf8_lossy(&out.stderr).contains("already holds"));
    // spawn_process refuses too, before it starts anything.
    let again = RemoteRunner::spawn_process(
        &bin(),
        SpawnOptions::new(env.agent, env.state(), env.run(), env.spec()),
    )
    .await;
    assert!(again.is_err());
    // The first runner still works.
    assert!(r.is_connected());
    r.kill().await.expect("kill");
    r.wait().await;
    env.until_gone().await;
}

#[tokio::test]
async fn a_new_daemon_connection_replaces_the_old_one() {
    let env = Env::new();
    let old = env.spawn().await;
    let new = env.connect(None).await;
    until("the old connection closes", || !old.is_connected()).await;
    // A call on the old handle fails; the new one works.
    assert!(old.screen().await.is_err());
    assert!(new.screen().await.is_ok());
    new.kill().await.expect("kill");
    new.wait().await;
    env.until_gone().await;
}

#[tokio::test]
async fn the_runner_process_exits_with_the_harness() {
    let env = Env::new();
    let r = env.spawn().await;
    let mut events = r.subscribe_events();
    until_status(&mut events, AgentStatus::Idle).await;
    // `/quit` is not a turn: no hook comes, so the delivery cannot confirm.
    let _ = r.type_turn("/quit".into());
    let status = timeout(wait(), r.wait()).await.expect("exit");
    assert!(status.success(), "{status:?}");
    let seen = until_status(&mut events, AgentStatus::Stopped).await;
    assert!(
        seen.iter()
            .any(|e| matches!(e.event, Event::Exit { code: Some(0) }))
    );
    env.until_gone().await;
    assert_eq!(read_exit_status(&env.state(), env.agent), Some(status));
    // No runner, no connect.
    assert!(RemoteRunner::connect(&env.run(), env.agent).await.is_err());
}

#[tokio::test]
async fn an_agent_request_waits_for_a_daemon_to_connect() {
    let env = Env::new();
    let first = env
        .spawn_spec(env.spec_with_queue(ms(10_000).as_millis() as u64))
        .await;
    let hooks = first.socket_path().to_path_buf();
    drop(first);
    // A request that comes while the runner still holds the old connection
    // goes to it, and gets `unavailable` when it closes. Wait until the
    // runner has seen the disconnect.
    env.until_log("the daemon disconnected").await;
    let pending = tokio::spawn(async move {
        hook_socket(
            &hooks,
            json!({"type":"request","id":9,"method":"echo","params":{"queued":true}}),
            true,
        )
        .await
    });
    tokio::time::sleep(Duration::from_millis(300)).await;
    assert!(!pending.is_finished(), "the request waits in the queue");
    let r = env.connect(None).await;
    let rpc = timeout(wait(), pending)
        .await
        .expect("answer")
        .expect("task")
        .expect("response");
    assert_eq!(rpc["result"]["params"]["queued"], true, "{rpc}");
    r.kill().await.expect("kill");
    r.wait().await;
    env.until_gone().await;
}

#[tokio::test]
async fn sigterm_stops_the_harness_and_the_runner() {
    use nix::sys::signal::{Signal, kill};
    use nix::unistd::Pid;
    let env = Env::new();
    let r = env.spawn().await;
    let mut events = r.subscribe_events();
    until_status(&mut events, AgentStatus::Idle).await;
    let pid = i32::try_from(r.hello().runner_pid).expect("pid");
    kill(Pid::from_raw(pid), Signal::SIGTERM).expect("SIGTERM");
    let status = timeout(wait(), r.wait()).await.expect("exit");
    let seen = until_status(&mut events, AgentStatus::Stopped).await;
    assert!(seen.iter().any(|e| matches!(e.event, Event::Exit { .. })));
    assert_ne!(
        status.signal.as_deref(),
        Some(terrarium_runner::LOST_SIGNAL)
    );
    env.until_gone().await;
}

/// The runner ends the attach stream with `lagged` (a 1-chunk output
/// channel in the runner), or the local reader falls behind (a 4-item
/// channel here, and a reader that does not read during a flood). Either
/// way `RemoteRunner::attach` attaches again by itself and gives
/// `Resynced`.
#[tokio::test]
async fn a_remote_attach_resyncs_after_a_lag() {
    let env = Env::new();
    let mut spec = env.spec();
    spec.output_capacity = Some(1);
    let opts = ConnectOptions {
        attach_capacity: 4,
        ..env.options(Some(0))
    };
    let (mut child, r) = env.spawn_direct(spec, opts).await;
    let mut events = r.subscribe_events();
    until_status(&mut events, AgentStatus::Idle).await;
    let (_, mut stream) = r.attach().await.expect("attach");
    r.type_turn("/flood 5000".into())
        .wait()
        .await
        .expect("delivered");
    let seen = until_status(&mut events, AgentStatus::Idle).await;
    assert!(seen.iter().any(|e| matches!(e.event, Event::TurnEnd)));

    let mut resyncs = 0;
    let mut text = Vec::new();
    timeout(wait(), async {
        while resyncs == 0 || !String::from_utf8_lossy(&text).contains("done: /flood 5000") {
            match stream.recv().await.expect("the stream goes on") {
                AttachEvent::Output(chunk) => text.extend_from_slice(&chunk),
                AttachEvent::Resynced(snap) => {
                    resyncs += 1;
                    text = snap.text().into_bytes();
                }
            }
        }
    })
    .await
    .expect("a resync, then the end of the flood");

    // The stream goes on after the resync.
    r.type_turn("after".into()).wait().await.expect("delivered");
    timeout(wait(), async {
        while !String::from_utf8_lossy(&text).contains("done: after") {
            match stream.recv().await.expect("the stream goes on") {
                AttachEvent::Output(chunk) => text.extend_from_slice(&chunk),
                AttachEvent::Resynced(snap) => text = snap.text().into_bytes(),
            }
        }
    })
    .await
    .expect("output after the resync");

    r.kill().await.expect("kill");
    timeout(wait(), async { while stream.recv().await.is_some() {} })
        .await
        .expect("the stream ends after the exit");
    timeout(wait(), child.wait())
        .await
        .expect("exit")
        .expect("wait");
}

/// One way for the harness to end, and what the runner must report.
struct Ending {
    env: Vec<(&'static str, &'static str)>,
    /// A turn to type first, and a screen text that shows it runs.
    turn: Option<(&'static str, &'static str)>,
    how: How,
    code: i32,
    signal: Option<i32>,
    status: AgentStatus,
}

enum How {
    /// The harness exits by itself after the turn.
    Itself,
    /// The `kill` call.
    Kill,
    /// SIGTERM to the runner process.
    Sigterm,
}

async fn check_ending(e: Ending) {
    use nix::sys::signal::{Signal, kill};
    use nix::unistd::Pid;
    let env = Env::new();
    let mut spec = env.spec();
    for (k, v) in &e.env {
        spec.launch.env.insert((*k).into(), (*v).into());
    }
    spec.kill_grace_ms = ms(300).as_millis() as u64;
    let (mut child, r) = env.spawn_direct(spec, env.options(Some(0))).await;
    let mut events = r.subscribe_events();
    until_status(&mut events, AgentStatus::Idle).await;
    if let Some((turn, shows)) = e.turn {
        let _ = r.type_turn(turn.into());
        // A harness that exits by itself can be gone before the screen
        // shows the text.
        if !matches!(e.how, How::Itself) {
            timeout(wait(), async {
                while !r.screen().await.expect("screen").text().contains(shows) {
                    tokio::time::sleep(Duration::from_millis(20)).await;
                }
            })
            .await
            .unwrap_or_else(|_| panic!("no `{shows}` on the screen"));
        }
    }
    match e.how {
        How::Itself => {}
        How::Kill => r.kill().await.expect("kill"),
        How::Sigterm => {
            let pid = i32::try_from(r.hello().runner_pid).expect("pid");
            kill(Pid::from_raw(pid), Signal::SIGTERM).expect("SIGTERM");
        }
    }
    let status = timeout(wait(), r.wait()).await.expect("exit");
    assert_eq!(status.exit_code(), e.code, "{status:?}");
    assert_eq!(status.signal_number(), e.signal, "{status:?}");
    until_status(&mut events, e.status).await;
    let code = timeout(wait(), child.wait())
        .await
        .expect("the runner exits")
        .expect("wait");
    assert_eq!(
        code.code(),
        Some(e.code),
        "the runner exit code reflects the harness"
    );
    assert_eq!(read_exit_status(&env.state(), env.agent), Some(status));
    env.until_gone().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_normal_exit_gives_code_0() {
    check_ending(Ending {
        env: vec![],
        turn: Some(("/quit", "bye")),
        how: How::Itself,
        code: 0,
        signal: None,
        status: AgentStatus::Stopped,
    })
    .await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_harness_crash_gives_its_code() {
    check_ending(Ending {
        env: vec![],
        turn: Some(("/exit 7", "bye")),
        how: How::Itself,
        code: 7,
        signal: None,
        status: AgentStatus::Error,
    })
    .await;
    check_ending(Ending {
        env: vec![],
        turn: Some(("/abort", "/abort")),
        how: How::Itself,
        code: 134,
        signal: Some(6),
        status: AgentStatus::Error,
    })
    .await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn kill_gives_sighup_then_sigkill() {
    check_ending(Ending {
        env: vec![],
        turn: None,
        how: How::Kill,
        code: 129,
        signal: Some(1),
        status: AgentStatus::Stopped,
    })
    .await;
    check_ending(Ending {
        env: vec![("FAKE_TUI_IGNORE_HUP", "1")],
        turn: None,
        how: How::Kill,
        code: 137,
        signal: Some(9),
        status: AgentStatus::Stopped,
    })
    .await;
}

/// SIGTERM to an idle harness: the exit keys, a clean exit. To a busy
/// harness: SIGHUP, and SIGKILL after the grace when it ignores SIGHUP.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn sigterm_stops_idle_with_exit_keys_and_busy_with_a_kill() {
    check_ending(Ending {
        env: vec![],
        turn: None,
        how: How::Sigterm,
        code: 0,
        signal: None,
        status: AgentStatus::Stopped,
    })
    .await;
    check_ending(Ending {
        env: vec![],
        turn: Some(("/hold", "holding...")),
        how: How::Sigterm,
        code: 129,
        signal: Some(1),
        status: AgentStatus::Stopped,
    })
    .await;
    check_ending(Ending {
        env: vec![("FAKE_TUI_IGNORE_HUP", "1")],
        turn: Some(("/hold", "holding...")),
        how: How::Sigterm,
        code: 137,
        signal: Some(9),
        status: AgentStatus::Stopped,
    })
    .await;
}
