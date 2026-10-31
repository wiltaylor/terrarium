//! Integration tests: the runner against the fake TUI in `tests/fake_tui/`.
//! Every wait is for a condition, with the limits of `common`.

mod common;

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use common::{ms, until, wait};
use semver::Version;
use serde_json::{Value, json};
use terrarium_core::{
    Adapter, Agent, AgentId, AgentStatus, Answer, Event, EventRecord, HarnessId, HarnessSessionId,
    HookPayload, LaunchSpec, ScreenHint,
};
use terrarium_runner::{
    AttachEvent, BoxFuture, DefaultReadyDetector, DeliveryError, NoHandler, RequestContext,
    RequestHandler, RpcError, Runner, RunnerConfig, RunnerHandle,
};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::net::UnixStream;
use tokio::sync::broadcast;
use tokio::time::timeout;

/// An adapter with the hook names of Claude Code. With `busy_screen` set,
/// `screen_hint` says `Busy` while the fake TUI shows `working...` or
/// `holding...`, and it cannot tell otherwise. A `UserPromptSubmit` with a
/// `<system_notification>` prompt is a system prompt, as for Copilot. The
/// exit keys are `/quit`, with `exit_pause` before the CR.
struct FakeAdapter {
    busy_screen: bool,
    exit_pause: Duration,
}

