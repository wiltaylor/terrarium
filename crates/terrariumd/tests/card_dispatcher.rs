//! Dispatcher tests run real transactions with a fake harness.
#![allow(clippy::unwrap_used, clippy::expect_used)]
use crate::common::TestDaemon;
use serde_json::{Value, json};
use terrariumd::card_dispatcher::dispatch;

async fn setup(limit: i64, concurrent: u32, daily: u32) -> TestDaemon {
    let td = TestDaemon::start().await;
    td.project("p").await;
    td.patch(
        "/api/projects/p/board",
        json!({"columns": [
            {"name":"backlog"}, {"name":"ready"},
            {"name":"coding", "role":"working", "wip_limit":limit},
            {"name":"review", "role":"review"},
            {"name":"merged", "role":"accepted"},
            {"name":"blocked", "role":"blocked"}, {"name":"done"}
        ]}),
    )
    .await
    .ok();
    for name in ["card-small", "card-impl", "card-impl-hard"] {
        td.post(
            "/api/templates",
            json!({"name":name,"project":"p",
            "spec":{"harness":"codex"}}),
        )
        .await
        .ok();
    }
    td.agent(
        "p",
        "lead",
        json!({"roles":["project_admin"],"policy":{
            "spawn":{"templates":["project:*"],"bound":true,
                "max_concurrent":concurrent,"max_per_day":daily}
        }}),
    )
    .await;
    td
}

fn state(td: &TestDaemon) -> terrariumd::api::Shared {
    td.daemon.as_ref().unwrap().state.clone()
}

async fn card(td: &TestDaemon, kind: &str) -> String {
    td.post(
        "/api/projects/p/cards",
        json!({"title":"Task", "column":"ready",
        "labels":[format!("type:{kind}")]}),
    )
    .await
    .ok()["id"]
        .as_str()
        .unwrap()
        .into()
}

async fn enabled(td: &TestDaemon) {
    td.patch("/api/projects/p", json!({"card_dispatcher":"lead"}))
        .await
        .ok();
}

async fn get(td: &TestDaemon, id: &str) -> Value {
    td.get(&format!("/api/cards/{id}")).await.ok().clone()
}

#[tokio::test]
async fn off_by_default_and_disabled_explicitly() {
    let td = setup(2, 2, 10).await;
    let id = card(&td, "small").await;
    assert!(!dispatch(&state(&td), "p").await.unwrap());
    enabled(&td).await;
    td.patch("/api/projects/p", json!({"card_dispatcher":"off"}))
        .await
        .ok();
    assert!(!dispatch(&state(&td), "p").await.unwrap());
    assert!(get(&td, &id).await["agent"].is_null());
}

#[tokio::test]
async fn claims_in_order_with_role_based_wip_and_no_duplicate() {
    let td = setup(1, 3, 10).await;
    let first = card(&td, "impl").await;
    let second = card(&td, "hard").await;
    enabled(&td).await;
    assert!(dispatch(&state(&td), "p").await.unwrap());
    let c = get(&td, &first).await;
    assert_eq!(c["column"], "coding");
    assert_eq!(c["agent"], c["assignee"]);
    assert_eq!(c["agent_name"], first.to_ascii_lowercase());
    assert!(!dispatch(&state(&td), "p").await.unwrap());
    assert!(get(&td, &second).await["agent"].is_null());
}

#[tokio::test]
async fn concurrent_dispatchers_cannot_overfill_wip() {
    let td = setup(1, 3, 10).await;
    let first = card(&td, "small").await;
    let second = card(&td, "small").await;
    enabled(&td).await;
    let s = state(&td);
    let (a, b) = tokio::join!(dispatch(&s, "p"), dispatch(&s, "p"));
    assert_eq!(
        usize::from(a.unwrap_or(false)) + usize::from(b.unwrap_or(false)),
        1
    );
    assert_eq!(get(&td, &first).await["column"], "coding");
    assert!(get(&td, &second).await["agent"].is_null());
}

