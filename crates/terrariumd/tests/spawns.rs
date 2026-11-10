//! Ephemeral agents (PLAN §18, "Ephemeral agents", docs/api.md §6.10),
//! with the fake supervisor: the spawn rights, the spawn, `agent done`,
//! the wait and the mail of the result, the lifetime, the cancel, the
//! removal of the owner (also down two levels), the lists that leave the
//! ephemeral agents out, and `GET /api/agents/{id}/spawned`. No harness
//! and no model turn runs.
#![allow(clippy::unwrap_used, clippy::expect_used)]

use crate::common;

use std::time::Duration;

use common::TestDaemon;
use serde_json::{Value, json};
use terrarium_core::AgentId;
use terrarium_core::config::{CompactMaxAge, IdleStop};

fn state(td: &TestDaemon) -> terrariumd::api::Shared {
    td.daemon.as_ref().unwrap().state.clone()
}

/// Short times: the lifecycle tick, the close after `done`, the waits.
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

/// The longest wait of [`until`]. A deadline, not a count of tries: on a
/// loaded machine each GET is slow (TER-108, TER-109). Only a failing
/// test waits it out.
const UNTIL: Duration = Duration::from_secs(30);

async fn until<F: std::future::Future<Output = bool>>(what: &str, mut f: impl FnMut() -> F) {
    let start = std::time::Instant::now();
    while !f().await {
        assert!(start.elapsed() < UNTIL, "timed out waiting for {what}");
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
}

/// Wait until agent `a` has `status`, with the deadline of [`until`].
async fn status_is(td: &TestDaemon, a: &str, status: &str) {
    until(&format!("{a} {status}"), || async {
        td.get(&format!("/api/agents/{a}")).await.body["status"] == status
    })
    .await;
}

/// A project `p` with the templates `worker` (project) and `helper`
/// (global), a lead, and a worker `w` that may spawn `worker`.
async fn setup() -> (TestDaemon, String, String) {
    let td = TestDaemon::start().await;
    fast(&td);
    td.project("p").await;
    td.post(
        "/api/templates",
        json!({"name": "worker", "project": "p", "spec": {"harness": "claude"}}),
    )
    .await
    .ok();
    td.post(
        "/api/templates",
        json!({"name": "helper", "spec": {"harness": "claude"}}),
    )
    .await
    .ok();
    let lead = id(&td
        .agent("p", "lead", json!({"roles": ["project_admin"]}))
        .await);
    let w = id(&td
        .agent(
            "p",
            "w",
            json!({"policy": {"spawn": {"templates": ["worker"], "max_concurrent": 2}}}),
        )
        .await);
    (td, lead, w)
}

async fn spawn_as(td: &TestDaemon, owner: &str, body: Value) -> common::Resp {
    td.post_as(owner, &format!("/api/agents/{owner}/spawn"), body)
        .await
}

async fn research_setup() -> (TestDaemon, String, String) {
    let (td, _, root) = setup().await;
    td.patch(
        &format!("/api/agents/{root}"),
        json!({"policy": {"spawn": {
            "templates": ["*"], "max_concurrent": 10, "may_spawn_children": true, "bound": true
        }}}),
    )
    .await
    .ok();
    td.post(
        "/api/templates",
        json!({"name": "card-research", "project": "p", "spec": {"harness": "claude"}}),
    )
    .await
    .ok();
    let card = id(td
        .post("/api/projects/p/cards", json!({"title": "Research this"}))
        .await
        .ok());
    let parent = id(spawn_as(
        &td,
        &root,
        json!({"template": "worker", "task": "implement", "bind": format!("card:{card}")}),
    )
    .await
    .ok());
    td.post(
        &format!("/api/cards/{card}/assign"),
        json!({"assignee": parent}),
    )
    .await
    .ok();
    (td, parent, card)
}

#[tokio::test]
async fn research_helpers_are_bounded_one_level_and_report_on_card() {
    let (td, parent, card) = research_setup().await;
    let body = json!({"template": "card-research", "task": "answer a bounded question"});
    spawn_as(
        &td,
        &parent,
        json!({"template": "worker", "task": "not research"}),
    )
    .await
    .err(403, "forbidden");
    let a = id(spawn_as(&td, &parent, body.clone()).await.ok());
    let b = id(spawn_as(&td, &parent, body.clone()).await.ok());
    spawn_as(&td, &a, body.clone()).await.err(403, "forbidden");
    for _ in 0..2 {
        spawn_as(&td, &parent, body.clone())
            .await
            .err(409, "conflict");
    }
    let alerts: i64 = state(&td)
        .db
        .read(move |c| {
            Ok(c.query_row(
                "SELECT count(*) FROM events WHERE kind = 'card.research_helper_limit'",
                [],
                |r| r.get(0),
            )?)
        })
        .await
        .unwrap();
    assert_eq!(alerts, 1);
    td.post_as(
        &a,
        &format!("/api/agents/{a}/done"),
        json!({"result": "The answer is in spawn.rs"}),
    )
    .await
    .ok();
    finished(&td, &a).await;
    let c = card.clone();
    let comments: String = state(&td).db.read(move |db| Ok(db.query_row("SELECT body FROM card_comments WHERE card = ?1 AND body LIKE '%The answer is in spawn.rs%'", [c], |r| r.get(0))?)).await.unwrap();
    assert!(comments.contains("Research helper"));
    spawn_as(&td, &parent, body).await.ok();
    assert!(spawn_row(&td, &b).await["finished_at"].is_null());
}

#[tokio::test]
async fn research_helpers_daily_history_and_research_wip() {
    let (td, parent, card) = research_setup().await;
    let body = json!({"template": "card-research", "task": "research"});
    state(&td).db.write("system", |t| {
        // Occupied research cards share capacity with helpers.
        t.execute("UPDATE board_columns SET name = 'research', wip_limit = 1 WHERE project = 'p' AND name = 'backlog'", [])?;
        Ok(())
    }).await.unwrap();
    spawn_as(&td, &parent, body.clone())
        .await
        .err(409, "conflict");
    state(&td).db.write("system", move |t| {
        t.execute("UPDATE board_columns SET name = 'backlog' WHERE project = 'p' AND name = 'research'", [])?;
        for n in 0..20 {
            // Finished and removed helper agents still count today.
            t.execute("INSERT INTO research_helpers(id, card, project, spawned_at, finished_at) VALUES (?1, ?2, 'p', ?3, ?3)", rusqlite::params![format!("history-{n}"), card, terrariumd::db::now()])?;
        }
        Ok(())
    }).await.unwrap();
    spawn_as(&td, &parent, body.clone())
        .await
        .err(409, "conflict");
    state(&td)
        .db
        .write("system", |t| {
            t.execute(
                "UPDATE research_helpers SET spawned_at = '2000-01-01T00:00:00Z'",
                [],
            )?;
            Ok(())
        })
        .await
        .unwrap();
    spawn_as(&td, &parent, body).await.ok();
}

#[tokio::test]
async fn research_helpers_daily_admission_is_atomic_and_project_scoped() {
    let (td, parent, card) = research_setup().await;
    state(&td).db.write("system", move |t| {
        for n in 0..19 {
            t.execute("INSERT INTO research_helpers(id, card, project, spawned_at, finished_at) VALUES (?1, ?2, 'p', ?3, ?3)", rusqlite::params![format!("history-{n}"), card, terrariumd::db::now()])?;
        }
        for n in 0..20 {
            t.execute("INSERT INTO research_helpers(id, card, project, spawned_at, finished_at) VALUES (?1, 'OTHER-1', 'other', ?2, ?2)", rusqlite::params![format!("other-{n}"), terrariumd::db::now()])?;
        }
        Ok(())
    }).await.unwrap();
    let body = json!({"template": "card-research", "task": "research"});
    let (a, b) = tokio::join!(
        spawn_as(&td, &parent, body.clone()),
        spawn_as(&td, &parent, body)
    );
    let mut codes = [a.status, b.status];
    codes.sort();
    assert_eq!(codes, [201, 409], "{} {}", a.body, b.body);
}

#[tokio::test]
async fn research_card_moves_count_live_helpers() {
    let (td, parent, _) = research_setup().await;
    spawn_as(
        &td,
        &parent,
        json!({"template": "card-research", "task": "research"}),
    )
    .await
    .ok();
    state(&td).db.write("system", |t| {
        t.execute("UPDATE board_columns SET name = 'research', wip_limit = 1 WHERE project = 'p' AND name = 'ready'", [])?;
        Ok(())
    }).await.unwrap();
    let card = id(td
        .post("/api/projects/p/cards", json!({"title": "Research queued"}))
        .await
        .ok());
    td.post(
        &format!("/api/cards/{card}/move"),
        json!({"column": "research"}),
    )
    .await
    .err(409, "wip_limit");
}

async fn spawn_ok(td: &TestDaemon, owner: &str, task: &str) -> Value {
    let r = spawn_as(td, owner, json!({"template": "worker", "task": task})).await;
    assert_eq!(r.status, 201, "{}", r.body);
    r.body
}

async fn spawn_row(td: &TestDaemon, id: &str) -> Value {
    td.get(&format!("/api/spawns/{id}")).await.ok().clone()
}

async fn finished(td: &TestDaemon, id: &str) -> Value {
    let key = id.to_owned();
    until("the spawn finishes", || async {
        spawn_row(td, &key).await["finished_at"].is_string()
    })
    .await;
    spawn_row(td, id).await
}

async fn events(td: &TestDaemon, agent: &str, kind: &str) -> Vec<Value> {
    td.get(&format!(
        "/api/agents/{agent}/events?kind={kind}&limit=1000"
    ))
    .await
    .ok()
    .as_array()
    .unwrap()
    .clone()
}

async fn system_mail(td: &TestDaemon, agent: &str) -> Vec<Value> {
    td.get(&format!("/api/mail?to={agent}&from=system"))
        .await
        .ok()
        .as_array()
        .unwrap()
        .clone()
}

/// The spawn rights: no block, a template not in the list, the two
/// counts, and the operator, who spawns with no policy.
#[tokio::test]
async fn spawn_rights_matrix() {
    let (td, lead, w) = setup().await;
    let other = id(&td.agent("p", "other", json!({})).await);
    let r = spawn_as(&td, &other, json!({"template": "worker", "task": "x"})).await;
    r.err(403, "forbidden");
    assert!(
        r.body["error"]["message"]
            .as_str()
            .unwrap()
            .contains("no spawn rights"),
        "{}",
        r.body
    );
    let r = spawn_as(&td, &w, json!({"template": "helper", "task": "x"})).await;
    r.err(403, "forbidden");
    assert!(
        r.body["error"]["message"]
            .as_str()
            .unwrap()
            .contains("spawn.templates"),
        "{}",
        r.body
    );
    // An empty task, an unknown template.
    spawn_as(&td, &w, json!({"template": "worker", "task": "  "}))
        .await
        .err(422, "invalid");
    assert_eq!(
        spawn_as(&td, &w, json!({"template": "nope", "task": "x"}))
            .await
            .status,
        404
    );
    // max_concurrent = 2.
    spawn_ok(&td, &w, "one").await;
    spawn_ok(&td, &w, "two").await;
    let r = spawn_as(&td, &w, json!({"template": "worker", "task": "three"})).await;
    r.err(409, "conflict");
    assert!(
        r.body["error"]["message"]
            .as_str()
            .unwrap()
            .contains("max_concurrent is 2"),
        "{}",
        r.body
    );
    // max_per_day: the two spawns of today count, running or not.
    td.patch(
        &format!("/api/agents/{w}"),
        json!({"policy": {"spawn": {"templates": ["*"], "max_concurrent": 10,
                                     "max_per_day": 2}}}),
    )
    .await
    .ok();
    let r = spawn_as(&td, &w, json!({"template": "helper", "task": "x"})).await;
    r.err(409, "conflict");
    assert!(
        r.body["error"]["message"]
            .as_str()
            .unwrap()
            .contains("max_per_day is 2"),
        "{}",
        r.body
    );
    // A worker cannot spawn for another agent; the lead can, with the
    // owner's rights.
    td.post_as(
        &other,
        &format!("/api/agents/{w}/spawn"),
        json!({"template": "worker", "task": "x"}),
    )
    .await
    .err(403, "forbidden");
    td.post_as(
        &lead,
        &format!("/api/agents/{w}/spawn"),
        json!({"template": "worker", "task": "x"}),
    )
    .await
    .err(409, "conflict");
    // The operator: for an agent with no spawn block, and for itself.
    let r = td
        .post(
            &format!("/api/agents/{other}/spawn"),
            json!({"template": "helper", "task": "x"}),
        )
        .await;
    assert_eq!(r.status, 201, "{}", r.body);
    assert_eq!(r.body["owner"], other);
    let r = td
        .post(
            "/api/agents/operator/spawn",
            json!({"template": "worker", "task": "x", "project": "p", "name": "op-1"}),
        )
        .await;
    assert_eq!(r.status, 201, "{}", r.body);
    assert_eq!(r.body["owner"], "operator");
    assert_eq!(r.body["name"], "op-1");
    td.post(
        "/api/agents/operator/spawn",
        json!({"template": "worker", "task": "x"}),
    )
    .await
    .err(422, "invalid");
    td.post_as(
        &lead,
        "/api/agents/operator/spawn",
        json!({"template": "worker", "task": "x", "project": "p"}),
    )
    .await
    .err(403, "forbidden");
}

/// Who may set a spawn block: a lead on a member of its project, not on
/// itself, not on an admin, and not `may_spawn_children`; sysadmin and
/// the operator on anyone. A bad block is `422`.
#[tokio::test]
async fn who_sets_spawn_rights() {
    let (td, lead, w) = setup().await;
    let lead2 = id(&td
        .agent("p", "lead2", json!({"roles": ["project_admin"]}))
        .await);
    let spawn = |b: Value| json!({"policy": {"spawn": b}});
    let r = td
        .patch_as(
            &lead,
            &format!("/api/agents/{w}"),
            spawn(
                json!({"templates": ["project:*", "global:helper"], "max_concurrent": 3,
                         "max_lifetime": "30m"}),
            ),
        )
        .await;
    assert_eq!(r.status, 200, "{}", r.body);
    assert_eq!(r.body["policy"]["spawn"]["max_concurrent"], 3);
    for (who, target, body) in [
        (&lead, &lead, spawn(json!({"templates": ["*"]}))),
        (&lead, &lead2, spawn(json!({"templates": ["*"]}))),
        (
            &lead,
            &w,
            spawn(json!({"templates": ["*"], "may_spawn_children": true})),
        ),
    ] {
        td.patch_as(who, &format!("/api/agents/{target}"), body)
            .await
            .err(403, "forbidden");
    }
    for bad in [
        json!({"templates": ["a b"]}),
        json!({"templates": ["*"], "max_concurrent": 0}),
        json!({"templates": ["*"], "max_lifetime": "48h"}),
        json!({"templates": ["*"], "max_lifetime": "soon"}),
    ] {
        td.patch(&format!("/api/agents/{w}"), spawn(bad))
            .await
            .err(422, "invalid");
    }
    // The operator may set anything, also on an admin.
    td.patch(
        &format!("/api/agents/{lead2}"),
        spawn(json!({"templates": ["*"], "may_spawn_children": true})),
    )
    .await
    .ok();
    // A lead that leaves the block as it is changes the other fields.
    let now = td.get(&format!("/api/agents/{w}")).await.ok()["policy"].clone();
    let mut p = now.clone();
    p["targets"] = json!([]);
    td.patch_as(&lead, &format!("/api/agents/{w}"), json!({"policy": p}))
        .await
        .ok();
    // An audit row for each policy write.
    let audit = td.get("/api/audit?action=agent.update&limit=100").await;
    assert!(audit.ok().as_array().unwrap().len() >= 3, "{}", audit.body);
}

/// The spawn makes the agent from the template in the owner's project,
/// starts it with the task as its first turn (tagged with the owner), and
/// writes `agent.spawned` on the owner with the session. The lists leave
/// the ephemeral agent out.
#[tokio::test]
async fn spawn_starts_the_task_and_hides_the_agent() {
    let (td, _lead, w) = setup().await;
    let s = spawn_ok(&td, &w, "Find the bug in foo.rs\nand fix it").await;
    let eph = id(&s);
    assert_eq!(s["name"], "w-worker-1");
    assert_eq!(s["owner"], w);
    assert_eq!(s["owner_name"], "w");
    assert_eq!(s["template"], "worker");
    assert_eq!(s["summary"], "Find the bug in foo.rs");
    assert!(s["session_id"].is_string(), "{s}");
    let aid: AgentId = eph.parse().unwrap();
    until("the task turn", || async {
        !td.sup.prompts(aid).is_empty()
    })
    .await;
    assert_eq!(
        td.sup.prompts(aid)[0],
        "[Agent:p/w] Find the bug in foo.rs\nand fix it"
    );
    let a = td.get(&format!("/api/agents/{eph}")).await.ok().clone();
    assert_eq!(a["ephemeral"], true);
    assert_eq!(a["owner"], w);
    assert_eq!(a["spawn"]["owner_name"], "w");
    assert_eq!(a["spawn"]["template"], "worker");
    assert_eq!(a["idle_stop"], "off");
    // The owner shows the count of its running ephemeral agents.
    let o = td.get(&format!("/api/agents/{w}")).await.ok().clone();
    assert_eq!(o["spawned_running"], 1);
    // The second gets the next number.
    let s2 = spawn_ok(&td, &w, "two").await;
    assert_eq!(s2["name"], "w-worker-2");
    // Lists: not by default; with `include=ephemeral`.
    let names = |v: &Value| -> Vec<String> {
        v.as_array()
            .unwrap()
            .iter()
            .map(|a| a["name"].as_str().unwrap().to_owned())
            .collect()
    };
    let all = td.get("/api/agents?project=p").await.ok().clone();
    assert_eq!(names(&all), ["lead", "w"]);
    let all = td.get("/api/projects/p/agents").await.ok().clone();
    assert_eq!(names(&all), ["lead", "w"]);
    let with = td
        .get("/api/agents?project=p&include=ephemeral")
        .await
        .ok()
        .clone();
    assert_eq!(names(&with).len(), 4);
    // The events of the owner.
    let ev = events(&td, &w, "agent.spawned").await;
    assert_eq!(ev.len(), 2, "{ev:#?}");
    assert_eq!(ev[0]["data"]["name"], "w-worker-1");
    assert_eq!(ev[0]["session_id"], s["session_id"], "{:#}", ev[0]);
    // An ephemeral agent is not paused; the pause of the owner does not
    // touch it.
    td.post(&format!("/api/agents/{eph}/pause"), json!({}))
        .await
        .err(409, "conflict");
    td.post(&format!("/api/agents/{w}/pause"), json!({}))
        .await
        .ok();
    td.wait_status(&eph, "idle").await;
    let a = td.get(&format!("/api/agents/{eph}")).await.ok().clone();
    assert_eq!(a["paused"], false);
}

/// `--wait`: the owner gets a wait; `agent done -m` resolves it with the
/// result, and the wake mail carries it. The agent goes as with `agent
/// rm`, and its session stays for the link.
#[tokio::test]
async fn done_resolves_the_wait_with_the_result() {
    let (td, _lead, w) = setup().await;
    let r = spawn_as(
        &td,
        &w,
        json!({"template": "worker", "task": "Count the files", "wait": true}),
    )
    .await;
    assert_eq!(r.status, 201, "{}", r.body);
    let eph = id(&r.body);
    let wait = r.body["wait"]["id"].as_str().unwrap().to_owned();
    assert_eq!(r.body["wait_id"], wait.as_str(), "{}", r.body);
    let o = td.get(&format!("/api/agents/{w}")).await.ok().clone();
    assert_eq!(o["waits"], 1);
    td.wait_status(&eph, "idle").await;
    // Only the ephemeral agent itself runs `done`.
    td.post_as(&w, &format!("/api/agents/{eph}/done"), json!({}))
        .await
        .err(403, "forbidden");
    td.post(&format!("/api/agents/{eph}/done"), json!({}))
        .await
        .err(403, "forbidden");
    td.post_as(&w, &format!("/api/agents/{w}/done"), json!({}))
        .await
        .err(403, "forbidden");
    let r = td
        .post_as(
            &eph,
            &format!("/api/agents/{eph}/done"),
            json!({"result": "42 files"}),
        )
        .await;
    assert_eq!(r.status, 202, "{}", r.body);
    let sp = finished(&td, &eph).await;
    assert_eq!(sp["outcome"], "done");
    assert_eq!(sp["result"], "42 files");
    assert_eq!(sp["status"], "finished");
    let sid = sp["session_id"].as_str().unwrap().to_owned();
    // The agent is gone; its session row stays.
    assert_eq!(td.get(&format!("/api/agents/{eph}")).await.status, 404);
    let ses = td.get(&format!("/api/sessions/{sid}")).await;
    assert_eq!(ses.status, 200, "{}", ses.body);
    // The wait resolved with the result; the wake mail carries it, and no
    // other mail comes.
    let wt = td.get(&format!("/api/waits/{wait}")).await.ok().clone();
    assert_eq!(wt["state"], "resolved");
    assert_eq!(wt["result"]["outcome"], "done");
    assert_eq!(wt["result"]["result"], "42 files");
    let wk = w.clone();
    until("the wake mail", || async {
        !system_mail(&td, &wk).await.is_empty()
    })
    .await;
    let mail = system_mail(&td, &w).await;
    assert_eq!(mail.len(), 1, "{mail:#?}");
    let body = mail[0]["body"].as_str().unwrap();
    assert!(body.contains("> 42 files"), "{body}");
    assert!(
        body.contains(&format!("session: /sessions/{sid}")),
        "{body}"
    );
    // The finish event on the owner, with the session.
    let ev = events(&td, &w, "agent.finished").await;
    assert_eq!(ev.len(), 1, "{ev:#?}");
    assert_eq!(ev[0]["data"]["outcome"], "done");
    // The event names the session in its data; the link of the log row
    // needs the archived file, which the fake supervisor does not write.
    assert_eq!(ev[0]["data"]["session_id"], sid.as_str());
    // A second `done` has no agent to run it.
    let o = td.get(&format!("/api/agents/{w}")).await.ok().clone();
    assert!(o.get("spawned_running").is_none(), "{o}");
}

/// Without a wait, the owner gets the result as mail.
#[tokio::test]
async fn done_without_a_wait_mails_the_owner() {
    let (td, _lead, w) = setup().await;
    let eph = id(&spawn_ok(&td, &w, "Read the docs").await);
    td.wait_status(&eph, "idle").await;
    td.post_as(
        &eph,
        &format!("/api/agents/{eph}/done"),
        json!({"result": "The docs say yes"}),
    )
    .await
    .ok();
    finished(&td, &eph).await;
    let mail = system_mail(&td, &w).await;
    assert_eq!(mail.len(), 1, "{mail:#?}");
    assert_eq!(mail[0]["subject"], "Spawned w-worker-1 finished: done");
    let body = mail[0]["body"].as_str().unwrap();
    assert!(body.contains("> Read the docs"), "{body}");
    assert!(body.contains("> The docs say yes"), "{body}");
    assert!(body.contains("Session: /sessions/"), "{body}");
}

/// An idle ephemeral agent that has not run `done` stays: no idle stop,
/// no finish. The end of its lifetime closes it (`timeout`).
#[tokio::test]
async fn an_idle_ephemeral_agent_stays_until_its_lifetime() {
    let (td, _lead, w) = setup().await;
    let s = state(&td);
    // A short global idle stop: the ephemeral agent ignores it.
    s.lifecycle.set_tuning(terrariumd::lifecycle::Tuning {
        idle_stop: IdleStop::After(Duration::from_millis(100)),
        tick: Duration::from_millis(20),
        compact_on_idle: false,
        compact_max_age: CompactMaxAge::Off,
        compact_wait: Duration::from_secs(5),
    });
    // The lifetime counts from the spawn, and under load the start alone
    // took up to 16 s (TER-108). So the lifetime is long while the test
    // checks that the agent stays, and the test then ends it: it moves the
    // deadline to now, which the spawn timer reads on each tick.
    td.patch(
        &format!("/api/agents/{w}"),
        json!({"policy": {"spawn": {"templates": ["worker"], "max_lifetime": "1h"}}}),
    )
    .await
    .ok();
    let eph = id(&spawn_ok(&td, &w, "Think").await);
    td.wait_task_done(&eph).await;
    tokio::time::sleep(Duration::from_millis(600)).await;
    let a = td.get(&format!("/api/agents/{eph}")).await.ok().clone();
    assert_eq!(a["status"], "idle", "{a}");
    assert!(spawn_row(&td, &eph).await["finished_at"].is_null());
    let (key, now) = (eph.clone(), terrariumd::waits::from_now(Duration::ZERO));
    s.db.write("operator", move |t| {
        t.execute(
            "UPDATE spawns SET deadline_at = ?2 WHERE id = ?1",
            [&key, &now],
        )?;
        Ok(())
    })
    .await
    .unwrap();
    let sp = finished(&td, &eph).await;
    assert_eq!(sp["outcome"], "timeout");
    let mail = system_mail(&td, &w).await;
    assert_eq!(mail.len(), 1, "{mail:#?}");
    assert_eq!(mail[0]["subject"], "Spawned w-worker-1 finished: timeout");
}

/// A cancel: the owner, a project_admin, or the operator; not another
/// member. `DELETE /api/agents/{id}` of an ephemeral agent is a cancel.
#[tokio::test]
async fn cancel_rights_and_delete() {
    let (td, lead, w) = setup().await;
    let other = id(&td.agent("p", "other", json!({})).await);
    let a = id(&spawn_ok(&td, &w, "one").await);
    let b = id(&spawn_ok(&td, &w, "two").await);
    td.post_as(&other, &format!("/api/agents/{a}/cancel"), json!({}))
        .await
        .err(403, "forbidden");
    let r = td
        .post_as(&w, &format!("/api/agents/{a}/cancel"), json!({}))
        .await;
    assert_eq!(r.ok()["outcome"], "cancelled");
    // A project_admin: with the cancel, or with a delete.
    assert_eq!(
        td.req(
            hyper::Method::DELETE,
            &format!("/api/agents/{b}"),
            None,
            Some(&lead)
        )
        .await
        .status,
        204
    );
    assert_eq!(spawn_row(&td, &b).await["outcome"], "cancelled");
    td.post_as(&lead, &format!("/api/agents/{b}/cancel"), json!({}))
        .await
        .err(404, "not_found");
    // The operator; a normal agent is not cancelled.
    let c = id(&spawn_ok(&td, &w, "three").await);
    td.post(&format!("/api/agents/{c}/cancel"), json!({}))
        .await
        .ok();
    td.post(&format!("/api/agents/{w}/cancel"), json!({}))
        .await
        .err(422, "invalid");
    // The mail of each cancel goes to the owner.
    let mail = system_mail(&td, &w).await;
    assert_eq!(mail.len(), 3, "{mail:#?}");
}

/// The removal of the owner closes its running ephemeral agents
/// (`owner_gone`, no mail), deletes the rows of the finished ones, and
/// goes down a level: a child that may spawn has its own child closed.
#[tokio::test]
async fn owner_removal_cascades() {
    let (td, _lead, w) = setup().await;
    td.patch(
        &format!("/api/agents/{w}"),
        json!({"policy": {"spawn": {"templates": ["worker"], "max_concurrent": 5,
                                     "may_spawn_children": true}}}),
    )
    .await
    .ok();
    let done = id(&spawn_ok(&td, &w, "finish first").await);
    td.wait_status(&done, "idle").await;
    td.post_as(&done, &format!("/api/agents/{done}/done"), json!({}))
        .await
        .ok();
    finished(&td, &done).await;
    let child = id(&spawn_ok(&td, &w, "spawn a grandchild").await);
    td.wait_status(&child, "idle").await;
    // The child has a copy of the policy with no children right.
    let c = td.get(&format!("/api/agents/{child}")).await.ok().clone();
    assert_eq!(c["policy"]["spawn"]["templates"], json!(["worker"]));
    assert!(
        c["policy"]["spawn"].get("may_spawn_children").is_none(),
        "{c}"
    );
    let g = id(&spawn_ok(&td, &child, "grandchild").await);
    assert_eq!(spawn_row(&td, &g).await["name"], "w-worker-2-worker-1");
    td.wait_status(&g, "idle").await;
    // The grandchild may not spawn.
    let r = spawn_as(&td, &g, json!({"template": "worker", "task": "x"})).await;
    r.err(403, "forbidden");
    assert!(
        r.body["error"]["message"]
            .as_str()
            .unwrap()
            .contains("may_spawn_children"),
        "{}",
        r.body
    );
    let before = system_mail(&td, &w).await.len();
    assert_eq!(
        td.delete(&format!("/api/agents/{w}?confirm=true"))
            .await
            .status,
        204
    );
    for a in [&child, &g] {
        let r = td.get(&format!("/api/agents/{a}")).await;
        assert_eq!(r.status, 404, "{} {}", r.body, spawn_row(&td, a).await);
    }
    // The rows went with the owner.
    for s in [&done, &child, &g] {
        assert_eq!(td.get(&format!("/api/spawns/{s}")).await.status, 404, "{s}");
    }
    // No mail for `owner_gone`: the owner is gone.
    assert_eq!(system_mail(&td, &w).await.len(), before);
}

/// A stop of the owner (not a removal) leaves its ephemeral agents.
#[tokio::test]
async fn a_stop_of_the_owner_leaves_them() {
    let (td, _lead, w) = setup().await;
    td.post_empty(&format!("/api/agents/{w}/start")).await.ok();
    status_is(&td, &w, "idle").await;
    let eph = id(&spawn_ok(&td, &w, "x").await);
    // Idle after its task turn: the check below then sees no turn end
    // (TER-109).
    td.wait_task_done(&eph).await;
    td.post_empty(&format!("/api/agents/{w}/stop")).await.ok();
    status_is(&td, &w, "stopped").await;
    tokio::time::sleep(Duration::from_millis(200)).await;
    let a = td.get(&format!("/api/agents/{eph}")).await.ok().clone();
    assert_eq!(a["status"], "idle");
}

/// `GET /api/agents/{id}/spawned`: newest first, paged, for whoever may
/// read the owner.
#[tokio::test]
async fn spawned_lists_page_and_rights() {
    let (td, lead, w) = setup().await;
    td.patch(
        &format!("/api/agents/{w}"),
        json!({"policy": {"spawn": {"templates": ["worker"], "max_concurrent": 5}}}),
    )
    .await
    .ok();
    let first = id(&spawn_ok(&td, &w, "one").await);
    let second = id(&spawn_ok(&td, &w, "two").await);
    let third = id(&spawn_ok(&td, &w, "three").await);
    td.post(&format!("/api/agents/{first}/cancel"), json!({}))
        .await
        .ok();
    let r = td.get(&format!("/api/agents/{w}/spawned?limit=2")).await;
    let page: Vec<String> = r.ok().as_array().unwrap().iter().map(id).collect();
    assert_eq!(page, [third.clone(), second.clone()]);
    let cursor = r.next_cursor().unwrap();
    let r = td
        .get(&format!("/api/agents/{w}/spawned?limit=2&cursor={cursor}"))
        .await;
    let rows = r.ok().as_array().unwrap().clone();
    assert_eq!(rows.len(), 1);
    assert_eq!(id(&rows[0]), first);
    assert_eq!(rows[0]["outcome"], "cancelled");
    assert_eq!(rows[0]["task"], "one");
    // A member of the project reads; another project does not.
    let other = id(&td.agent("p", "other", json!({})).await);
    td.get_as(&other, &format!("/api/agents/{w}/spawned"))
        .await
        .ok();
    td.project("q").await;
    let stranger = id(&td.agent("q", "s", json!({})).await);
    td.get_as(&stranger, &format!("/api/agents/{w}/spawned"))
        .await
        .err(403, "forbidden");
    td.get_as(&stranger, &format!("/api/spawns/{first}"))
        .await
        .err(403, "forbidden");
    // The operator's own list.
    let op = id(td
        .post(
            "/api/agents/operator/spawn",
            json!({"template": "worker", "task": "x", "project": "p"}),
        )
        .await
        .ok());
    // A spawn of the operator: the operator only, not a worker or the lead
    // of its project (TER-558).
    td.get(&format!("/api/spawns/{op}")).await.ok();
    td.get_as(&other, &format!("/api/spawns/{op}"))
        .await
        .err(403, "forbidden");
    td.get_as(&lead, &format!("/api/spawns/{op}"))
        .await
        .err(403, "forbidden");
    td.get_as(&other, &format!("/api/spawns/{first}"))
        .await
        .ok();
    let ops = td.get("/api/agents/operator/spawned?project=p").await;
    assert_eq!(ops.ok().as_array().unwrap().len(), 1);
    assert_eq!(ops.body[0]["name"], "operator-worker-1");
    td.get_as(&other, "/api/agents/operator/spawned")
        .await
        .err(403, "forbidden");
}

/// A spawn of the operator's own ephemeral agent mails the operator at
/// the finish.
#[tokio::test]
async fn the_operator_gets_the_result_of_its_own() {
    let (td, ..) = setup().await;
    let r = td
        .post(
            "/api/agents/operator/spawn",
            json!({"template": "worker", "task": "x", "project": "p"}),
        )
        .await;
    let eph = id(r.ok());
    td.wait_status(&eph, "idle").await;
    td.post_as(
        &eph,
        &format!("/api/agents/{eph}/done"),
        json!({"result": "ok"}),
    )
    .await
    .ok();
    finished(&td, &eph).await;
    let mail = td.get("/api/mail?to=operator&from=system").await;
    let subjects: Vec<String> = mail
        .ok()
        .as_array()
        .unwrap()
        .iter()
        .map(|m| m["subject"].as_str().unwrap_or("").to_owned())
        .collect();
    assert!(
        subjects
            .iter()
            .any(|s| s == "Spawned operator-worker-1 finished: done"),
        "{subjects:?}"
    );
}

/// TER-75: `include=card` lists the card agents among the ephemeral
/// agents. A plain spawn shows once it is the assignee of a card, with
/// that card as `current_card` also outside the `working` column.
#[tokio::test]
async fn include_card_lists_the_ephemeral_assignee_of_a_card() {
    let (td, _lead, w) = setup().await;
    let s = spawn_ok(&td, &w, "TER-1: do it").await;
    let eph = id(&s);
    let names = |v: &Value| -> Vec<String> {
        v.as_array()
            .unwrap()
            .iter()
            .map(|a| a["name"].as_str().unwrap().to_owned())
            .collect()
    };
    let list = td
        .get("/api/agents?project=p&include=card")
        .await
        .ok()
        .clone();
    assert_eq!(names(&list), ["lead", "w"]);
    let card = td
        .post("/api/projects/p/cards", json!({"title": "Ship it"}))
        .await
        .ok()
        .clone();
    let card = id(&card);
    td.post(
        &format!("/api/cards/{card}/assign"),
        json!({"assignee": eph}),
    )
    .await
    .ok();
    let list = td
        .get("/api/agents?project=p&include=card")
        .await
        .ok()
        .clone();
    assert_eq!(names(&list), ["lead", "w", "w-worker-1"]);
    let a = list
        .as_array()
        .unwrap()
        .iter()
        .find(|a| a["id"] == eph.as_str())
        .unwrap();
    assert_eq!(a["current_card"], card.as_str(), "{a}");
    // The default list and the project list still leave it out.
    let all = td.get("/api/agents?project=p").await.ok().clone();
    assert_eq!(names(&all), ["lead", "w"]);
    let all = td.get("/api/projects/p/agents").await.ok().clone();
    assert_eq!(names(&all), ["lead", "w"]);
}
