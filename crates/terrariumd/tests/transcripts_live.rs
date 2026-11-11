//! A live check of the transcript endpoints against the session files of a
//! running stack, read-only. Ignored by default. Run it with a copy of the
//! stack's database (never the live file) and the stack's state dir:
//!
//! ```sh
//! sqlite3 -readonly state/terrarium.db ".backup /tmp/live.db"
//! TERRARIUM_LIVE_DB=/tmp/live.db TERRARIUM_LIVE_STATE=$PWD/state \
//! TERRARIUM_LIVE_HOMES=claude=$PWD/config/claude,codex=$PWD/config/codex,copilot=$PWD/config/copilot,pi=$PWD/config/pi \
//! TERRARIUM_LIVE_AGENTS=smoke/probe,smoke/probe-codex,smoke/probe-copilot \
//!   cargo test -p terrariumd --test transcripts_live -- --ignored --nocapture
//! ```
//!
//! It serves the API router over the copy, with the fake supervisor (no
//! agent starts), and prints the sessions and the block sequence of the
//! newest session with a transcript of each agent.
#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::path::PathBuf;
use std::sync::{Arc, RwLock};
use std::time::Instant;

use axum::body::Body;
use axum::http::Request;
use http_body_util::BodyExt;
use serde_json::Value;
use terrariumd::api::{AppState, router};
use terrariumd::db::Db;
use terrariumd::usage_collector::{UsageConfig, UsageRuntime, parse_homes};
use tower::ServiceExt;

async fn get(state: &Arc<AppState>, uri: &str) -> (u16, Value) {
    let resp = router(state.clone())
        .oneshot(Request::builder().uri(uri).body(Body::empty()).unwrap())
        .await
        .unwrap();
    let status = resp.status().as_u16();
    let bytes = resp.into_body().collect().await.unwrap().to_bytes();
    (
        status,
        serde_json::from_slice(&bytes).unwrap_or(Value::Null),
    )
}

fn line(b: &Value) -> String {
    let what = b["text"]
        .as_str()
        .map(str::to_owned)
        .or_else(|| b["tool"]["summary"].as_str().map(str::to_owned))
        .or_else(|| b["result"]["output"].as_str().map(str::to_owned))
        .unwrap_or_default()
        .replace('\n', " ");
    let what: String = what.chars().take(70).collect();
    let mut flags = Vec::new();
    for k in ["origin", "raw_type"] {
        if let Some(v) = b[k].as_str() {
            flags.push(format!("{k}={v}"));
        }
    }
    if b["redacted"] == true {
        flags.push("redacted".into());
    }
    if b["usage"].is_object() {
        flags.push("usage".into());
    }
    if b["result"]["is_error"] == true {
        flags.push("error".into());
    }
    format!(
        "  {:>3} {:<9} {:<11} {} {:?}",
        b["seq"],
        b["role"].as_str().unwrap_or(""),
        b["kind"].as_str().unwrap_or(""),
        flags.join(","),
        what
    )
}

#[tokio::test]
#[ignore = "reads a copy of a live stack's database and its state dir"]
async fn live_transcripts() {
    let (Ok(db), Ok(state_dir)) = (
        std::env::var("TERRARIUM_LIVE_DB"),
        std::env::var("TERRARIUM_LIVE_STATE"),
    ) else {
        eprintln!("set TERRARIUM_LIVE_DB and TERRARIUM_LIVE_STATE");
        return;
    };
    let conn = terrariumd::db::open(std::path::Path::new(&db)).unwrap();
    let homes = std::env::var("TERRARIUM_LIVE_HOMES")
        .map(|h| parse_homes(&h))
        .unwrap_or_default();
    let state = Arc::new(AppState {
        agent_calls: Default::default(),
        started: Instant::now(),
        started_at: chrono::Utc::now(),
        db: Db::new(conn, terrariumd::events::EventHub::default()),
        definitions: RwLock::new(Default::default()),
        supervisor: Arc::new(terrariumd::supervisor::InMemorySupervisor::new(
            std::time::Duration::from_millis(5),
        )),
        state_dir: PathBuf::from(state_dir),
        config_dir: PathBuf::from("/nonexistent"),
        work_dir: PathBuf::from("/nonexistent"),
        max_targets_per_project: terrariumd::settings::DEFAULT_MAX_TARGETS_PER_PROJECT,
        repos_dir: PathBuf::from("/nonexistent"),
        insecure_agent_header: false,
        usage: Arc::new(UsageRuntime::new(UsageConfig {
            homes,
            ..UsageConfig::default()
        })),
        executor: Arc::default(),
        harness_latest: Arc::default(),
        harness_tools: Arc::default(),
        agent_tools: Arc::default(),
        harness_auto_update: Arc::default(),
        self_update: Arc::default(),
        web: Default::default(),
        artifact_listen: String::new(),
        transcripts: Arc::default(),
        operator_auth: Arc::new(terrariumd::password::OperatorAuth::new(
            std::path::Path::new("/nonexistent"),
        )),
        waits: Arc::default(),
        config_repo: Arc::default(),
        lifecycle: Arc::default(),
        tool_rates: Arc::default(),
        lease_sweep: Arc::default(),
        rule_timers: Arc::default(),
        monitors: Arc::default(),
        webhooks: Arc::default(),
        pipelines: Arc::default(),
        git_proxy: Arc::default(),
        job_queue: Arc::default(),
        links: Arc::default(),
    });
    let agents = std::env::var("TERRARIUM_LIVE_AGENTS").unwrap_or_default();
    for key in agents.split(',').filter(|k| !k.is_empty()) {
        let enc = key.replace('/', "%2F");
        let (status, rows) = get(&state, &format!("/api/agents/{enc}/sessions?limit=5")).await;
        println!("== {key}: sessions {status}");
        for r in rows.as_array().into_iter().flatten() {
            println!(
                "  {} harness_session={} transcript={} blocks={} active={} title={:?}",
                r["id"].as_str().unwrap_or(""),
                r["harness_session_id"].as_str().unwrap_or("-"),
                r["transcript"],
                r["blocks"],
                r["active"],
                r["title"].as_str().unwrap_or("")
            );
        }
        let Some(s) = rows
            .as_array()
            .into_iter()
            .flatten()
            .find(|r| r["transcript"] == true)
        else {
            println!("  no session with a transcript");
            continue;
        };
        let (status, page) = get(
            &state,
            &format!(
                "/api/agents/{enc}/sessions/{}/transcript?limit=5000",
                s["id"].as_str().unwrap()
            ),
        )
        .await;
        let blocks = page["blocks"].as_array().cloned().unwrap_or_default();
        println!(
            "  transcript {status}: harness={} blocks={} complete={} next_after={}",
            page["harness"],
            blocks.len(),
            page["complete"],
            page["next_after"]
        );
        let unknown: Vec<&str> = blocks
            .iter()
            .filter(|b| b["text"].as_str().is_some_and(|t| t.starts_with('[')))
            .filter_map(|b| b["raw_type"].as_str())
            .collect();
        println!("  unmapped records: {unknown:?}");
        let from = blocks.len().saturating_sub(12);
        for b in &blocks[from..] {
            println!("{}", line(b));
        }
    }
}
