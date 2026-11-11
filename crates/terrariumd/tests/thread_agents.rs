//! The attached agent of an Inbox thread (TER-92, `crate::thread_agent`,
//! docs/api.md §6.13), with the fake supervisor: the spawn at the open,
//! the `[Thread:<id>]` turns, the park while the thread waits on the
//! operator, the restart in the same session (claude, codex, pi), the life
//! past 24 hours, the close, the A1 check at the spawn, and the memory of
//! the project it runs in (A2). No harness and no model turn runs.
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
    q_lead: String,
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
    let q_lead = id(&td
        .agent("q", "lead", json!({"roles": ["project_admin"]}))
        .await);
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
    Lab {
        td,
        p_lead,
        q_lead,
        w,
    }
}

/// META-136 supplies the real template. These tests exercise the daemon
/// contract with a fake harness, without starting a model or changing code.
async fn request(l: &Lab) -> Value {
    let r =
        l.td.post(
            "/api/templates",
            json!({"name": "request", "spec": {"harness": "claude"}}),
        )
        .await;
    assert_eq!(r.status, 201, "{}", r.body);
    l.open(
        json!({"report_to": "p/lead", "topic": "A request", "text": "Please explain the result."}),
    )
    .await
}

#[tokio::test]
async fn request_failed_starts_deliver_mail_and_visible_reason() {
    for missing_template in [true, false] {
        let l = lab().await;
        if !missing_template {
            // A valid saved template whose skill library is unavailable at start.
            let r =
                l.td.post(
                    "/api/templates",
                    json!({"name": "request", "spec": {
                        "harness": "claude"
                    }}),
                )
                .await;
            assert_eq!(r.status, 201, "{}", r.body);
            // Simulate a skill disappearing after the template was saved.
            state(&l.td).db.write("system", |t| {
                t.execute("UPDATE agent_templates SET spec = json_set(spec, '$.skills', json('[\"unavailable-request-skill\"]')) WHERE name = 'request' AND project IS NULL", [])?;
                Ok(())
            }).await.unwrap();
        }
        let th = l
            .open(json!({"report_to": "p/lead", "topic": "Never lose this",
            "text": "Please preserve every word.\nSecond line."}))
            .await;
        assert_eq!(th["close_reason"], "failed_start", "{th}");
        let summary = th["handoff"]["summary"].as_str().unwrap();
        assert!(summary.contains("fallback"), "{summary}");
        assert!(summary.contains("p/lead"), "{summary}");
        assert!(summary.contains("Reason:"), "{summary}");
        let mail =
            l.td.get(&format!("/api/mail?to={}&from=operator", l.p_lead))
                .await;
        let messages = mail.ok().as_array().unwrap();
        assert_eq!(messages.len(), 1, "{messages:?}");
        assert!(
            messages[0]["body"]
                .as_str()
                .unwrap()
                .contains("Please preserve every word.\nSecond line.")
        );
        let s = state(&l.td);
        terrariumd::thread_agent::recover_failed_requests(&s)
            .await
            .unwrap();
        let mail =
            l.td.get(&format!("/api/mail?to={}&from=operator", l.p_lead))
                .await;
        assert_eq!(mail.ok().as_array().unwrap().len(), 1);
    }
}

#[tokio::test]
async fn request_upgrade_recovers_stored_messages_once() {
    let l = lab().await;
    let tid = unstarted(&l, "operator", "global:request").await;
    let s = state(&l.td);
    let key = tid.clone();
    let lead = l.p_lead.clone();
    s.db.write("system", move |t| {
        t.execute(
            "UPDATE threads SET request = 1, report_to = ?2 WHERE id = ?1",
            [&key, &lead],
        )?;
        terrariumd::store::threads::close(
            t,
            &key,
            "system",
            terrariumd::store::threads::CloseReason::FailedStart,
            Some("The thread agent did not start: missing template"),
            Vec::new(),
        )?;
        Ok(())
    })
    .await
    .unwrap();
    for _ in 0..2 {
        terrariumd::thread_agent::recover_failed_requests(&s)
            .await
            .unwrap();
    }
    let mail =
        l.td.get(&format!("/api/mail?to={}&from=operator", l.p_lead))
            .await;
    let messages = mail.ok().as_array().unwrap();
    assert_eq!(messages.len(), 1);
    assert!(messages[0]["body"].as_str().unwrap().contains("Go on."));
    let full = l.thread(&tid).await;
    assert!(
        full["handoff"]["summary"]
            .as_str()
            .unwrap()
            .contains("fallback")
    );
}

