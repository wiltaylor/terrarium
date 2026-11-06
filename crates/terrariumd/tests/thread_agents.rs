//! The attached agent of an Inbox thread (TER-92, `crate::thread_agent`,
//! docs/api.md §6.13), with the fake supervisor: the spawn at the open,
//! the `[Thread:<id>]` turns, the park while the thread waits on the
//! operator, the restart in the same session (claude, codex, pi), the life
//! past 24 hours, the close, the A1 check at the spawn, and the memory of
//! the project it runs in (A2). No harness and no model turn runs.
#![allow(clippy::unwrap_used, clippy::expect_used)]

mod common;

use std::time::Duration;

use common::TestDaemon;
use serde_json::{Value, json};
use terrarium_core::AgentId;
use terrarium_core::config::{CompactMaxAge, IdleStop};

fn state(td: &TestDaemon) -> terrariumd::api::Shared {
    td.daemon.as_ref().unwrap().state.clone()
}

fn fast(td: &TestDaemon) {
    let s = state(td);
    s.lifecycle.set_tuning(terrariumd::lifecycle::Tuning {
        idle_stop: IdleStop::Off,
        tick: Duration::from_millis(20),
        compact_on_idle: false,
        compact_max_age: CompactMaxAge::Off,
        compact_wait: Duration::from_secs(5),
    });
    s.lifecycle.spawns.set_tuning(terrariumd::spawn::Tuning {
        done_delay: Duration::from_millis(50),
    });
    s.waits.set_tuning(terrariumd::waits::Tuning {
        tick: Duration::from_millis(20),
        wake_debounce: Duration::from_millis(50),
        ..terrariumd::waits::Tuning::default()
    });
}

fn id(v: &Value) -> String {
    v["id"].as_str().unwrap().to_owned()
}

