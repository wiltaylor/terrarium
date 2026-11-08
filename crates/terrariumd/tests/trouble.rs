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

#[tokio::test]
async fn notice_is_once_across_ticks_resolution_recurrence_and_restart() {
    let (mut td, lead, worker, card) = setup().await;
    eta(&td, &card, true).await;
    terrariumd::trouble::pass(&state(&td)).await;
    terrariumd::trouble::pass(&state(&td)).await;
    assert_eq!(usage(&td, &card).await["signals"], json!(["eta_overdue"]));
    assert_eq!(notices(&td, &lead).await, 1);
    assert_eq!(notices(&td, &worker).await, 1);
    eta(&td, &card, false).await;
    terrariumd::trouble::pass(&state(&td)).await;
    assert_eq!(usage(&td, &card).await["signals"], json!([]));
    td.boot(None, None).await;
    eta(&td, &card, true).await;
    terrariumd::trouble::pass(&state(&td)).await;
    assert_eq!(notices(&td, &lead).await, 1);
    assert_eq!(notices(&td, &worker).await, 1);
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
    terrariumd::trouble::pass(&state(&td)).await;
    assert_eq!(usage(&td, &card).await["signals"], json!([]));
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
    terrariumd::trouble::pass(&state(&td)).await;
    assert_eq!(usage(&td, &card).await["signals"], json!(["turn_budget"]));
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
    terrariumd::trouble::pass(&state(&td)).await;
    assert_eq!(
        usage(&td, &card).await["signals"],
        json!(["token_budget", "turn_budget"])
    );
    td.put_as(&lead, "/api/projects/p/trouble", json!({}))
        .await
        .ok();
    terrariumd::trouble::pass(&state(&td)).await;
    assert_eq!(usage(&td, &card).await["signals"], json!([]));
}
