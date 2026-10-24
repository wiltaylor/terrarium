//! One real Claude Code run inside `bwrap`, through the whole daemon. Not in
//! `just ci::check`: it needs a logged-in Claude home and spends one model
//! turn. Run it with:
//!
//! ```sh
//! cargo build -p terrarium -p terrarium-runner
//! TERRARIUM_E2E_CLAUDE_HOME=$HOME/.claude \
//!   cargo test -p terrariumd --test e2e_claude -- --ignored --nocapture
//! ```
//!
//! The agents run in `terrarium-runner` processes, as in the daemon. Set
//! `TERRARIUM_E2E_RUNNER_MODE=inprocess` to run them in the test process.
//!
//! It binds the Claude home read-write, as the container does with
//! `/auth/claude`. It does not copy or change the credentials. The results
//! are in `docs/compat.md`, "Claude inside bwrap".
#![allow(clippy::unwrap_used, clippy::expect_used, clippy::print_stdout)]

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, Instant};

use http_body_util::{BodyExt, Full};
use hyper::body::Bytes;
use hyper::{Method, Request};
use hyper_util::rt::TokioIo;
use serde_json::{Value, json};
use terrarium_adapter_claude::screen::screen_state;
use terrarium_core::{AgentId, HarnessId};
use terrarium_runner::RunnerHandle;
use terrariumd::runner_supervisor::{RunnerMode, RunnerSupervisor, RunnerSupervisorConfig};
use terrariumd::sandbox::{self, SandboxConfig, SandboxMode};
use terrariumd::{Daemon, DaemonConfig};
use tokio::net::UnixStream;

async fn req(socket: &Path, method: Method, path: &str, body: Option<Value>) -> (u16, Value) {
    let stream = UnixStream::connect(socket).await.unwrap();
    let (mut sender, conn) = hyper::client::conn::http1::handshake(TokioIo::new(stream))
        .await
        .unwrap();
    tokio::spawn(conn);
    let mut r = Request::builder()
        .method(method)
        .uri(path)
        .header("host", "localhost");
    let bytes = match body {
        Some(b) => {
            r = r.header("content-type", "application/json");
            Bytes::from(b.to_string())
        }
        None => Bytes::new(),
    };
    let resp = sender
        .send_request(r.body(Full::new(bytes)).unwrap())
        .await
        .unwrap();
    let status = resp.status().as_u16();
    let raw = resp.into_body().collect().await.unwrap().to_bytes();
    (status, serde_json::from_slice(&raw).unwrap_or(Value::Null))
}

/// The runner mode of the run: one `terrarium-runner` process per agent
/// (built next to the test binaries: `cargo build -p terrarium-runner`),
/// or in the test process with `TERRARIUM_E2E_RUNNER_MODE=inprocess`.
fn with_runner_mode(mut cfg: RunnerSupervisorConfig) -> RunnerSupervisorConfig {
    if std::env::var("TERRARIUM_E2E_RUNNER_MODE").as_deref() == Ok("inprocess") {
        println!("runner mode: in-process");
        return cfg;
    }
    let bin = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../target/debug/terrarium-runner");
    assert!(bin.is_file(), "build the runner first: {}", bin.display());
    println!("runner mode: process ({})", bin.display());
    cfg.mode = RunnerMode::Process {
        bin: std::fs::canonicalize(bin).unwrap(),
    };
    cfg
}

/// The screen text of a running agent, for a failure message.
async fn screen_of(sup: &RunnerSupervisor, id: AgentId) -> String {
    match sup.runner(id) {
        Some(r) => r.screen().await.map(|s| s.text()).unwrap_or_default(),
        None => String::new(),
    }
}

