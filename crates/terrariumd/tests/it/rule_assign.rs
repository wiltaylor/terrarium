//! The `card.assign` rule action with a template (TER-1115, kanban rework
//! C4, D6): on a columns board a column rule starts the bound agent that
//! the `card_dispatcher` setting starts on other boards, with conditions
//! on the card (`if_card.any_labels`) and a `<label>=<template>` map; the
//! setting is refused there.
#![allow(clippy::unwrap_used, clippy::expect_used)]

use crate::common;

use std::time::Duration;

use common::TestDaemon;
use serde_json::{Value, json};
use terrariumd::api::Shared;
use terrariumd::rules_engine;

fn state(td: &TestDaemon) -> Shared {
    td.daemon.as_ref().unwrap().state.clone()
}

/// A project `p` with a working column `coding` (WIP `limit`), the three
/// type templates, a lead that may spawn bound agents, on column rules.
async fn setup(limit: i64) -> TestDaemon {
    let td = TestDaemon::start().await;
    // The dispatcher timer of the setting sleeps: only the rules act.
    let s = state(&td);
    s.lifecycle.set_tuning(terrariumd::lifecycle::Tuning {
        tick: Duration::from_secs(24 * 60 * 60),
        ..s.lifecycle.tuning()
    });
    td.project("p").await;
    td.patch(
        "/api/projects/p/board",
        json!({"columns": [
            {"name":"backlog"}, {"name":"ready"},
            {"name":"coding", "role":"working", "wip_limit":limit},
            {"name":"review", "role":"review"},
            {"name":"blocked", "role":"blocked"}, {"name":"done"}
        ]}),
    )
    .await
    .ok();
    for name in ["card-small", "card-impl", "card-impl-hard"] {
        td.post(
            "/api/templates",
            json!({"name":name,"project":"p","spec":{"harness":"codex"}}),
        )
        .await
        .ok();
    }
    td.agent(
        "p",
        "lead",
        json!({"roles":["project_admin"],"policy":{
            "spawn":{"templates":["project:*"],"bound":true,
                "max_concurrent":8,"max_per_day":20}
        }}),
    )
    .await;
    s.db.write("operator", |t| {
        terrariumd::store::projects::set_columns_flow(t, "p")?;
        Ok(())
    })
    .await
    .unwrap();
    td
}

fn path(column: &str) -> String {
    format!("/api/projects/p/board/columns/{column}/options")
}

async fn set_rules(td: &TestDaemon, column: &str, rules: Value) {
    let o = td.get(&path(column)).await.ok().clone();
    td.put(
        &path(column),
        json!({"base_version": o["version"], "checks": o["checks"], "queue": o["queue"],
               "rules": rules}),
    )
    .await
    .ok();
}

async fn new_card(td: &TestDaemon, column: &str, labels: Value) -> String {
    td.post(
        "/api/projects/p/cards",
        json!({"title": "Task", "column": column, "labels": labels}),
    )
    .await
    .ok()["id"]
        .as_str()
        .unwrap()
        .to_owned()
}

async fn card(td: &TestDaemon, id: &str) -> Value {
    td.get(&format!("/api/cards/{id}")).await.ok().clone()
}

/// The run log of `p`, oldest first.
async fn runs(td: &TestDaemon) -> Vec<Value> {
    let mut v = td.get("/api/projects/p/rules/runs?limit=500").await.ok()["runs"]
        .as_array()
        .unwrap()
        .clone();
    v.reverse();
    v
}

