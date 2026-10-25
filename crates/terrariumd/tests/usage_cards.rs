//! Per-card usage attribution (PLAN §16.2, docs/api.md §16): the collector
//! tags each sample with the card that its agent has in `in_progress`, and
//! `GET /api/usage?group=card` and `GET /api/cards/{id}/usage` sum them.
#![allow(clippy::unwrap_used, clippy::expect_used)]

mod common;

use std::collections::HashMap;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use common::TestDaemon;
use serde_json::{Value, json};
use terrarium_core::HarnessId;
use terrariumd::events::EventDraft;
use terrariumd::supervisor::InMemorySupervisor;
use terrariumd::usage_collector::UsageConfig;

/// A daemon whose Claude home is `<dir>/auth/claude`.
async fn start() -> TestDaemon {
    let dir = tempfile::tempdir().unwrap();
    let config = dir.path().join("config");
    std::fs::create_dir_all(&config).unwrap();
    let state = dir.path().join("state");
    let socket = dir.path().join("api.sock");
    let home = dir.path().join("auth/claude");
    std::fs::create_dir_all(&home).unwrap();
    let sup = Arc::new(InMemorySupervisor::new(common::DELAY));
    let usage = UsageConfig {
        homes: HashMap::from([(HarnessId::Claude, home)]),
        ..UsageConfig::default()
    };
    let daemon = terrariumd::start_with_usage(
        terrariumd::DaemonConfig {
            socket: socket.clone(),
            listen: None,
            token: None,
            state_dir: state.clone(),
            config_dir: config.clone(),
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
    TestDaemon {
        daemon: Some(daemon),
        dir,
        socket,
        config,
        state,
        sup,
        web: Default::default(),
        artifact_listen: None,
    }
}

/// The directory of the Claude transcripts of `agent`: its own project
/// dir in its harness dir (the per-agent overlay of the sandbox).
fn sessions_dir(td: &TestDaemon, agent: &Value) -> PathBuf {
    common::claude_project_dir(&td.state, agent["id"].as_str().unwrap())
}

/// The directory of the Claude transcripts in the shared home.
fn shared_sessions_dir(td: &TestDaemon) -> PathBuf {
    let dir = td.dir.path().join("auth/claude/projects/-home-agent-work");
    std::fs::create_dir_all(&dir).unwrap();
    dir
}

const SINCE: &str = "since=2026-01-01T00:00:00Z";

/// One Claude Code `assistant` record with usage.
fn line(id: &str, model: &str, input: u64, output: u64) -> String {
    json!({
        "type": "assistant",
        "sessionId": "s",
        "uuid": format!("u-{id}"),
        "timestamp": "2026-10-24T06:01:33.413Z",
        "message": {
            "model": model, "id": id, "type": "message", "role": "assistant",
            "content": [{"type": "text", "text": "..."}], "stop_reason": "end_turn",
            "usage": {"input_tokens": input, "output_tokens": output,
                      "cache_creation_input_tokens": 0, "cache_read_input_tokens": 0}
        }
    })
    .to_string()
        + "\n"
}

fn append(path: &Path, text: &str) {
    let mut f = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(path)
        .unwrap();
    f.write_all(text.as_bytes()).unwrap();
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

/// `(message_id, card_id)` of every stored sample, by message id.
async fn samples(td: &TestDaemon) -> Vec<(String, Option<String>)> {
    td.daemon
        .as_ref()
        .unwrap()
        .state
        .db
        .read(|c| {
            let mut stmt =
                c.prepare("SELECT message_id, card_id FROM usage_samples ORDER BY message_id")?;
            let rows = stmt.query_map([], |r| Ok((r.get(0)?, r.get(1)?)))?;
            Ok(rows.collect::<Result<Vec<_>, _>>()?)
        })
        .await
        .unwrap()
}

async fn wait_samples(td: &TestDaemon, n: usize) -> Vec<(String, Option<String>)> {
    for _ in 0..400 {
        let s = samples(td).await;
        if s.len() >= n {
            return s;
        }
        tokio::time::sleep(Duration::from_millis(25)).await;
    }
    panic!("timed out waiting for {n} samples");
}

#[tokio::test]
async fn samples_carry_the_in_progress_card() {
    let td = start().await;
    td.project("p").await;
    td.project("q").await;
    let worker = td.agent("p", "worker", json!({})).await;
    let idle = td.agent("p", "idle", json!({})).await;
    let spy = td.agent("q", "spy", json!({})).await;
    let wid = worker["id"].as_str().unwrap().to_owned();

    // The worker claims a card: it is now in `in_progress`, assigned to it.
    let card = td
        .post(
            "/api/projects/p/cards",
            json!({"title": "Write the parser"}),
        )
        .await
        .ok()
        .clone();
    let cid = card["id"].as_str().unwrap().to_owned();
    let claimed = td
        .post_empty_as(&wid, &format!("/api/cards/{cid}/claim"))
        .await
        .ok()
        .clone();
    assert_eq!(claimed["column"], "in_progress");

    // Two responses on two models while the card is in progress.
    let file = sessions_dir(&td, &worker).join("s.jsonl");
    append(
        &file,
        &(line("m1", "claude-haiku-5-5", 10, 5) + &line("m2", "claude-opus-5", 20, 7)),
    );
    emit(
        &td,
        &worker,
        "session_started",
        json!({"harness_session": "s", "transcript_path": file}),
    )
    .await;
    let s = wait_samples(&td, 2).await;
    assert_eq!(
        s,
        [
            ("m1".to_owned(), Some(cid.clone())),
            ("m2".to_owned(), Some(cid.clone()))
        ]
    );

    // `group=card`: the card ID as the key, with the title.
    let by_card = td
        .get(&format!("/api/usage?{SINCE}&bucket=day&group=card"))
        .await
        .ok()
        .clone();
    assert_eq!(by_card.as_array().unwrap().len(), 1, "{by_card}");
    assert_eq!(by_card[0]["key"], cid.as_str());
    assert_eq!(by_card[0]["title"], "Write the parser");
    assert_eq!(by_card[0]["input_tokens"], 30);
    assert_eq!(by_card[0]["output_tokens"], 12);
    assert_eq!(by_card[0]["messages"], 2);
    // Other groups have no title.
    let by_model = td
        .get(&format!("/api/usage?{SINCE}&bucket=day&group=model"))
        .await
        .ok()
        .clone();
    assert!(by_model[0].get("title").is_none(), "{by_model}");
    // An ungrouped point of two models names no model.
    let all = td
        .get(&format!("/api/usage?{SINCE}&bucket=day"))
        .await
        .ok()
        .clone();
    assert!(all[0].get("model").is_none(), "{all}");

    // `GET /api/cards/{id}/usage`: total, by model (biggest first), by agent.
    let u = td
        .get(&format!("/api/cards/{}/usage", cid.to_lowercase()))
        .await
        .ok()
        .clone();
    assert_eq!(u["card"], cid.as_str());
    assert_eq!(u["title"], "Write the parser");
    assert_eq!(u["project"], "p");
    assert_eq!(u["total"]["input_tokens"], 30);
    assert_eq!(u["total"]["output_tokens"], 12);
    assert_eq!(u["total"]["messages"], 2);
    let models: Vec<(&str, u64)> = u["models"]
        .as_array()
        .unwrap()
        .iter()
        .map(|m| {
            (
                m["model"].as_str().unwrap(),
                m["input_tokens"].as_u64().unwrap() + m["output_tokens"].as_u64().unwrap(),
            )
        })
        .collect();
    assert_eq!(models, [("claude-opus-5", 27), ("claude-haiku-5-5", 15)]);
    assert_eq!(u["agents"].as_array().unwrap().len(), 1);
    assert_eq!(u["agents"][0]["id"], wid.as_str());
    assert_eq!(u["agents"][0]["name"], "p/worker");
    assert_eq!(u["agents"][0]["input_tokens"], 30);

    // The card filter on the series, any case.
    let f = td
        .get(&format!(
            "/api/usage?{SINCE}&bucket=day&card={}",
            cid.to_lowercase()
        ))
        .await
        .ok()
        .clone();
    assert_eq!(f[0]["messages"], 2);

    // The card leaves `in_progress`: the cached card goes, and the next
    // sample has no card.
    td.post_as(
        &wid,
        &format!("/api/cards/{cid}/move"),
        json!({"column": "review"}),
    )
    .await
    .ok();
    append(&file, &line("m3", "claude-opus-5", 100, 1));
    emit(&td, &worker, "turn_end", json!({})).await;
    let s = wait_samples(&td, 3).await;
    assert_eq!(s[2], ("m3".to_owned(), None));

    // An agent with no card in progress: no card.
    let other = sessions_dir(&td, &idle).join("s2.jsonl");
    append(&other, &line("m4", "claude-opus-5", 1, 1));
    emit(
        &td,
        &idle,
        "session_started",
        json!({"harness_session": "s2", "transcript_path": other}),
    )
    .await;
    let s = wait_samples(&td, 4).await;
    assert_eq!(s[3], ("m4".to_owned(), None));

    // The card's totals did not change; samples with no card have no key.
    let u = td
        .get(&format!("/api/cards/{cid}/usage"))
        .await
        .ok()
        .clone();
    assert_eq!(u["total"]["messages"], 2);
    let by_card = td
        .get(&format!("/api/usage?{SINCE}&bucket=day&group=card"))
        .await
        .ok()
        .clone();
    let keys: Vec<&Value> = by_card
        .as_array()
        .unwrap()
        .iter()
        .map(|p| &p["key"])
        .collect();
    assert_eq!(keys, [&Value::Null, &json!(cid)]);
    assert_eq!(by_card[0]["messages"], 2);

    // A window with no samples is zero; scope and errors.
    let none = td
        .get(&format!(
            "/api/cards/{cid}/usage?since=2027-01-01T00:00:00Z"
        ))
        .await
        .ok()
        .clone();
    assert_eq!(none["total"]["messages"], 0);
    assert_eq!(none["since"], "2027-01-01T00:00:00.000Z");
    assert_eq!(none["models"], json!([]));
    let sid = spy["id"].as_str().unwrap();
    td.get_as(sid, &format!("/api/cards/{cid}/usage"))
        .await
        .err(403, "forbidden");
    td.get_as(&wid, &format!("/api/cards/{cid}/usage"))
        .await
        .ok();
    td.get("/api/cards/P-999/usage").await.err(404, "not_found");
    td.get(&format!("/api/cards/{cid}/usage?since=yesterday"))
        .await
        .err(400, "bad_request");
    td.get("/api/usage?group=board")
        .await
        .err(400, "bad_request");
}

#[tokio::test]
async fn a_reassigned_card_follows_its_new_assignee() {
    let td = start().await;
    td.project("p").await;
    let a = td.agent("p", "a", json!({})).await;
    let b = td.agent("p", "b", json!({})).await;
    let aid = a["id"].as_str().unwrap().to_owned();
    let card = td
        .post("/api/projects/p/cards", json!({"title": "Shared"}))
        .await
        .ok()
        .clone();
    let cid = card["id"].as_str().unwrap().to_owned();
    td.post_empty_as(&aid, &format!("/api/cards/{cid}/claim"))
        .await
        .ok();

    let fa = sessions_dir(&td, &a).join("sa.jsonl");
    let fb = sessions_dir(&td, &b).join("sb.jsonl");
    append(&fa, &line("a1", "claude-opus-5", 1, 1));
    std::fs::write(&fb, "").unwrap();
    emit(
        &td,
        &a,
        "session_started",
        json!({"harness_session": "sa", "transcript_path": fa}),
    )
    .await;
    emit(
        &td,
        &b,
        "session_started",
        json!({"harness_session": "sb", "transcript_path": fb}),
    )
    .await;
    wait_samples(&td, 1).await;

    // The operator gives the card to b; a's cached card goes too.
    td.post(
        &format!("/api/cards/{cid}/assign"),
        json!({"assignee": b["id"]}),
    )
    .await
    .ok();
    append(&fa, &line("a2", "claude-opus-5", 1, 1));
    append(&fb, &line("b1", "claude-opus-5", 1, 1));
    emit(&td, &a, "turn_end", json!({})).await;
    emit(&td, &b, "turn_end", json!({})).await;
    let s = wait_samples(&td, 3).await;
    assert_eq!(
        s,
        [
            ("a1".to_owned(), Some(cid.clone())),
            ("a2".to_owned(), None),
            ("b1".to_owned(), Some(cid.clone())),
        ]
    );
}

/// The `usage.rejected_path` events: `(agent, reason)`.
async fn rejected(td: &TestDaemon) -> Vec<(String, String)> {
    let r = td.get("/api/events?kind=usage.rejected_path").await;
    r.ok()
        .as_array()
        .unwrap()
        .iter()
        .map(|e| {
            (
                e["agent"]["id"]
                    .as_str()
                    .or(e["agent_id"].as_str())
                    .unwrap_or_default()
                    .to_owned(),
                e["data"]["reason"].as_str().unwrap().to_owned(),
            )
        })
        .collect()
}

async fn wait_rejected(td: &TestDaemon, n: usize) -> Vec<(String, String)> {
    for _ in 0..400 {
        let r = rejected(td).await;
        if r.len() >= n {
            return r;
        }
        tokio::time::sleep(Duration::from_millis(25)).await;
    }
    panic!("timed out waiting for {n} usage.rejected_path events");
}

/// F12 (`docs/security-review.md`): a forged `session_started` cannot point
/// the collector at a file outside the agent's own session dir (another
/// agent's harness dir, the shared home, `/etc`), at a symlink, or at the
/// transcript of another agent.
#[tokio::test]
async fn forged_transcript_paths_are_refused() {
    let td = start().await;
    td.project("p").await;
    let victim = td.agent("p", "victim", json!({})).await;
    let spy = td.agent("p", "spy", json!({})).await;
    let vid = victim["id"].as_str().unwrap().to_owned();
    let sid = spy["id"].as_str().unwrap().to_owned();
    let dir = sessions_dir(&td, &spy);

    // The victim's own session counts.
    let vfile = sessions_dir(&td, &victim).join("sv.jsonl");
    append(&vfile, &line("v1", "claude-opus-5", 10, 10));
    emit(
        &td,
        &victim,
        "session_started",
        json!({"harness_session": "sv", "transcript_path": vfile}),
    )
    .await;
    wait_samples(&td, 1).await;

    // A file outside the home.
    emit(
        &td,
        &spy,
        "session_started",
        json!({"harness_session": "passwd", "transcript_path": "/etc/passwd"}),
    )
    .await;
    let r = wait_rejected(&td, 1).await;
    assert!(r[0].1.contains("not under"), "{r:?}");

    // The victim's transcript, with the right session id: it is in the
    // victim's harness dir, outside the spy's own session dir.
    emit(
        &td,
        &spy,
        "session_started",
        json!({"harness_session": "sv", "transcript_path": vfile}),
    )
    .await;
    let r = wait_rejected(&td, 2).await;
    assert!(r[1].1.contains("not under"), "{r:?}");

    // The same, by the sandbox path, after the victim's session ended:
    // the store gives `sv` to the victim.
    emit(&td, &victim, "session_ended", json!({})).await;
    let v = vid.clone();
    td.daemon
        .as_ref()
        .unwrap()
        .state
        .db
        .write("system", move |t| {
            t.execute(
                "INSERT INTO sessions (id, agent_id, current, started_at, harness_session_id)
                 VALUES ('S1', ?1, 0, '2026-10-24T00:00:00Z', 'sv')",
                [&v],
            )?;
            Ok(())
        })
        .await
        .unwrap();
    emit(
        &td,
        &spy,
        "session_started",
        json!({"harness_session": "sv",
               "transcript_path": "/home/agent/.claude/projects/-home-agent-work/sv.jsonl"}),
    )
    .await;
    let r = wait_rejected(&td, 3).await;
    assert!(r[2].1.contains("belongs to the agent"), "{r:?}");

    // A file in the shared home (an older layout, or the operator's own
    // sessions): not the spy's.
    let shared = shared_sessions_dir(&td).join("sv.jsonl");
    append(&shared, &line("x1", "claude-opus-5", 10, 10));
    emit(
        &td,
        &spy,
        "session_started",
        json!({"harness_session": "sv", "transcript_path": shared}),
    )
    .await;
    let r = wait_rejected(&td, 4).await;
    assert!(r[3].1.contains("not under"), "{r:?}");

    // A file in the spy's own dir with another session id: the name does
    // not match.
    let mine = dir.join("other.jsonl");
    append(&mine, &line("o1", "claude-opus-5", 1, 1));
    emit(
        &td,
        &spy,
        "session_started",
        json!({"harness_session": "mine", "transcript_path": mine}),
    )
    .await;
    let r = wait_rejected(&td, 5).await;
    assert!(r[4].1.contains("does not name the session"), "{r:?}");

    // A symlink in the spy's dir, and a `..` out of it.
    let link = dir.join("sl.jsonl");
    std::os::unix::fs::symlink(&vfile, &link).unwrap();
    emit(
        &td,
        &spy,
        "session_started",
        json!({"harness_session": "sl", "transcript_path": link}),
    )
    .await;
    let r = wait_rejected(&td, 6).await;
    assert!(r[5].1.contains("symlink"), "{r:?}");
    let dotdot = dir.join("../../../../../../etc/passwd");
    emit(
        &td,
        &spy,
        "session_started",
        json!({"harness_session": "passwd", "transcript_path": dotdot}),
    )
    .await;
    let r = wait_rejected(&td, 7).await;
    assert!(r[6].1.contains(".."), "{r:?}");
    assert!(r.iter().all(|(a, _)| a == &sid), "{r:?}");

    // The victim's usage stays the victim's: the spy got nothing.
    append(&vfile, &line("v2", "claude-opus-5", 10, 10));
    emit(&td, &spy, "turn_end", json!({})).await;
    tokio::time::sleep(Duration::from_millis(200)).await;
    let s = samples(&td).await;
    assert_eq!(s.len(), 1, "{s:?}");
    let spy_usage = td
        .get(&format!("/api/usage?{SINCE}&bucket=day&agent={sid}"))
        .await
        .ok()
        .clone();
    assert_eq!(spy_usage, json!([]), "{spy_usage}");

    // The spy's own session file in its own dir is fine.
    let own = dir.join("ss.jsonl");
    append(&own, &line("s1", "claude-opus-5", 1, 1));
    emit(
        &td,
        &spy,
        "session_started",
        json!({"harness_session": "ss", "transcript_path": own}),
    )
    .await;
    wait_samples(&td, 2).await;
    assert_eq!(rejected(&td).await.len(), 7);
}

/// The model in `usage.sample` and in an ungrouped `GET /api/usage` point
/// is the model of the session file (`claude-haiku-5-5`), not the alias of
/// the agent definition (`haiku`).
#[tokio::test]
async fn usage_names_the_model_of_the_session_file() {
    let td = start().await;
    td.project("p").await;
    let a = td.agent("p", "h", json!({"model": "haiku"})).await;
    assert_eq!(a["model"], "haiku", "{a}");
    let id = a["id"].as_str().unwrap().to_owned();
    let file = sessions_dir(&td, &a).join("sh.jsonl");
    append(&file, &line("h1", "claude-haiku-5-5", 3, 4));
    emit(
        &td,
        &a,
        "session_started",
        json!({"harness_session": "sh", "transcript_path": file}),
    )
    .await;
    wait_samples(&td, 1).await;
    let mut sample = Value::Null;
    for _ in 0..400 {
        let ev = td
            .get("/api/events?kind=usage.sample&limit=100")
            .await
            .ok()
            .clone();
        if let Some(e) = ev.as_array().and_then(|a| a.first()) {
            sample = e.clone();
            break;
        }
        tokio::time::sleep(Duration::from_millis(25)).await;
    }
    assert_eq!(sample["data"]["model"], "claude-haiku-5-5", "{sample}");

    let points = td
        .get(&format!("/api/usage?{SINCE}&bucket=day&agent={id}"))
        .await
        .ok()
        .clone();
    assert_eq!(points[0]["model"], "claude-haiku-5-5", "{points}");
    // With `group=model` the model is the key.
    let by_model = td
        .get(&format!(
            "/api/usage?{SINCE}&bucket=day&agent={id}&group=model"
        ))
        .await
        .ok()
        .clone();
    assert_eq!(by_model[0]["key"], "claude-haiku-5-5");
    assert!(by_model[0].get("model").is_none(), "{by_model}");
}
