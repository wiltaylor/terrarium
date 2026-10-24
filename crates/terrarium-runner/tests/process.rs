//! Integration tests: the `terrarium-runner` process against the fake TUI,
//! driven through `RemoteRunner` over the control socket.

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use serde_json::{Value, json};
use terrarium_core::{AgentId, AgentStatus, Event, EventRecord, LaunchSpec};
use terrarium_runner::events::read_since;
use terrarium_runner::{
    AdapterKind, BoxFuture, ConnectOptions, RemoteRunner, RequestContext, RequestHandler, RpcError,
    RunnerHandle, RunnerSpec, SpawnOptions, live_runner_pid, read_exit_status,
};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::net::UnixStream;
use tokio::sync::broadcast;
use tokio::time::timeout;

const WAIT: Duration = Duration::from_secs(15);

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

struct Env {
    dir: tempfile::TempDir,
    agent: AgentId,
}

impl Env {
    fn new() -> Self {
        Self {
            dir: tempfile::tempdir().expect("tempdir"),
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
            "submit_timeout_ms": 5000,
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
        RemoteRunner::spawn_process(&bin(), opts)
            .await
            .expect("spawn the runner process")
    }
    async fn connect(&self, since: Option<u64>) -> RemoteRunner {
        RemoteRunner::connect_with(&self.run(), self.agent, self.options(since))
            .await
            .expect("connect")
    }
    /// Wait until the runner process is gone: no pid lock, no control socket.
    async fn until_gone(&self) {
        let control = terrarium_runner::control_socket_path(&self.run(), self.agent);
        timeout(WAIT, async {
            while live_runner_pid(&self.run(), self.agent).is_some() || control.exists() {
                tokio::time::sleep(Duration::from_millis(20)).await;
            }
        })
        .await
        .expect("the runner process did not exit");
    }
}

async fn until_status(
    events: &mut broadcast::Receiver<EventRecord>,
    to: AgentStatus,
) -> Vec<EventRecord> {
    let mut seen = Vec::new();
    timeout(WAIT, async {
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
    timeout(WAIT, reader.read_line(&mut text))
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
    timeout(WAIT, async {
        while !String::from_utf8_lossy(&seen).contains("done: hello") {
            seen.extend_from_slice(&output.recv().await.expect("output"));
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
    timeout(WAIT, async {
        while !std::fs::read_to_string(&events_path)
            .unwrap_or_default()
            .contains("s-offline")
        {
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    })
    .await
    .expect("the offline hook is in events.jsonl");
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
    let replayed = timeout(WAIT, events.recv())
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
    let status = timeout(WAIT, r.wait()).await.expect("exit");
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
    timeout(WAIT, async {
        while old.is_connected() {
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    })
    .await
    .expect("the old connection closes");
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
    let status = timeout(WAIT, r.wait()).await.expect("exit");
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
    let first = env.spawn_spec(env.spec_with_queue(10_000)).await;
    let hooks = first.socket_path().to_path_buf();
    drop(first);
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
    let rpc = timeout(WAIT, pending)
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
    let status = timeout(WAIT, r.wait()).await.expect("exit");
    let seen = until_status(&mut events, AgentStatus::Stopped).await;
    assert!(seen.iter().any(|e| matches!(e.event, Event::Exit { .. })));
    assert_ne!(
        status.signal.as_deref(),
        Some(terrarium_runner::LOST_SIGNAL)
    );
    env.until_gone().await;
}
