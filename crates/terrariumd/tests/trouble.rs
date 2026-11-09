//! Durable card trouble notices, project settings rights, and usage display data.
#![allow(clippy::unwrap_used, clippy::expect_used)]
use crate::common::TestDaemon;
use serde_json::{Value, json};

fn state(td: &TestDaemon) -> terrariumd::api::Shared {
    td.daemon.as_ref().unwrap().state.clone()
}

fn id(v: &Value) -> String {
    v["id"].as_str().unwrap().into()
}

async fn setup() -> (TestDaemon, String, String, String) {
    let td = TestDaemon::start().await;
    td.project("p").await;
    let lead = id(&td
        .agent("p", "lead", json!({"roles": ["project_admin"]}))
        .await);
    let worker = id(&td.agent("p", "worker", json!({})).await);
    let card = id(td
        .post_as(
            &worker,
            "/api/projects/p/cards",
            json!({"title": "build", "labels": ["type:small"]}),
        )
        .await
        .ok());
    td.post_as(&worker, &format!("/api/cards/{card}/claim"), json!({}))
        .await
        .ok();
    (td, lead, worker, card)
}

async fn usage(td: &TestDaemon, card: &str) -> Value {
    td.get(&format!("/api/cards/{card}/usage"))
        .await
        .ok()
        .clone()
}

async fn eta(td: &TestDaemon, card: &str, overdue: bool) {
    let card = card.to_owned();
    state(td)
        .db
        .write("system", move |t| {
            t.execute(
                "UPDATE cards SET eta_at = ?2 WHERE id = ?1",
                rusqlite::params![card, overdue.then_some("2000-01-01T00:00:00Z")],
            )?;
            Ok(())
        })
        .await
        .unwrap();
}

async fn notices(td: &TestDaemon, who: &str) -> usize {
    td.mail_of(who)
        .await
        .iter()
        .filter(|m| {
            m["subject"]
                .as_str()
                .is_some_and(|s| s.contains("trouble signal"))
        })
        .count()
}

async fn assert_signals(td: &TestDaemon, card: &str, expected: Value) {
    terrariumd::trouble::pass(&state(td)).await;
    assert_eq!(usage(td, card).await["signals"], expected);
}

#[tokio::test]
async fn notice_is_once_across_ticks_resolution_recurrence_and_restart() {
    let (mut td, lead, worker, card) = setup().await;
    eta(&td, &card, true).await;
    assert_signals(&td, &card, json!(["eta_overdue"])).await;
    terrariumd::trouble::pass(&state(&td)).await;
    assert_eq!(usage(&td, &card).await["signals"], json!(["eta_overdue"]));
    assert_eq!(notices(&td, &lead).await, 1);
    assert_eq!(notices(&td, &worker).await, 0);
    eta(&td, &card, false).await;
    assert_signals(&td, &card, json!([])).await;
    td.boot(None, None).await;
    eta(&td, &card, true).await;
    assert_signals(&td, &card, json!(["eta_overdue"])).await;
    assert_eq!(notices(&td, &lead).await, 1);
    assert_eq!(notices(&td, &worker).await, 0);
    assert_eq!(
        td.get("/api/mail?to=operator")
            .await
            .ok()
            .as_array()
            .unwrap()
            .iter()
            .filter(|m| m["subject"]
                .as_str()
                .is_some_and(|s| s.contains("trouble signal")))
            .count(),
        0
    );
    td.post_as(
        &worker,
        &format!("/api/cards/{card}/move"),
        json!({"column": "blocked", "comment": "needs dependency"}),
    )
    .await
    .ok();
    assert_signals(&td, &card, json!([])).await;
}