#[tokio::test]
async fn request_opens_with_selected_lead_and_handoff_task() {
    let l = lab().await;
    let th = request(&l).await;
    assert_eq!(th["request"], true);
    assert_eq!(th["report_to"], l.p_lead);
    assert_eq!(th["scope"], "p");
    assert_eq!(th["template"], "global:request");
    let tid = id(&th);
    let full = l.thread(&tid).await;
    let typed: terrariumd::store::threads::Thread = serde_json::from_value(full).unwrap();
    let task = terrariumd::thread_agent::first_turn(&typed);
    assert!(task.contains("Mail p/lead"));
    assert!(task.contains("Do not change code"));
    assert!(task.contains("24 hours"));
    let bad =
        l.td.post("/api/threads", json!({"report_to": "p/w", "topic": "bad"}))
            .await;
    assert_eq!(bad.status, 422);
    let bad =
        l.td.post(
            "/api/threads",
            json!({"report_to": "p/lead", "topic": "bad", "agent": "p/w"}),
        )
        .await;
    assert_eq!(bad.status, 422);
}

#[tokio::test]
async fn request_close_confirm_and_reopen_with_history_and_new_agent() {
    let l = lab().await;
    let th = request(&l).await;
    let tid = id(&th);
    let old = th["agent"].as_str().unwrap();
    let proposed =
        l.td.post_as(
            old,
            &format!("/api/threads/{tid}/close"),
            json!({"summary": "Here is the answer."}),
        )
        .await;
    assert_eq!(proposed.status, 200, "{}", proposed.body);
    assert_eq!(proposed.body["status"], "waiting_operator");
    let closed =
        l.td.post(
            &format!("/api/threads/{tid}/close"),
            json!({"confirm": true}),
        )
        .await;
    assert_eq!(closed.status, 200, "{}", closed.body);
    let refused =
        l.td.post_as(
            &l.w,
            &format!("/api/threads/{tid}/reopen"),
            json!({"reason": "Related result"}),
        )
        .await;
    assert_eq!(refused.status, 403);
    let empty =
        l.td.post_as(
            &l.p_lead,
            &format!("/api/threads/{tid}/reopen"),
            json!({"reason": " "}),
        )
        .await;
    assert_eq!(empty.status, 422);
    let reopened =
        l.td.post_as(
            &l.p_lead,
            &format!("/api/threads/{tid}/reopen"),
            json!({"reason": "Related result"}),
        )
        .await;
    assert_eq!(reopened.status, 200, "{}", reopened.body);
    assert_eq!(reopened.body["status"], "waiting_agent");
    assert_ne!(reopened.body["agent"], old);
    let typed = serde_json::from_value(reopened.body.clone()).unwrap();
    let task = terrariumd::thread_agent::first_turn(&typed);
    assert!(task.contains("Related result"));
    assert!(task.contains("Please explain the result."));
    // A delayed close event for the first agent must not end its replacement.
    tokio::time::sleep(Duration::from_millis(150)).await;
    assert_eq!(
        l.spawn_row(reopened.body["agent"].as_str().unwrap()).await["finished_at"],
        Value::Null
    );
    let closed =
        l.td.post(
            &format!("/api/threads/{tid}/close"),
            json!({"summary": "Operator closes any time"}),
        )
        .await;
    assert_eq!(closed.status, 200);
}

#[tokio::test]
async fn request_lead_swaps_to_architect_in_same_thread() {
    let l = lab().await;
    let th = request(&l).await;
    let tid = id(&th);
    let made =
        l.td.post(
            "/api/templates",
            json!({"name": "architect", "spec": {"harness": "claude", "roles": ["architect"]}}),
        )
        .await;
    assert_eq!(made.status, 201, "{}", made.body);
    let bad =
        l.td.post_as(
            &l.w,
            &format!("/api/threads/{tid}/swap"),
            json!({"template": "global:design", "reason": "Needs design"}),
        )
        .await;
    assert_eq!(bad.status, 403);
    let bad =
        l.td.post_as(
            &l.p_lead,
            &format!("/api/threads/{tid}/swap"),
            json!({"template": "global:request", "reason": "Needs design"}),
        )
        .await;
    assert_eq!(bad.status, 403);
    let swapped =
        l.td.post_as(
            &l.p_lead,
            &format!("/api/threads/{tid}/swap"),
            json!({"template": "global:architect", "reason": "Needs design"}),
        )
        .await;
    assert_eq!(swapped.status, 200, "{}", swapped.body);
    assert_eq!(swapped.body["id"], tid);
    assert_ne!(swapped.body["agent"], th["agent"]);
    assert_eq!(swapped.body["request"], true);
    assert_eq!(swapped.body["template"], "global:architect");
    let agent = swapped.body["agent"].as_str().unwrap();
    assert_eq!(l.agent(agent).await["roles"], json!(["architect"]));
    assert_eq!(l.spawn_row(agent).await["owner"], l.p_lead);
}