async fn wait_for(what: &str, limit: Duration, mut f: impl AsyncFnMut() -> bool) {
    let start = Instant::now();
    while !f().await {
        assert!(start.elapsed() < limit, "timed out waiting for {what}");
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    println!("[{:>6.2}s] {what}", start.elapsed().as_secs_f32());
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[ignore = "needs a logged-in Claude home and bwrap; spends one model turn"]
async fn claude_in_bwrap_answers_one_turn() {
    let home = PathBuf::from(
        std::env::var("TERRARIUM_E2E_CLAUDE_HOME").expect("set TERRARIUM_E2E_CLAUDE_HOME"),
    );
    let cli = std::env::var("TERRARIUM_E2E_CLI")
        .map(PathBuf::from)
        .unwrap_or_else(|_| {
            Path::new(env!("CARGO_MANIFEST_DIR")).join("../../target/debug/terrarium")
        });
    assert!(cli.is_file(), "build the CLI first: {}", cli.display());
    let root = tempfile::tempdir().unwrap();
    // Unix socket paths must stay short.
    let run = tempfile::Builder::new()
        .prefix("te2e")
        .tempdir_in("/tmp")
        .unwrap();

    let mut sandbox = SandboxConfig {
        mode: SandboxMode::Bwrap,
        cli: Some(std::fs::canonicalize(&cli).unwrap()),
        new_session: std::env::var("TERRARIUM_E2E_NEW_SESSION").as_deref() == Ok("1"),
        ..SandboxConfig::default()
    };
    sandbox.bwrap = sandbox.check().expect("bwrap works");
    let mut homes = HashMap::new();
    homes.insert(HarnessId::Claude, home.clone());
    let cfg = RunnerSupervisorConfig::new(
        root.path().join("state"),
        run.path().to_path_buf(),
        root.path().join("prompts"),
        sandbox,
        homes,
    );
    let sup = Arc::new(RunnerSupervisor::new(with_runner_mode(cfg)));
    let socket = run.path().join("api.sock");
    std::fs::create_dir_all(root.path().join("config")).unwrap();
    let daemon: Daemon = terrariumd::start(DaemonConfig {
        socket: socket.clone(),
        listen: None,
        token: None,
        state_dir: root.path().join("state"),
        config_dir: root.path().join("config"),
        supervisor: sup.clone(),
        insecure_agent_header: false,
        artifact_listen: None,
    })
    .await
    .unwrap();

    req(
        &socket,
        Method::POST,
        "/api/projects",
        Some(json!({"name": "e2e"})),
    )
    .await;
    let (s, a) = req(
        &socket,
        Method::POST,
        "/api/agents",
        Some(
            json!({"name": "pong", "project": "e2e", "harness": "claude",
                    "model": "haiku", "restart": "never"}),
        ),
    )
    .await;
    assert_eq!(s, 201, "{a}");
    let id: AgentId = a["id"].as_str().unwrap().parse().unwrap();
    let agent_path = format!("/api/agents/{id}");

    // The full sandbox command.
    let view = {
        let (_, v) = req(&socket, Method::GET, &agent_path, None).await;
        let mut agent: terrarium_core::Agent = serde_json::from_value(v).unwrap();
        agent.workspace = root.path().join(format!("state/agents/{id}/workspace"));
        agent
    };
    let spec = sup.launch_spec(&view, None).await.unwrap();
    println!(
        "--- sandbox command ---\n{}\n---",
        sandbox::command_line(&spec)
    );

    let (s, b) = req(&socket, Method::POST, &format!("{agent_path}/start"), None).await;
    assert_eq!(s, 202, "{b}");
    let status = async || {
        req(&socket, Method::GET, &agent_path, None).await.1["status"]
            .as_str()
            .unwrap_or("")
            .to_owned()
    };
    let started = Instant::now();
    wait_for("idle after start", Duration::from_secs(60), async || {
        let s = status().await;
        if s == "error" || s == "stopped" {
            let screen = screen_of(&sup, id).await;
            panic!("the agent ended: {s}\n{screen}");
        }
        s == "idle"
    })
    .await;
    println!(
        "prompt ready {:.1}s after start",
        started.elapsed().as_secs_f32()
    );
    let runner = sup.runner(id).expect("runner");
    println!(
        "--- idle screen ---\n{}\n---",
        runner.screen().await.unwrap().text()
    );

    // Clear-input keys: type text without Enter, then Ctrl-U.
    runner.write(b"draft text").await.unwrap();
    tokio::time::sleep(Duration::from_millis(800)).await;
    let before = screen_state(&runner.screen().await.unwrap().lines);
    println!("input before clear: {:?}", before.input_text);
    let keys = terrarium_core::Adapter::clear_input_keys(
        &terrarium_adapter_claude::ClaudeAdapter::default(),
    );
    runner.write(&keys).await.unwrap();
    tokio::time::sleep(Duration::from_millis(800)).await;
    let after = screen_state(&runner.screen().await.unwrap().lines);
    println!(
        "input after clear ({keys:?}): {:?}, ready = {}",
        after.input_text, after.ready
    );
    assert!(after.ready, "{}", runner.screen().await.unwrap().text());
    assert_eq!(
        status().await,
        "idle",
        "the clear keys must not end the agent"
    );

    // Resize: with `--new-session` the harness has no controlling
    // terminal. Does it still redraw at the new size?
    let width = async |r: &Arc<dyn RunnerHandle>| {
        r.screen()
            .await
            .unwrap()
            .lines
            .iter()
            .filter(|l| l.starts_with('─'))
            .map(|l| l.chars().count())
            .max()
            .unwrap_or(0)
    };
    println!("rule width before resize: {}", width(&runner).await);
    runner.resize(40, 150).await.unwrap();
    tokio::time::sleep(Duration::from_millis(1500)).await;
    println!(
        "rule width after resize to 150 cols: {}",
        width(&runner).await
    );

    // The per-agent `sessions/` overlay: the registry file lands in the
    // agent dir, not in the shared home.
    let own = root
        .path()
        .join(format!("state/agents/{id}/claude/sessions"));
    let names: Vec<String> = std::fs::read_dir(&own)
        .unwrap()
        .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
        .collect();
    println!("per-agent sessions dir: {names:?}");

    if std::env::var("TERRARIUM_E2E_NO_TURN").is_ok() {
        let (s, _) = req(&socket, Method::POST, &format!("{agent_path}/stop"), None).await;
        assert_eq!(s, 202);
        daemon.shutdown();
        return;
    }

    // One model turn.
    let (s, b) = req(
        &socket,
        Method::POST,
        &format!("{agent_path}/send"),
        Some(json!({"text": "Reply with exactly: pong"})),
    )
    .await;
    assert_eq!(s, 202, "{b}");
    let turn = Instant::now();
    let kinds = async |kind: &str| {
        req(
            &socket,
            Method::GET,
            &format!("{agent_path}/events?kind={kind}"),
            None,
        )
        .await
        .1
        .as_array()
        .cloned()
        .unwrap_or_default()
    };
    wait_for(
        "turn_start (UserPromptSubmit hook)",
        Duration::from_secs(20),
        async || !kinds("turn_start").await.is_empty(),
    )
    .await;
    wait_for(
        "turn_end (Stop hook)",
        Duration::from_secs(90),
        async || !kinds("turn_end").await.is_empty(),
    )
    .await;
    println!("turn took {:.1}s", turn.elapsed().as_secs_f32());
    wait_for("idle after the turn", Duration::from_secs(20), async || {
        status().await == "idle"
    })
    .await;
    let statuses: Vec<String> = kinds("status")
        .await
        .iter()
        .map(|e| {
            format!(
                "{} ({})",
                e["data"]["status"].as_str().unwrap_or(""),
                e["data"]["reason"].as_str().unwrap_or("")
            )
        })
        .collect();
    println!("statuses: {statuses:?}");
    let session = kinds("session_started").await;
    println!("session_started: {session:?}");
    let all = req(&socket, Method::GET, &format!("{agent_path}/events"), None)
        .await
        .1;
    let names: Vec<&str> = all
        .as_array()
        .unwrap()
        .iter()
        .map(|e| e["kind"].as_str().unwrap_or(""))
        .collect();
    println!("event kinds: {names:?}");
    let screen = runner.screen().await.unwrap().text();
    println!("--- screen after the turn ---\n{screen}\n---");
    assert!(screen.contains("pong"));

    let (s, _) = req(&socket, Method::POST, &format!("{agent_path}/stop"), None).await;
    assert_eq!(s, 202);
    wait_for("stopped", Duration::from_secs(20), async || {
        status().await == "stopped"
    })
    .await;
    daemon.shutdown();
}

/// One agent of the two-agent run.
struct Pair {
    name: &'static str,
    id: AgentId,
    path: String,
    token: String,
}

async fn events_of(socket: &Path, agent_path: &str, kind: &str) -> Vec<Value> {
    req(
        socket,
        Method::GET,
        &format!("{agent_path}/events?kind={kind}"),
        None,
    )
    .await
    .1
    .as_array()
    .cloned()
    .unwrap_or_default()
}

async fn status_of(socket: &Path, agent_path: &str) -> String {
    req(socket, Method::GET, agent_path, None).await.1["status"]
        .as_str()
        .unwrap_or("")
        .to_owned()
}

/// The text of the last assistant message in a Claude transcript.
fn last_assistant_text(transcript: &Path) -> String {
    let raw = std::fs::read_to_string(transcript).unwrap_or_default();
    raw.lines()
        .filter_map(|l| serde_json::from_str::<Value>(l).ok())
        .filter(|v| v["type"] == "assistant")
        .filter_map(|v| {
            let parts = v["message"]["content"].as_array()?.clone();
            let text: String = parts
                .iter()
                .filter_map(|p| p["text"].as_str())
                .collect::<Vec<_>>()
                .join("");
            (!text.trim().is_empty()).then_some(text)
        })
        .next_back()
        .unwrap_or_default()
}

/// Two Claude agents at the same time on one shared home, each with one
/// turn, then a stop and a start of both (a real `--resume` of each
/// session) and one more turn each: four model turns. Run it alone:
///
/// ```sh
/// cargo build -p terrarium -p terrarium-runner
/// TERRARIUM_E2E_CLAUDE_HOME=$HOME/.claude cargo test -p terrariumd \
///   --test e2e_claude -- --ignored --exact two_claude_agents_share_one_home --nocapture
/// ```
///
/// `spikes/claude/concurrency/run.py` runs it and checks the shared home
/// before and after. With `TERRARIUM_E2E_REPORT=<file>`, the test writes
/// what it saw there as JSON.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[ignore = "needs a logged-in Claude home and bwrap; spends four model turns"]
async fn two_claude_agents_share_one_home() {
    let home = PathBuf::from(
        std::env::var("TERRARIUM_E2E_CLAUDE_HOME").expect("set TERRARIUM_E2E_CLAUDE_HOME"),
    );
    let cli = std::env::var("TERRARIUM_E2E_CLI")
        .map(PathBuf::from)
        .unwrap_or_else(|_| {
            Path::new(env!("CARGO_MANIFEST_DIR")).join("../../target/debug/terrarium")
        });
    assert!(cli.is_file(), "build the CLI first: {}", cli.display());
    let nonce =
        std::env::var("TERRARIUM_E2E_NONCE").unwrap_or_else(|_| format!("{}", std::process::id()));
    let root = tempfile::tempdir().unwrap();
    let run = tempfile::Builder::new()
        .prefix("te2e")
        .tempdir_in("/tmp")
        .unwrap();
    let mut sandbox = SandboxConfig {
        mode: SandboxMode::Bwrap,
        cli: Some(std::fs::canonicalize(&cli).unwrap()),
        ..SandboxConfig::default()
    };
    sandbox.bwrap = sandbox.check().expect("bwrap works");
    let mut homes = HashMap::new();
    homes.insert(HarnessId::Claude, home.clone());
    let cfg = RunnerSupervisorConfig::new(
        root.path().join("state"),
        run.path().to_path_buf(),
        root.path().join("prompts"),
        sandbox,
        homes,
    );
    let sup = Arc::new(RunnerSupervisor::new(with_runner_mode(cfg)));
    let socket = run.path().join("api.sock");
    std::fs::create_dir_all(root.path().join("config")).unwrap();
    let daemon: Daemon = terrariumd::start(DaemonConfig {
        socket: socket.clone(),
        listen: None,
        token: None,
        state_dir: root.path().join("state"),
        config_dir: root.path().join("config"),
        supervisor: sup.clone(),
        insecure_agent_header: false,
        artifact_listen: None,
    })
    .await
    .unwrap();
    req(
        &socket,
        Method::POST,
        "/api/projects",
        Some(json!({"name": "e2e2"})),
    )
    .await;

    let mut pairs = Vec::new();
    for name in ["alpha", "beta"] {
        let (s, a) = req(
            &socket,
            Method::POST,
            "/api/agents",
            Some(json!({"name": name, "project": "e2e2", "harness": "claude",
                        "model": "haiku", "restart": "never"})),
        )
        .await;
        assert_eq!(s, 201, "{a}");
        let id: AgentId = a["id"].as_str().unwrap().parse().unwrap();
        pairs.push(Pair {
            name,
            id,
            path: format!("/api/agents/{id}"),
            token: format!("{name}-{nonce}"),
        });
    }

    // Start both at once, then wait for both.
    let starts = pairs.iter().map(|p| {
        let socket = socket.clone();
        let path = format!("{}/start", p.path);
        async move { req(&socket, Method::POST, &path, None).await }
    });
    for (s, b) in futures_join(starts).await {
        assert_eq!(s, 202, "{b}");
    }
    let wait_idle = async |what: &str| {
        for p in &pairs {
            wait_for(
                &format!("{} {what}", p.name),
                Duration::from_secs(90),
                async || {
                    let s = status_of(&socket, &p.path).await;
                    if s == "error" || s == "stopped" {
                        let screen = screen_of(&sup, p.id).await;
                        panic!("{} ended: {s}\n{screen}", p.name);
                    }
                    s == "idle"
                },
            )
            .await;
        }
    };
    wait_idle("idle after start").await;

    // One turn each, sent back to back.
    let send_all = async |text: &dyn Fn(&Pair) -> String| {
        for p in &pairs {
            let (s, b) = req(
                &socket,
                Method::POST,
                &format!("{}/send", p.path),
                Some(json!({"text": text(p)})),
            )
            .await;
            assert_eq!(s, 202, "{b}");
        }
    };
    let wait_turns = async |n: usize| {
        for p in &pairs {
            wait_for(
                &format!("{} turn_end #{n}", p.name),
                Duration::from_secs(120),
                async || events_of(&socket, &p.path, "turn_end").await.len() >= n,
            )
            .await;
        }
    };
    send_all(&|p| format!("Reply with exactly: {}", p.token)).await;
    wait_turns(1).await;
    wait_idle("idle after turn 1").await;

    let session_ids = async |p: &Pair| -> Vec<String> {
        events_of(&socket, &p.path, "session_started")
            .await
            .iter()
            .filter_map(|e| e["data"]["harness_session"].as_str().map(str::to_owned))
            .collect()
    };
    let mut first = Vec::new();
    for p in &pairs {
        let ids = session_ids(p).await;
        println!("{} session_started: {ids:?}", p.name);
        assert!(!ids.is_empty(), "{}: no session_started", p.name);
        first.push(ids[0].clone());
    }
    assert_ne!(first[0], first[1], "two agents, one session id");

    // Stop both, start both: the start resumes the last session.
    for p in &pairs {
        let (s, _) = req(&socket, Method::POST, &format!("{}/stop", p.path), None).await;
        assert_eq!(s, 202);
    }
    for p in &pairs {
        wait_for(
            &format!("{} stopped", p.name),
            Duration::from_secs(30),
            async || status_of(&socket, &p.path).await == "stopped",
        )
        .await;
    }
    let starts = pairs.iter().map(|p| {
        let socket = socket.clone();
        let path = format!("{}/start", p.path);
        async move { req(&socket, Method::POST, &path, None).await }
    });
    for (s, b) in futures_join(starts).await {
        assert_eq!(s, 202, "{b}");
    }
    wait_idle("idle after the resume").await;
    send_all(&|_| {
        "What exact text did you reply with to my previous message? Reply with only that text."
            .to_owned()
    })
    .await;
    wait_turns(2).await;
    wait_idle("idle after turn 2").await;

    let mut report = serde_json::Map::new();
    report.insert("nonce".into(), json!(nonce));
    report.insert("state_dir".into(), json!(root.path().join("state")));
    for (i, p) in pairs.iter().enumerate() {
        let ids = session_ids(p).await;
        let reasons: Vec<String> = events_of(&socket, &p.path, "status")
            .await
            .iter()
            .filter_map(|e| e["data"]["reason"].as_str().map(str::to_owned))
            .collect();
        let sources: Vec<Value> = events_of(&socket, &p.path, "session_started")
            .await
            .iter()
            .map(|e| e["data"]["source"].clone())
            .collect();
        let transcripts: Vec<Value> = events_of(&socket, &p.path, "session_started")
            .await
            .iter()
            .map(|e| e["data"]["transcript_path"].clone())
            .collect();
        let all = req(&socket, Method::GET, &format!("{}/events", p.path), None)
            .await
            .1;
        let other = &first[1 - i];
        let leaks = all.to_string().contains(other.as_str());
        let transcript = home
            .join("projects/-home-agent-work")
            .join(format!("{}.jsonl", first[i]));
        let answer = last_assistant_text(&transcript);
        let own = root.path().join(format!("state/agents/{}/claude", p.id));
        let history = std::fs::read_to_string(own.join("history.jsonl")).unwrap_or_default();
        let list = |d: &str| -> Vec<String> {
            std::fs::read_dir(own.join(d))
                .map(|r| {
                    r.filter_map(|e| e.ok())
                        .map(|e| e.file_name().to_string_lossy().into_owned())
                        .collect()
                })
                .unwrap_or_default()
        };
        let state: Value =
            serde_json::from_slice(&std::fs::read(own.join("claude.json")).unwrap_or_default())
                .unwrap_or(Value::Null);
        let screen = screen_of(&sup, p.id).await;
        println!(
            "--- {} screen after the resumed turn ---\n{screen}\n---",
            p.name
        );
        println!(
            "{}: sessions {ids:?}, sources {sources:?}, answer after resume {answer:?}",
            p.name
        );
        report.insert(
            p.name.into(),
            json!({
                "id": p.id.to_string(),
                "token": p.token,
                "session_ids": ids,
                "sources": sources,
                "transcript_paths": transcripts,
                "status_reasons": reasons,
                "events_mention_other_session": leaks,
                "answer_after_resume": answer,
                "history_lines": history.lines().count(),
                "history_has_own_token": history.contains(&p.token),
                "history_has_other_token": history.contains(&pairs[1 - i].token),
                "sessions_dir": list("sessions"),
                "backups_dir": list("backups"),
                "memory_dir": list("memory"),
                "state_num_startups": state["numStartups"],
                "state_keys": state.as_object().map(|o| o.keys().cloned().collect::<Vec<_>>()),
            }),
        );
    }
    if let Ok(path) = std::env::var("TERRARIUM_E2E_REPORT") {
        std::fs::write(&path, serde_json::to_vec_pretty(&report).unwrap()).unwrap();
    }

    for p in &pairs {
        let (s, _) = req(&socket, Method::POST, &format!("{}/stop", p.path), None).await;
        assert_eq!(s, 202);
    }
    for p in &pairs {
        wait_for(
            &format!("{} stopped", p.name),
            Duration::from_secs(30),
            async || status_of(&socket, &p.path).await == "stopped",
        )
        .await;
    }
    daemon.shutdown();

    // The checks, after the report is written.
    for (i, p) in pairs.iter().enumerate() {
        let r = &report[p.name];
        let ids = r["session_ids"].as_array().unwrap();
        assert!(
            ids.iter().all(|s| s == &json!(first[i])),
            "{}: the resume changed the session: {ids:?}",
            p.name
        );
        assert!(
            ids.len() >= 2,
            "{}: no session_started after the resume",
            p.name
        );
        assert_eq!(r["events_mention_other_session"], false, "{}", p.name);
        assert!(
            r["answer_after_resume"]
                .as_str()
                .unwrap()
                .contains(&p.token),
            "{}: the resumed session did not remember its reply: {}",
            p.name,
            r["answer_after_resume"]
        );
        assert_eq!(r["history_has_other_token"], false, "{}", p.name);
    }
}

/// Run futures at the same time and collect their outputs in order.
async fn futures_join<F>(fs: impl Iterator<Item = F>) -> Vec<F::Output>
where
    F: std::future::Future + Send + 'static,
    F::Output: Send + 'static,
{
    let handles: Vec<_> = fs.map(tokio::spawn).collect();
    let mut out = Vec::new();
    for h in handles {
        out.push(h.await.unwrap());
    }
    out
}