#[tokio::test]
async fn skips_unfinished_dependencies_and_unknown_types() {
    let td = setup(2, 3, 10).await;
    let dependency = card(&td, "unknown").await;
    let blocked = card(&td, "small").await;
    td.patch(
        &format!("/api/cards/{blocked}"),
        json!({"depends_on":[dependency]}),
    )
    .await
    .ok();
    let eligible = card(&td, "hard").await;
    enabled(&td).await;
    assert!(dispatch(&state(&td), "p").await.unwrap());
    assert!(get(&td, &blocked).await["agent"].is_null());
    assert_eq!(get(&td, &eligible).await["column"], "coding");
}

#[tokio::test]
async fn daily_spawn_cap_is_not_bypassed_by_daemon() {
    let td = setup(3, 3, 1).await;
    card(&td, "small").await;
    let second = card(&td, "small").await;
    enabled(&td).await;
    assert!(dispatch(&state(&td), "p").await.unwrap());
    let error = dispatch(&state(&td), "p").await.unwrap_err();
    assert!(error.message.contains("max_per_day"), "{}", error.message);
    assert!(get(&td, &second).await["agent"].is_null());
    assert_eq!(get(&td, &second).await["column"], "ready");
}

#[tokio::test]
async fn concurrent_spawn_cap_leaves_card_ready() {
    let td = setup(3, 1, 10).await;
    card(&td, "small").await;
    let second = card(&td, "small").await;
    enabled(&td).await;
    assert!(dispatch(&state(&td), "p").await.unwrap());
    let error = dispatch(&state(&td), "p").await.unwrap_err();
    assert!(
        error.message.contains("max_concurrent"),
        "{}",
        error.message
    );
    assert!(get(&td, &second).await["agent"].is_null());
}

#[tokio::test]
async fn missing_owner_leaves_card_unclaimed() {
    let td = setup(2, 2, 10).await;
    let id = card(&td, "small").await;
    td.patch("/api/projects/p", json!({"card_dispatcher":"missing"}))
        .await
        .ok();
    assert!(dispatch(&state(&td), "p").await.is_err());
    assert!(get(&td, &id).await["assignee"].is_null());
    assert!(get(&td, &id).await["agent"].is_null());
}

#[tokio::test]
async fn skips_existing_bound_agent_even_without_assignee() {
    let td = setup(2, 3, 10).await;
    let first = card(&td, "small").await;
    let second = card(&td, "small").await;
    let lead = state(&td)
        .db
        .read(|c| {
            Ok(terrariumd::store::agents::find(c, "lead", Some("p"))?
                .agent
                .id
                .to_string())
        })
        .await
        .unwrap();
    td.post_as(
        &lead,
        &format!("/api/agents/{lead}/spawn"),
        json!({"template":"card-small", "task":"Own this card",
            "bind":format!("card:{first}")}),
    )
    .await
    .ok();
    let original = get(&td, &first).await["agent"].clone();
    assert!(original.is_string());
    assert!(get(&td, &first).await["assignee"].is_null());
    enabled(&td).await;
    assert!(dispatch(&state(&td), "p").await.unwrap());
    assert_eq!(get(&td, &first).await["agent"], original);
    assert_eq!(get(&td, &second).await["column"], "coding");
}

#[tokio::test]
async fn name_conflict_does_not_claim_or_bind_the_card() {
    let td = setup(2, 3, 10).await;
    let id = card(&td, "small").await;
    td.agent("p", &id.to_ascii_lowercase(), json!({})).await;
    enabled(&td).await;
    assert!(dispatch(&state(&td), "p").await.is_err());
    let c = get(&td, &id).await;
    assert!(c["agent"].is_null());
    assert!(c["assignee"].is_null());
    assert_eq!(c["column"], "ready");
}