#[tokio::test]
async fn request_swap_refuses_project_and_excess_role_templates() {
    let l = lab().await;
    let th = request(&l).await;
    let tid = id(&th);
    for body in [
        json!({"name": "architect", "project": "p", "spec": {"harness": "claude", "roles": ["architect", "sysadmin"]}}),
        json!({"name": "unapproved", "spec": {"harness": "claude", "roles": ["architect"]}}),
        json!({"name": "architect", "spec": {"harness": "claude", "roles": ["architect", "sysadmin"]}}),
    ] {
        assert_eq!(l.td.post("/api/templates", body).await.status, 201);
    }
    for template in ["p/architect", "global:unapproved", "global:architect"] {
        let r =
            l.td.post_as(
                &l.p_lead,
                &format!("/api/threads/{tid}/swap"),
                json!({"template": template, "reason": "Design"}),
            )
            .await;
        assert_eq!(r.status, 403, "{template}: {}", r.body);
        assert_eq!(l.thread(&tid).await["agent"], th["agent"]);
    }
}

#[tokio::test]
async fn request_obsolete_swap_cannot_spawn_or_attach_after_later_restart() {
    let l = lab().await;
    let th = request(&l).await;
    let tid = id(&th);
    l.td.post(
        "/api/templates",
        json!({"name": "architect", "spec": {"harness": "claude", "roles": ["architect"]}}),
    )
    .await
    .ok();
    l.td.post(&format!("/api/threads/{tid}/close"), json!({}))
        .await
        .ok();
    let s = state(&l.td);
    terrariumd::bound::end(
        &s,
        &terrariumd::thread_agent::bound(&tid),
        terrariumd::store::spawns::DONE,
        "test closes original".into(),
        false,
    )
    .await
    .unwrap();
    let key = tid.clone();
    let (a, b) =
        s.db.write("system", move |t| {
            let a = terrariumd::store::threads::restart_request(
                t,
                &key,
                "operator",
                "Swap A",
                "global:architect",
                true,
                (0, None),
            )?;
            let b = terrariumd::store::threads::restart_request(
                t,
                &key,
                "operator",
                "Swap B",
                "global:architect",
                false,
                (a.request_epoch, None),
            )?;
            Ok((a, b))
        })
        .await
        .unwrap();
    // A resumes only after B committed. Its task and template are stale,
    // even though B has not attached an agent yet.
    assert!(terrariumd::thread_agent::start(&s, a, None).await.is_err());
    let pending = l.thread(&tid).await;
    assert_eq!(pending["status"], "waiting_agent");
    assert!(pending["agent"].is_null());
    let started = terrariumd::thread_agent::start(&s, b, None).await.unwrap();
    assert_eq!(started.request_epoch, 2);
    assert!(started.agent.is_some());
    assert_eq!(l.thread(&tid).await["agent"], json!(started.agent));
}

#[tokio::test]
async fn request_timer_closes_more_than_one_ordered_batch() {
    let l = lab().await;
    let count = terrariumd::thread_close::REQUEST_CLOSE_BATCH + 3;
    state(&l.td).db.write("system", move |t| {
        for _ in 0..count {
            let th = terrariumd::store::threads::open(t, terrariumd::store::threads::NewThread {
                topic: "Answer-only request".into(), scope: "p".into(), opened_by: "operator".into(), ..Default::default()
            })?;
            terrariumd::store::threads::propose_close(t, &th.id, "operator", Some("Answer"), vec![])?;
            t.execute("UPDATE threads SET request = 1, updated_at = '2020-01-01T00:00:00.000Z' WHERE id = ?1", [&th.id])?;
        }
        Ok(())
    }).await.unwrap();
    terrariumd::thread_close::sweep(&state(&l.td)).await;
    let closed = state(&l.td).db.read(|c| {
        Ok(c.query_row("SELECT COUNT(*) FROM threads WHERE request = 1 AND status = 'closed' AND closed_by = 'system'", [], |r| r.get::<_, usize>(0))?)
    }).await.unwrap();
    assert_eq!(closed, count);
}

#[tokio::test]
async fn request_answer_only_auto_confirms_after_24h_but_reply_withdraws() {
    let l = lab().await;
    let th = request(&l).await;
    let tid = id(&th);
    let agent = th["agent"].as_str().unwrap();
    l.td.post_as(
        agent,
        &format!("/api/threads/{tid}/close"),
        json!({"summary": "The answer"}),
    )
    .await
    .ok();
    // Not before 24 hours.
    terrariumd::thread_close::sweep(&state(&l.td)).await;
    assert_eq!(l.thread(&tid).await["status"], "waiting_operator");
    let key = tid.clone();
    state(&l.td)
        .db
        .write("system", move |t| {
            let at = (chrono::Utc::now() - chrono::Duration::hours(25))
                .to_rfc3339_opts(chrono::SecondsFormat::Millis, true);
            t.execute(
                "UPDATE threads SET updated_at = ?2 WHERE id = ?1",
                [&key, &at],
            )?;
            Ok(())
        })
        .await
        .unwrap();
    terrariumd::thread_close::sweep(&state(&l.td)).await;
    let closed = l.thread(&tid).await;
    assert_eq!(closed["status"], "closed");
    assert_eq!(closed["handoff"]["summary"], "The answer");
    let reopened =
        l.td.post(
            &format!("/api/threads/{tid}/reopen"),
            json!({"reason": "More context"}),
        )
        .await;
    assert_eq!(reopened.status, 200, "{}", reopened.body);
    let agent = reopened.body["agent"].as_str().unwrap();
    l.td.post_as(
        agent,
        &format!("/api/threads/{tid}/close"),
        json!({"summary": "New answer"}),
    )
    .await
    .ok();
    let reply =
        l.td.post(
            &format!("/api/threads/{tid}/messages"),
            json!({"text": "Please clarify"}),
        )
        .await;
    assert_eq!(reply.status, 201, "{}", reply.body);
    assert!(reply.body["close_proposal"].is_null());
    terrariumd::thread_close::sweep(&state(&l.td)).await;
    assert_eq!(l.thread(&tid).await["status"], "waiting_agent");
}

