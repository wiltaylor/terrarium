//! Subscriptions, quota probes, the quota policy, usage accounting, login
//! sessions, and API keys (PLAN §16). Probes answer from recorded fixtures
//! through a fake HTTP client; login and status commands are fake scripts.
#![allow(clippy::unwrap_used, clippy::expect_used)]

mod common;

use std::collections::HashMap;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use common::{TestDaemon, claude_project_dir, restamp_transcript, write};
use futures_util::{SinkExt, StreamExt};
use serde_json::{Value, json};
use terrarium_core::{AgentId, HarnessId};
use terrariumd::events::EventDraft;
use terrariumd::probes::{HttpGet, HttpRequest, HttpResponse};
use terrariumd::supervisor::{BoxFuture, InMemorySupervisor};
use terrariumd::usage_collector::UsageConfig;
use tokio_tungstenite::tungstenite::Message;

const ANTHROPIC: &str = "https://api.anthropic.com/api/oauth/usage";
const OPENAI: &str = "https://chatgpt.com/backend-api/wham/usage";

fn fixture(rel: &str) -> String {
    std::fs::read_to_string(format!(
        "{}/tests/fixtures/{rel}",
        env!("CARGO_MANIFEST_DIR")
    ))
    .unwrap()
}

/// Answers each URL from a map that the test can change.
#[derive(Default, Clone)]
struct FakeHttp {
    answers: Arc<Mutex<HashMap<String, (u16, String)>>>,
    calls: Arc<Mutex<Vec<String>>>,
}

impl FakeHttp {
    fn set(&self, url: &str, status: u16, body: &str) {
        self.answers
            .lock()
            .unwrap()
            .insert(url.into(), (status, body.into()));
    }
}

impl HttpGet for FakeHttp {
    fn get(&self, req: HttpRequest) -> BoxFuture<'_, Result<HttpResponse, String>> {
        self.calls.lock().unwrap().push(req.url.clone());
        let a = self.answers.lock().unwrap().get(&req.url).cloned();
        Box::pin(async move {
            a.map(|(status, body)| HttpResponse { status, body })
                .ok_or_else(|| "connection refused".into())
        })
    }
}

struct Env {
    td: TestDaemon,
    http: FakeHttp,
    homes: HashMap<HarnessId, PathBuf>,
}

fn script(bin: &Path, name: &str, body: &str) -> String {
    let p = bin.join(name);
    std::fs::write(&p, format!("#!/bin/sh\n{body}\n")).unwrap();
    std::fs::set_permissions(&p, std::fs::Permissions::from_mode(0o755)).unwrap();
    p.display().to_string()
}

/// A daemon with temp harness homes, fake harness programs, and the fake
/// HTTP client.
async fn boot(config: impl FnOnce(&Path), tune: impl FnOnce(&mut UsageConfig, &Path)) -> Env {
    let dir = tempfile::tempdir().unwrap();
    let cfg_dir = dir.path().join("config");
    std::fs::create_dir_all(&cfg_dir).unwrap();
    config(&cfg_dir);
    let state = dir.path().join("state");
    let socket = dir.path().join("api.sock");
    let bin = dir.path().join("bin");
    std::fs::create_dir_all(&bin).unwrap();
    let mut homes = HashMap::new();
    for h in HarnessId::ALL {
        let p = dir.path().join("auth").join(h.as_str());
        std::fs::create_dir_all(&p).unwrap();
        homes.insert(h, p);
    }
    let http = FakeHttp::default();
    let mut usage = UsageConfig {
        homes: homes.clone(),
        probe_interval: Duration::from_secs(3600),
        probe_tick: Duration::from_secs(3600),
        poll_interval: Duration::from_secs(3600),
        sample_throttle: Duration::ZERO,
        login_timeout: Duration::from_secs(20),
        http: Arc::new(http.clone()),
        ..UsageConfig::default()
    };
    // Status commands: nobody is logged in unless a test says so.
    usage.programs.insert(
        HarnessId::Claude,
        script(
            &bin,
            "claude",
            r#"echo '{"loggedIn": false, "authMethod": "none"}'; exit 1"#,
        ),
    );
    usage.programs.insert(
        HarnessId::Codex,
        script(&bin, "codex", "echo 'Not logged in'; exit 1"),
    );
    usage.programs.insert(
        HarnessId::Pi,
        script(
            &bin,
            "pi",
            r#"echo '{"status":"not_ready","provider":"x","reason":"credentials_not_configured"}'"#,
        ),
    );
    usage
        .programs
        .insert(HarnessId::Copilot, script(&bin, "copilot", "exit 1"));
    tune(&mut usage, &bin);
    let sup = Arc::new(InMemorySupervisor::new(common::DELAY));
    let daemon = terrariumd::start_with_usage(
        terrariumd::DaemonConfig {
            socket: socket.clone(),
            listen: None,
            token: None,
            state_dir: state.clone(),
            config_dir: cfg_dir.clone(),
            work_dir: state.clone().with_file_name("work"),
            max_targets_per_project: terrariumd::settings::DEFAULT_MAX_TARGETS_PER_PROJECT,
            repos_dir: std::path::PathBuf::from("/repos"),
            supervisor: sup.clone(),
            insecure_agent_header: true,
            artifact_listen: None,
            web: Default::default(),
        },
        usage,
    )
    .await
    .unwrap();
    Env {
        td: TestDaemon {
            daemon: Some(daemon),
            dir,
            socket,
            config: cfg_dir,
            state,
            sup,
            web: Default::default(),
            artifact_listen: None,
        },
        http,
        homes,
    }
}

/// The `login_sessions` row of an ended login has no URL and no device
/// code (`docs/security-review.md` F6).
async fn assert_login_row_clean(td: &TestDaemon, id: &str) {
    let key = id.to_owned();
    let (url, code): (Option<String>, Option<String>) = td
        .daemon
        .as_ref()
        .unwrap()
        .state
        .db
        .read(move |c| {
            Ok(c.query_row(
                "SELECT url, device_code FROM login_sessions WHERE id = ?1",
                [&key],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )?)
        })
        .await
        .unwrap();
    assert_eq!((url, code), (None, None), "login {id}");
}

fn claude_creds(home: &Path) {
    std::fs::write(
        home.join(".credentials.json"),
        r#"{"claudeAiOauth": {"accessToken": "tok", "refreshToken": "r", "expiresAt": 4102444800000,
            "subscriptionType": "max", "rateLimitTier": "default_claude_max_20x"}}"#,
    )
    .unwrap();
}

const MAX_SUB: &str = "provider = \"anthropic\"\nplan = \"Max 20x\"\nharnesses = [\"claude\", \"pi\"]\nprobe = \"anthropic-oauth-usage\"\nalert_at = 80\n";

async fn wait_for<F, Fut>(what: &str, mut f: F)
where
    F: FnMut() -> Fut,
    Fut: std::future::Future<Output = bool>,
{
    for _ in 0..400 {
        if f().await {
            return;
        }
        tokio::time::sleep(Duration::from_millis(25)).await;
    }
    panic!("timed out waiting for {what}");
}

async fn events_of(td: &TestDaemon, kind: &str) -> Vec<Value> {
    td.get(&format!("/api/events?kind={kind}"))
        .await
        .ok()
        .as_array()
        .unwrap()
        .clone()
}

/// Emit a harness event for an agent as the pump would.
async fn emit(td: &TestDaemon, agent: &Value, kind: &str, data: Value) {
    let (id, name, project) = (
        agent["id"].as_str().unwrap().to_owned(),
        agent["name"].as_str().unwrap().to_owned(),
        agent["project"].as_str().unwrap().to_owned(),
    );
    let kind = kind.to_owned();
    td.daemon
        .as_ref()
        .unwrap()
        .state
        .db
        .write("system", move |t| {
            t.emit(
                EventDraft::new(kind, "test", data)
                    .agent(id, name)
                    .project(project),
            )
        })
        .await
        .unwrap();
}

