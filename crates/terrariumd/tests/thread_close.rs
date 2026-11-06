//! The close of an Inbox thread (TER-95, wiki `inbox-threads` §5,
//! docs/api.md §6.13), with the fake supervisor: the agent proposes the
//! close and the operator confirms it; the operator writes on (a decline)
//! or closes at any time; at the close the agent ends, its session is
//! archived and linked from the thread and the handoff cards. An untouched
//! thread gets one Inbox reminder and is closed as stale, with short test
//! settings. No harness and no model turn runs.
#![allow(clippy::unwrap_used, clippy::expect_used)]

mod common;

use std::path::Path;
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
    /// `p/lead`, the report-to lead of the `p` threads.
    lead: String,
}

/// Projects `meta` and `p`, each with a lead, the template `consult`
/// (claude, a member of `p`), and the member `p/w`.
async fn lab() -> Lab {
    let td = TestDaemon::start().await;
    fast(&td);
    for p in ["meta", "p"] {
        td.project(p).await;
    }
    td.agent("meta", "lead", json!({"roles": ["project_admin"]}))
        .await;
    let lead = id(&td
        .agent("p", "lead", json!({"roles": ["project_admin"]}))
        .await);
    let r = td
        .post(
            "/api/templates",
            json!({"name": "consult", "project": "p", "spec": {"harness": "claude"}}),
        )
        .await;
    assert_eq!(r.status, 201, "{}", r.body);
    td.agent("p", "w", json!({})).await;
    Lab { td, lead }
}

impl Lab {
    async fn thread(&self, tid: &str) -> Value {
        self.td
            .get(&format!("/api/threads/{tid}"))
            .await
            .ok()
            .clone()
    }

    async fn agent(&self, eph: &str) -> Value {
        self.td
            .get(&format!("/api/agents/{eph}"))
            .await
            .ok()
            .clone()
    }

    async fn spawn_row(&self, eph: &str) -> Value {
        self.td
            .get(&format!("/api/spawns/{eph}"))
            .await
            .ok()
            .clone()
    }

    async fn parked(&self, eph: &str) {
        until("the park stop", || async {
            let a = self.agent(eph).await;
            a["status"] == "stopped" && a["status_reason"] == "parked"
        })
        .await;
    }

    async fn closed(&self, eph: &str) {
        until("the close of the agent", || async {
            self.spawn_row(eph).await["finished_at"].is_string()
        })
        .await;
    }

    /// Wait for a prompt of `eph` that starts with `start`.
    async fn prompt(&self, eph: &str, start: &str) {
        let aid: AgentId = eph.parse().unwrap();
        until(&format!("a turn `{start}`"), || async {
            self.td
                .sup
                .prompts(aid)
                .iter()
                .any(|p| p.starts_with(start))
        })
        .await;
    }