#[tokio::test]
async fn project_thresholds_require_admin_and_are_isolated() {
    let (td, lead, worker, _) = setup().await;
    td.project("other").await;
    assert_eq!(
        td.get("/api/projects/p/trouble").await.ok()["red_streak"],
        3
    );
    td.put_as(&worker, "/api/projects/p/trouble", json!({"red_streak": 4}))
        .await
        .err(403, "forbidden");
    td.put_as(
        &lead,
        "/api/projects/p/trouble",
        json!({"red_streak": 4, "tokens": {"small": 100}}),
    )
    .await
    .ok();
    assert_eq!(
        td.get("/api/projects/p/trouble").await.ok()["tokens"]["small"],
        100
    );
    assert_eq!(
        td.get("/api/projects/other/trouble").await.ok()["tokens"],
        json!({})
    );
    td.get_as(&worker, "/api/projects/other/trouble")
        .await
        .err(403, "forbidden");
}

#[tokio::test]
async fn token_and_turn_budget_crossings_clear_when_disabled() {
    let (td, lead, worker, card) = setup().await;
    td.put_as(
        &lead,
        "/api/projects/p/trouble",
        json!({"tokens": {"small": 100}, "turns": {"small": 1}}),
    )
    .await
    .ok();
    let c = card.clone();
    state(&td)
        .db
        .write("system", move |t| {
            terrariumd::store::usage::insert(
                t,
                &[terrariumd::store::usage::NewUsage {
                    agent: worker,
                    project: "p".into(),
                    card: Some(c),
                    input_tokens: 99,
                    message_id: "m1".into(),
                    turn_seq: Some(1),
                    ..Default::default()
                }],
            )?;
            Ok(())
        })
        .await
        .unwrap();
    assert_signals(&td, &card, json!(["turn_budget"])).await;
    let c = card.clone();
    state(&td)
        .db
        .write("system", move |t| {
            t.execute(
                "UPDATE usage_samples SET input_tokens = 100 WHERE card_id = ?1",
                [c],
            )?;
            Ok(())
        })
        .await
        .unwrap();
    assert_signals(&td, &card, json!(["token_budget", "turn_budget"])).await;
    td.put_as(&lead, "/api/projects/p/trouble", json!({}))
        .await
        .ok();
    assert_signals(&td, &card, json!([])).await;
}