#[tokio::test]
async fn file_subscriptions_list_and_probe() {
    let env = boot(
        |c| write(c, "subscriptions/anthropic-max.toml", MAX_SUB),
        |_, _| {},
    )
    .await;
    let td = &env.td;
    claude_creds(&env.homes[&HarnessId::Claude]);

    let list = td.get("/api/subscriptions").await;
    let s = &list.ok()[0];
    assert_eq!(s["name"], "anthropic-max");
    assert_eq!(s["source"], "file");
    assert_eq!(s["probe_status"], "none");
    assert_eq!(s["local_estimate_only"], false);
    assert_eq!(s["windows"], json!([]));

    env.http.set(
        ANTHROPIC,
        200,
        &fixture("probes/anthropic_oauth_usage.json"),
    );
    let out = td
        .post_empty("/api/subscriptions/anthropic-max/probe")
        .await;
    assert_eq!(out.ok()["probe_status"], "ok");
    assert_eq!(out.body["windows"].as_array().unwrap().len(), 2);

    let s = td.get("/api/subscriptions/anthropic-max").await;
    let s = s.ok();
    assert_eq!(s["probe_status"], "ok");
    let ok_at = s["last_success_at"].as_str().unwrap().to_owned();
    let w = s["windows"].as_array().unwrap();
    assert_eq!(w[0]["window"], "5h");
    assert_eq!(w[0]["used_pct"], 16.0);
    assert_eq!(w[0]["resets_at"], "2026-10-24T10:59:59Z");
    assert_eq!(w[1]["window"], "7d");

    let q = td.get("/api/subscriptions/anthropic-max/quota").await;
    assert_eq!(q.ok().as_array().unwrap().len(), 2);
    assert_eq!(q.body[0]["raw"]["utilization"], 16.0);
    let q = td
        .get("/api/subscriptions/anthropic-max/quota?window=7d")
        .await;
    assert_eq!(q.ok().as_array().unwrap().len(), 1);
    td.get("/api/subscriptions/anthropic-max/quota?since=yesterday")
        .await
        .err(400, "bad_request");
    assert_eq!(events_of(td, "quota.sample").await.len(), 1);

    // A failure keeps the last success time (PLAN §21).
    env.http.set(ANTHROPIC, 401, "{}");
    let out = td
        .post_empty("/api/subscriptions/anthropic-max/probe")
        .await;
    assert_eq!(out.ok()["probe_status"], "failed");
    assert!(out.body["error"].as_str().unwrap().contains("401"));
    let s = td.get("/api/subscriptions/anthropic-max").await;
    assert_eq!(s.body["probe_status"], "failed");
    assert_eq!(s.body["last_success_at"], ok_at.as_str());
    // The windows of the last success stay on the tile.
    assert_eq!(s.body["windows"].as_array().unwrap().len(), 2);

    // No credential: a clear failure, no request. The start-up migration
    // may have moved the shared login into the account (PLAN §16.3).
    let state = td.daemon.as_ref().unwrap().state.state_dir.clone();
    let account = terrariumd::accounts::credential_path(
        &terrariumd::accounts::store_dir(&state, "anthropic-max"),
        HarnessId::Claude,
    );
    let shared = env.homes[&HarnessId::Claude].join(".credentials.json");
    assert!(account.exists() || shared.exists());
    let _ = std::fs::remove_file(account);
    let _ = std::fs::remove_file(shared);
    let n = env.http.calls.lock().unwrap().len();
    let out = td
        .post_empty("/api/subscriptions/anthropic-max/probe")
        .await;
    assert!(
        out.body["error"]
            .as_str()
            .unwrap()
            .contains("no credential"),
        "{}",
        out.body
    );
    assert_eq!(env.http.calls.lock().unwrap().len(), n);
    td.get("/api/subscriptions/nope")
        .await
        .err(404, "not_found");
}

#[tokio::test]
async fn create_delete_and_permissions() {
    let env = boot(|_| {}, |_, _| {}).await;
    let td = &env.td;
    td.project("p").await;
    let member = td.agent("p", "worker", json!({})).await;
    let lead = td
        .agent("p", "lead", json!({"roles": ["project_admin"]}))
        .await;
    let (m, l) = (member["id"].as_str().unwrap(), lead["id"].as_str().unwrap());

    let body = json!({"name": "openai-pro", "provider": "openai", "harnesses": ["codex"], "probe": "codex-rate-limits"});
    td.post_as(l, "/api/subscriptions", body.clone())
        .await
        .err(403, "forbidden");
    let c = td.post("/api/subscriptions", body.clone()).await;
    assert_eq!(c.status.as_u16(), 201);
    assert_eq!(c.body["source"], "api");
    td.post("/api/subscriptions", body)
        .await
        .err(409, "conflict");
    td.post(
        "/api/subscriptions",
        json!({"name": "x", "provider": "openai", "probe": "nope"}),
    )
    .await
    .err(422, "invalid");
    td.post(
        "/api/subscriptions",
        json!({"name": "login", "provider": "openai"}),
    )
    .await
    .err(422, "invalid");
    td.post(
        "/api/subscriptions",
        json!({"name": "y", "provider": "openai", "extra": 1}),
    )
    .await
    .err(400, "bad_request");

    // Members may not list all plans; a project_admin may.
    td.get_as(m, "/api/subscriptions")
        .await
        .err(403, "forbidden");
    assert_eq!(
        td.get_as(l, "/api/subscriptions")
            .await
            .ok()
            .as_array()
            .unwrap()
            .len(),
        1
    );
    // Login, keys, and logout are operator-only.
    td.post_as(l, "/api/subscriptions/login", json!({"kind": "codex"}))
        .await
        .err(403, "forbidden");
    td.post_as(
        l,
        "/api/subscriptions/openai-pro/api-key",
        json!({"provider": "openai", "key": "sk"}),
    )
    .await
    .err(403, "forbidden");
    td.post_empty_as(l, "/api/subscriptions/openai-pro/logout")
        .await
        .err(403, "forbidden");
    td.post_empty_as(m, "/api/subscriptions/openai-pro/probe")
        .await
        .err(403, "forbidden");

    assert_eq!(
        td.delete("/api/subscriptions/openai-pro")
            .await
            .status
            .as_u16(),
        204
    );
    td.get("/api/subscriptions/openai-pro")
        .await
        .err(404, "not_found");
}

#[tokio::test]
async fn file_subscription_cannot_be_deleted_and_survives_reload() {
    let env = boot(
        |c| write(c, "subscriptions/anthropic-max.toml", MAX_SUB),
        |_, _| {},
    )
    .await;
    let td = &env.td;
    td.delete("/api/subscriptions/anthropic-max")
        .await
        .err(409, "file_managed");
    std::fs::remove_file(td.config.join("subscriptions/anthropic-max.toml")).unwrap();
    td.post_empty("/api/reload").await.ok();
    let s = td.get("/api/subscriptions/anthropic-max").await;
    assert_eq!(s.ok()["source"], "api");
}

#[tokio::test]
async fn quota_policy_alerts_holds_and_resumes() {
    let env = boot(
        |c| write(c, "subscriptions/anthropic-max.toml", MAX_SUB),
        |_, _| {},
    )
    .await;
    let td = &env.td;
    claude_creds(&env.homes[&HarnessId::Claude]);
    td.project("p").await;
    let a = td
        .agent("p", "worker", json!({"subscription": "anthropic-max"}))
        .await;
    let other = td.agent("p", "other", json!({})).await;
    let id = a["id"].as_str().unwrap().to_owned();
    td.post_empty(&format!("/api/agents/{id}/start")).await.ok();
    td.wait_status(&id, "idle").await;

    // 85 percent: an alert, no hold.
    let soon = (chrono::Utc::now() + chrono::Duration::hours(2)).to_rfc3339();
    let at = |pct: f64| {
        json!({"five_hour": {"utilization": pct, "resets_at": soon},
               "seven_day": {"utilization": 30.0, "resets_at": "2099-01-01T00:00:00Z"}})
        .to_string()
    };
    env.http.set(ANTHROPIC, 200, &at(85.0));
    td.post_empty("/api/subscriptions/anthropic-max/probe")
        .await
        .ok();
    let alerts = events_of(td, "quota.alert").await;
    assert_eq!(alerts.len(), 1, "{alerts:?}");
    assert_eq!(alerts[0]["data"]["window"], "5h");
    assert_eq!(alerts[0]["data"]["level"], "alert");
    assert_eq!(alerts[0]["actor"], "system");
    let mail = td.get("/api/mail?to=operator").await;
    let mail = mail.ok().as_array().unwrap().clone();
    let alert = mail
        .iter()
        .find(|m| m["subject"].as_str().unwrap().contains("5h window at 85%"))
        .unwrap();
    // The daemon speaks as `system`.
    assert_eq!(alert["from"], "system");
    assert_eq!(alert["from_kind"], "system");
    // The mail links the dashboard (TERRARIUM_PUBLIC_URL, by default
    // http://localhost:8080).
    let body = alert["body"].as_str().unwrap();
    assert!(
        body.contains("Open: http://localhost:8080/subscriptions"),
        "{body}"
    );
    // The same window period alerts once.
    td.post_empty("/api/subscriptions/anthropic-max/probe")
        .await
        .ok();
    assert_eq!(events_of(td, "quota.alert").await.len(), 1);
    assert_eq!(
        td.get(&format!("/api/agents/{id}")).await.body["status"],
        "idle"
    );

    // 100 percent: exhausted alert, the agent is blocked, turns wait.
    env.http.set(ANTHROPIC, 200, &at(100.0));
    td.post_empty("/api/subscriptions/anthropic-max/probe")
        .await
        .ok();
    td.wait_status(&id, "blocked").await;
    let blocked = events_of(td, "quota.blocked").await;
    assert_eq!(blocked.len(), 1);
    assert_eq!(blocked[0]["agent_id"], id.as_str());
    assert_eq!(blocked[0]["data"]["enforced"], true);
    assert!(blocked[0]["data"]["until"].as_str().is_some());
    assert_eq!(events_of(td, "quota.alert").await.len(), 2);
    let s = td.get("/api/subscriptions/anthropic-max").await;
    assert_eq!(s.body["held_agents"], 1);
    // The agent on another subscription runs on.
    assert!(
        td.get_as(other["id"].as_str().unwrap(), "/api/usage/me")
            .await
            .ok()["holds"]
            .as_array()
            .unwrap()
            .is_empty()
    );
    let me = td.get_as(&id, "/api/usage/me").await;
    assert_eq!(me.ok()["holds"][0]["cause"], "quota:anthropic-max");

    td.post(&format!("/api/agents/{id}/send"), json!({"text": "later"}))
        .await
        .ok();
    let agent_id: AgentId = id.parse().unwrap();
    assert_eq!(td.sup.waiting_turns(agent_id), ["[User] later"]);

    // After the reset the probe reads 10 percent: the hold ends and the
    // waiting turn goes out.
    env.http.set(ANTHROPIC, 200, &at(10.0));
    td.post_empty("/api/subscriptions/anthropic-max/probe")
        .await
        .ok();
    assert_eq!(events_of(td, "quota.resumed").await.len(), 1);
    assert!(td.sup.waiting_turns(agent_id).is_empty());
    wait_for("the waiting turn", || async {
        events_of(td, "turn_start")
            .await
            .iter()
            .any(|e| e["data"]["prompt"] == "[User] later")
    })
    .await;
    td.wait_status(&id, "idle").await;
    assert_eq!(
        td.get("/api/subscriptions/anthropic-max").await.body["held_agents"],
        0
    );
}

