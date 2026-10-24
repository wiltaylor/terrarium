//! Integration tests: the runner against the fake TUI in `tests/fake_tui/`.

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use semver::{Version, VersionReq};
use serde_json::{Value, json};
use terrarium_core::{
    Adapter, Agent, AgentId, AgentStatus, Answer, Event, EventRecord, HarnessId, HarnessSessionId,
    HookPayload, LaunchSpec, ScreenHint,
};
use terrarium_runner::{
    BoxFuture, DefaultReadyDetector, DeliveryError, NoHandler, RequestContext, RequestHandler,
    RpcError, Runner, RunnerConfig,
};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::net::UnixStream;
use tokio::sync::broadcast;
use tokio::time::timeout;

/// An adapter with the hook names of Claude Code. With `busy_screen` set,
/// `screen_hint` says `Busy` while the fake TUI shows `working...`, and it
/// cannot tell otherwise.
struct FakeAdapter {
    busy_screen: bool,
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
    fn launch(&self, agent: &Agent, _resume: Option<&HarnessSessionId>) -> LaunchSpec {
        LaunchSpec::new("python3", &agent.workspace)
    }
    fn hook_event(&self, hook: &HookPayload) -> Vec<Event> {
        match hook.hook.as_str() {
            "UserPromptSubmit" => vec![Event::TurnStart {
                prompt: hook.str_field("prompt").map(str::to_owned),
            }],
            "Stop" => vec![Event::TurnEnd],
            "Notification" => vec![Event::Notification {
                message: hook.str_field("message").unwrap_or_default().to_owned(),
                question: true,
            }],
            _ => vec![],
        }
    }
    fn screen_hint(&self, screen: &terrarium_core::adapter::Screen) -> ScreenHint {
        let contents = screen.contents();
        let last = contents.lines().rev().find(|l| !l.trim().is_empty());
        if self.busy_screen && last.is_some_and(|l| l.trim() == "working...") {
            ScreenHint::Busy
        } else {
            ScreenHint::Unknown
        }
    }
    /// Visible keys, so a test can see that the retry path sends them.
    fn clear_input_keys(&self) -> Vec<u8> {
        b"cleared\r".to_vec()
    }
    fn answer(&self, _request_id: &str, answer: &Answer) -> Vec<u8> {
        match answer {
            Answer::Allow => b"y\r".to_vec(),
            Answer::Deny => b"n\r".to_vec(),
            Answer::Choice(_) => Vec::new(),
        }
    }
}

fn fake_tui() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fake_tui/fake_tui.py")
}

struct Harness {
    runner: Runner,
    events: broadcast::Receiver<EventRecord>,
    _dir: tempfile::TempDir,
}

struct Opts {
    env: Vec<(&'static str, &'static str)>,
    stall: Duration,
    submit: Duration,
    busy_screen: bool,
}

impl Default for Opts {
    fn default() -> Self {
        Self {
            env: vec![],
            stall: Duration::from_secs(30),
            submit: Duration::from_secs(5),
            busy_screen: false,
        }
    }
}

async fn start(opts: Opts) -> Harness {
    start_with(opts, Arc::new(NoHandler)).await
}

async fn start_with(opts: Opts, handler: Arc<dyn RequestHandler>) -> Harness {
    let dir = tempfile::tempdir().expect("tempdir");
    let mut config = RunnerConfig::new(
        AgentId::new(),
        dir.path().join("state"),
        dir.path().join("run"),
    );
    config.name = "tester".into();
    config.project = "demo".into();
    config.rows = 24;
    config.cols = 80;
    config.stall_timeout = opts.stall;
    config.submit_timeout = opts.submit;
    config.tick = Duration::from_millis(20);
    config.ready_detector = Some(Arc::new(DefaultReadyDetector {
        quiet: Duration::from_millis(100),
        pattern: None,
    }));
    let mut spec = LaunchSpec::new("python3", dir.path()).arg(fake_tui().display().to_string());
    for (k, v) in opts.env {
        spec = spec.env(k, v);
    }
    let adapter = Arc::new(FakeAdapter {
        busy_screen: opts.busy_screen,
    });
    let runner = Runner::spawn(config, adapter, spec, handler)
        .await
        .expect("spawn");
    // The Starting event goes out during spawn, before this subscribe. Every
    // later event reaches the receiver.
    let events = runner.subscribe_events();
    Harness {
        runner,
        events,
        _dir: dir,
    }
}