#[tokio::test]
async fn request_tracks_lead_cards_and_linked_lead_can_reopen() {
    let l = lab().await;
    let th = request(&l).await;
    let tid = id(&th);
    let made =
        l.td.post("/api/projects/p/cards", json!({"title": "Requested work"}))
            .await;
    assert_eq!(made.status, 201, "{}", made.body);
    let card = made.body["id"].as_str().unwrap();
    // The operator links across projects, then the chosen lead can track
    // its own cards through the existing handoff path.
    let linked =
        l.td.post(
            &format!("/api/threads/{tid}/cards"),
            json!({"card": format!("p:{card}")}),
        )
        .await;
    assert_eq!(linked.status, 200, "{}", linked.body);
    assert_eq!(linked.body["cards"].as_array().unwrap().len(), 1);
    let tracked =
        l.td.post_as(
            th["agent"].as_str().unwrap(),
            &format!("/api/threads/{tid}/track"),
            json!({}),
        )
        .await;
    assert_eq!(tracked.status, 200, "{}", tracked.body);
    assert_eq!(tracked.body["status"], "tracking");
    l.td.post(&format!("/api/threads/{tid}/close"), json!({}))
        .await
        .ok();
    let new_card =
        l.td.post(
            "/api/projects/q/cards",
            json!({"title": "Related new work"}),
        )
        .await;
    assert_eq!(new_card.status, 201, "{}", new_card.body);
    let reopened =
        l.td.post_as(
            &l.q_lead,
            &format!("/api/threads/{tid}/reopen"),
            json!({"reason": "The new card needs follow-up", "card": new_card.body["id"]}),
        )
        .await;
    assert_eq!(reopened.status, 200, "{}", reopened.body);
    assert_eq!(reopened.body["cards"].as_array().unwrap().len(), 1);
    assert_eq!(reopened.body["cards"][0]["project"], "q");
}

#[tokio::test]
async fn request_cross_project_card_rights_end_at_close() {
    let l = lab().await;
    // Model a global request agent in meta reporting to p/lead.
    let th = l.open(json!({"scope": "global", "template": "global:helper", "topic": "Cross-project request"})).await;
    let tid = id(&th);
    let (thread, lead) = (tid.clone(), l.p_lead.clone());
    state(&l.td)
        .db
        .write("operator", move |t| {
            t.execute(
                "UPDATE threads SET request = 1, report_to = ?2 WHERE id = ?1",
                rusqlite::params![thread, lead],
            )?;
            Ok(())
        })
        .await
        .unwrap();
    let agent = th["agent"].as_str().unwrap();
    let made =
        l.td.post("/api/projects/p/cards", json!({"title": "Lead work"}))
            .await;
    let card = id(made.ok());
    let path = format!("/api/cards/{card}");
    assert_eq!(l.td.get_as(agent, &path).await.status, 200);
    assert_eq!(
        l.td.get_as(agent, "/api/projects/p/cards").await.status,
        200
    );
    assert_eq!(
        l.td.patch_as(agent, &path, json!({"title": "No"}))
            .await
            .status,
        403
    );
    assert_eq!(
        l.td.post_as(agent, &format!("{path}/comments"), json!({"body": "No"}))
            .await
            .status,
        403
    );
    let other =
        l.td.post("/api/projects/q/cards", json!({"title": "Other work"}))
            .await;
    let other_card = id(other.ok());
    assert_eq!(
        l.td.get_as(agent, &format!("/api/cards/{other_card}"))
            .await
            .status,
        403
    );
    assert_eq!(
        l.td.post_as(
            agent,
            &format!("/api/threads/{tid}/cards"),
            json!({"card": format!("q:{other_card}")})
        )
        .await
        .status,
        404
    );
    let second = l.open(json!({"scope": "p", "topic": "Other thread"})).await;
    // The caller can read this thread as its opener, but is not its
    // attached agent. Refusal must come from the own-request link gate.
    let (other_thread, opener) = (id(&second), agent.to_owned());
    state(&l.td)
        .db
        .write("operator", move |t| {
            t.execute(
                "UPDATE threads SET opened_by = ?2 WHERE id = ?1",
                rusqlite::params![other_thread, opener],
            )?;
            Ok(())
        })
        .await
        .unwrap();
    assert_eq!(
        l.td.get_as(agent, &format!("/api/threads/{}", id(&second)))
            .await
            .status,
        200
    );
    assert_eq!(
        l.td.post_as(
            agent,
            &format!("/api/threads/{}/cards", id(&second)),
            json!({"card": format!("p:{card}")})
        )
        .await
        .status,
        404
    );
    let tracked =
        l.td.post_as(
            agent,
            &format!("/api/threads/{tid}/track"),
            json!({"cards": [format!("p:{card}")]}),
        )
        .await;
    assert_eq!(tracked.status, 200, "{}", tracked.body);
    assert_eq!(tracked.body["cards"][0]["card"], card);
    l.td.post(&format!("/api/threads/{tid}/close"), json!({}))
        .await
        .ok();
    assert_ne!(l.td.get_as(agent, &path).await.status, 200);
}