impl Adapter for FakeAdapter {
    fn id(&self) -> HarnessId {
        HarnessId::Claude
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
        if self.busy_screen && last.is_some_and(|l| matches!(l.trim(), "working..." | "holding..."))
        {
            ScreenHint::Busy
        } else {
            ScreenHint::Unknown
        }
    }
    fn is_system_prompt(&self, hook: &HookPayload) -> bool {
        hook.hook == "UserPromptSubmit"
            && hook
                .str_field("prompt")
                .is_some_and(|p| p.starts_with("<system_notification>"))
    }
    fn exit_keys(&self) -> Vec<u8> {
        b"/quit\r".to_vec()
    }
    fn exit_key_pause(&self) -> Duration {
        self.exit_pause
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

/// A test that fails half way leaves the harness running: the PTY stays
/// open while the test binary runs. End it: SIGHUP, then SIGKILL to its
/// process group after the kill grace (on a thread of the runner, so it
/// works while the runtime unwinds). It does nothing after an exit.
impl Drop for Harness {
    fn drop(&mut self) {
        let _ = self.runner.kill();
    }
}

struct Opts {
    env: Vec<(&'static str, String)>,
    stall: Duration,
    submit: Duration,
    busy_screen: bool,
    output_capacity: Option<usize>,
    kill_grace: Option<Duration>,
    stop_grace: Option<Duration>,
    exit_pause: Duration,
}

impl Default for Opts {
    fn default() -> Self {
        Self {
            env: vec![],
            stall: ms(60_000),
            submit: ms(10_000),
            busy_screen: false,
            output_capacity: None,
            kill_grace: None,
            stop_grace: None,
            exit_pause: Duration::ZERO,
        }
    }
}

fn env(pairs: &[(&'static str, &str)]) -> Vec<(&'static str, String)> {
    pairs.iter().map(|(k, v)| (*k, (*v).to_owned())).collect()
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
    if let Some(n) = opts.output_capacity {
        config.output_capacity = n;
    }
    if let Some(g) = opts.kill_grace {
        config.kill_grace = g;
    }
    if let Some(g) = opts.stop_grace {
        config.stop_grace = g;
    }
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
        exit_pause: opts.exit_pause,
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

/// Wait for a status-change event to `to`. Returns every event seen on the way.
async fn until_status(
    events: &mut broadcast::Receiver<EventRecord>,
    to: AgentStatus,
) -> Vec<Event> {
    let mut seen = Vec::new();
    timeout(wait(), async {
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
    until(&format!("`{text}` on the screen"), || {
        h.runner.screen().text().contains(text)
    })
    .await;
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
    timeout(wait(), delivery.wait())
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
        env: env(&[("FAKE_TUI_DELAY_MS", "600")]),
        ..Opts::default()
    })
    .await;
    until_idle(&mut h).await;

    let first = h.runner.type_turn("first");
    until_status(&mut h.events, AgentStatus::Working).await;
    let second = h.runner.type_turn("second");
    timeout(wait(), first.wait())
        .await
        .expect("time")
        .expect("first");

    // The second turn goes out only after the first turn ends.
    let seen = until_status(&mut h.events, AgentStatus::Idle).await;
    assert!(!seen.iter().any(|e| matches!(e,
        Event::TurnStart { prompt: Some(p) } if p == "second")));
    timeout(wait(), second.wait())
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
    // `/hold` stays silent until the test types a line, so the stall does
    // not race a timer in the fake TUI.
    let mut h = start(Opts {
        stall: Duration::from_millis(400),
        ..Opts::default()
    })
    .await;
    until_idle(&mut h).await;
    let d = h.runner.type_turn("/hold");
    timeout(wait(), d.wait())
        .await
        .expect("time")
        .expect("delivered");
    let seen = until_status(&mut h.events, AgentStatus::Blocked).await;
    assert!(seen.iter().any(|e| matches!(e,
        Event::StatusChanged { to: AgentStatus::Blocked, reason: Some(r), .. } if r == "stalled")));
    // The fake TUI prints and sends Stop after the line.
    h.runner.write(b"\r").await.expect("release");
    until_status(&mut h.events, AgentStatus::Idle).await;
    until_screen(&h, "done: /hold").await;
    h.runner.kill().expect("kill");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn question_blocks_and_answer_resumes() {
    // The fake TUI keeps the turn open after the answer until the test types
    // one more line. Without that hold, the Stop hook can come before the
    // test reads the status, and the status is already `idle`.
    let mut h = start(Opts {
        env: env(&[("FAKE_TUI_ANSWER_HOLD", "1")]),
        ..Opts::default()
    })
    .await;
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
    let seen = until_status(&mut h.events, AgentStatus::Working).await;
    assert!(
        seen.iter().any(|e| matches!(e,
        Event::StatusChanged { to: AgentStatus::Working, reason: Some(r), .. } if r == "answered"))
    );
    until_screen(&h, "answer: y").await;
    h.runner.write(b"\r").await.expect("release");
    until_status(&mut h.events, AgentStatus::Idle).await;
    h.runner.kill().expect("kill");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn delivery_fails_after_one_retry() {
    let mut h = start(Opts {
        env: env(&[("FAKE_TUI_NO_SUBMIT", "1")]),
        submit: ms(500),
        ..Opts::default()
    })
    .await;
    until_idle(&mut h).await;
    let d = h.runner.type_turn("lost");
    let err = timeout(wait(), d.wait())
        .await
        .expect("time")
        .expect_err("fails");
    assert_eq!(err, DeliveryError::NoSubmitHook { attempts: 2 });
    // The fake TUI saw the turn twice, and the clear keys before the retry.
    // The echo of the second try can reach the screen after the failure.
    until("the second try on the screen", || {
        h.runner.screen().text().matches("lost").count() >= 2
    })
    .await;
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
    // `/hold` keeps the turn busy until the test types a line, so the turn
    // runs past `submit_timeout` on any machine.
    let submit = ms(1000);
    let mut h = start(Opts {
        env: env(&[("FAKE_TUI_NO_SUBMIT_HOOK", "1")]),
        submit,
        busy_screen: true,
        ..Opts::default()
    })
    .await;
    until_idle(&mut h).await;
    let d = h.runner.type_turn("/hold");
    timeout(wait(), d.wait())
        .await
        .expect("time")
        .expect("the busy screen confirms the turn");
    // A retry would come `submit` after the typing. Wait past it, then let
    // the turn end.
    tokio::time::sleep(submit + Duration::from_millis(300)).await;
    h.runner.write(b"\r").await.expect("release");
    let seen = until_status(&mut h.events, AgentStatus::Idle).await;
    assert!(seen.contains(&Event::TurnEnd));
    assert!(!seen.iter().any(|e| matches!(e, Event::TurnStart { .. })));
    // Typed once: no clear keys and no retry.
    until_screen(&h, "done: /hold").await;
    let text = h.runner.screen().text();
    assert_eq!(text.matches("done: /hold").count(), 1, "{text}");
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

/// Copilot sends a `UserPromptSubmit` with a `<system_notification>`
/// prompt when a background shell ends. It must not confirm a typed turn,
/// and it is not a user turn.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_system_prompt_does_not_confirm_a_delivery() {
    let dir = tempfile::tempdir().expect("tempdir");
    let gate = dir.path().join("gate");
    let mut h = start(Opts {
        env: vec![("FAKE_TUI_SUBMIT_GATE", gate.display().to_string())],
        ..Opts::default()
    })
    .await;
    until_idle(&mut h).await;
    let mut delivery = Box::pin(h.runner.type_turn("hello").wait());
    // The fake TUI holds its own UserPromptSubmit until the gate opens.
    // A system prompt comes first.
    until_screen(&h, "hello").await;
    let system = "<system_notification>Shell command completed</system_notification>";
    let mut s = UnixStream::connect(h.runner.socket_path())
        .await
        .expect("connect");
    let hook = json!({"type":"hook","hook":"UserPromptSubmit","payload":{"prompt": system}});
    s.write_all(format!("{hook}\n").as_bytes())
        .await
        .expect("hook");
    drop(s);
    let seen = until_status(&mut h.events, AgentStatus::Working).await;
    assert!(seen.iter().any(|e| matches!(e,
        Event::Notification { message, question: false } if message == system)));
    assert!(!seen.iter().any(|e| matches!(e, Event::TurnStart { .. })));
    assert!(
        timeout(Duration::from_millis(200), &mut delivery)
            .await
            .is_err(),
        "the system prompt confirmed the delivery"
    );

    // The real prompt-submit hook confirms it.
    std::fs::write(&gate, b"").expect("open the gate");
    timeout(wait(), delivery)
        .await
        .expect("time")
        .expect("the real hook confirms the turn");
    let seen = until_status(&mut h.events, AgentStatus::Idle).await;
    assert!(seen.contains(&Event::TurnStart {
        prompt: Some("hello".into())
    }));
    h.runner.kill().expect("kill");
    h.runner.wait().await;
    let all = read_events(h.runner.events_path());
    let starts: Vec<_> = all
        .iter()
        .filter(|r| matches!(r.event, Event::TurnStart { .. }))
        .collect();
    assert_eq!(starts.len(), 1, "only the typed turn is a TurnStart");
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
    timeout(wait(), async {
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
    let status = timeout(wait(), h.runner.wait()).await.expect("exit");
    assert!(!status.success());
    assert_eq!(h.runner.status(), AgentStatus::Stopped);
    // The driver removes the socket a tick after the exit.
    until("the socket is removed", || !h.runner.socket_path().exists()).await;
    let late = h.runner.type_turn("too late");
    assert_eq!(late.wait().await, Err(DeliveryError::Exited));

    let mut h = start(Opts::default()).await;
    until_idle(&mut h).await;
    let d = h.runner.type_turn("/quit");
    let status = timeout(wait(), h.runner.wait()).await.expect("exit");
    assert!(status.success());
    assert_eq!(d.wait().await, Err(DeliveryError::Exited));
    // An exit with code 0 that no one asked for is `stopped`, not `error`.
    assert_eq!(h.runner.status(), AgentStatus::Stopped);

    let mut h = start(Opts::default()).await;
    until_idle(&mut h).await;
    let _ = h.runner.type_turn("/exit 3");
    let status = timeout(wait(), h.runner.wait()).await.expect("exit");
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
    timeout(wait(), stream.read_line(&mut reply))
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

/// A tiny output channel and a reader that does not read: the attach stream
/// lags, attaches again by itself, and gives `Resynced` with a new
/// snapshot. The output then goes on.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn an_attach_stream_resyncs_after_a_lag() {
    let mut h = start(Opts {
        output_capacity: Some(2),
        ..Opts::default()
    })
    .await;
    until_idle(&mut h).await;
    let (_, mut stream) = RunnerHandle::attach(&h.runner).await.expect("attach");
    timeout(wait(), h.runner.type_turn("/flood 3000").wait())
        .await
        .expect("time")
        .expect("delivered");
    until_status(&mut h.events, AgentStatus::Idle).await;
    until_screen(&h, "done: /flood 3000").await;

    // The stream fell behind during the flood.
    let mut resynced = None;
    timeout(wait(), async {
        while let Some(item) = stream.recv().await {
            if let AttachEvent::Resynced(snap) = item {
                resynced = Some(snap);
                return;
            }
        }
    })
    .await
    .expect("a resync in time");
    let snap = resynced.expect("resynced");
    assert!(!snap.formatted.is_empty());

    // After the resync, the stream has live output again.
    h.runner.type_turn("after-resync");
    let mut got = Vec::new();
    timeout(wait(), async {
        while !String::from_utf8_lossy(&got).contains("done: after-resync") {
            match stream.recv().await.expect("the stream goes on") {
                AttachEvent::Output(chunk) => got.extend_from_slice(&chunk),
                AttachEvent::Resynced(snap) => got = snap.text().into_bytes(),
            }
        }
    })
    .await
    .expect("output after the resync");

    // The stream ends after the harness exits.
    h.runner.kill().expect("kill");
    timeout(wait(), async { while stream.recv().await.is_some() {} })
        .await
        .expect("the stream ends");
}

/// `kill` sends SIGHUP. A harness that ignores it gets SIGKILL after
/// `kill_grace`.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn kill_escalates_to_sigkill() {
    let mut h = start(Opts {
        env: env(&[("FAKE_TUI_IGNORE_HUP", "1")]),
        kill_grace: Some(ms(300)),
        ..Opts::default()
    })
    .await;
    until_idle(&mut h).await;
    h.runner.kill().expect("kill");
    let status = timeout(wait(), h.runner.wait()).await.expect("exit");
    assert_eq!(status.signal_number(), Some(9), "{status:?}");
    assert_eq!(status.exit_code(), 137);
    assert_eq!(h.runner.status(), AgentStatus::Stopped);
}

/// `stop` types the exit keys into an idle harness: a clean exit, status
/// `stopped`. A busy harness gets `kill`.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn stop_types_the_exit_keys_when_idle_and_kills_when_busy() {
    let mut h = start(Opts::default()).await;
    until_idle(&mut h).await;
    let status = timeout(wait(), h.runner.stop()).await.expect("stop");
    assert!(status.success(), "{status:?}");
    assert_eq!(h.runner.status(), AgentStatus::Stopped);
    until_screen(&h, "bye").await;

    let mut h = start(Opts {
        stop_grace: Some(ms(200)),
        ..Opts::default()
    })
    .await;
    until_idle(&mut h).await;
    timeout(wait(), h.runner.type_turn("/hold").wait())
        .await
        .expect("time")
        .expect("delivered");
    until_screen(&h, "holding...").await;
    let status = timeout(wait(), h.runner.stop()).await.expect("stop");
    // SIGHUP ends the fake TUI.
    assert_eq!(status.signal_number(), Some(1), "{status:?}");
    assert_eq!(status.exit_code(), 129);
    assert_eq!(h.runner.status(), AgentStatus::Stopped);
    assert!(!h.runner.screen().text().contains("bye"));
}

/// A harness that submits a slash command only when its CR comes after a
/// pause (as Codex with `/exit`): the exit keys in one write do not exit
/// it, and `stop` falls back to the kill. With `exit_key_pause`, the CR
/// goes in a second write after the pause, and the harness exits 0.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn stop_types_the_exit_cr_after_the_exit_key_pause() {
    // One write: the text stays in the line, so the kill ends it.
    let mut h = start(Opts {
        env: env(&[("FAKE_TUI_SLASH_NEEDS_PAUSE", "1")]),
        stop_grace: Some(ms(500)),
        ..Opts::default()
    })
    .await;
    until_idle(&mut h).await;
    let status = timeout(wait(), h.runner.stop()).await.expect("stop");
    assert_eq!(status.signal_number(), Some(1), "{status:?}");
    assert!(!h.runner.screen().text().contains("bye"));

    // The CR after the pause: a clean exit.
    let mut h = start(Opts {
        env: env(&[("FAKE_TUI_SLASH_NEEDS_PAUSE", "1")]),
        exit_pause: ms(300),
        ..Opts::default()
    })
    .await;
    until_idle(&mut h).await;
    let status = timeout(wait(), h.runner.stop()).await.expect("stop");
    assert!(status.success(), "{status:?}");
    assert_eq!(status.exit_code(), 0);
    assert_eq!(h.runner.status(), AgentStatus::Stopped);
    until_screen(&h, "bye").await;
}

/// Two stops at once (a stop, then a delete that stops again): only the
/// first types the exit keys. The terminal reads `/quit` once and the CR
/// once, in a read of its own, and the harness exits 0.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_second_stop_types_no_more_exit_keys() {
    let logs = tempfile::tempdir().expect("tempdir");
    let log = logs.path().join("input.log");
    let mut h = start(Opts {
        env: env(&[
            ("FAKE_TUI_SLASH_NEEDS_PAUSE", "1"),
            ("FAKE_TUI_INPUT_LOG", log.to_str().expect("utf-8")),
        ]),
        exit_pause: ms(300),
        ..Opts::default()
    })
    .await;
    until_idle(&mut h).await;
    let (a, b) = timeout(wait(), async {
        tokio::join!(h.runner.stop(), async {
            tokio::time::sleep(ms(50)).await;
            h.runner.stop().await
        })
    })
    .await
    .expect("stop");
    assert!(a.success() && b.success(), "{a:?} {b:?}");
    let reads: Vec<String> = std::fs::read_to_string(&log)
        .expect("input log")
        .lines()
        .filter(|l| !l.is_empty())
        .map(str::to_owned)
        .collect();
    // `/quit` and CR, as hex.
    assert_eq!(reads.concat(), "2f717569740d", "{reads:?}");
    assert_eq!(reads.last().map(String::as_str), Some("0d"), "{reads:?}");
}

/// A runner config and a launch spec of the fake TUI in `dir`, for the
/// tests of a planted symlink (`docs/security-review.md`, F11).
fn planted_config(dir: &Path) -> (RunnerConfig, LaunchSpec) {
    let config = RunnerConfig::new(AgentId::new(), dir.join("state"), dir.join("run"));
    let spec = LaunchSpec::new("python3", dir).arg(fake_tui().display().to_string());
    (config, spec)
}

async fn spawn_err(config: RunnerConfig, spec: LaunchSpec) -> terrarium_runner::RunnerError {
    let adapter = Arc::new(FakeAdapter {
        busy_screen: false,
        exit_pause: Duration::ZERO,
    });
    match Runner::spawn(config, adapter, spec, Arc::new(NoHandler)).await {
        Ok(runner) => {
            let _ = runner.kill();
            panic!("the runner started over a planted symlink");
        }
        Err(e) => e,
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_planted_events_symlink_stops_the_start() {
    let dir = tempfile::tempdir().expect("tempdir");
    let (config, spec) = planted_config(dir.path());
    let private = config.private_dir();
    std::fs::create_dir_all(&private).expect("private");
    let target = dir.path().join("workspace-events");
    std::fs::write(&target, b"").expect("target");
    let events = private.join("events.jsonl");
    std::os::unix::fs::symlink(&target, &events).expect("plant");
    let socket = config.socket_path();
    let err = spawn_err(config, spec).await;
    assert!(
        matches!(&err, terrarium_runner::RunnerError::UnsafePath { path, .. } if *path == events),
        "{err}"
    );
    assert!(err.to_string().contains("events.jsonl"), "{err}");
    // Nothing went through the link, and the runner bound no socket.
    assert_eq!(std::fs::read(&target).expect("read"), b"");
    assert!(!socket.exists());
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_planted_private_symlink_stops_the_start() {
    let dir = tempfile::tempdir().expect("tempdir");
    let (config, spec) = planted_config(dir.path());
    let agent_dir = config.agent_dir();
    let work = agent_dir.join("workspace");
    std::fs::create_dir_all(&work).expect("workspace");
    let private = config.private_dir();
    std::os::unix::fs::symlink(&work, &private).expect("plant");
    let err = spawn_err(config, spec).await;
    assert!(
        matches!(&err, terrarium_runner::RunnerError::UnsafePath { path, .. } if *path == private),
        "{err}"
    );
    assert!(std::fs::read_dir(&work).expect("ls").next().is_none());
}