    /// A thread of `p` with a spawned agent, past its first turn, with a
    /// claude session file: the thread ID, the agent, and its session.
    async fn started(&self, topic: &str) -> (String, String, String) {
        let r = self
            .td
            .post(
                "/api/threads",
                json!({"topic": topic, "scope": "p", "template": "consult",
                       "text": "Plan the cache."}),
            )
            .await;
        assert_eq!(r.status, 201, "{}", r.body);
        let tid = id(&r.body);
        let eph = r.body["agent"].as_str().unwrap().to_owned();
        self.prompt(&eph, &format!("[Thread:{tid}]")).await;
        self.td.wait_status(&eph, "idle").await;
        let mut session = Value::Null;
        for _ in 0..3000 {
            session = self
                .td
                .get(&format!("/api/agents/{eph}/sessions"))
                .await
                .ok()[0]
                .clone();
            if session["harness_session_id"].is_string() {
                break;
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
        assert!(session["harness_session_id"].is_string(), "{session}");
        let file = common::claude_project_dir(&self.td.state, &eph).join(format!(
            "{}.jsonl",
            session["harness_session_id"].as_str().unwrap()
        ));
        std::fs::write(file, fixture()).unwrap();
        (tid, eph, id(&session))
    }

    async fn card(&self, title: &str) -> String {
        id(self
            .td
            .post("/api/projects/p/cards", json!({"title": title}))
            .await
            .ok())
    }

    async fn propose(&self, eph: &str, tid: &str, body: Value) -> Value {
        let r = self
            .td
            .post_as(eph, &format!("/api/threads/{tid}/close"), body)
            .await;
        assert_eq!(r.status, 200, "{}", r.body);
        r.body
    }
}

/// The Claude fixture of the adapter: 30 blocks.
fn fixture() -> String {
    std::fs::read_to_string(
        Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("../adapters/claude/tests/fixtures/transcript.jsonl"),
    )
    .unwrap()
}

/// The archived session `sid` of `eph`: the session reads as removed, its
/// transcript comes from the archive, and the thread and each handoff
/// card link to it.
async fn archived_and_linked(lab: &Lab, tid: &str, eph: &str, sid: &str, cards: &[&str]) {
    lab.closed(eph).await;
    lab.td
        .get(&format!("/api/agents/{eph}"))
        .await
        .err(404, "not_found");
    let th = lab.thread(tid).await;
    assert_eq!(th["session_id"], sid, "{th:#}");
    let key = sid.to_owned();
    // The delete marks the session removed first; the archive copy of its
    // file follows.
    until("the archived session", || async {
        lab.td.get(&format!("/api/sessions/{key}")).await.ok()["removed"] == true
            && lab
                .td
                .get(&format!("/api/sessions/{key}/transcript"))
                .await
                .status
                == 200
    })
    .await;
    let t = lab.td.get(&format!("/api/sessions/{sid}/transcript")).await;
    assert_eq!(t.ok()["blocks"].as_array().unwrap().len(), 30);
    for card in cards {
        let list = lab.td.get(&format!("/api/cards/{card}/sessions")).await;
        let list = list.ok().as_array().unwrap().clone();
        assert!(list.iter().any(|s| s["id"] == sid), "{card}: {list:?}");
        let c = lab.td.get(&format!("/api/cards/{card}")).await.ok().clone();
        let comment = c["comments"]
            .as_array()
            .unwrap()
            .iter()
            .find(|m| m["author"] == "system")
            .cloned()
            .unwrap_or_else(|| panic!("no system comment on {card}: {c:#}"));
        let body = comment["body"].as_str().unwrap();
        assert!(
            body.contains(&format!("Inbox thread {tid} was closed")),
            "{body}"
        );
        assert!(body.contains(&format!("/sessions/{sid}")), "{body}");
        assert_eq!(comment["session"]["id"], sid, "{comment}");
    }
}

#[tokio::test]
async fn the_agent_proposes_and_the_operator_confirms() {
    let lab = lab().await;
    let (tid, eph, sid) = lab.started("Cache layer").await;
    let (c1, c2) = (
        lab.card("Cache the board").await,
        lab.card("Cache cards").await,
    );

    // The agent's close is a proposal: the thread stays open and waits on
    // the operator, and the agent parks.
    let th = lab
        .propose(
            &eph,
            &tid,
            json!({"summary": "Two cards cut.",
                   "links": [{"kind": "card", "ref": c1}, {"kind": "card", "ref": c2},
                             {"kind": "wiki", "ref": "cache"}]}),
        )
        .await;
    assert_eq!(th["status"], "waiting_operator", "{th:#}");
    assert!(th["closed_at"].is_null());
    assert_eq!(th["close_proposal"]["summary"], "Two cards cut.");
    assert_eq!(th["close_proposal"]["by"], eph.as_str());
    assert_eq!(th["close_proposal"]["links"].as_array().unwrap().len(), 3);
    lab.parked(&eph).await;
    assert!(lab.spawn_row(&eph).await["finished_at"].is_null());
    // The list and the Inbox show the proposal.
    let list = lab.td.get("/api/threads").await.ok().clone();
    assert_eq!(list[0]["close_proposal"]["summary"], "Two cards cut.");
    let inbox = lab.td.get("/api/inbox?kind=thread").await.ok().clone();
    let item = &inbox.as_array().unwrap()[0];
    assert_eq!(item["open"], true, "{item}");

    // Only the operator confirms, with the proposed handoff alone.
    lab.td
        .post_as(
            &eph,
            &format!("/api/threads/{tid}/close"),
            json!({"confirm": true}),
        )
        .await
        .err(403, "forbidden");
    lab.td
        .post(
            &format!("/api/threads/{tid}/close"),
            json!({"confirm": true, "summary": "other"}),
        )
        .await
        .err(422, "invalid");
    let th = lab
        .td
        .post(
            &format!("/api/threads/{tid}/close"),
            json!({"confirm": true}),
        )
        .await
        .ok()
        .clone();
    assert_eq!(th["status"], "closed", "{th:#}");
    assert_eq!(th["close_reason"], "confirmed");
    assert_eq!(th["closed_by"], "operator");
    assert_eq!(th["handoff"]["summary"], "Two cards cut.");
    assert_eq!(th["handoff"]["by"], eph.as_str());
    assert_eq!(th["handoff"]["links"][0]["ref"], c1.as_str());
    assert!(th["close_proposal"].is_null());
    assert_eq!(th["session_id"], sid.as_str());

    // The agent ends; its session is archived and linked.
    archived_and_linked(&lab, &tid, &eph, &sid, &[&c1, &c2]).await;
    let sp = lab.spawn_row(&eph).await;
    assert_eq!(sp["outcome"], "done", "{sp}");
    // The thread stays readable with the closed filter.
    let closed = lab.td.get("/api/threads?status=closed").await.ok().clone();
    assert_eq!(closed[0]["id"], tid.as_str());
    lab.td
        .post(
            &format!("/api/threads/{tid}/close"),
            json!({"confirm": true}),
        )
        .await
        .err(409, "conflict");
    // The audit has the proposal and the close.
    for action in ["thread.propose_close", "thread.close"] {
        let rows = lab
            .td
            .get(&format!("/api/audit?action={action}"))
            .await
            .ok()
            .clone();
        assert!(!rows.as_array().unwrap().is_empty(), "{action}");
    }
}

#[tokio::test]
async fn the_operator_declines_and_closes_at_any_time() {
    let lab = lab().await;
    let (tid, eph, sid) = lab.started("Backups").await;
    let card = lab.card("Daily backups").await;
    lab.propose(&eph, &tid, json!({"summary": "Done."})).await;
    lab.parked(&eph).await;
    // The operator writes on: the proposal is declined and the agent
    // starts again with the message.
    let th = lab
        .td
        .post(
            &format!("/api/threads/{tid}/messages"),
            json!({"text": "Not yet: what about restores?"}),
        )
        .await
        .ok()
        .clone();
    assert!(th["close_proposal"].is_null(), "{th:#}");
    assert_eq!(th["status"], "waiting_agent");
    lab.prompt(&eph, &format!("[Thread:{tid}] Not yet")).await;
    lab.td.wait_status(&eph, "idle").await;
    lab.td
        .post(
            &format!("/api/threads/{tid}/close"),
            json!({"confirm": true}),
        )
        .await
        .err(409, "conflict");
    // A new proposal replaces the old one; the response declines it too.
    lab.propose(&eph, &tid, json!({"summary": "First."})).await;
    let th = lab.propose(&eph, &tid, json!({"summary": "Second."})).await;
    assert_eq!(th["close_proposal"]["summary"], "Second.");
    let th = lab
        .td
        .post(
            &format!("/api/threads/{tid}/respond"),
            json!({"text": "Approved: restores weekly."}),
        )
        .await
        .ok()
        .clone();
    assert!(th["close_proposal"].is_null(), "{th:#}");
    lab.td.wait_status(&eph, "idle").await;

    // The operator closes while the thread waits on the agent, with a
    // handoff of its own.
    let th = lab
        .td
        .post(
            &format!("/api/threads/{tid}/close"),
            json!({"summary": "Closed by me.", "links": [{"kind": "card", "ref": card}]}),
        )
        .await
        .ok()
        .clone();
    assert_eq!(th["status"], "closed");
    assert_eq!(th["close_reason"], "operator");
    assert_eq!(th["handoff"]["summary"], "Closed by me.");
    archived_and_linked(&lab, &tid, &eph, &sid, &[&card]).await;
}

#[tokio::test]
async fn untouched_threads_get_a_reminder_then_close_as_stale() {
    let lab = lab().await;
    let (tid, eph, sid) = lab.started("Old question").await;
    let card = lab.card("Old work").await;
    // A thread that waits on its agent is never untouched (the named
    // agent `p/w` does not run).
    let busy = lab
        .td
        .post(
            "/api/threads",
            json!({"topic": "Busy", "scope": "p", "agent": "p/w", "text": "Go."}),
        )
        .await;
    assert_eq!(busy.status, 201, "{}", busy.body);
    let busy = id(&busy.body);
    lab.propose(
        &eph,
        &tid,
        json!({"summary": "Proposed.", "links": [{"kind": "card", "ref": card}]}),
    )
    .await;
    lab.parked(&eph).await;

    // Short settings: the reminder after 300 ms, the stale close after 3 s.
    state(&lab.td)
        .lifecycle
        .threads
        .set_tuning(terrariumd::thread_close::Tuning {
            reminder_after: Duration::from_millis(300),
            stale_after: Duration::from_secs(3),
        });
    let reminders = || async {
        lab.td
            .get("/api/mail?to=operator&from=system")
            .await
            .ok()
            .as_array()
            .unwrap()
            .iter()
            .filter(|m| {
                m["subject"]
                    .as_str()
                    .is_some_and(|s| s.starts_with("Reminder: thread"))
            })
            .cloned()
            .collect::<Vec<_>>()
    };
    until("the reminder", || async { !reminders().await.is_empty() }).await;
    let th = lab.thread(&tid).await;
    assert!(th["reminded_at"].is_string(), "{th:#}");
    assert_eq!(th["status"], "waiting_operator");
    let mail = reminders().await;
    let body = mail[0]["body"].as_str().unwrap();
    assert!(body.contains(&tid), "{body}");
    assert!(body.contains("--confirm"), "{body}");
    // One reminder only, and none for the thread that waits on its agent.
    tokio::time::sleep(Duration::from_millis(600)).await;
    assert_eq!(reminders().await.len(), 1);
    assert!(lab.thread(&busy).await["reminded_at"].is_null());

    // The stale close, with a mail to the report-to lead.
    until("the stale close", || async {
        lab.thread(&tid).await["status"] == "closed"
    })
    .await;
    let th = lab.thread(&tid).await;
    assert_eq!(th["close_reason"], "stale", "{th:#}");
    assert_eq!(th["closed_by"], "system");
    let summary = th["handoff"]["summary"].as_str().unwrap();
    assert!(summary.starts_with("Closed as stale"), "{summary}");
    assert!(summary.contains("Proposed."), "{summary}");
    let to_lead = lab.td.mail_of(&lab.lead).await;
    let stale = to_lead
        .iter()
        .find(|m| m["subject"] == format!("Thread {tid} closed as stale"))
        .unwrap_or_else(|| panic!("no stale mail: {to_lead:?}"));
    assert_eq!(stale["from"], "system");
    let body = stale["body"].as_str().unwrap();
    assert!(body.contains(&format!("/sessions/{sid}")), "{body}");
    archived_and_linked(&lab, &tid, &eph, &sid, &[&card]).await;
    // The busy thread stays open, and `p/w` got no mail about it.
    assert_eq!(lab.thread(&busy).await["status"], "waiting_agent");
}

#[tokio::test]
async fn activity_after_a_reminder_allows_a_new_one_and_off_sends_none() {
    let lab = lab().await;
    let (tid, eph, _) = lab.started("Quiet").await;
    lab.td
        .post_as(
            &eph,
            &format!("/api/threads/{tid}/messages"),
            json!({"text": "Any news?"}),
        )
        .await
        .ok();
    let s = state(&lab.td);
    s.lifecycle
        .threads
        .set_tuning(terrariumd::thread_close::Tuning {
            reminder_after: Duration::from_millis(200),
            stale_after: Duration::ZERO,
        });
    let count = || async {
        lab.td
            .get("/api/mail?to=operator&from=system")
            .await
            .ok()
            .as_array()
            .unwrap()
            .iter()
            .filter(|m| {
                m["subject"]
                    .as_str()
                    .is_some_and(|s| s.starts_with("Reminder: thread"))
            })
            .count()
    };
    until("the first reminder", || async { count().await == 1 }).await;
    // The agent writes again: new activity, so a new reminder later.
    lab.td.wait_status(&eph, "stopped").await;
    lab.td
        .post(
            &format!("/api/threads/{tid}/messages"),
            json!({"text": "Soon."}),
        )
        .await
        .ok();
    lab.td.wait_status(&eph, "idle").await;
    lab.td
        .post_as(
            &eph,
            &format!("/api/threads/{tid}/messages"),
            json!({"text": "Waiting."}),
        )
        .await
        .ok();
    until("the second reminder", || async { count().await == 2 }).await;
    // `0` for the stale close: the thread stays open.
    tokio::time::sleep(Duration::from_millis(400)).await;
    assert_eq!(lab.thread(&tid).await["status"], "waiting_operator");
    // Both off: nothing more.
    s.lifecycle
        .threads
        .set_tuning(terrariumd::thread_close::Tuning {
            reminder_after: Duration::ZERO,
            stale_after: Duration::ZERO,
        });
    lab.td
        .post(
            &format!("/api/threads/{tid}/messages"),
            json!({"text": "x"}),
        )
        .await
        .ok();
    lab.td
        .post_as(
            &eph,
            &format!("/api/threads/{tid}/messages"),
            json!({"text": "y"}),
        )
        .await
        .ok();
    tokio::time::sleep(Duration::from_millis(500)).await;
    assert_eq!(count().await, 2);
}