#[tokio::test]
async fn holds_end_at_their_reset_time() {
    let env = boot(
        |c| write(c, "subscriptions/anthropic-max.toml", MAX_SUB),
        |_, _| {},
    )
    .await;
    let td = &env.td;
    td.project("p").await;
    let a = td
        .agent("p", "worker", json!({"subscription": "anthropic-max"}))
        .await;
    let id = a["id"].as_str().unwrap().to_owned();
    td.post_empty(&format!("/api/agents/{id}/start")).await.ok();
    td.wait_status(&id, "idle").await;
    let state = td.daemon.as_ref().unwrap().state.clone();
    let past = chrono::Utc::now() - chrono::Duration::seconds(1);
    terrariumd::quota_policy::hold(&state, &id, "quota:anthropic-max", Some(past), "test")
        .await
        .unwrap();
    td.wait_status(&id, "blocked").await;
    assert_eq!(
        terrariumd::quota_policy::release_expired(&state)
            .await
            .unwrap(),
        1
    );
    td.wait_status(&id, "idle").await;
    assert_eq!(
        terrariumd::quota_policy::release_expired(&state)
            .await
            .unwrap(),
        0
    );
}

const SESSION: &str = "/../adapters/claude/tests/fixtures/session.jsonl";

/// The Claude session fixture, re-stamped to now (see `restamp_transcript`).
fn session_now() -> String {
    let path = format!("{}{SESSION}", env!("CARGO_MANIFEST_DIR"));
    restamp_transcript(&std::fs::read_to_string(path).unwrap())
}

#[tokio::test]
async fn collector_tails_a_growing_transcript() {
    let env = boot(
        |c| {
            write(c, "subscriptions/anthropic-max.toml", MAX_SUB);
            write(
                c,
                "projects/p.toml",
                "[[budget]]\nsubscription = \"anthropic-max\"\ntokens_per_day = 60\nalert_at = 50\n",
            );
        },
        |_, _| {},
    )
    .await;
    let td = &env.td;
    let a = td
        .agent(
            "p",
            "worker",
            json!({"subscription": "anthropic-max", "model": "haiku"}),
        )
        .await;
    let id = a["id"].as_str().unwrap().to_owned();
    // The harness reports a sandbox path; the file is in the agent's own
    // project dir (the per-agent overlay), not in the shared home.
    let file = claude_project_dir(&td.state, &id).join("s1.jsonl");
    let text = session_now();
    let lines: Vec<&str> = text.lines().collect();
    let cut = lines
        .iter()
        .position(|l| l.contains("msg_011CgLZ4D8Uzva9rUiX7ME9A"))
        .unwrap()
        + 1;
    std::fs::write(&file, lines[..cut].join("\n") + "\n").unwrap();
    emit(td, &a, "turn_start", json!({"prompt": "hi"})).await;
    emit(
        td,
        &a,
        "session_started",
        json!({"harness_session": "s1",
               "transcript_path": "/home/agent/.claude/projects/-home-agent-work/s1.jsonl",
               "source": "startup"}),
    )
    .await;
    wait_for("the first sample", || async {
        let r = td.get(&format!("/api/usage?agent={id}&bucket=day")).await;
        r.body
            .as_array()
            .is_some_and(|a| a.len() == 1 && a[0]["messages"] == 1)
    })
    .await;
    let r = td.get(&format!("/api/usage?agent={id}&bucket=day")).await;
    assert_eq!(r.body[0]["input_tokens"], 2);
    assert_eq!(r.body[0]["output_tokens"], 45);
    assert_eq!(r.body[0]["cache_write"], 36712);

    // The file grows; the turn ends.
    std::fs::write(&file, text.clone()).unwrap();
    emit(td, &a, "turn_end", json!({})).await;
    wait_for("the second sample", || async {
        let r = td.get(&format!("/api/usage?agent={id}&bucket=day")).await;
        r.body
            .as_array()
            .is_some_and(|a| a.len() == 1 && a[0]["messages"] == 2)
    })
    .await;
    let r = td
        .get(&format!("/api/usage?agent={id}&bucket=day&group=model"))
        .await;
    assert_eq!(r.body[0]["key"], "claude-haiku-5-5");
    assert_eq!(r.body[0]["input_tokens"], 4);
    assert_eq!(r.body[0]["cache_read"], 36712);
    assert_eq!(r.body[0]["turns"], 1);

    // A resume reads the file from the top: nothing counts twice.
    emit(
        td,
        &a,
        "session_started",
        json!({"harness_session": "s1", "transcript_path": file, "source": "resume"}),
    )
    .await;
    emit(td, &a, "exit", json!({"code": 0})).await;
    tokio::time::sleep(Duration::from_millis(200)).await;
    let r = td.get(&format!("/api/usage?agent={id}&bucket=day")).await;
    assert_eq!(r.body[0]["messages"], 2);

    let samples = events_of(td, "usage.sample").await;
    let total: u64 = samples
        .iter()
        .map(|e| e["data"]["input_tokens"].as_u64().unwrap())
        .sum();
    assert_eq!(total, 4);
    assert_eq!(samples[0]["data"]["subscription"], "anthropic-max");

    // 59 tokens of a 60 budget: an alert at 50 percent, no hold yet.
    let alerts = events_of(td, "quota.alert").await;
    assert_eq!(alerts.len(), 1, "{alerts:?}");
    assert_eq!(alerts[0]["data"]["window"], "tokens_per_day");
    assert_eq!(alerts[0]["project"], "p");
    let mail = td.get("/api/mail?to=operator").await;
    let budget = mail
        .ok()
        .as_array()
        .unwrap()
        .iter()
        .find(|m| m["subject"].as_str().unwrap().starts_with("Budget:"))
        .cloned()
        .unwrap();
    assert!(
        budget["body"]
            .as_str()
            .unwrap()
            .contains("Open: http://localhost:8080/p/p\n"),
        "the budget alert links the project page, not the old spend page: {budget}"
    );

    // The per-agent split and my share.
    let p = td.get_as(&id, "/api/usage/project").await;
    let p = p.ok();
    assert_eq!(p["project"], "p");
    assert_eq!(p["total"]["messages"], 2);
    assert_eq!(p["agents"][0]["name"], "worker");
    assert_eq!(p["agents"][0]["share_pct"], 100.0);
    assert_eq!(p["budgets"][0]["tokens_per_day"], 60);
    assert_eq!(p["budgets"][0]["used_tokens"], 59);
    let me = td.get_as(&id, "/api/usage/me").await;
    let me = me.ok();
    assert_eq!(me["agent"]["name"], "worker");
    assert_eq!(me["subscription"]["name"], "anthropic-max");
    assert_eq!(me["today"]["mine"]["input_tokens"], 4);
    assert_eq!(me["today"]["share_pct"], 100.0);
    td.get("/api/usage/me").await.err(400, "bad_request");
    assert_eq!(
        td.get(&format!("/api/usage/me?agent={id}")).await.ok()["agent"]["id"],
        id.as_str()
    );
    td.get("/api/usage/project").await.err(400, "bad_request");
}