const WAIT: Duration = Duration::from_secs(10);

/// Wait for a status-change event to `to`. Returns every event seen on the way.
async fn until_status(
    events: &mut broadcast::Receiver<EventRecord>,
    to: AgentStatus,
) -> Vec<Event> {
    let mut seen = Vec::new();
    timeout(WAIT, async {
        loop {
            let rec = events.recv().await.expect("event");
            let done = matches!(rec.event, Event::StatusChanged { to: t, .. } if t == to);
            seen.push(rec.event);
            if done {
                return;
            }
        }
    })
    .await
    .unwrap_or_else(|_| panic!("no change to {to}; saw {seen:?}"));
    seen
}

/// Wait until the screen shows `text`. A hook can reach the runner before
/// the PTY bytes that the harness wrote just before it.
async fn until_screen(h: &Harness, text: &str) {
    timeout(WAIT, async {
        while !h.runner.screen().text().contains(text) {
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    })
    .await
    .unwrap_or_else(|_| panic!("no `{text}` on the screen: {}", h.runner.screen().text()));
}

async fn until_idle(h: &mut Harness) {
    if h.runner.status() != AgentStatus::Idle {
        until_status(&mut h.events, AgentStatus::Idle).await;
    }
}

fn read_events(path: &Path) -> Vec<EventRecord> {
    std::fs::read_to_string(path)
        .expect("events.jsonl")
        .lines()
        .map(|l| serde_json::from_str(l).expect("event line"))
        .collect()
}

fn transitions(events: &[EventRecord]) -> Vec<AgentStatus> {
    events
        .iter()
        .filter_map(|r| match r.event {
            Event::StatusChanged { to, .. } => Some(to),
            _ => None,
        })
        .collect()
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn turn_runs_starting_idle_working_idle() {
    let mut h = start(Opts::default()).await;
    until_idle(&mut h).await;
    assert!(h.runner.screen().text().contains("fake tui 1.0"));

    let delivery = h.runner.type_turn("hello");
    timeout(WAIT, delivery.wait())
        .await
        .expect("delivery in time")
        .expect("delivered");
    let seen = until_status(&mut h.events, AgentStatus::Idle).await;
    assert!(seen.contains(&Event::TurnStart {
        prompt: Some("hello".into())
    }));
    assert!(seen.contains(&Event::TurnEnd));
    until_screen(&h, "done: hello").await;

    h.runner.kill().expect("kill");
    h.runner.wait().await;
    let all = read_events(h.runner.events_path());
    assert_eq!(
        transitions(&all),
        [
            AgentStatus::Starting,
            AgentStatus::Idle,
            AgentStatus::Working,
            AgentStatus::Idle,
            AgentStatus::Stopped
        ]
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn second_turn_waits_for_stop() {
    let mut h = start(Opts {
        env: vec![("FAKE_TUI_DELAY_MS", "600")],
        ..Opts::default()
    })
    .await;
    until_idle(&mut h).await;

    let first = h.runner.type_turn("first");
    until_status(&mut h.events, AgentStatus::Working).await;
    let second = h.runner.type_turn("second");
    timeout(WAIT, first.wait())
        .await
        .expect("time")
        .expect("first");

    // The second turn goes out only after the first turn ends.
    let seen = until_status(&mut h.events, AgentStatus::Idle).await;
    assert!(!seen.iter().any(|e| matches!(e,
        Event::TurnStart { prompt: Some(p) } if p == "second")));
    timeout(WAIT, second.wait())
        .await
        .expect("time")
        .expect("second");
    let seen = until_status(&mut h.events, AgentStatus::Idle).await;
    assert!(seen.contains(&Event::TurnEnd));
    until_screen(&h, "done: second").await;
    h.runner.kill().expect("kill");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn stall_blocks_then_stop_unblocks() {
    let mut h = start(Opts {
        env: vec![("FAKE_TUI_STALL_MS", "1500")],
        stall: Duration::from_millis(400),
        ..Opts::default()
    })
    .await;
    until_idle(&mut h).await;
    let d = h.runner.type_turn("/stall");
    timeout(WAIT, d.wait())
        .await
        .expect("time")
        .expect("delivered");
    let seen = until_status(&mut h.events, AgentStatus::Blocked).await;
    assert!(seen.iter().any(|e| matches!(e,
        Event::StatusChanged { to: AgentStatus::Blocked, reason: Some(r), .. } if r == "stalled")));
    // The fake TUI prints and sends Stop after the silence.
    until_status(&mut h.events, AgentStatus::Idle).await;
    h.runner.kill().expect("kill");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn question_blocks_and_answer_resumes() {
    let mut h = start(Opts::default()).await;
    until_idle(&mut h).await;
    h.runner.type_turn("/question");
    let seen = until_status(&mut h.events, AgentStatus::Blocked).await;
    assert!(
        seen.iter()
            .any(|e| matches!(e, Event::Notification { question: true, .. }))
    );
    assert!(h.runner.answer("q1", &Answer::Choice(0)).await.is_err());
    h.runner.answer("q1", &Answer::Allow).await.expect("answer");
    assert_eq!(h.runner.status(), AgentStatus::Working);
    until_status(&mut h.events, AgentStatus::Idle).await;
    until_screen(&h, "answer: y").await;
    h.runner.kill().expect("kill");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn delivery_fails_after_one_retry() {
    let mut h = start(Opts {
        env: vec![("FAKE_TUI_NO_SUBMIT", "1")],
        submit: Duration::from_millis(500),
        ..Opts::default()
    })
    .await;
    until_idle(&mut h).await;
    let d = h.runner.type_turn("lost");
    let err = timeout(WAIT, d.wait())
        .await
        .expect("time")
        .expect_err("fails");
    assert_eq!(err, DeliveryError::NoSubmitHook { attempts: 2 });
    // The fake TUI saw the turn twice, and the clear keys before the retry.
    let text = h.runner.screen().text();
    assert_eq!(text.matches("lost").count(), 2, "{text}");
    let cleared = text.find("cleared").expect("clear keys before the retry");
    assert!(
        text.find("lost").is_some_and(|first| first < cleared),
        "{text}"
    );
    assert!(
        text.rfind("lost").is_some_and(|last| last > cleared),
        "{text}"
    );
    h.runner.kill().expect("kill");
    h.runner.wait().await;
    let all = read_events(h.runner.events_path());
    assert!(all.iter().any(|r| matches!(&r.event,
        Event::DeliveryFailed { text, .. } if text == "lost")));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn busy_screen_confirms_a_turn_without_the_submit_hook() {
    let mut h = start(Opts {
        env: vec![
            ("FAKE_TUI_NO_SUBMIT_HOOK", "1"),
            ("FAKE_TUI_DELAY_MS", "1000"),
        ],
        submit: Duration::from_millis(500),
        busy_screen: true,
        ..Opts::default()
    })
    .await;
    until_idle(&mut h).await;
    let d = h.runner.type_turn("steady");
    timeout(WAIT, d.wait())
        .await
        .expect("time")
        .expect("the busy screen confirms the turn");
    let seen = until_status(&mut h.events, AgentStatus::Idle).await;
    assert!(seen.contains(&Event::TurnEnd));
    assert!(!seen.iter().any(|e| matches!(e, Event::TurnStart { .. })));
    // Typed once: no clear keys and no retry, though the turn ran for
    // longer than `submit_timeout`.
    until_screen(&h, "done: steady").await;
    tokio::time::sleep(Duration::from_millis(300)).await;
    let text = h.runner.screen().text();
    assert_eq!(text.matches("done: steady").count(), 1, "{text}");
    assert!(!text.contains("cleared"), "{text}");
    h.runner.kill().expect("kill");
    h.runner.wait().await;
    let all = read_events(h.runner.events_path());
    assert!(
        !all.iter()
            .any(|r| matches!(r.event, Event::DeliveryFailed { .. }))
    );
    assert!(all.iter().any(|r| matches!(&r.event,
        Event::StatusChanged { to: AgentStatus::Working, reason: Some(r), .. }
            if r.contains("screen busy"))));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn attach_sees_output() {
    let mut h = start(Opts::default()).await;
    until_idle(&mut h).await;
    let (snap, mut rx) = h.runner.attach();
    assert_eq!(
        snap.lines
            .iter()
            .rfind(|l| !l.is_empty())
            .map(String::as_str),
        Some(">")
    );
    assert!(!snap.formatted.is_empty());
    h.runner.type_turn("ping-attach");
    let mut got = Vec::new();
    timeout(WAIT, async {
        loop {
            let chunk = rx.recv().await.expect("chunk");
            got.extend_from_slice(&chunk);
            if String::from_utf8_lossy(&got).contains("done: ping-attach") {
                return;
            }
        }
    })
    .await
    .expect("output in time");
    h.runner.kill().expect("kill");
    let pty_log = std::fs::read(h.runner.events_path().with_file_name("pty.log")).expect("pty.log");
    assert!(String::from_utf8_lossy(&pty_log).contains("fake tui 1.0"));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn kill_stops_and_exit_code_picks_stopped_or_error() {
    let mut h = start(Opts::default()).await;
    until_idle(&mut h).await;
    h.runner.kill().expect("kill");
    let status = timeout(WAIT, h.runner.wait()).await.expect("exit");
    assert!(!status.success());
    assert_eq!(h.runner.status(), AgentStatus::Stopped);
    // The driver removes the socket a tick after the exit.
    tokio::time::sleep(Duration::from_millis(200)).await;
    assert!(!h.runner.socket_path().exists());
    let late = h.runner.type_turn("too late");
    assert_eq!(late.wait().await, Err(DeliveryError::Exited));

    let mut h = start(Opts::default()).await;
    until_idle(&mut h).await;
    let d = h.runner.type_turn("/quit");
    let status = timeout(WAIT, h.runner.wait()).await.expect("exit");
    assert!(status.success());
    assert_eq!(d.wait().await, Err(DeliveryError::Exited));
    // An exit with code 0 that no one asked for is `stopped`, not `error`.
    assert_eq!(h.runner.status(), AgentStatus::Stopped);

    let mut h = start(Opts::default()).await;
    until_idle(&mut h).await;
    let _ = h.runner.type_turn("/exit 3");
    let status = timeout(WAIT, h.runner.wait()).await.expect("exit");
    assert_eq!(status.code, 3);
    assert_eq!(h.runner.status(), AgentStatus::Error);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn resize_changes_the_screen() {
    let mut h = start(Opts::default()).await;
    until_idle(&mut h).await;
    h.runner.resize(30, 100).expect("resize");
    let snap = h.runner.screen();
    assert_eq!((snap.rows, snap.cols), (30, 100));
    assert_eq!(snap.lines.len(), 30);
    h.runner.kill().expect("kill");
}

/// A handler that knows `echo`.
struct EchoHandler;

impl RequestHandler for EchoHandler {
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

async fn call(stream: &mut BufReader<UnixStream>, line: &str) -> Value {
    stream
        .get_mut()
        .write_all(format!("{line}\n").as_bytes())
        .await
        .expect("send");
    let mut reply = String::new();
    timeout(WAIT, stream.read_line(&mut reply))
        .await
        .expect("reply in time")
        .expect("read");
    serde_json::from_str(&reply).expect("json reply")
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn socket_answers_requests() {
    let mut h = start_with(Opts::default(), Arc::new(EchoHandler)).await;
    until_idle(&mut h).await;
    let stream = UnixStream::connect(h.runner.socket_path())
        .await
        .expect("connect");
    let mut s = BufReader::new(stream);

    let r = call(&mut s, r#"{"type":"request","id":1,"method":"ping"}"#).await;
    assert_eq!(r, json!({"type":"response","id":1,"result":"pong"}));

    let r = call(&mut s, r#"{"type":"request","id":"w","method":"whoami"}"#).await;
    assert_eq!(r["id"], "w");
    assert_eq!(r["result"]["agent"], h.runner.agent().to_string());
    assert_eq!(r["result"]["name"], "tester");
    assert_eq!(r["result"]["project"], "demo");
    assert_eq!(r["result"]["harness"], "claude");
    assert_eq!(r["result"]["status"], "idle");

    // A hook on the same connection gets no answer; the next request does.
    s.get_mut()
        .write_all(b"{\"type\":\"hook\",\"hook\":\"Unknown\",\"payload\":{}}\n")
        .await
        .expect("hook");
    let r = call(
        &mut s,
        r#"{"type":"request","id":3,"method":"echo","params":{"x":1}}"#,
    )
    .await;
    assert_eq!(r["result"]["params"], json!({"x": 1}));

    let r = call(&mut s, r#"{"type":"request","id":4,"method":"nope"}"#).await;
    assert_eq!(r["error"]["code"], "method_not_found");

    let r = call(&mut s, "not json").await;
    assert_eq!(r["error"]["code"], "parse_error");
    h.runner.kill().expect("kill");
}