#[tokio::test]
async fn history_signals_and_working_time_reach_the_card_and_board() {
    let (td, lead, worker, card) = setup().await;
    td.put_as(
        &lead,
        "/api/projects/p/trouble",
        json!({"time_seconds": {"small": 1}}),
    )
    .await
    .ok();
    let c = card.clone();
    state(&td).db.write("system", move |t| {
        let base = chrono::Utc::now() - chrono::Duration::seconds(100);
        let claim = json!({"change": "claimed", "assignee": worker, "column_role": "working"});
        let mut events = vec![("status", json!({"status": "working"})), ("card", claim.clone())];
        for n in 0..4 {
            events.push(("card", json!({"change": "eta", "eta_at": (base + chrono::Duration::hours(1 + n)).to_rfc3339()})));
        }
        for _ in 0..2 {
            events.push(("card", json!({"change": "moved", "to_role": "review"})));
            events.push(("card", claim.clone()));
        }
        for n in 0..3 {
            let id = format!("poll{n}");
            let due = (base + chrono::Duration::seconds(50)).to_rfc3339();
            t.execute("INSERT INTO waits(id, agent, project, kind, spec, summary, created_by, created_at, due_at, state)
                VALUES (?1, ?2, 'p', 'time', '{}', 'poll', ?2, ?3, ?4, 'resolved')", rusqlite::params![id, worker, base.to_rfc3339(), due])?;
            events.push(("wait", json!({"change": "opened", "wait_id": id, "kind": "time"})));
            events.push(("wait", json!({"change": "resolved", "wait_id": id, "kind": "time"})));
            events.push(("agent.reset", json!({"kind": "compact"})));
        }
        for (n, (kind, data)) in events.into_iter().enumerate() {
            let mut draft = terrariumd::events::EventDraft::new(kind, "history", data).project("p").agent(&worker, "worker");
            if kind == "card" { draft = draft.card(&c); }
            let e = t.emit(draft)?;
            t.execute("UPDATE events SET at = ?2 WHERE seq = ?1", rusqlite::params![e.seq, (base + chrono::Duration::seconds(n as i64)).to_rfc3339()])?;
        }
        Ok(())
    }).await.unwrap();
    terrariumd::trouble::pass(&state(&td)).await;
    let expected = json!([
        "compactions",
        "eta_delayed",
        "review_rounds",
        "time_budget",
        "timer_polls"
    ]);
    assert_signals(&td, &card, expected.clone()).await;
    let u = usage(&td, &card).await;
    assert_eq!(u["signals"], expected);
    assert!(u["working_seconds"].as_u64().unwrap() >= 90);
    assert_eq!(
        td.get(&format!("/api/cards/{card}")).await.ok()["trouble"],
        expected
    );
    let b = td.get("/api/projects/p/board?view=summary").await;
    assert_eq!(b.ok()["cards"][0]["trouble"], expected);
    assert_eq!(notices(&td, &lead).await, 5);
    terrariumd::trouble::pass(&state(&td)).await;
    assert_eq!(notices(&td, &lead).await, 5);
    td.put_as(&lead, "/api/projects/p/trouble", json!({"time_seconds": {"small": 0}, "review_rounds": 0, "eta_delays": 0, "compactions": 0, "timer_polls": 0})).await.ok();
    assert_signals(&td, &card, json!([])).await;
}

#[tokio::test]
async fn deploy_baseline_ignores_existing_cards_until_a_new_claim() {
    let (mut td, lead, worker, card) = setup().await;
    state(&td)
        .db
        .write("system", |t| {
            t.execute(
                "UPDATE card_trouble_baseline SET seq = (SELECT max(seq) FROM events)",
                [],
            )?;
            Ok(())
        })
        .await
        .unwrap();
    eta(&td, &card, true).await;
    terrariumd::trouble::pass(&state(&td)).await;
    assert_eq!(notices(&td, &lead).await, 0);
    assert_eq!(usage(&td, &card).await["working_seconds"], 0);
    td.boot(None, None).await;
    terrariumd::trouble::pass(&state(&td)).await;
    assert_eq!(notices(&td, &lead).await, 0);
    td.post_as(
        &worker,
        &format!("/api/cards/{card}/move"),
        json!({"column": "ready"}),
    )
    .await
    .ok();
    td.post_as(
        &worker,
        &format!("/api/cards/{card}/assign"),
        json!({"assignee": null}),
    )
    .await
    .ok();
    td.post_as(&worker, &format!("/api/cards/{card}/claim"), json!({}))
        .await
        .ok();
    eta(&td, &card, true).await;
    assert_signals(&td, &card, json!(["eta_overdue"])).await;
    assert_eq!(notices(&td, &lead).await, 1);
}

#[tokio::test]
async fn review_wait_marks_and_clears_on_return_to_work() {
    let (td, lead, worker, card) = setup().await;
    td.put_as(
        &lead,
        "/api/projects/p/trouble",
        json!({"review_wait_seconds": 1}),
    )
    .await
    .ok();
    td.post_as(
        &worker,
        &format!("/api/cards/{card}/move"),
        json!({"column": "review"}),
    )
    .await
    .ok();
    let c = card.clone();
    state(&td).db.write("system", move |t| {
        t.execute("UPDATE events SET at = ?2 WHERE card = ?1 AND kind = 'card'
            AND json_extract(payload, '$.change') = 'moved' AND json_extract(payload, '$.to_role') = 'review'",
            rusqlite::params![c, (chrono::Utc::now() - chrono::Duration::seconds(5)).to_rfc3339()])?;
        Ok(())
    }).await.unwrap();
    assert_signals(&td, &card, json!(["review_wait"])).await;
    td.post_as(&worker, &format!("/api/cards/{card}/claim"), json!({}))
        .await
        .ok();
    assert_signals(&td, &card, json!([])).await;
}

#[tokio::test]
async fn real_claim_status_and_wait_events_drive_the_working_clock() {
    use std::time::Duration;
    let (td, _, worker, card) = setup().await;
    td.post_empty(&format!("/api/agents/{worker}/start"))
        .await
        .ok();
    td.wait_status(&worker, "idle").await;
    td.sup.open_turn(worker.parse().unwrap());
    td.wait_status(&worker, "working").await;
    tokio::time::sleep(Duration::from_millis(1100)).await;
    let running = usage(&td, &card).await["working_seconds"].as_u64().unwrap();
    assert!(running >= 1);
    let wait = td
        .post_as(
            &worker,
            &format!("/api/agents/{worker}/waits"),
            json!({"kind": "time",
        "at": (chrono::Utc::now() + chrono::Duration::minutes(5)).to_rfc3339()}),
        )
        .await;
    let wait_id = id(wait.ok());
    // Registering a wait during an open turn intentionally leaves its
    // status working. The clock must still exclude that open wait.
    let resting = usage(&td, &card).await["working_seconds"].clone();
    tokio::time::sleep(Duration::from_millis(1100)).await;
    assert_eq!(usage(&td, &card).await["working_seconds"], resting);
    td.req(
        hyper::Method::DELETE,
        &format!("/api/waits/{wait_id}"),
        None,
        Some(&worker),
    )
    .await
    .ok();
    td.wait_status(&worker, "working").await;
    tokio::time::sleep(Duration::from_millis(1100)).await;
    assert!(
        usage(&td, &card).await["working_seconds"].as_u64().unwrap() > resting.as_u64().unwrap()
    );
}

#[tokio::test]
async fn lead_assigned_card_is_monitored_without_a_claimed_event() {
    use std::time::Duration;
    let td = TestDaemon::start().await;
    td.project("p").await;
    let lead = id(&td
        .agent("p", "lead", json!({"roles": ["project_admin"]}))
        .await);
    let worker = id(&td.agent("p", "worker", json!({})).await);
    td.post_empty(&format!("/api/agents/{worker}/start"))
        .await
        .ok();
    td.wait_status(&worker, "idle").await;
    let card = id(td
        .post_as(
            &lead,
            "/api/projects/p/cards",
            json!({"title": "Assigned", "column": "ready"}),
        )
        .await
        .ok());
    td.post_as(
        &lead,
        &format!("/api/cards/{card}/assign"),
        json!({"assignee": worker}),
    )
    .await
    .ok();
    // The lead's assignment means this claim emits only the working move.
    td.post_as(&worker, &format!("/api/cards/{card}/claim"), json!({}))
        .await
        .ok();
    let c = card.clone();
    let claims: i64 = state(&td).db.read(move |db| Ok(db.query_row("SELECT count(*) FROM events WHERE card = ?1 AND json_extract(payload, '$.change') = 'claimed'", [c], |r| r.get(0))?)).await.unwrap();
    assert_eq!(claims, 0);
    td.wait_status(&worker, "idle").await;
    td.sup.open_turn(worker.parse().unwrap());
    td.wait_status(&worker, "working").await;
    tokio::time::sleep(Duration::from_millis(1100)).await;
    assert!(usage(&td, &card).await["working_seconds"].as_u64().unwrap() >= 1);
    eta(&td, &card, true).await;
    assert_signals(&td, &card, json!(["eta_overdue"])).await;
    assert_eq!(notices(&td, &lead).await, 1);
}

#[tokio::test]
async fn research_card_starts_its_clock_and_signals_without_working_entry() {
    use std::time::Duration;
    let td = TestDaemon::start().await;
    td.project("p").await;
    td.patch(
        "/api/projects/p/board",
        json!({"columns": [
            {"name":"backlog"}, {"name":"ready"}, {"name":"research"},
            {"name":"in_progress"}, {"name":"review"}, {"name":"blocked"}, {"name":"done"}
        ]}),
    )
    .await
    .ok();
    let lead = id(&td
        .agent("p", "lead", json!({"roles":["project_admin"]}))
        .await);
    let worker = id(&td.agent("p", "worker", json!({})).await);
    td.post_empty(&format!("/api/agents/{worker}/start"))
        .await
        .ok();
    td.wait_status(&worker, "idle").await;
    let card = id(td
        .post_as(
            &worker,
            "/api/projects/p/cards",
            json!({"title":"Research", "column":"research", "assignee":worker}),
        )
        .await
        .ok());
    td.wait_status(&worker, "idle").await;
    td.sup.open_turn(worker.parse().unwrap());
    td.wait_status(&worker, "working").await;
    tokio::time::sleep(Duration::from_millis(1100)).await;
    assert!(usage(&td, &card).await["working_seconds"].as_u64().unwrap() >= 1);
    eta(&td, &card, true).await;
    assert_signals(&td, &card, json!(["eta_overdue"])).await;
    assert_eq!(notices(&td, &lead).await, 1);
}

#[tokio::test]
async fn passed_eta_has_one_worker_reminder_and_only_a_lead_trouble_mail() {
    use std::time::{Duration, Instant};
    use terrarium_core::config::{CompactMaxAge, IdleStop};
    let (td, lead, worker, card) = setup().await;
    let s = state(&td);
    s.lifecycle.reminders.set_default(true);
    s.lifecycle.set_tuning(terrariumd::lifecycle::Tuning {
        idle_stop: IdleStop::Off,
        tick: Duration::from_millis(20),
        compact_on_idle: false,
        compact_max_age: CompactMaxAge::Off,
        compact_wait: Duration::from_secs(5),
    });
    td.post_empty(&format!("/api/agents/{worker}/start"))
        .await
        .ok();
    td.wait_status(&worker, "idle").await;
    eta(&td, &card, true).await;
    assert_signals(&td, &card, json!(["eta_overdue"])).await;
    assert_eq!(notices(&td, &worker).await, 0);
    td.post(
        &format!("/api/agents/{worker}/send"),
        json!({"text":"work"}),
    )
    .await
    .ok();
    let aid = worker.parse().unwrap();
    let started = Instant::now();
    loop {
        let reminders: Vec<_> = td
            .sup
            .prompts(aid)
            .into_iter()
            .filter(|p| p.starts_with("[System:card]"))
            .collect();
        if !reminders.is_empty() {
            assert_eq!(reminders.len(), 1);
            assert!(reminders[0].contains("ETA has passed"));
            break;
        }
        assert!(
            started.elapsed() < crate::common::WAIT,
            "ETA reminder did not arrive"
        );
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    td.wait_status(&worker, "idle").await;
    terrariumd::trouble::pass(&s).await;
    tokio::time::sleep(Duration::from_millis(100)).await;
    assert_eq!(
        td.sup
            .prompts(aid)
            .iter()
            .filter(|p| p.starts_with("[System:card]"))
            .count(),
        1
    );
    assert_eq!(notices(&td, &worker).await, 0);
    assert_eq!(notices(&td, &lead).await, 1);
    assert_eq!(usage(&td, &card).await["signals"], json!(["eta_overdue"]));
}

#[tokio::test]
async fn passed_eta_still_mails_worker_when_reminders_are_disabled() {
    let (td, lead, worker, card) = setup().await;
    td.patch(
        &format!("/api/agents/{worker}"),
        json!({"card_reminder":false}),
    )
    .await
    .ok();
    eta(&td, &card, true).await;
    assert_signals(&td, &card, json!(["eta_overdue"])).await;
    assert_eq!(notices(&td, &worker).await, 1);
    assert_eq!(notices(&td, &lead).await, 1);
}