#[tokio::test]
async fn budgets_hold_a_project() {
    let env = boot(
        |c| write(c, "projects/p.toml", "[budget]\ntokens_per_day = 10\n"),
        |_, _| {},
    )
    .await;
    let td = &env.td;
    let a = td.agent("p", "worker", json!({})).await;
    let id = a["id"].as_str().unwrap().to_owned();
    td.post_empty(&format!("/api/agents/{id}/start")).await.ok();
    td.wait_status(&id, "idle").await;
    let file = claude_project_dir(&td.state, &id).join("s.jsonl");
    std::fs::write(&file, session_now()).unwrap();
    emit(
        td,
        &a,
        "session_started",
        json!({"harness_session": "s", "transcript_path": file}),
    )
    .await;
    td.wait_status(&id, "blocked").await;
    let b = events_of(td, "quota.blocked").await;
    assert_eq!(b[0]["data"]["cause"], "budget:project:p");
    let levels: Vec<Value> = events_of(td, "quota.alert")
        .await
        .iter()
        .map(|e| e["data"]["level"].clone())
        .collect();
    assert_eq!(levels, [json!("alert"), json!("exhausted")]);
}

#[tokio::test]
async fn usage_scope_for_agents() {
    let env = boot(|_| {}, |_, _| {}).await;
    let td = &env.td;
    td.project("p").await;
    td.project("q").await;
    let a = td.agent("p", "worker", json!({})).await;
    td.agent("q", "spy", json!({})).await;
    let id = a["id"].as_str().unwrap();
    assert_eq!(td.get_as(id, "/api/usage").await.ok(), &json!([]));
    td.get_as(id, "/api/usage?project=q")
        .await
        .err(403, "forbidden");
    td.get_as(id, "/api/usage?agent=q%2Fspy")
        .await
        .err(403, "forbidden");
    td.get("/api/usage?bucket=week")
        .await
        .err(400, "bad_request");
    td.get("/api/usage?since=2026-10-24T00:00:00Z&bucket=hour&group=agent")
        .await
        .ok();
    td.get_as(id, "/api/usage/project?project=q")
        .await
        .err(403, "forbidden");
}

#[tokio::test]
async fn self_report_is_an_event_only() {
    let env = boot(|_| {}, |_, _| {}).await;
    let td = &env.td;
    td.project("p").await;
    let a = td.agent("p", "worker", json!({})).await;
    let b = td.agent("p", "peer", json!({})).await;
    let (id, peer) = (a["id"].as_str().unwrap(), b["id"].as_str().unwrap());
    let r = td
        .post_as(
            id,
            &format!("/api/agents/{id}/status"),
            json!({"status": "working", "note": "on TER-1"}),
        )
        .await;
    assert_eq!(r.ok()["kind"], "self_report");
    assert_eq!(r.body["data"]["note"], "on TER-1");
    // The real status does not change.
    assert_eq!(
        td.get(&format!("/api/agents/{id}")).await.body["status"],
        "stopped"
    );
    td.post_as(
        peer,
        &format!("/api/agents/{id}/status"),
        json!({"status": "idle"}),
    )
    .await
    .err(403, "forbidden");
    td.post_as(
        id,
        &format!("/api/agents/{id}/status"),
        json!({"status": "error"}),
    )
    .await
    .err(422, "invalid");
    td.post_as(
        id,
        &format!("/api/agents/{id}/status"),
        json!({"status": "idle", "x": 1}),
    )
    .await
    .err(400, "bad_request");
    assert_eq!(events_of(td, "self_report").await.len(), 1);
}

#[tokio::test]
async fn api_keys_are_write_only() {
    let env = boot(|_| {}, |_, _| {}).await;
    let td = &env.td;
    let secret = "sk-very-secret-value";
    td.post(
        "/api/subscriptions/local/api-key",
        json!({"provider": "openai-compatible", "key": secret}),
    )
    .await
    .err(422, "invalid");
    td.post(
        "/api/subscriptions/local/api-key",
        json!({"provider": "nope", "key": secret}),
    )
    .await
    .err(422, "invalid");
    let r = td
        .post(
            "/api/subscriptions/local/api-key",
            json!({"provider": "openai-compatible", "key": secret,
                   "base_url": "https://llm.example/v1", "models": ["m1"]}),
        )
        .await;
    let body = r.ok().to_string();
    assert!(!body.contains(secret), "{body}");
    assert_eq!(r.body["env_var"], "OPENAI_API_KEY");
    assert_eq!(r.body["has_api_key"], true);
    assert_eq!(r.body["login_status"], "logged_in");
    assert_eq!(r.body["source"], "api");
    for path in [
        "/api/subscriptions",
        "/api/subscriptions/local",
        "/api/audit",
        "/api/events",
    ] {
        let text = td.get(path).await.body.to_string();
        assert!(!text.contains(secret), "{path}: {text}");
    }
    let file = td.state.join("credentials/subscriptions/local.json");
    let mode = std::fs::metadata(&file).unwrap().permissions().mode() & 0o777;
    assert_eq!(mode, 0o600);
    let sub = td
        .daemon
        .as_ref()
        .unwrap()
        .state
        .db
        .read(|c| terrariumd::store::subscriptions::get(c, "local"))
        .await
        .unwrap();
    let env_vars = terrariumd::login_session::credential_env_for(&td.state, &sub);
    assert_eq!(env_vars[0], ("OPENAI_API_KEY".into(), secret.into()));
    assert_eq!(env_vars[1].0, "OPENAI_BASE_URL");
    // Verify trusts the stored key.
    let v = td.post_empty("/api/subscriptions/local/verify").await;
    assert_eq!(v.ok()["status"]["logged_in"], true);
}

#[tokio::test]
async fn zai_key_is_a_claude_and_pi_subscription_with_no_probe() {
    let env = boot(|_| {}, |_, _| {}).await;
    let td = &env.td;
    let secret = "zk-very-secret-value";
    // A plan that Z.ai does not have, and a plan for another provider.
    td.post(
        "/api/subscriptions/glm/api-key",
        json!({"provider": "zai", "plan": "Max 20x", "key": secret}),
    )
    .await
    .err(422, "invalid");
    td.post(
        "/api/subscriptions/or/api-key",
        json!({"provider": "openrouter", "plan": "api", "key": secret}),
    )
    .await
    .err(422, "invalid");
    td.post(
        "/api/subscriptions",
        json!({"name": "glm-def", "provider": "zai", "plan": "pro"}),
    )
    .await
    .err(422, "invalid");
    let r = td
        .post(
            "/api/subscriptions/glm/api-key",
            json!({"provider": "zai", "plan": "api", "key": secret}),
        )
        .await;
    let body = r.ok().to_string();
    assert!(!body.contains(secret), "{body}");
    assert_eq!(r.body["provider"], "zai");
    assert_eq!(r.body["plan"], "api");
    assert_eq!(r.body["harnesses"], json!(["claude", "pi"]));
    assert_eq!(r.body["env_var"], "ZAI_API_KEY");
    assert_eq!(r.body["login_status"], "api_key");
    assert_eq!(r.body["local_estimate_only"], true);
    assert_eq!(r.body["has_api_key"], true);
    assert!(r.body.get("probe").is_none(), "{body}");
    // No plan: the GLM Coding Plan.
    let r = td
        .post(
            "/api/subscriptions/glm2/api-key",
            json!({"provider": "zai", "key": secret}),
        )
        .await;
    assert_eq!(r.ok()["plan"], "coding");
    // The model that each harness gets when the agent sets none: pi's
    // built-in `zai` provider for the coding plan, the custom `zai-api`
    // for the api plan, and Claude Code's mapped tier.
    assert_eq!(
        r.body["default_models"],
        json!({"pi": "glm-5.3-flash", "claude": "glm-5.3"})
    );
    let list = td.get("/api/subscriptions").await;
    let glm = list
        .ok()
        .as_array()
        .unwrap()
        .iter()
        .find(|s| s["name"] == "glm")
        .unwrap();
    assert_eq!(glm["default_models"]["pi"], "glm-5.3-flash");
    // The agent page shows the subscription without a second request.
    td.project("p").await;
    let a = td
        .agent("p", "est", json!({"harness": "pi", "subscription": "glm2"}))
        .await;
    let shown = td
        .get(&format!("/api/agents/{}", a["id"].as_str().unwrap()))
        .await;
    assert_eq!(
        shown.ok()["subscription_detail"],
        json!({"name": "glm2", "provider": "zai", "plan": "coding", "login_status": "api_key",
               "harnesses": ["claude", "pi"], "default_model": "glm-5.3-flash"})
    );
    let none = td.agent("p", "plain", json!({"harness": "pi"})).await;
    let shown = td
        .get(&format!("/api/agents/{}", none["id"].as_str().unwrap()))
        .await;
    assert!(shown.ok().get("subscription_detail").is_none());
    let sub = td
        .daemon
        .as_ref()
        .unwrap()
        .state
        .db
        .read(|c| terrariumd::store::subscriptions::get(c, "glm"))
        .await
        .unwrap();
    let vars: std::collections::BTreeMap<String, String> =
        terrariumd::login_session::credential_env_for(&td.state, &sub)
            .into_iter()
            .collect();
    assert_eq!(vars["ZAI_API_KEY"], secret);
    assert_eq!(vars["ANTHROPIC_AUTH_TOKEN"], secret);
    assert_eq!(vars["ANTHROPIC_BASE_URL"], "https://api.z.ai/api/anthropic");
    assert!(!vars.contains_key("ANTHROPIC_API_KEY"));
    // Verify does not ask Claude Code; logout would end the shared login.
    let v = td.post_empty("/api/subscriptions/glm/verify").await;
    assert_eq!(v.ok()["subscription"]["login_status"], "api_key");
    td.post_empty("/api/subscriptions/glm/logout")
        .await
        .err(422, "invalid");
    for path in ["/api/subscriptions", "/api/audit", "/api/events"] {
        let text = td.get(path).await.body.to_string();
        assert!(!text.contains(secret), "{path}: {text}");
    }
}

