//! One real Claude Code run inside `bwrap`, through the whole daemon. Not in
//! `just ci::check`: it needs a logged-in Claude home and spends one model
//! turn. Run it with:
//!
//! ```sh
//! cargo build -p terrarium
//! TERRARIUM_E2E_CLAUDE_HOME=$HOME/.claude \
//!   cargo test -p terrariumd --test e2e_claude -- --ignored --nocapture
//! ```
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
use terrariumd::runner_supervisor::{RunnerSupervisor, RunnerSupervisorConfig};
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
    let sup = Arc::new(RunnerSupervisor::new(cfg));
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
            let screen = sup
                .runner(id)
                .map(|r| r.screen().text())
                .unwrap_or_default();
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
    println!("--- idle screen ---\n{}\n---", runner.screen().text());

    // Clear-input keys: type text without Enter, then Ctrl-U.
    runner.write(b"draft text").await.unwrap();
    tokio::time::sleep(Duration::from_millis(800)).await;
    let before = screen_state(&runner.screen().lines);
    println!("input before clear: {:?}", before.input_text);
    let keys = terrarium_core::Adapter::clear_input_keys(
        &terrarium_adapter_claude::ClaudeAdapter::default(),
    );
    runner.write(&keys).await.unwrap();
    tokio::time::sleep(Duration::from_millis(800)).await;
    let after = screen_state(&runner.screen().lines);
    println!(
        "input after clear ({keys:?}): {:?}, ready = {}",
        after.input_text, after.ready
    );
    assert!(after.ready, "{}", runner.screen().text());
    assert_eq!(
        status().await,
        "idle",
        "the clear keys must not end the agent"
    );

    // Resize: with `--new-session` the harness has no controlling
    // terminal. Does it still redraw at the new size?
    let width = |r: &terrarium_runner::Runner| {
        r.screen()
            .lines
            .iter()
            .filter(|l| l.starts_with('─'))
            .map(|l| l.chars().count())
            .max()
            .unwrap_or(0)
    };
    println!("rule width before resize: {}", width(&runner));
    runner.resize(40, 150).unwrap();
    tokio::time::sleep(Duration::from_millis(1500)).await;
    println!("rule width after resize to 150 cols: {}", width(&runner));

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
    let screen = runner.screen().text();
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