#[tokio::test]
async fn timer_pass_fills_only_available_places() {
    let td = setup(2, 3, 10).await;
    let first = card(&td, "small").await;
    let second = card(&td, "impl").await;
    let third = card(&td, "hard").await;
    enabled(&td).await;
    terrariumd::card_dispatcher::pass(&state(&td)).await;
    assert_eq!(get(&td, &first).await["column"], "coding");
    assert_eq!(get(&td, &second).await["column"], "coding");
    assert_eq!(get(&td, &third).await["column"], "ready");
    terrariumd::card_dispatcher::pass(&state(&td)).await;
    assert!(get(&td, &third).await["agent"].is_null());
}

async fn lead_id(td: &TestDaemon) -> String {
    state(td)
        .db
        .read(|c| {
            Ok(terrariumd::store::agents::find(c, "lead", Some("p"))?
                .agent
                .id
                .to_string())
        })
        .await
        .unwrap()
}

#[tokio::test]
async fn failures_back_off_once_and_do_not_block_later_cards() {
    let td = setup(3, 3, 20).await;
    let bad = card(&td, "small").await;
    let obstruction = td.agent("p", &bad.to_ascii_lowercase(), json!({})).await;
    let good = card(&td, "impl").await;
    enabled(&td).await;
    terrariumd::card_dispatcher::pass(&state(&td)).await;
    assert_eq!(get(&td, &good).await["column"], "coding");
    assert_eq!(get(&td, &bad).await["column"], "ready");
    assert_eq!(
        get(&td, &bad).await["comments"].as_array().unwrap().len(),
        1
    );
    assert!(!dispatch(&state(&td), "p").await.unwrap());
    for attempt in 2..=8 {
        let key = bad.clone();
        state(&td)
            .db
            .write("operator", move |t| {
                t.execute(
                    "UPDATE card_dispatch_retries SET retry_at = 0 WHERE card = ?1",
                    [&key],
                )?;
                Ok(())
            })
            .await
            .unwrap();
        assert!(dispatch(&state(&td), "p").await.is_err());
        let key = bad.clone();
        let (n, remaining): (u32, i64) = state(&td)
            .db
            .read(move |c| {
                Ok(c.query_row(
                    "SELECT failures, retry_at FROM card_dispatch_retries WHERE card = ?1",
                    [&key],
                    |r| {
                        Ok((
                            r.get(0)?,
                            r.get::<_, i64>(1)? - chrono::Utc::now().timestamp(),
                        ))
                    },
                )?)
            })
            .await
            .unwrap();
        assert_eq!(n, attempt);
        let delay = (60_i64 << (attempt - 1).min(5)).min(1800);
        assert!(
            (delay - 2..=delay).contains(&remaining),
            "{remaining} vs {delay}"
        );
    }
    assert_eq!(
        get(&td, &bad).await["comments"].as_array().unwrap().len(),
        1
    );
    let mails: i64 = state(&td).db.read(|c| Ok(c.query_row("SELECT count(*) FROM messages WHERE subject LIKE '%dispatcher could not start agent'", [], |r| r.get(0))?)).await.unwrap();
    assert_eq!(mails, 1);
    td.delete(&format!(
        "/api/agents/{}",
        obstruction["id"].as_str().unwrap()
    ))
    .await
    .ok();
    let key = bad.clone();
    state(&td)
        .db
        .write("operator", move |t| {
            t.execute(
                "UPDATE card_dispatch_retries SET retry_at = 0 WHERE card = ?1",
                [&key],
            )?;
            Ok(())
        })
        .await
        .unwrap();
    assert!(dispatch(&state(&td), "p").await.unwrap());
    assert_eq!(get(&td, &bad).await["column"], "coding");
    let remaining: i64 = state(&td)
        .db
        .read(|c| {
            Ok(
                c.query_row("SELECT count(*) FROM card_dispatch_retries", [], |r| {
                    r.get(0)
                })?,
            )
        })
        .await
        .unwrap();
    assert_eq!(remaining, 0);
}