#[tokio::test]
async fn copilot_probe_takes_a_stored_pat() {
    let env = boot(|_| {}, |_, _| {}).await;
    let td = &env.td;
    env.http.set(
        "https://api.github.com/copilot_internal/user",
        200,
        &fixture("probes/github_copilot_user.json"),
    );
    let r = td
        .post(
            "/api/subscriptions/copilot-pro/api-key",
            json!({"provider": "github", "key": "github_pat_x"}),
        )
        .await;
    assert_eq!(r.ok()["probe"], "github-copilot-quota");
    let p = td.post_empty("/api/subscriptions/copilot-pro/probe").await;
    assert_eq!(p.ok()["probe_status"], "ok");
    assert_eq!(p.body["windows"][0]["window"], "monthly");
    assert_eq!(p.body["windows"][0]["used_pct"], 22.7);
}

#[tokio::test]
async fn detect_lists_logins_without_a_definition() {
    let env = boot(
        |_| {},
        |u, bin| {
            u.programs.insert(
                HarnessId::Claude,
                script(
                    bin,
                    "claude-in",
                    r#"echo '{"loggedIn": true, "authMethod": "claude.ai", "subscriptionType": "max"}'"#,
                ),
            );
        },
    )
    .await;
    let td = &env.td;
    claude_creds(&env.homes[&HarnessId::Claude]);
    let d = td.get("/api/subscriptions/detect").await;
    let d = d.ok().as_array().unwrap().clone();
    assert_eq!(d.len(), 1, "{d:?}");
    assert_eq!(d[0]["kind"], "claude");
    assert_eq!(d[0]["plan"], "Max 20x");
    assert_eq!(d[0]["suggested"]["name"], "anthropic-max-20x");
    assert_eq!(d[0]["suggested"]["probe"], "anthropic-oauth-usage");
    // With a definition that covers the login, nothing is offered.
    td.post(
        "/api/subscriptions",
        json!({"name": "anthropic-max", "provider": "anthropic", "harnesses": ["claude"]}),
    )
    .await
    .ok();
    assert_eq!(td.get("/api/subscriptions/detect").await.ok(), &json!([]));
    // Verify records the login state.
    let v = td
        .post_empty("/api/subscriptions/anthropic-max/verify")
        .await;
    assert_eq!(v.ok()["subscription"]["login_status"], "logged_in");
    assert_eq!(v.body["subscription"]["plan"], "Max 20x");
}

