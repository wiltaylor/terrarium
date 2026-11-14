//! The grok adapter through the runner (TER-1192): the real
//! `GrokAdapter` (its launch args, hook mapping, turn keys, interrupt and
//! exit keys) drives the fake Grok Build TUI in `tests/fake_grok/`. A
//! fixture, not a signed-in grok: the PTY capture of the real TUI is a
//! release gate that needs the operator's OK.

mod common;

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use common::{until, wait};
use terrarium_adapter_grok::{GrokAdapter, GrokOptions, SessionStart};
use terrarium_core::config::AgentDef;
use terrarium_core::{Agent, AgentId, AgentStatus, Event, EventRecord, HarnessSessionId};
use terrarium_runner::{DefaultReadyDetector, NoHandler, Runner, RunnerConfig};
use tokio::sync::broadcast;
use tokio::time::timeout;

const SESSION: &str = "0f8e4c2a-7b1d-4e3f-9a6c-5d2b8e1f4a7c";

fn fake_grok() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fake_grok/fake_grok.py")
}

fn agent(root: &Path) -> Agent {
    let def: AgentDef = toml::from_str(
        "name = \"g\"\nproject = \"p\"\nharness = \"grok\"\nmodel = \"grok-build\"\n",
    )
    .expect("def");
    Agent::from_def(&def, root)
}

struct Harness {
    runner: Runner,
    events: broadcast::Receiver<EventRecord>,
}

impl Drop for Harness {
    fn drop(&mut self) {
        let _ = self.runner.kill();
    }
}

/// Start the fake grok with the adapter's own launch spec: the program is
/// the fake, the args and the env are the adapter's.
async fn start(dir: &Path, session: SessionStart<'_>) -> Harness {
    let agent = agent(dir);
    let adapter = GrokAdapter::new(GrokOptions {
        home_dir: dir.join("grok-home"),
        ..GrokOptions::default()
    });
    let spec = adapter.launch_with(&agent, session, Some("You are a test agent."));
    let mut launch =
        terrarium_core::LaunchSpec::new("python3", dir).arg(fake_grok().display().to_string());
    for a in &spec.args {
        launch = launch.arg(a.clone());
    }
    for (k, v) in &spec.env {
        launch = launch.env(k.clone(), v.clone());
    }
    let mut config = RunnerConfig::new(AgentId::new(), dir.join("state"), dir.join("run"));
    config.name = "g".into();
    config.project = "p".into();
    config.rows = 24;
    config.cols = 80;
    config.tick = Duration::from_millis(20);
    config.ready_detector = Some(Arc::new(DefaultReadyDetector {
        quiet: Duration::from_millis(100),
        pattern: None,
    }));
    let runner = Runner::spawn(config, Arc::new(adapter), launch, Arc::new(NoHandler))
        .await
        .expect("spawn");
    let events = runner.subscribe_events();
    Harness { runner, events }
}

async fn until_status(h: &mut Harness, to: AgentStatus) -> Vec<Event> {
    let mut seen = Vec::new();
    timeout(wait(), async {
        loop {
            let rec = h.events.recv().await.expect("event");
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

async fn until_screen(h: &Harness, text: &str) {
    until(&format!("`{text}` on the screen"), || {
        h.runner.screen().text().contains(text)
    })
    .await;
}

fn read_events(path: &Path) -> Vec<Event> {
    std::fs::read_to_string(path)
        .expect("events.jsonl")
        .lines()
        .map(|l| {
            serde_json::from_str::<EventRecord>(l)
                .expect("event line")
                .event
        })
        .collect()
}

/// Start, two turns, a slash text that stays text, a cancel with Ctrl-C
/// that keeps grok running, and a clean quit with `/quit`.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_grok_agent_runs_turns_cancels_and_quits() {
    let tmp = tempfile::tempdir().expect("tmp");
    let mut h = start(tmp.path(), SessionStart::Fresh(SESSION)).await;
    let seen = until_status(&mut h, AgentStatus::Idle).await;
    until_screen(&h, "fake grok 1.0.77 model=grok-build prompt=append").await;
    let started = seen
        .iter()
        .chain(read_events(h.runner.events_path()).iter())
        .any(|e| {
            matches!(e, Event::SessionStarted { harness_session, source, .. }
                if harness_session.0 == SESSION && source.as_deref() == Some("startup"))
        });
    assert!(started, "no SessionStarted: {seen:?}");

    // A turn: the camelCase hooks open and close it, with the tool events.
    let d = h.runner.type_turn("hello grok");
    timeout(wait(), d.wait())
        .await
        .expect("time")
        .expect("delivered");
    let seen = until_status(&mut h, AgentStatus::Idle).await;
    assert!(seen.contains(&Event::TurnStart {
        prompt: Some("hello grok".into())
    }));
    assert!(seen.contains(&Event::TurnEnd));
    assert!(
        seen.iter()
            .any(|e| matches!(e, Event::ToolUse { tool, .. } if tool == "Bash"))
    );
    until_screen(&h, "done: hello grok").await;

    // `/quit` typed as a turn is text for the model, not the command.
    let d = h.runner.type_turn("/quit");
    timeout(wait(), d.wait())
        .await
        .expect("time")
        .expect("delivered");
    until_status(&mut h, AgentStatus::Idle).await;
    until_screen(&h, "/quit").await;
    assert!(!h.runner.screen().text().contains("bye"));

    // Ctrl-C cancels the running turn; grok stays up.
    let d = h.runner.type_turn("hold this");
    timeout(wait(), d.wait())
        .await
        .expect("time")
        .expect("delivered");
    until("working", || h.runner.status() == AgentStatus::Working).await;
    assert!(h.runner.interrupt().await.expect("interrupt"));
    let seen = until_status(&mut h, AgentStatus::Idle).await;
    assert!(
        seen.iter().any(|e| matches!(e,
        Event::StatusChanged { to: AgentStatus::Idle, reason: Some(r), .. } if r == "interrupted"))
    );
    until_screen(&h, "cancelled").await;
    let d = h.runner.type_turn("again");
    timeout(wait(), d.wait())
        .await
        .expect("time")
        .expect("delivered");
    until_screen(&h, "done: again").await;
    until_status(&mut h, AgentStatus::Idle).await;

    // The exit keys quit grok cleanly, with its SessionEnd hook.
    let status = timeout(wait(), h.runner.stop()).await.expect("stop");
    assert!(status.success(), "{status:?}");
    let all = read_events(h.runner.events_path());
    assert!(
        all.iter().any(|e| matches!(e,
            Event::SessionEnded { reason } if reason.as_deref() == Some("exit"))),
        "{all:?}"
    );
}

/// A resume passes `--resume <id>`; grok reports the session as resumed.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_grok_agent_resumes_its_session() {
    let tmp = tempfile::tempdir().expect("tmp");
    let id = HarnessSessionId(SESSION.into());
    let mut h = start(tmp.path(), SessionStart::Resume(&id)).await;
    until_status(&mut h, AgentStatus::Idle).await;
    until("SessionStarted resume", || {
        read_events(h.runner.events_path()).iter().any(|e| {
            matches!(e, Event::SessionStarted { harness_session, source, .. }
                if *harness_session == id && source.as_deref() == Some("resume"))
        })
    })
    .await;
    let status = timeout(wait(), h.runner.stop()).await.expect("stop");
    assert!(status.success(), "{status:?}");
}