#[tokio::test]
async fn dispatch_uses_template_targets_not_all_owner_targets() {
    let td = setup(2, 3, 10).await;
    for name in ["p-needed", "p-extra"] {
        td.post(
            "/api/targets",
            json!({"name":name,"kind":"ssh","project":"p","address":"u@h"}),
        )
        .await
        .ok();
    }
    let lead = lead_id(&td).await;
    td.patch(
        &format!("/api/agents/{lead}"),
        json!({"policy":{
            "targets":["p-needed","p-extra"],
            "spawn":{"templates":["project:*"],"bound":true,"max_concurrent":3}
        }}),
    )
    .await
    .ok();
    td.patch(
        "/api/templates/p%2Fcard-small",
        json!({"spec":{
            "harness":"codex","policy":{"targets":["p-needed"]}
        }}),
    )
    .await
    .ok();
    let id = card(&td, "small").await;
    enabled(&td).await;
    assert!(dispatch(&state(&td), "p").await.unwrap());
    let c = get(&td, &id).await;
    let a = td
        .get(&format!("/api/agents/{}", c["agent"].as_str().unwrap()))
        .await;
    assert_eq!(a.ok()["policy"]["targets"], json!(["p-needed"]));
    let manual = td
        .post_as(
            &lead,
            &format!("/api/agents/{lead}/spawn"),
            json!({"template":"card-small","task":"Compare target rights"}),
        )
        .await;
    let child = &manual.ok()["agent"];
    assert_eq!(child["policy"]["targets"], a.ok()["policy"]["targets"]);
}

#[tokio::test]
async fn paused_owner_does_not_dispatch() {
    let td = setup(2, 2, 10).await;
    let id = card(&td, "small").await;
    enabled(&td).await;
    td.post_empty(&format!("/api/agents/{}/pause", lead_id(&td).await))
        .await
        .ok();
    assert!(!dispatch(&state(&td), "p").await.unwrap());
    assert!(get(&td, &id).await["agent"].is_null());
}

#[tokio::test]
async fn accepted_dependency_allows_claim_without_waiting_for_release() {
    let td = setup(2, 2, 10).await;
    let dep = card(&td, "unknown").await;
    let id = card(&td, "small").await;
    td.patch(&format!("/api/cards/{id}"), json!({"depends_on":[dep]}))
        .await
        .ok();
    td.post(
        &format!("/api/cards/{dep}/move"),
        json!({"column":"merged"}),
    )
    .await
    .ok();
    assert_eq!(get(&td, &id).await["blocked_by"], json!([]));
    enabled(&td).await;
    assert!(dispatch(&state(&td), "p").await.unwrap());
    assert_eq!(get(&td, &id).await["column"], "coding");
}

#[tokio::test]
async fn file_removal_of_setting_disables_dispatch() {
    let td = setup(2, 2, 10).await;
    for setting in [Some("lead"), None] {
        let mut def = json!({"name":"p"});
        if let Some(owner) = setting {
            def["card_dispatcher"] = json!(owner);
        }
        let defs = terrarium_core::config::Definitions {
            projects: vec![serde_json::from_value(def).unwrap()],
            ..Default::default()
        };
        let path = td.state.clone();
        state(&td)
            .db
            .write("operator", move |t| {
                terrariumd::store::sync::sync(t, &defs, &path)
            })
            .await
            .unwrap();
        assert_eq!(
            td.get("/api/projects/p").await.ok()["card_dispatcher"],
            json!(setting)
        );
    }
    card(&td, "small").await;
    assert!(!dispatch(&state(&td), "p").await.unwrap());
}

#[tokio::test]
async fn fleet_setting_can_enable_and_disable_dispatch() {
    let td = TestDaemon::start().await;
    for setting in ["card_dispatcher = \"lead\"", ""] {
        let content = format!("name = \"dispatch\"\n[[projects]]\nname = \"p\"\n{setting}\n");
        td.post("/api/fleet/apply", json!({"content":content}))
            .await
            .ok();
        let expected = if setting.is_empty() {
            Value::Null
        } else {
            json!("lead")
        };
        assert_eq!(
            td.get("/api/projects/p").await.ok()["card_dispatcher"],
            expected
        );
    }
}