/// The real outputs of a container login: `codex login status` prints its
/// line to stderr only, and Copilot's `config.json` is JSON with comment
/// lines that keeps the token in `authTokens`. Both are logins.
#[tokio::test]
async fn detect_reads_the_real_codex_and_copilot_outputs() {
    let fixture = |rel: &str| {
        std::fs::read_to_string(format!("{}/../adapters/{rel}", env!("CARGO_MANIFEST_DIR")))
            .unwrap()
    };
    let codex: serde_json::Value =
        serde_json::from_str(&fixture("codex/tests/fixtures/auth/login-status.json")).unwrap();
    let stderr = codex["logged_in"]["stderr"]
        .as_str()
        .unwrap()
        .trim()
        .to_owned();
    assert_eq!(codex["logged_in"]["stdout"], "");
    let env = boot(
        |_| {},
        |u, bin| {
            u.programs.insert(
                HarnessId::Codex,
                script(
                    bin,
                    "codex-real",
                    &format!(r#"[ "$1 $2" = "login status" ] && echo '{stderr}' >&2"#),
                ),
            );
        },
    )
    .await;
    let td = &env.td;
    let claims = base64_url(
        &json!({"https://api.openai.com/auth": {"chatgpt_plan_type": "pro"}}).to_string(),
    );
    std::fs::write(
        env.homes[&HarnessId::Codex].join("auth.json"),
        json!({"tokens": {"id_token": format!("h.{claims}.s")}}).to_string(),
    )
    .unwrap();
    std::fs::write(
        env.homes[&HarnessId::Copilot].join("config.json"),
        fixture("copilot/tests/fixtures/auth/config-plaintext-login.json"),
    )
    .unwrap();
    let d = td.get("/api/subscriptions/detect").await;
    let d = d.ok().as_array().unwrap().clone();
    let kinds: Vec<_> = d.iter().map(|x| x["kind"].as_str().unwrap()).collect();
    assert_eq!(kinds, ["codex", "copilot"], "{d:?}");
    assert_eq!(d[0]["detail"], "Logged in using ChatGPT");
    assert_eq!(d[0]["suggested"]["name"], "openai-pro");
    assert_eq!(d[1]["suggested"]["name"], "github-copilot");
    assert!(
        d[1]["detail"].as_str().unwrap().contains("for octo"),
        "{d:?}"
    );
    assert!(!d[1].to_string().contains("gho_"), "no token in the answer");
}

/// Every file under `dir`, recursively.
fn all_files(dir: &std::path::Path) -> Vec<std::path::PathBuf> {
    let mut out = Vec::new();
    for e in std::fs::read_dir(dir).into_iter().flatten().flatten() {
        let p = e.path();
        if p.is_dir() {
            out.extend(all_files(&p));
        } else {
            out.push(p);
        }
    }
    out
}

/// F6 of `docs/security-review.md`: a live login session keeps its output
/// in memory only. Its temp dir has no `pty.log`, and no file in it holds
/// the device code.
fn assert_no_login_output_on_disk(id: &str, code: &str) {
    let dir = terrariumd::login_session::login_dir(id);
    assert!(dir.is_dir(), "the live session has its dir");
    for f in all_files(&dir) {
        assert!(
            !f.file_name()
                .unwrap()
                .to_string_lossy()
                .starts_with("pty.log"),
            "{}",
            f.display()
        );
        let bytes = std::fs::read(&f).unwrap_or_default();
        assert!(
            !String::from_utf8_lossy(&bytes).contains(code),
            "{} holds the device code",
            f.display()
        );
    }
}

/// The temp dir of an ended login session is gone.
fn assert_login_dir_gone(id: &str) {
    let dir = terrariumd::login_session::login_dir(id);
    assert!(!dir.exists(), "{} is left behind", dir.display());
}

async fn wait_login(td: &TestDaemon, id: &str, state: &str) -> Value {
    let deadline = tokio::time::Instant::now() + Duration::from_secs(40);
    while tokio::time::Instant::now() < deadline {
        let r = td.get(&format!("/api/subscriptions/login/{id}")).await;
        if r.body["state"] == state {
            return r.body;
        }
        tokio::time::sleep(Duration::from_millis(25)).await;
    }
    panic!(
        "login {id} never reached {state}: {}",
        td.get(&format!("/api/subscriptions/login/{id}")).await.body
    );
}

#[tokio::test]
async fn claude_login_with_a_pasted_code() {
    let fixture_path = format!(
        "{}/tests/fixtures/login/claude.raw",
        env!("CARGO_MANIFEST_DIR")
    );
    let env = boot(
        |_| {},
        |u, bin| {
            let marker = bin.join("logged-in");
            // The login writes its credential into the home it gets: the
            // scratch home of the session (PLAN §16.3).
            let creds = bin.join("claude-creds.json");
            claude_creds(bin);
            std::fs::rename(bin.join(".credentials.json"), &creds).unwrap();
            u.programs.insert(
                HarnessId::Claude,
                script(
                    bin,
                    "claude-login",
                    &format!(
                        r#"case "$1 $2" in
"auth login") cat '{fixture_path}'; read code; if [ "$code" = "good-code" ]; then touch '{m}'; cp '{c}' "$CLAUDE_CONFIG_DIR/.credentials.json"; echo 'Login successful.'; exit 0; fi; echo 'bad code'; exit 1;;
"auth status") if [ -f '{m}' ]; then echo '{{"loggedIn": true, "authMethod": "claude.ai", "subscriptionType": "max"}}'; else echo '{{"loggedIn": false}}'; exit 1; fi;;
"auth logout") rm -f '{m}'; echo 'Logged out.';;
esac"#,
                        m = marker.display(),
                        c = creds.display()
                    ),
                ),
            );
        },
    )
    .await;
    let td = &env.td;
    env.http.set(
        ANTHROPIC,
        200,
        &fixture("probes/anthropic_oauth_usage.json"),
    );

    let r = td
        .post("/api/subscriptions/login", json!({"kind": "claude"}))
        .await;
    assert_eq!(r.status.as_u16(), 201, "{}", r.body);
    let id = r.body["id"].as_str().unwrap().to_owned();
    assert!(r.body["instructions"].as_str().unwrap().contains("paste"));
    let w = wait_login(td, &id, "waiting").await;
    assert!(
        w["url"]
            .as_str()
            .unwrap()
            .contains("/oauth/authorize?code=true")
    );
    assert_eq!(w["done"], false);

    // The embedded terminal shows the flow.
    let mut ws = td
        .ws(&format!("/api/subscriptions/login/{id}/pty"), None)
        .await
        .unwrap();
    let first = tokio::time::timeout(Duration::from_secs(5), ws.next())
        .await
        .unwrap()
        .unwrap()
        .unwrap();
    assert!(matches!(first, Message::Binary(_)));
    ws.send(Message::Text(
        r#"{"type":"resize","rows":30,"cols":100}"#.into(),
    ))
    .await
    .unwrap();

    td.post(
        &format!("/api/subscriptions/login/{id}/input"),
        json!({"code": "good-code"}),
    )
    .await
    .ok();
    let done = wait_login(td, &id, "done").await;
    assert_eq!(done["done"], true);
    assert_eq!(done["subscription"], "anthropic-max-20x");
    assert!(!done.to_string().contains("good-code"));
    let s = td.get("/api/subscriptions/anthropic-max-20x").await;
    let s = s.ok();
    assert_eq!(s["login_status"], "logged_in");
    assert_eq!(s["plan"], "Max 20x");
    assert_eq!(s["harnesses"], json!(["claude"]));
    // The first probe ran.
    assert_eq!(s["probe_status"], "ok");
    let audit = td.get("/api/audit").await.body.to_string();
    assert!(
        audit.contains("login.start")
            && audit.contains("login.input")
            && audit.contains("login.end")
    );
    assert!(!audit.contains("good-code"), "{audit}");
    // The session is gone: no input, no terminal.
    td.post(
        &format!("/api/subscriptions/login/{id}/input"),
        json!({"code": "x"}),
    )
    .await
    .err(409, "not_running");

    // The login is in the account of the new subscription; the shared
    // home has no credential.
    let state = td.daemon.as_ref().unwrap().state.state_dir.clone();
    assert!(terrariumd::accounts::has_credential(
        &state,
        "anthropic-max-20x",
        HarnessId::Claude
    ));
    assert!(
        !env.homes[&HarnessId::Claude]
            .join(".credentials.json")
            .exists()
    );
    assert_eq!(s["account"]["harness"], "claude", "{s}");
    assert_eq!(s["account"]["has_login"], true, "{s}");
    // The first Claude account is the default (PLAN §16.3).
    let first = td.get("/api/subscriptions/anthropic-max-20x").await;
    assert_eq!(first.ok()["default"], true, "{}", first.body);

    // A second login of the provider makes a second account.
    let r = td
        .post("/api/subscriptions/login", json!({"kind": "claude"}))
        .await;
    let id = r.ok()["id"].as_str().unwrap().to_owned();
    wait_login(td, &id, "waiting").await;
    td.post(
        &format!("/api/subscriptions/login/{id}/input"),
        json!({"code": "good-code"}),
    )
    .await
    .ok();
    let done = wait_login(td, &id, "done").await;
    assert_eq!(done["subscription"], "anthropic-max-20x-2");
    assert!(terrariumd::accounts::has_credential(
        &state,
        "anthropic-max-20x-2",
        HarnessId::Claude
    ));
    // The second account keeps the default where it is.
    let second = td.get("/api/subscriptions/anthropic-max-20x-2").await;
    assert_eq!(second.ok()["default"], false, "{}", second.body);
    assert_eq!(
        td.get("/api/subscriptions/anthropic-max-20x").await.body["default"],
        true
    );

    // Logout removes the account, and nothing else.
    let out = td
        .post_empty("/api/subscriptions/anthropic-max-20x/logout")
        .await;
    assert_eq!(out.ok()["login_status"], "missing");
    // The logout of the default makes the account left the default.
    assert_eq!(out.body["default"], false);
    assert_eq!(
        td.get("/api/subscriptions/anthropic-max-20x-2").await.body["default"],
        true
    );
    assert!(!terrariumd::accounts::exists(&state, "anthropic-max-20x"));
    assert!(terrariumd::accounts::has_credential(
        &state,
        "anthropic-max-20x-2",
        HarnessId::Claude
    ));
}

#[tokio::test]
async fn codex_device_login_and_failures() {
    let fixture_path = format!(
        "{}/tests/fixtures/login/codex.raw",
        env!("CARGO_MANIFEST_DIR")
    );
    let env = boot(
        |_| {},
        |u, bin| {
            // Long enough that a busy CI runner still sees `waiting`
            // before the never-ending flow below expires.
            u.login_timeout = Duration::from_secs(10);
            u.programs.insert(
                HarnessId::Codex,
                script(
                    bin,
                    "codex-login",
                    &format!(
                        r#"case "$1 $2" in
"login --device-auth") cat '{fixture_path}'; cp '{auth}' "$CODEX_HOME/auth.json"; sleep 0.5; exit 0;;
"login status") echo 'Logged in using ChatGPT' >&2;;
esac"#,
                        auth = bin.join("codex-auth.json").display()
                    ),
                ),
            );
            u.programs.insert(
                HarnessId::Copilot,
                script(bin, "copilot-login", "echo 'To authenticate, visit https://github.com/login/device and enter code WXYZ-1234'; sleep 30"),
            );
            u.programs.insert(
                HarnessId::Claude,
                script(bin, "claude-broken", "echo 'boom'; exit 3"),
            );
        },
    )
    .await;
    let td = &env.td;
    // A plan claim in the id token.
    let claims = base64_url(
        &json!({"https://api.openai.com/auth": {"chatgpt_plan_type": "pro"}}).to_string(),
    );
    // What the login writes into its scratch home.
    let bin = env.homes[&HarnessId::Codex]
        .parent()
        .unwrap()
        .with_file_name("bin");
    std::fs::write(
        bin.join("codex-auth.json"),
        json!({"tokens": {"id_token": format!("h.{claims}.s"), "access_token": "tok", "account_id": "acct"}}).to_string(),
    )
    .unwrap();
    env.http
        .set(OPENAI, 200, &fixture("probes/openai_wham_usage.json"));

    // A dir that a daemon crash left behind goes at the next start.
    let stale = terrariumd::login_session::login_dir(&ulid::Ulid::new().to_string());
    std::fs::create_dir_all(stale.join("agents")).unwrap();
    std::fs::write(stale.join("agents/pty.log"), "device code").unwrap();
    std::fs::File::open(&stale)
        .unwrap()
        .set_modified(std::time::SystemTime::now() - Duration::from_secs(3600))
        .unwrap();
    let r = td
        .post("/api/subscriptions/login", json!({"kind": "codex"}))
        .await;
    assert!(!stale.exists(), "the stale login dir is removed");
    let id = r.ok()["id"].as_str().unwrap().to_owned();
    let done = wait_login(td, &id, "done").await;
    // F6: an ended login keeps neither the URL nor the code, in the
    // answer or in the row.
    assert!(done["url"].is_null(), "{done}");
    assert!(done["device_code"].is_null(), "{done}");
    assert_login_row_clean(td, &id).await;
    assert_eq!(done["subscription"], "openai-pro");
    assert_login_dir_gone(&id);
    let s = td.get("/api/subscriptions/openai-pro").await;
    assert_eq!(s.ok()["probe"], "openai-codex-usage");
    assert_eq!(s.body["windows"][0]["window"], "7d");

    // A flow that never finishes expires.
    let r = td
        .post("/api/subscriptions/login", json!({"kind": "copilot"}))
        .await;
    let id = r.ok()["id"].as_str().unwrap().to_owned();
    let w = wait_login(td, &id, "waiting").await;
    assert_eq!(w["device_code"], "WXYZ-1234");
    assert_no_login_output_on_disk(&id, "WXYZ-1234");
    let e = wait_login(td, &id, "expired").await;
    assert!(e["error"].as_str().unwrap().contains("did not finish"));
    assert!(e["device_code"].is_null() && e["url"].is_null(), "{e}");
    assert!(
        e["started_at"].is_string() && e["finished_at"].is_string(),
        "{e}"
    );
    assert_login_row_clean(td, &id).await;
    assert_login_dir_gone(&id);

    // pi uses API keys only: no pi login.
    for body in [
        json!({"kind": "pi"}),
        json!({"kind": "pi", "provider": "anthropic"}),
    ] {
        let r = td.post("/api/subscriptions/login", body).await;
        r.err(422, "invalid");
        assert!(
            r.body["error"]["message"]
                .as_str()
                .unwrap_or_default()
                .contains("pi uses API keys only"),
            "{}",
            r.body
        );
    }

    // A command that fails.
    let r = td
        .post("/api/subscriptions/login", json!({"kind": "claude"}))
        .await;
    let id = r.ok()["id"].as_str().unwrap().to_owned();
    let f = wait_login(td, &id, "failed").await;
    assert!(f["error"].as_str().unwrap().contains("code 3"), "{f}");
    assert_login_dir_gone(&id);

    // Cancel.
    let r = td
        .post("/api/subscriptions/login", json!({"kind": "copilot"}))
        .await;
    let id = r.ok()["id"].as_str().unwrap().to_owned();
    assert_eq!(
        td.delete(&format!("/api/subscriptions/login/{id}"))
            .await
            .status
            .as_u16(),
        202
    );
    wait_login(td, &id, "cancelled").await;
    assert_login_dir_gone(&id);
    td.get("/api/subscriptions/login/01NOPE")
        .await
        .err(404, "not_found");
    td.post("/api/subscriptions/login", json!({"kind": "gemini"}))
        .await
        .err(400, "bad_request");
}