/// Wait until the engine read the whole log and ran every pending row.
async fn settle(td: &TestDaemon) {
    let start = std::time::Instant::now();
    let mut quiet = 0;
    loop {
        let (cursor, last, pending) = state(td)
            .db
            .read(|c| {
                let cursor = rules_engine::cursor(c)?.unwrap_or(0);
                let last = terrariumd::events::last_seq(c)?;
                let pending: i64 = c.query_row(
                    "SELECT count(*) FROM rule_runs WHERE state = 'pending'",
                    [],
                    |r| r.get(0),
                )?;
                Ok((cursor, last, pending))
            })
            .await
            .unwrap();
        if cursor >= last && pending == 0 {
            quiet += 1;
            if quiet >= 3 {
                return;
            }
        } else {
            quiet = 0;
        }
        assert!(
            start.elapsed() < Duration::from_secs(30),
            "the engine did not settle"
        );
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
}

/// Wait until card `id` is in `column` (the spawn runs after the commit).
async fn wait_column(td: &TestDaemon, id: &str, column: &str) -> Value {
    let start = std::time::Instant::now();
    loop {
        let c = card(td, id).await;
        if c["column"] == column {
            return c;
        }
        assert!(
            start.elapsed() < Duration::from_secs(30),
            "{id} is not in {column}: {c}"
        );
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
}

fn type_map() -> Value {
    json!([
        "type:small=card-small",
        "type:impl=card-impl",
        "type:hard=card-impl-hard",
        "type:impl-hard=card-impl-hard"
    ])
}

/// The dispatcher map as an editable rule: a card that lands in ready with
/// a type label gets a bound agent from its template, which claims it.
/// `any_labels` keeps the rule off a card with no type label.
#[tokio::test]
async fn a_lands_rule_starts_the_template_agent_that_claims_the_card() {
    let td = setup(2).await;
    set_rules(
        &td,
        "ready",
        json!([{
            "name": "dispatch", "trigger": {"kind": "lands"},
            "if_card": [{"any_labels": ["type:small", "type:impl", "type:hard",
                                        "type:impl-hard"]}],
            "actions": [{"verb": "card.assign",
                         "args": {"templates": type_map(), "owner": "lead"}}]
        }]),
    )
    .await;
    let small = new_card(&td, "ready", json!(["type:impl"])).await;
    let plain = new_card(&td, "ready", json!(["docs"])).await;
    settle(&td).await;
    let c = wait_column(&td, &small, "coding").await;
    assert_eq!(
        c["assignee_name"],
        format!("p/{}", small.to_ascii_lowercase()),
        "{c}"
    );
    // The bound agent comes from the template of the card's label, with
    // the owner's spawn rights.
    let id = small.clone();
    let (template, owner): (String, String) = state(&td)
        .db
        .read(move |c| {
            Ok(c.query_row(
                "SELECT template, owner_name FROM spawns
                  WHERE bound_kind = 'card' AND bound_id = ?1",
                [&id],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )?)
        })
        .await
        .unwrap();
    assert!(template.ends_with("card-impl"), "{template}");
    assert!(owner.ends_with("lead"), "{owner}");
    // No type label: the condition holds the rule back.
    settle(&td).await;
    let p = card(&td, &plain).await;
    assert_eq!(p["column"], "ready");
    assert!(p["assignee"].is_null());
    let r = runs(&td).await;
    let mine: Vec<&Value> = r.iter().filter(|r| r["card"] == small.as_str()).collect();
    assert_eq!(mine.len(), 1, "{r:?}");
    assert_eq!(mine[0]["state"], "done", "{:?}", mine[0]);
    assert!(
        r.iter()
            .filter(|r| r["card"] == plain.as_str())
            .all(|r| r["state"] == "skipped"),
        "{r:?}"
    );
}

/// `from` picks the top eligible cards of a column up to `limit`, and only
/// while `room` is under its WIP limit; a card with two map labels has no
/// template and stays.
#[tokio::test]
async fn from_takes_the_top_cards_while_the_working_column_has_room() {
    let td = setup(2).await;
    let two = new_card(&td, "ready", json!(["type:small", "type:impl"])).await;
    let a = new_card(&td, "ready", json!(["type:small"])).await;
    let b = new_card(&td, "ready", json!(["type:hard"])).await;
    let c = new_card(&td, "ready", json!(["type:small"])).await;
    set_rules(
        &td,
        "backlog",
        json!([{
            "name": "kick", "trigger": {"kind": "lands"},
            "actions": [{"verb": "card.assign",
                         "args": {"from": "@ready", "limit": 4, "templates": type_map(),
                                  "owner": "lead"}}]
        }]),
    )
    .await;
    new_card(&td, "backlog", json!([])).await;
    settle(&td).await;
    wait_column(&td, &a, "coding").await;
    wait_column(&td, &b, "coding").await;
    // WIP 2: the third eligible card waits in ready.
    settle(&td).await;
    assert_eq!(card(&td, &c).await["column"], "ready");
    assert_eq!(card(&td, &two).await["column"], "ready");
    // A full column is a no-op with the reason.
    new_card(&td, "backlog", json!([])).await;
    settle(&td).await;
    let r = runs(&td).await;
    let last = r
        .iter()
        .rev()
        .find(|r| r["rule"] == "backlog/kick")
        .unwrap();
    assert_eq!(last["state"], "noop", "{last}");
    assert!(last.to_string().contains("no room"), "{last}");
}

/// On a columns board the `card_dispatcher` setting is refused; `off`
/// stays accepted, and the timer of the setting starts nothing.
#[tokio::test]
async fn the_card_dispatcher_setting_is_refused_on_a_columns_board() {
    let td = setup(2).await;
    td.patch("/api/projects/p", json!({"card_dispatcher": "lead"}))
        .await
        .err(409, "conflict");
    td.patch("/api/projects/p", json!({"card_dispatcher": "off"}))
        .await
        .ok();
    let id = new_card(&td, "ready", json!(["type:small"])).await;
    assert!(
        !terrariumd::card_dispatcher::dispatch(&state(&td), "p")
            .await
            .unwrap()
    );
    assert_eq!(card(&td, &id).await["column"], "ready");
}

/// The rule's checks: exactly one of `who`, `template` and `templates`; a
/// new agent needs `owner`; a bad map entry is a 422 with its path.
#[tokio::test]
async fn a_template_assign_needs_an_owner_and_well_formed_entries() {
    let td = setup(2).await;
    let o = td.get(&path("ready")).await.ok().clone();
    for (args, needle) in [
        (json!({"template": "card-small"}), "needs `owner`"),
        (
            json!({"who": "lead", "template": "card-small", "owner": "lead"}),
            "exactly one of",
        ),
        (
            json!({"templates": ["type:small"], "owner": "lead"}),
            "is not `<label>=<template>`",
        ),
        (json!({"who": "lead", "from": "@ready"}), "for a new agent"),
        (
            json!({"template": "card-small", "owner": "lead", "limit": 2}),
            "limit needs `from`",
        ),
    ] {
        let r = td
            .put(
                &path("ready"),
                json!({"base_version": o["version"], "checks": o["checks"],
                       "queue": o["queue"],
                       "rules": [{"name": "x", "trigger": {"kind": "lands"},
                                  "actions": [{"verb": "card.assign", "args": args}]}]}),
            )
            .await;
        assert_eq!(r.status.as_u16(), 422, "{args}: {}", r.body);
        assert!(r.body.to_string().contains(needle), "{args}: {}", r.body);
    }
}