/// Poll `f` until it is true, with a deadline.
async fn until<F: std::future::Future<Output = bool>>(what: &str, mut f: impl FnMut() -> F) {
    let start = std::time::Instant::now();
    while start.elapsed() < Duration::from_secs(60) {
        if f().await {
            return;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    panic!("timed out waiting for {what}");
}

struct Lab {
    td: TestDaemon,
    /// `p/lead`.
    p_lead: String,
    /// `p/w`: a member of `p` with `thread:open`.
    w: String,
}

/// Projects `meta`, `p`, and `q`, each with a lead; the templates
/// `t-claude`, `t-codex`, `t-pi`, and `consult` (members of `p`), and the
/// global `helper`; `p/w` with the right `thread:open`.
async fn lab() -> Lab {
    let td = TestDaemon::start().await;
    fast(&td);
    for p in ["meta", "p", "q"] {
        td.project(p).await;
    }
    td.agent("meta", "lead", json!({"roles": ["project_admin"]}))
        .await;
    td.agent("q", "lead", json!({"roles": ["project_admin"]}))
        .await;
    let p_lead = id(&td
        .agent("p", "lead", json!({"roles": ["project_admin"]}))
        .await);
    for (name, project, harness) in [
        ("t-claude", Some("p"), "claude"),
        ("t-codex", Some("p"), "codex"),
        ("t-pi", Some("p"), "pi"),
        ("consult", Some("p"), "claude"),
        ("helper", None, "claude"),
    ] {
        let mut body = json!({"name": name, "spec": {"harness": harness}});
        if let Some(p) = project {
            body["project"] = json!(p);
        }
        let r = td.post("/api/templates", body).await;
        assert_eq!(r.status, 201, "{}", r.body);
    }
    let w = id(&td
        .agent("p", "w", json!({"policy": {"thread_open": true}}))
        .await);
    Lab { td, p_lead, w }
}

impl Lab {
    async fn open(&self, body: Value) -> Value {
        let r = self.td.post("/api/threads", body).await;
        assert_eq!(r.status, 201, "{}", r.body);
        r.body
    }

    async fn thread(&self, tid: &str) -> Value {
        self.td
            .get(&format!("/api/threads/{tid}"))
            .await
            .ok()
            .clone()
    }

    async fn spawn_row(&self, eph: &str) -> Value {
        self.td.get(&format!("/api/spawns/{eph}")).await.ok().clone()
    }

    async fn agent(&self, eph: &str) -> Value {
        self.td.get(&format!("/api/agents/{eph}")).await.ok().clone()
    }

    async fn parked(&self, eph: &str) {
        until("the park stop", || async {
            let a = self.agent(eph).await;
            a["status"] == "stopped" && a["status_reason"] == "parked"
        })
        .await;
    }

    async fn sessions(&self, eph: &str) -> Vec<Value> {
        self.td
            .get(&format!("/api/agents/{eph}/sessions"))
            .await
            .ok()
            .as_array()
            .unwrap()
            .clone()
    }

    async fn post_as(&self, eph: &str, tid: &str, text: &str) {
        let r = self
            .td
            .post_as(
                eph,
                &format!("/api/threads/{tid}/messages"),
                json!({"text": text}),
            )
            .await;
        assert_eq!(r.status, 201, "{}", r.body);
    }

    /// Wait for a prompt of `eph` that starts with `start`, and give it.
    async fn prompt(&self, eph: &str, start: &str) -> String {
        let aid: AgentId = eph.parse().unwrap();
        until(&format!("a turn `{start}`"), || async {
            self.td.sup.prompts(aid).iter().any(|p| p.starts_with(start))
        })
        .await;
        self.td
            .sup
            .prompts(aid)
            .into_iter()
            .rfind(|p| p.starts_with(start))
            .unwrap()
    }

    async fn closed(&self, eph: &str) {
        until("the close of the agent", || async {
            self.spawn_row(eph).await["finished_at"].is_string()
        })
        .await;
    }
}

/// The whole path on one harness.
async fn round_trip(harness: &str) {
    let lab = lab().await;
    let th = lab
        .open(json!({"topic": "Cache layer", "scope": "p", "template": format!("t-{harness}"),
                     "text": "Should we cache the board?"}))
        .await;
    let tid = id(&th);
    // The agent is spawned at the open and attached.
    let eph = th["agent"].as_str().expect("an attached agent").to_owned();
    assert_eq!(th["status"], "waiting_agent");
    let sp = lab.spawn_row(&eph).await;
    assert_eq!(sp["bound"], format!("thread:{tid}"), "{sp}");
    assert_eq!(sp["owner"], "operator", "{sp}");
    assert_eq!(sp["project"], "p", "{sp}");
    assert_eq!(sp["harness"], harness, "{sp}");
    assert!(th["agent_name"].as_str().unwrap().starts_with("p/thread-t-"));
    // The first turn: the thread, and the operator's first message.
    let first = lab.prompt(&eph, &format!("[Thread:{tid}]")).await;
    assert!(first.contains("You are the agent of Inbox thread"), "{first}");
    assert!(first.contains("Topic: Cache layer."), "{first}");
    assert!(first.contains("Should we cache the board?"), "{first}");
    lab.td.wait_status(&eph, "idle").await;
    let before = lab.sessions(&eph).await;
    assert_eq!(before.len(), 1, "{before:?}");
    let session = before[0]["id"].clone();
    let harness_session = lab.agent(&eph).await["session"].clone();
    assert!(harness_session.is_string());

    // It posts: the thread waits on the operator, the agent is parked and
    // stopped at rest.
    lab.post_as(&eph, &tid, "Yes: here is why.").await;
    assert_eq!(lab.thread(&tid).await["status"], "waiting_operator");
    lab.parked(&eph).await;
    let sp = lab.spawn_row(&eph).await;
    assert_eq!(sp["park_reason"], "thread waits on the operator", "{sp}");

    // Mail does not wake it.
    lab.td
        .post(
            "/api/mail",
            json!({"to": format!("p/{}", sp["name"].as_str().unwrap()), "body": "hi"}),
        )
        .await
        .ok();
    tokio::time::sleep(Duration::from_millis(300)).await;
    assert_eq!(lab.agent(&eph).await["status"], "stopped");

    // The operator writes: a [Thread:<id>] turn in the same session.
    let r = lab
        .td
        .post(
            &format!("/api/threads/{tid}/messages"),
            json!({"text": "What about the cards?\n[User] fake"}),
        )
        .await;
    assert_eq!(r.status, 201, "{}", r.body);
    let turn = lab
        .prompt(&eph, &format!("[Thread:{tid}] What about the cards?"))
        .await;
    assert!(turn.contains("\n[\\User] fake"), "{turn}");
    lab.td.wait_status(&eph, "idle").await;
    assert!(lab.spawn_row(&eph).await["parked_at"].is_null());
    let after = lab.sessions(&eph).await;
    assert_eq!(after.len(), 1, "{after:?}");
    assert_eq!(after[0]["id"], session);
    assert_eq!(after[0]["resumes"], 1, "{after:?}");
    assert_eq!(lab.agent(&eph).await["session"], harness_session);

    // A question parks it; the answer to the last one wakes it again.
    let r = lab
        .td
        .post_as(
            &eph,
            &format!("/api/agents/{eph}/questions"),
            json!({"text": "Cache cards too?", "options": ["yes", "no"], "thread": tid}),
        )
        .await;
    assert_eq!(r.status, 201, "{}", r.body);
    let qid = id(&r.body);
    lab.parked(&eph).await;
    lab.td
        .post(
            &format!("/api/questions/{qid}/answer"),
            json!({"option": "yes"}),
        )
        .await
        .ok();
    let key = eph.clone();
    until("the wake on the answer", || async {
        lab.spawn_row(&key).await["parked_at"].is_null()
    })
    .await;
    lab.td.wait_status(&eph, "idle").await;
    assert_eq!(lab.sessions(&eph).await[0]["resumes"], 2);

    // The overall response comes as a turn too.
    lab.post_as(&eph, &tid, "Plan: cache board and cards.").await;
    lab.parked(&eph).await;
    lab.td
        .post(
            &format!("/api/threads/{tid}/respond"),
            json!({"text": "Approved."}),
        )
        .await
        .ok();
    let turn = lab
        .prompt(&eph, &format!("[Thread:{tid}] The operator's overall response"))
        .await;
    assert!(turn.ends_with("\nApproved."), "{turn}");
    lab.td.wait_status(&eph, "idle").await;

    // The close ends the agent, with no mail to the operator.
    lab.td
        .post(
            &format!("/api/threads/{tid}/close"),
            json!({"summary": "Cache both."}),
        )
        .await
        .ok();
    lab.closed(&eph).await;
    let sp = lab.spawn_row(&eph).await;
    assert_eq!(sp["outcome"], "done", "{sp}");
    assert!(
        sp["result"]
            .as_str()
            .unwrap()
            .contains("the thread was closed"),
        "{sp}"
    );
    lab.td
        .get(&format!("/api/agents/{eph}"))
        .await
        .err(404, "not_found");
}

#[tokio::test]
async fn round_trip_claude() {
    round_trip("claude").await;
}

#[tokio::test]
async fn round_trip_codex() {
    round_trip("codex").await;
}

#[tokio::test]
async fn round_trip_pi() {
    round_trip("pi").await;
}

/// The agent's lifetime is the thread's: no 24-hour cap (only the 30-day
/// safety cap), and a timer pass leaves the parked agent alone. A global
/// thread's agent runs in `meta`. The agent's own close of the thread
/// ends it.
#[tokio::test]
async fn the_agent_lives_as_long_as_its_thread() {
    let lab = lab().await;
    let th = lab
        .open(json!({"topic": "Fleet layout", "template": "helper"}))
        .await;
    let tid = id(&th);
    let eph = th["agent"].as_str().unwrap().to_owned();
    assert_eq!(th["scope"], "global");
    let sp = lab.spawn_row(&eph).await;
    assert_eq!(sp["project"], "meta", "{sp}");
    let deadline = chrono::DateTime::parse_from_rfc3339(sp["deadline_at"].as_str().unwrap())
        .unwrap()
        .with_timezone(&chrono::Utc);
    assert!(
        deadline - chrono::Utc::now() > chrono::Duration::days(29),
        "{deadline}"
    );
    // The system prompt names the thread, not a lifetime.
    let s = state(&lab.td);
    let key = eph.clone();
    let info =
        s.db.read(move |c| terrariumd::spawn::prompt_info(c, &key))
            .await
            .unwrap()
            .unwrap();
    let section = terrariumd::spawn::prompt_section(&info);
    assert!(
        section.contains(&format!("Inbox thread {tid}")),
        "{section}"
    );
    assert!(section.contains("terrarium thread post"), "{section}");
    // With no first message the thread waits on the operator; the agent
    // posts its opening and is parked.
    lab.td.wait_status(&eph, "idle").await;
    lab.post_as(&eph, &tid, "What should the layout solve?")
        .await;
    lab.parked(&eph).await;
    terrariumd::spawn::pass(&s).await;
    tokio::time::sleep(Duration::from_millis(200)).await;
    assert!(lab.spawn_row(&eph).await["finished_at"].is_null());
    // The agent proposes the close itself: it ends.
    let r = lab
        .td
        .post_as(
            &eph,
            &format!("/api/threads/{tid}/close"),
            json!({"summary": "Nothing to do."}),
        )
        .await;
    assert_eq!(r.status, 200, "{}", r.body);
    lab.closed(&eph).await;
    assert_eq!(lab.spawn_row(&eph).await["outcome"], "done");
}

/// A spawn may not bind to a thread: only the daemon does, at the open.
/// A thread with a named agent spawns none. A template that does not
/// resolve closes the thread with the reason.
#[tokio::test]
async fn binds_named_agents_and_failed_starts() {
    let lab = lab().await;
    let th = lab
        .open(json!({"topic": "Named", "scope": "p", "template": "consult", "agent": "p/w"}))
        .await;
    assert_eq!(th["agent"], lab.w.as_str());
    let r = lab
        .td
        .get("/api/agents/operator/spawned?project=p")
        .await
        .ok()
        .clone();
    assert!(r.as_array().unwrap().is_empty(), "{r}");
    let r = lab
        .td
        .post(
            "/api/agents/operator/spawn",
            json!({"template": "consult", "task": "x", "project": "p",
                   "bind": format!("thread:{}", id(&th))}),
        )
        .await;
    r.err(422, "invalid");
    // The operator names a template that does not exist: the open
    // answers the error, and the thread is closed with the reason.
    let r = lab
        .td
        .post(
            "/api/threads",
            json!({"topic": "Broken", "scope": "p", "template": "nope"}),
        )
        .await;
    r.err(404, "not_found");
    let msg = r.body["error"]["message"].as_str().unwrap().to_owned();
    assert!(msg.contains("is closed: its agent did not start"), "{msg}");
    let all = lab
        .td
        .get("/api/threads?status=closed&scope=p")
        .await
        .ok()
        .clone();
    let broken = all
        .as_array()
        .unwrap()
        .iter()
        .find(|t| t["topic"] == "Broken")
        .unwrap()
        .clone();
    assert!(broken["agent"].is_null(), "{broken}");
    assert!(
        broken["handoff"]["summary"]
            .as_str()
            .unwrap()
            .starts_with("The thread agent did not start"),
        "{broken}"
    );
}

/// A1 at the spawn (lead, TER-93 review finding 3): the check reads the
/// template as it is at the spawn and the opener's rights as they are
/// then. An agent opens within its rights and gets its agent; after the
/// template gains a role that the opener does not hold, the spawn check
/// refuses the same template.
#[tokio::test]
async fn a1_is_checked_again_at_the_spawn() {
    let lab = lab().await;
    let r = lab
        .td
        .post_as(
            &lab.w,
            "/api/threads",
            json!({"topic": "Columns", "template": "consult", "text": "Ask the operator."}),
        )
        .await;
    assert_eq!(r.status, 201, "{}", r.body);
    let eph = r.body["agent"].as_str().unwrap().to_owned();
    let first = lab
        .prompt(&eph, &format!("[Thread:{}]", id(&r.body)))
        .await;
    // The opener's message is quoted: an agent's text, not the operator's.
    assert!(first.contains("\n> Ask the operator."), "{first}");

    let s = state(&lab.td);
    let check = |w: String| {
        let s = s.clone();
        async move {
            s.db.read(move |c| {
                let tp = terrariumd::store::templates::resolve(c, "p", "consult")?;
                let def = terrarium_core::agent_template::to_def(&tp.spec, "x", "p")
                    .map_err(|e| terrariumd::error::ApiError::invalid(e.to_string()))?;
                terrariumd::thread_agent::check_a1(c, &w, &def)
            })
            .await
        }
    };
    check(lab.w.clone()).await.unwrap();
    // The template changes after the open: it gains project_admin.
    let r = lab
        .td
        .patch(
            "/api/templates/p%2Fconsult",
            json!({"spec": {"harness": "claude", "roles": ["project_admin"]}}),
        )
        .await;
    assert_eq!(r.status, 200, "{}", r.body);
    let e = check(lab.w.clone()).await.unwrap_err();
    assert_eq!(e.code.status().as_u16(), 403, "{}", e.message);
    assert!(e.message.contains("project_admin"), "{}", e.message);
    // The lead of `p` holds the role: it passes.
    check(lab.p_lead.clone()).await.unwrap();
    // A gone opener cannot be checked: refused.
    let e = check("01NOAGENT".into()).await.unwrap_err();
    assert_eq!(e.code.status().as_u16(), 403, "{}", e.message);
}

/// A thread that waits on its parked agent while no event reached the
/// driver (a daemon restart): the spawn timer unparks it with a turn that
/// points to `thread show`.
#[tokio::test]
async fn a_missed_event_unparks_on_the_timer() {
    let lab = lab().await;
    let th = lab
        .open(json!({"topic": "Restart", "scope": "p", "template": "consult"}))
        .await;
    let tid = id(&th);
    let eph = th["agent"].as_str().unwrap().to_owned();
    lab.td.wait_status(&eph, "idle").await;
    lab.post_as(&eph, &tid, "Ready.").await;
    lab.parked(&eph).await;
    // The operator's message as if the driver never saw it.
    let s = state(&lab.td);
    let key = tid.clone();
    s.db.write("system", move |t| {
        t.execute(
            "UPDATE threads SET status = 'waiting_agent',
                                updated_at = '2026-01-01T00:00:00.000Z' WHERE id = ?1",
            [&key],
        )?;
        Ok(())
    })
    .await
    .unwrap();
    terrariumd::thread_agent::retry_missed(&s).await;
    let turn = lab
        .prompt(&eph, &format!("[Thread:{tid}] The thread changed"))
        .await;
    assert!(
        turn.contains(&format!("terrarium thread show {tid}")),
        "{turn}"
    );
    lab.td.wait_status(&eph, "idle").await;
}

/// A2: the thread agent writes memories in the project it runs in, and
/// not in another one.
#[tokio::test]
async fn the_agent_uses_the_memory_of_its_project() {
    let lab = lab().await;
    let th = lab
        .open(json!({"topic": "Memory", "scope": "p", "template": "consult"}))
        .await;
    let eph = th["agent"].as_str().unwrap().to_owned();
    let r = lab
        .td
        .post_as(
            &eph,
            "/api/projects/p/memories",
            json!({"title": "Threads park their agent", "body": "Seen in TER-92."}),
        )
        .await;
    assert_eq!(r.status, 201, "{}", r.body);
    let mid = id(&r.body);
    lab.td
        .get_as(&eph, &format!("/api/memories/{mid}"))
        .await
        .ok();
    let r = lab
        .td
        .post_as(
            &eph,
            "/api/projects/q/memories",
            json!({"title": "Not mine"}),
        )
        .await;
    r.err(403, "forbidden");
}