fn base64_url(s: &str) -> String {
    use base64::Engine;
    base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(s)
}

/// Run each probe once against the real logins of this machine
/// (`~/.claude`, `~/.codex`, and `COPILOT_GITHUB_TOKEN`/`GH_TOKEN`). Read-only
/// GETs; prints the window numbers, never a token.
/// `cargo test -p terrariumd --test subscriptions -- --ignored live_probes`
#[tokio::test]
#[ignore = "needs real logins and the network"]
async fn live_probes_against_real_homes() {
    let home = PathBuf::from(std::env::var("HOME").unwrap());
    let http = terrariumd::probes::ReqwestHttp::new();
    for (probe, dir) in [
        ("anthropic-oauth-usage", home.join(".claude")),
        ("openai-codex-usage", home.join(".codex")),
        ("github-copilot-quota", home.join(".copilot")),
    ] {
        let p = terrariumd::probes::by_name(probe).unwrap();
        let result = match p.credentials(&dir) {
            Ok(c) => p.run(&http, &c).await.map_err(|e| e.to_string()),
            Err(e) => Err(e.to_string()),
        };
        match result {
            Ok(w) => {
                for q in w {
                    eprintln!(
                        "{probe}: {} {:.1}% resets {:?}",
                        q.window, q.used_pct, q.resets_at
                    );
                }
            }
            Err(e) => eprintln!("{probe}: {e}"),
        }
    }
}

#[tokio::test]
async fn models_come_from_the_provider_then_pi_then_the_table() {
    const ZAI_MODELS: &str = "https://api.z.ai/api/coding/paas/v4/models";
    let env = boot(
        |_| {},
        |usage, bin| {
            // pi lists the zai catalogue when the key var is set; it
            // records the env it got, so the test can check that the key
            // never reaches it.
            let table = format!(
                "{}/tests/fixtures/models/pi_list_zai.txt",
                env!("CARGO_MANIFEST_DIR")
            );
            let seen = bin.join("pi-env");
            usage.programs.insert(
                HarnessId::Pi,
                script(
                    bin,
                    "pi",
                    &format!(
                        "env >> '{}'\nif [ \"$2\" = --list-models ] && [ -n \"$ZAI_API_KEY\" ] && [ \"$3\" = zai ]; then cat '{table}'; fi",
                        seen.display()
                    ),
                ),
            );
        },
    )
    .await;
    let td = &env.td;
    let secret = "zk-models-secret";
    td.post(
        "/api/subscriptions/glm/api-key",
        json!({"provider": "zai", "key": secret}),
    )
    .await
    .ok();
    // (a) The provider's own list, with the stored key as a bearer token.
    env.http
        .set(ZAI_MODELS, 200, &fixture("models/zai_coding.json"));
    let r = td.get("/api/subscriptions/glm/models").await;
    let body = r.ok().clone();
    assert_eq!(body["source"], "provider");
    let ids: Vec<&str> = body["models"]
        .as_array()
        .unwrap()
        .iter()
        .map(|m| m["id"].as_str().unwrap())
        .collect();
    assert!(ids.contains(&"glm-5.3-flash"), "{ids:?}");
    assert!(body["fetched_at"].is_string());
    // Cached: a broken endpoint does not show until a refresh.
    env.http.set(ZAI_MODELS, 500, "{}");
    let calls = env.http.calls.lock().unwrap().len();
    assert_eq!(
        td.get("/api/subscriptions/glm/models").await.ok()["source"],
        "provider"
    );
    assert_eq!(env.http.calls.lock().unwrap().len(), calls);
    // (b) pi's catalogue, run with a placeholder key, never the key.
    let r = td.get("/api/subscriptions/glm/models?refresh=true").await;
    let body = r.ok().clone();
    assert_eq!(body["source"], "pi", "{body}");
    assert_eq!(
        body["models"][0],
        json!({"id": "glm-4.7", "context": 204_800})
    );
    assert!(body["notes"][0].as_str().unwrap().contains("status 500"));
    let seen = std::fs::read_to_string(env.td.dir.path().join("bin/pi-env")).unwrap();
    assert!(seen.contains("ZAI_API_KEY=terrarium-list-models"), "{seen}");
    assert!(!seen.contains(secret));
    assert!(!body.to_string().contains(secret));
    // (c) The table: the `api` plan is pi's custom `zai-api`, which this
    // pi does not list, and the endpoint fails.
    td.post(
        "/api/subscriptions/payg/api-key",
        json!({"provider": "zai", "plan": "api", "key": secret}),
    )
    .await
    .ok();
    let r = td.get("/api/subscriptions/payg/models").await;
    let body = r.ok().clone();
    assert_eq!(body["source"], "fallback", "{body}");
    assert_eq!(
        body["models"],
        json!([{"id": "glm-5.3"}, {"id": "glm-5.3-flash"}])
    );
    assert_eq!(body["notes"].as_array().unwrap().len(), 2, "{body}");
    // Who may read it: not a member agent; a project_admin of a project
    // with an agent on it.
    td.project("p").await;
    td.agent(
        "p",
        "worker",
        json!({"harness": "pi", "subscription": "glm"}),
    )
    .await;
    td.agent("p", "lead", json!({"roles": ["project_admin"]}))
        .await;
    td.get_as("p/worker", "/api/subscriptions/glm/models")
        .await
        .err(403, "forbidden");
    assert_eq!(
        td.get_as("p/lead", "/api/subscriptions/glm/models")
            .await
            .ok()["source"],
        "pi"
    );
    td.get_as("p/lead", "/api/subscriptions/payg/models")
        .await
        .err(403, "forbidden");
    td.get("/api/subscriptions/nope/models")
        .await
        .err(404, "not_found");
}