#[tokio::test]
async fn request_concurrent_reopen_has_one_winner() {
    let l = lab().await;
    let th = request(&l).await;
    let tid = id(&th);
    l.td.post(&format!("/api/threads/{tid}/close"), json!({}))
        .await
        .ok();
    let path = format!("/api/threads/{tid}/reopen");
    let (a, b) = tokio::join!(
        l.td.post(&path, json!({"reason": "First related request"})),
        l.td.post(&path, json!({"reason": "Second related request"})),
    );
    let statuses = [a.status, b.status];
    assert_eq!(statuses.iter().filter(|&&s| s == 200).count(), 1);
    assert!(statuses.iter().any(|s| *s == 409));
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
        self.td
            .get(&format!("/api/spawns/{eph}"))
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
            self.td
                .sup
                .prompts(aid)
                .iter()
                .any(|p| p.starts_with(start))
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
        .open(
            json!({"topic": "Cache layer", "scope": "p", "template": format!("t-{harness}"),
                     "text": "Should we cache the board?"}),
        )
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
    assert!(
        th["agent_name"]
            .as_str()
            .unwrap()
            .starts_with("p/thread-t-")
    );
    // The first turn: the thread, and the operator's first message.
    let first = lab.prompt(&eph, &format!("[Thread:{tid}]")).await;
    assert!(
        first.contains("You are the agent of Inbox thread"),
        "{first}"
    );
    assert!(
        first.contains("The operator's topic: Cache layer"),
        "{first}"
    );
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
    assert_eq!(
        lab.td
            .sup
            .prompts(eph.parse().unwrap())
            .iter()
            .filter(|p| p.contains("Before acting after this restart"))
            .count(),
        2
    );

    // The overall response comes as a turn too.
    lab.post_as(&eph, &tid, "Plan: cache board and cards.")
        .await;
    lab.parked(&eph).await;
    lab.td
        .post(
            &format!("/api/threads/{tid}/respond"),
            json!({"text": "Approved."}),
        )
        .await
        .ok();
    let turn = lab
        .prompt(
            &eph,
            &format!("[Thread:{tid}] The operator's overall response"),
        )
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
    // More than 24 hours of waiting (review round 1, finding 4): the spawn
    // and the park move back 25 hours, and a timer pass leaves it alone.
    let back = (chrono::Utc::now() - chrono::Duration::hours(25))
        .to_rfc3339_opts(chrono::SecondsFormat::Millis, true);
    let key = eph.clone();
    s.db.write("system", move |t| {
        t.execute(
            "UPDATE spawns SET spawned_at = ?2, parked_at = ?2 WHERE id = ?1",
            [&key, &back],
        )?;
        Ok(())
    })
    .await
    .unwrap();
    terrariumd::spawn::pass(&s).await;
    tokio::time::sleep(Duration::from_millis(200)).await;
    let sp = lab.spawn_row(&eph).await;
    assert!(sp["finished_at"].is_null(), "{sp}");
    assert!(sp["parked_at"].is_string(), "{sp}");
    assert_eq!(lab.agent(&eph).await["status"], "stopped");
    // It still restarts in its session on the next operator message.
    lab.td
        .post(
            &format!("/api/threads/{tid}/messages"),
            json!({"text": "Two regions."}),
        )
        .await
        .ok();
    lab.prompt(&eph, &format!("[Thread:{tid}] Two regions."))
        .await;
    lab.td.wait_status(&eph, "idle").await;
    assert_eq!(lab.sessions(&eph).await[0]["resumes"], 1);
    // The agent proposes the close: it parks; the operator confirms, and
    // it ends (TER-95).
    let r = lab
        .td
        .post_as(
            &eph,
            &format!("/api/threads/{tid}/close"),
            json!({"summary": "Nothing to do."}),
        )
        .await;
    assert_eq!(r.status, 200, "{}", r.body);
    assert_eq!(r.body["status"], "waiting_operator", "{}", r.body);
    lab.parked(&eph).await;
    lab.td
        .post(
            &format!("/api/threads/{tid}/close"),
            json!({"confirm": true}),
        )
        .await
        .ok();
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
    let first = lab.prompt(&eph, &format!("[Thread:{}]", id(&r.body))).await;
    // The opener's message is quoted: an agent's text, not the operator's.
    assert!(first.contains("\n> Ask the operator."), "{first}");

    let s = state(&lab.td);
    let check = |w: String| {
        let s = s.clone();
        async move {
            s.db.read(move |c| {
                let tp = terrariumd::store::templates::resolve(c, "p", "consult")?;
                let mut def = terrarium_core::agent_template::to_def(&tp.spec, "x", "p")
                    .map_err(|e| terrariumd::error::ApiError::invalid(e.to_string()))?;
                terrariumd::thread_agent::check_a1(c, &w, &mut def)
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

/// Open a thread in the store only, as if the daemon stopped between the
/// open and the spawn: no agent, opened 2 minutes ago.
async fn unstarted(lab: &Lab, opener: &str, template: &str) -> String {
    let s = state(&lab.td);
    let opener = opener.to_owned();
    let template = template.to_owned();
    s.db.write("system", move |t| {
        let th = terrariumd::store::threads::open(
            t,
            terrariumd::store::threads::NewThread {
                topic: "Unstarted".into(),
                scope: "p".into(),
                template: Some(template),
                opened_by: opener,
                agent: None,
                text: Some("Go on.".into()),
            },
        )?;
        let back = (chrono::Utc::now() - chrono::Duration::minutes(2))
            .to_rfc3339_opts(chrono::SecondsFormat::Millis, true);
        t.execute(
            "UPDATE threads SET created_at = ?2 WHERE id = ?1",
            [&th.id, &back],
        )?;
        Ok(th.id)
    })
    .await
    .unwrap()
}

/// Review round 1, findings 3 and 6: a thread that a daemon stop left
/// with a template and no agent gets its agent on the timer, attached in
/// the spawn transaction. For an agent opener the spawn checks A1 against
/// the template as it is then: after the template gains project_admin,
/// the spawn is refused, no agent is made, and the thread is closed with
/// the reason.
#[tokio::test]
async fn the_timer_starts_unstarted_threads_with_a1() {
    let lab = lab().await;
    let s = state(&lab.td);
    let tid = unstarted(&lab, &lab.w, "consult").await;
    terrariumd::thread_agent::retry_missed(&s).await;
    let th = lab.thread(&tid).await;
    let eph = th["agent"].as_str().expect("an agent").to_owned();
    assert_eq!(lab.spawn_row(&eph).await["bound"], format!("thread:{tid}"));
    lab.prompt(&eph, &format!("[Thread:{tid}]")).await;
    // A second pass starts nothing more.
    terrariumd::thread_agent::retry_missed(&s).await;
    let all = lab
        .td
        .get("/api/agents/operator/spawned?project=p")
        .await
        .ok()
        .clone();
    assert_eq!(all.as_array().unwrap().len(), 1, "{all}");

    // The template changes after the open: the spawn refuses it.
    let r = lab
        .td
        .patch(
            "/api/templates/p%2Fconsult",
            json!({"spec": {"harness": "claude", "roles": ["project_admin"]}}),
        )
        .await;
    assert_eq!(r.status, 200, "{}", r.body);
    let tid = unstarted(&lab, &lab.w, "consult").await;
    terrariumd::thread_agent::retry_missed(&s).await;
    let th = lab.thread(&tid).await;
    assert_eq!(th["status"], "closed", "{th}");
    assert!(th["agent"].is_null(), "{th}");
    let why = th["handoff"]["summary"].as_str().unwrap();
    assert!(why.starts_with("The thread agent did not start"), "{why}");
    assert!(why.contains("project_admin"), "{why}");
    let all = lab
        .td
        .get("/api/agents/operator/spawned?project=p")
        .await
        .ok()
        .clone();
    assert_eq!(all.as_array().unwrap().len(), 1, "no new spawn: {all}");
    // The operator opens with any template: its unstarted thread starts.
    let tid = unstarted(&lab, "operator", "consult").await;
    terrariumd::thread_agent::retry_missed(&s).await;
    assert!(lab.thread(&tid).await["agent"].is_string());
}

/// Review round 1, finding 1: the opener is the owner for the rights. The
/// thread agent keeps `thread_open` only while the opener holds it.
#[tokio::test]
async fn the_agent_keeps_thread_open_only_with_the_opener() {
    let lab = lab().await;
    let s = state(&lab.td);
    let check = |w: String| {
        let s = s.clone();
        async move {
            s.db.read(move |c| {
                let tp = terrariumd::store::templates::resolve(c, "p", "consult")?;
                let mut def = terrarium_core::agent_template::to_def(&tp.spec, "x", "p")
                    .map_err(|e| terrariumd::error::ApiError::invalid(e.to_string()))?;
                def.policy.thread_open = true;
                terrariumd::thread_agent::check_a1(c, &w, &mut def)?;
                Ok(def.policy.thread_open)
            })
            .await
            .unwrap()
        }
    };
    assert!(check(lab.w.clone()).await);
    lab.td
        .patch(
            &format!("/api/agents/{}", lab.w),
            json!({"policy": {"thread_open": false}}),
        )
        .await
        .ok();
    assert!(!check(lab.w.clone()).await);
    // A real thread agent of `w` has no thread_open (a template cannot
    // carry it, and `w` holds it at the open).
    lab.td
        .patch(
            &format!("/api/agents/{}", lab.w),
            json!({"policy": {"thread_open": true}}),
        )
        .await
        .ok();
    let r = lab
        .td
        .post_as(
            &lab.w,
            "/api/threads",
            json!({"topic": "T", "template": "consult"}),
        )
        .await;
    assert_eq!(r.status, 201, "{}", r.body);
    let eph = r.body["agent"].as_str().unwrap().to_owned();
    assert_ne!(lab.agent(&eph).await["policy"]["thread_open"], true);
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
    let restart = lab
        .prompt(
            &eph,
            &format!("[Thread:{tid}] Before acting after this restart"),
        )
        .await;
    assert!(restart.contains("linked cards"), "{restart}");
    assert!(restart.contains("terrarium card show"), "{restart}");
    let turn = lab
        .prompt(&eph, &format!("[Thread:{tid}] The thread changed"))
        .await;
    assert!(
        turn.contains(&format!("terrarium thread show {tid}")),
        "{turn}"
    );
    lab.td.wait_status(&eph, "idle").await;
}

/// Every finish path shares the transaction that marks the thread and mails
/// its report-to lead. A retry and the ordinary thread close stay quiet.
#[tokio::test]
async fn an_ended_agent_marks_only_its_open_thread_once() {
    let lab = lab().await;
    for outcome in ["done", "cancelled", "timeout", "failed"] {
        let th = lab
            .open(json!({"topic": "Ended", "scope": "p", "template": "consult"}))
            .await;
        let tid = id(&th);
        let agent = th["agent"].as_str().unwrap();
        lab.td.wait_status(agent, "idle").await;
        let asked = lab
            .td
            .post_as(
                agent,
                &format!("/api/agents/{agent}/questions"),
                json!({"text": "Continue?", "options": ["yes", "no"], "thread": tid}),
            )
            .await;
        assert_eq!(asked.status, 201, "{}", asked.body);
        let qid = id(&asked.body);
        lab.parked(agent).await;
        let s = state(&lab.td);
        terrariumd::spawn::close(&s, agent, outcome, Some("ended elsewhere".into()), true)
            .await
            .unwrap();
        terrariumd::spawn::close(&s, agent, outcome, None, true)
            .await
            .unwrap();
        let marked = lab.thread(&tid).await;
        assert_ne!(marked["status"], "closed");
        assert_eq!(marked["agent_end_outcome"], outcome);
        let q = &marked["questions"].as_array().unwrap()[0];
        assert_eq!(q["status"], "retracted");
        assert!(q["retract_reason"].as_str().unwrap().contains(outcome));
        let late = lab
            .td
            .post(
                &format!("/api/questions/{qid}/answer"),
                json!({"answer": "yes"}),
            )
            .await;
        assert_ne!(late.status, 200, "{}", late.body);
        let messages = marked["messages"].as_array().unwrap();
        let marks: Vec<_> = messages
            .iter()
            .filter(|m| {
                m["author"] == "system"
                    && m["text"].as_str().unwrap().contains("Thread agent ended:")
            })
            .collect();
        assert_eq!(marks.len(), 1, "{marked}");
        assert!(marks[0]["text"].as_str().unwrap().contains(outcome));
        let mail = lab
            .td
            .get(&format!("/api/mail?to={}&from=system", lab.p_lead))
            .await
            .ok()
            .clone();
        assert!(
            mail.to_string()
                .contains(&format!("Thread {tid}: agent ended")),
            "{mail}"
        );
        assert_eq!(
            mail.as_array()
                .unwrap()
                .iter()
                .filter(|m| m["subject"] == format!("Thread {tid}: agent ended"))
                .count(),
            1
        );
        let endpoint = if outcome == "done" || outcome == "failed" {
            "respond"
        } else {
            "messages"
        };
        let late_post = lab
            .td
            .post(
                &format!("/api/threads/{tid}/{endpoint}"),
                json!({"text": "Keep this operator follow-up."}),
            )
            .await;
        assert_eq!(
            late_post.status,
            if endpoint == "respond" { 200 } else { 201 },
            "{}",
            late_post.body
        );
        let saved = lab.thread(&tid).await;
        assert_eq!(saved["status"], "closed");
        assert_eq!(saved["close_reason"], "agent_ended");
        assert_eq!(saved["agent_end_outcome"], outcome);
        if endpoint == "respond" {
            assert_eq!(saved["response"]["text"], "Keep this operator follow-up.");
        }
        assert!(
            saved["messages"]
                .as_array()
                .unwrap()
                .iter()
                .any(|m| m["author"] == "operator" && m["text"] == "Keep this operator follow-up.")
        );
        assert!(saved["messages"].to_string().contains("Nobody will answer"));
        let operator = lab.td.get("/api/mail?to=operator&from=system").await;
        assert!(
            operator.ok().as_array().unwrap().is_empty(),
            "{}",
            operator.body
        );
    }
}

#[tokio::test]
async fn ending_while_waiting_agent_warns_when_report_to_is_gone() {
    let lab = lab().await;
    let th = lab
        .open(json!({"topic": "Unanswered", "scope": "p", "template": "consult"}))
        .await;
    let tid = id(&th);
    let agent = th["agent"].as_str().unwrap().to_owned();
    lab.td.wait_status(&agent, "idle").await;
    let log = tempfile::NamedTempFile::new().unwrap();
    let file = log.reopen().unwrap();
    let key = tid.clone();
    state(&lab.td)
        .db
        .write("system", move |t| {
            t.execute(
                "UPDATE threads SET report_to = 'missing-lead', status = 'waiting_agent' WHERE id = ?1",
                [&key],
            )?;
            let subscriber = tracing_subscriber::fmt()
                .with_ansi(false)
                .with_writer(move || file.try_clone().unwrap())
                .finish();
            tracing::subscriber::with_default(subscriber, || {
                terrariumd::store::threads::agent_ended(t, &key, &agent, "failed")
            })
        })
        .await
        .unwrap();
    let marked = lab.thread(&tid).await;
    assert_eq!(marked["status"], "waiting_operator");
    let warning = std::fs::read_to_string(log.path()).unwrap();
    assert!(warning.contains("WARN"), "{warning}");
    assert!(warning.contains("no agent-end mail"), "{warning}");
    assert!(warning.contains("missing-lead"), "{warning}");
}

#[tokio::test]
async fn normal_thread_close_retracts_questions_without_an_abnormal_flag() {
    let lab = lab().await;
    let th = lab
        .open(json!({"topic": "Close", "scope": "p", "template": "consult"}))
        .await;
    let tid = id(&th);
    let agent = th["agent"].as_str().unwrap();
    lab.td.wait_status(agent, "idle").await;
    lab.td
        .post_as(
            agent,
            &format!("/api/agents/{agent}/questions"),
            json!({"text": "Continue?", "options": ["yes", "no"], "thread": tid}),
        )
        .await
        .ok();
    lab.parked(agent).await;
    lab.td
        .post(
            &format!("/api/threads/{tid}/close"),
            json!({"summary": "No further work."}),
        )
        .await
        .ok();
    lab.closed(agent).await;
    let closed = lab.thread(&tid).await;
    assert_eq!(closed["status"], "closed");
    assert!(closed["agent_end_outcome"].is_null());
    assert_eq!(closed["questions"][0]["status"], "retracted");
    assert!(
        closed["questions"][0]["retract_reason"]
            .as_str()
            .unwrap()
            .contains("was closed")
    );
    assert!(
        !closed["messages"]
            .to_string()
            .contains("Thread agent ended:")
    );
    let mail = lab
        .td
        .get(&format!("/api/mail?to={}&from=system", lab.p_lead))
        .await;
    assert!(mail.ok().as_array().unwrap().is_empty());
}

/// A2: the thread agent reads and writes the memories of the project it
/// runs in, and neither reads nor writes another one; a global thread's
/// agent writes in `meta`.
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
    // A memory of `q`: not readable for the agent of a `p` thread.
    let q_lead = lab.td.get("/api/agents/q%2Flead").await.ok().clone();
    let r = lab
        .td
        .post_as(
            q_lead["id"].as_str().unwrap(),
            "/api/projects/q/memories",
            json!({"title": "Q only"}),
        )
        .await;
    assert_eq!(r.status, 201, "{}", r.body);
    let qid = id(&r.body);
    let qref = if qid.contains('/') {
        qid.replace('/', "%2F")
    } else {
        format!("q%2F{qid}")
    };
    lab.td
        .get_as(&eph, &format!("/api/memories/{qref}"))
        .await
        .err(403, "forbidden");
    lab.td
        .get_as(&eph, "/api/projects/q/memories")
        .await
        .err(403, "forbidden");
    // A global thread's agent runs in `meta` and writes there.
    let g = lab
        .open(json!({"topic": "Global memory", "template": "helper"}))
        .await;
    let geph = g["agent"].as_str().unwrap().to_owned();
    let r = lab
        .td
        .post_as(
            &geph,
            "/api/projects/meta/memories",
            json!({"title": "Global threads run in meta"}),
        )
        .await;
    assert_eq!(r.status, 201, "{}", r.body);
    lab.td
        .post_as(&geph, "/api/projects/p/memories", json!({"title": "No"}))
        .await
        .err(403, "forbidden");
}