/// PLAN §16.3: at the start, the shared Claude login moves into the
/// account of the one subscription that uses it, with a backup and an
/// audit row. A pool must hold accounts of one provider. When a probe
/// finds an account full, an agent with a pool moves to the next account
/// with room (auto-swap); an agent without a pool is held. An account
/// that an agent runs on cannot be logged out.
#[tokio::test]
async fn shared_logins_migrate_and_full_accounts_swap() {
    let max_a =
        "provider = \"anthropic\"\nharnesses = [\"claude\"]\nprobe = \"anthropic-oauth-usage\"\n";
    let env = boot(
        |c| write(c, "subscriptions/max-a.toml", max_a),
        |u, _| claude_creds(&u.homes[&HarnessId::Claude]),
    )
    .await;
    let td = &env.td;
    let state = td.daemon.as_ref().unwrap().state.state_dir.clone();
    let shared = env.homes[&HarnessId::Claude].join(".credentials.json");
    wait_for("the migration", || async {
        terrariumd::accounts::has_credential(&state, "max-a", HarnessId::Claude) && !shared.exists()
    })
    .await;
    let backups = std::fs::read_dir(state.join("credentials/backup"))
        .unwrap()
        .count();
    assert_eq!(backups, 1);
    let audit = td
        .get("/api/audit?action=credentials.migrate")
        .await
        .body
        .to_string();
    assert!(audit.contains("max-a"), "{audit}");
    let s = td.get("/api/subscriptions/max-a").await;
    assert_eq!(s.ok()["account"]["source"], "migration", "{}", s.body);

    // A second account, as the wizard makes it.
    td.post(
        "/api/subscriptions",
        json!({"name": "max-b", "provider": "anthropic", "harnesses": ["claude"],
               "probe": "anthropic-oauth-usage"}),
    )
    .await
    .ok();
    td.post(
        "/api/subscriptions",
        json!({"name": "glm", "provider": "zai", "plan": "coding", "harnesses": ["claude"]}),
    )
    .await
    .ok();
    let login = r#"{"claudeAiOauth": {"accessToken": "tok-b", "refreshToken": "rb", "expiresAt": 4102444800000}}"#;
    terrariumd::accounts::write_credential(&state, "max-b", HarnessId::Claude, login.as_bytes())
        .unwrap();

    td.post("/api/projects", json!({"name": "demo"})).await.ok();
    let pooled = td
        .post(
            "/api/agents",
            json!({"name": "a1", "project": "demo", "harness": "claude",
                   "subscription": "max-a", "subscription_pool": ["max-b"]}),
        )
        .await;
    // The current subscription joins the pool at the front.
    assert_eq!(pooled.ok()["subscription_pool"], json!(["max-a", "max-b"]));
    let a1 = pooled.body["id"].as_str().unwrap().to_owned();
    let a2 = td
        .post(
            "/api/agents",
            json!({"name": "a2", "project": "demo", "harness": "claude", "subscription": "max-a"}),
        )
        .await;
    let a2 = a2.ok()["id"].as_str().unwrap().to_owned();
    for (pool, why) in [
        (json!(["max-a", "nope"]), "no such subscription"),
        (json!(["max-a", "glm"]), "mixes providers"),
    ] {
        let r = td
            .post(
                "/api/agents",
                json!({"name": "bad", "project": "demo", "harness": "claude",
                       "subscription": "max-a", "subscription_pool": pool}),
            )
            .await;
        assert_eq!(r.status.as_u16(), 422, "{}", r.body);
        assert!(r.body.to_string().contains(why), "{}", r.body);
    }
    let r = td
        .patch(
            &format!("/api/agents/{a2}"),
            json!({"subscription_pool": ["glm"]}),
        )
        .await;
    assert_eq!(r.status.as_u16(), 422, "{}", r.body);

    // max-a is full.
    let reset = (chrono::Utc::now() + chrono::Duration::hours(2)).to_rfc3339();
    env.http.set(
        ANTHROPIC,
        200,
        &json!({"five_hour": {"utilization": 100.0, "resets_at": reset}}).to_string(),
    );
    td.post_empty("/api/subscriptions/max-a/probe").await.ok();
    let a = td.get(&format!("/api/agents/{a1}")).await;
    assert_eq!(a.ok()["subscription"], "max-b", "{}", a.body);
    let b = td.get(&format!("/api/agents/{a2}")).await;
    assert_eq!(b.ok()["subscription"], "max-a");
    let s = td.get("/api/subscriptions/max-a").await;
    assert_eq!(s.ok()["held_agents"], 1, "only a2 waits: {}", s.body);
    assert_eq!(s.body["agents"][0]["name"], "a2");
    let audit = td
        .get("/api/audit?action=agent.swap")
        .await
        .body
        .to_string();
    assert!(
        audit.contains("\"auto\":true") && audit.contains("max-b"),
        "{audit}"
    );
    assert!(!events_of(td, "agent.swapped").await.is_empty());

    // The account of a running agent cannot go.
    let r = td.post_empty("/api/subscriptions/max-b/logout").await;
    assert_eq!(r.status.as_u16(), 409, "{}", r.body);
    assert!(terrariumd::accounts::exists(&state, "max-b"));
    td.delete("/api/subscriptions/max-b")
        .await
        .err(409, "conflict");
}

/// PLAN §16.3: the default account. The start makes the first (oldest)
/// account of a harness its default. The operator or a sysadmin moves it
/// to another plan account, with an audit row and a `subscription` event.
/// An API key or a subscription with no account cannot be the default. A
/// delete of the default makes the oldest account left the default.
#[tokio::test]
async fn the_default_account_follows_the_operator() {
    let max_a =
        "provider = \"anthropic\"\nharnesses = [\"claude\"]\nprobe = \"anthropic-oauth-usage\"\n";
    let env = boot(
        |c| write(c, "subscriptions/max-a.toml", max_a),
        |u, _| claude_creds(&u.homes[&HarnessId::Claude]),
    )
    .await;
    let td = &env.td;
    let state = td.daemon.as_ref().unwrap().state.state_dir.clone();
    wait_for("the default after the migration", || async {
        td.get("/api/subscriptions/max-a").await.body["default"] == json!(true)
    })
    .await;
    let audit = td.get("/api/audit?action=subscription.default").await;
    assert_eq!(
        audit.ok()[0]["detail"]["reason"],
        "backfill",
        "{}",
        audit.body
    );

    // A second account, as the wizard makes it: the default stays.
    td.post(
        "/api/subscriptions",
        json!({"name": "max-b", "provider": "anthropic", "harnesses": ["claude"]}),
    )
    .await
    .ok();
    let scratch = td.dir.path().join("scratch-b");
    std::fs::create_dir_all(&scratch).unwrap();
    claude_creds(&scratch);
    terrariumd::accounts::import_login(&state, "max-b", HarnessId::Claude, "anthropic", &scratch)
        .unwrap();
    let list = td.get("/api/subscriptions").await;
    let row = |n: &str| {
        list.body
            .as_array()
            .unwrap()
            .iter()
            .find(|s| s["name"] == n)
            .unwrap()
            .clone()
    };
    assert_eq!(row("max-a")["default"], true);
    assert_eq!(row("max-b")["default"], false);

    // Rights: the operator and a sysadmin; not a member or a project_admin.
    td.project("p").await;
    td.agent("p", "worker", json!({})).await;
    td.agent("p", "lead", json!({"roles": ["project_admin"]}))
        .await;
    td.agent("p", "root", json!({"roles": ["sysadmin"]})).await;
    for who in ["p/worker", "p/lead"] {
        td.post_empty_as(who, "/api/subscriptions/max-b/default")
            .await
            .err(403, "forbidden");
    }
    let r = td
        .post_empty_as("p/root", "/api/subscriptions/max-b/default")
        .await;
    assert_eq!(r.ok()["default"], true, "{}", r.body);
    assert_eq!(
        td.get("/api/subscriptions/max-a").await.body["default"],
        false
    );
    let audit = td.get("/api/audit?action=subscription.default").await;
    let last = audit.ok().as_array().unwrap().last().unwrap()["detail"].clone();
    assert_eq!(
        (&last["from"], &last["to"], &last["harness"]),
        (&json!("max-a"), &json!("max-b"), &json!("claude")),
        "{}",
        audit.body
    );
    let ev = events_of(td, "subscription").await;
    assert!(
        ev.iter()
            .any(|e| e["data"]["change"] == "default" && e["data"]["to"] == "max-b"),
        "{ev:?}"
    );

    // Not a plan account: an API key, a subscription with no account, and
    // an unknown name.
    td.post(
        "/api/subscriptions/glm/api-key",
        json!({"provider": "zai", "plan": "coding", "key": "zk-secret"}),
    )
    .await
    .ok();
    td.post_empty("/api/subscriptions/glm/default")
        .await
        .err(422, "invalid");
    td.post(
        "/api/subscriptions",
        json!({"name": "max-c", "provider": "anthropic", "harnesses": ["claude"]}),
    )
    .await
    .ok();
    td.post_empty("/api/subscriptions/max-c/default")
        .await
        .err(422, "invalid");
    td.post_empty("/api/subscriptions/nope/default")
        .await
        .err(404, "not_found");

    // Delete the default: the oldest account left takes over.
    assert_eq!(
        td.delete("/api/subscriptions/max-b").await.status.as_u16(),
        204
    );
    let list = td.get("/api/subscriptions").await;
    let defaults: Vec<&str> = list
        .ok()
        .as_array()
        .unwrap()
        .iter()
        .filter(|s| s["default"] == true)
        .map(|s| s["name"].as_str().unwrap())
        .collect();
    assert_eq!(defaults, ["max-a"], "{}", list.body);
    let ev = events_of(td, "subscription").await;
    assert!(
        ev.iter().any(|e| e["data"]["change"] == "default_cleared"),
        "{ev:?}"
    );
}
