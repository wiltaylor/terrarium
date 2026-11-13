//! The column rules in the engine (TER-1113, kanban rework C2a): a project
//! with `board_flow = columns` runs the rules of its board columns on their
//! triggers, in the order of TER-1112 Q3, with the loop guards of the
//! daemon; a project without the switch runs none of them; a column rename
//! rewrites the rules that name it, and a removal is refused while a rule
//! names it (TER-1159).
#![allow(clippy::unwrap_used, clippy::expect_used)]

use crate::common;

use std::collections::HashMap;
use std::time::Duration;

use common::TestDaemon;
use serde_json::{Value, json};
use terrariumd::api::Shared;
use terrariumd::rules_engine;

fn state(td: &TestDaemon) -> Shared {
    td.daemon.as_ref().unwrap().state.clone()
}

async fn setup() -> TestDaemon {
    let td = TestDaemon::start().await;
    td.project("flow").await;
    td
}

/// Switch `project` to the column rules, as the C5 upgrade will.
async fn switch(td: &TestDaemon, project: &str) {
    let p = project.to_owned();
    state(td)
        .db
        .write("operator", move |t| {
            terrariumd::store::projects::set_columns_flow(t, &p)?;
            Ok(())
        })
        .await
        .unwrap();
}

fn path(project: &str, column: &str) -> String {
    format!("/api/projects/{project}/board/columns/{column}/options")
}

async fn options(td: &TestDaemon, project: &str, column: &str) -> Value {
    td.get(&path(project, column)).await.ok().clone()
}

async fn set_rules(td: &TestDaemon, project: &str, column: &str, rules: Value) {
    let o = options(td, project, column).await;
    td.put(
        &path(project, column),
        json!({"base_version": o["version"], "checks": o["checks"], "queue": o["queue"],
               "rules": rules}),
    )
    .await
    .ok();
}

fn note(name: &str, trigger: Value, text: &str) -> Value {
    json!({"name": name, "trigger": trigger,
           "actions": [{"verb": "card.comment", "args": {"text": text}}]})
}

async fn new_card(td: &TestDaemon, project: &str, title: &str) -> String {
    td.post(
        &format!("/api/projects/{project}/cards"),
        json!({"title": title}),
    )
    .await
    .ok()["id"]
        .as_str()
        .unwrap()
        .to_owned()
}

async fn move_to(td: &TestDaemon, card: &str, column: &str) {
    td.post(
        &format!("/api/cards/{card}/move"),
        json!({"column": column}),
    )
    .await
    .ok();
}

/// The comments of a card: (author, body), oldest first.
async fn comments(td: &TestDaemon, card: &str) -> Vec<(String, String)> {
    td.get(&format!("/api/cards/{card}")).await.ok()["comments"]
        .as_array()
        .unwrap()
        .iter()
        .map(|c| {
            (
                c["author"].as_str().unwrap().to_owned(),
                c["body"].as_str().unwrap().to_owned(),
            )
        })
        .collect()
}

fn bodies(c: &[(String, String)]) -> Vec<&str> {
    c.iter().map(|(_, b)| b.as_str()).collect()
}

/// The run log of a project, oldest first.
async fn runs(td: &TestDaemon, project: &str) -> Vec<Value> {
    let mut v = td
        .get(&format!("/api/projects/{project}/rules/runs?limit=500"))
        .await
        .ok()["runs"]
        .as_array()
        .unwrap()
        .clone();
    v.reverse();
    v
}

/// Wait until the engine read the whole log and ran every pending row,
/// three times in a row.
async fn settle(td: &TestDaemon) {
    let mut quiet = 0;
    let start = std::time::Instant::now();
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
            start.elapsed() < Duration::from_secs(120),
            "the rule engine did not settle: cursor={cursor} last={last} pending={pending}"
        );
        tokio::time::sleep(Duration::from_millis(30)).await;
    }
}

#[tokio::test]
async fn leaves_runs_before_lands_with_the_column_rule_actor() {
    let td = setup().await;
    set_rules(
        &td,
        "flow",
        "review",
        json!([
            note("in", json!({"kind": "lands"}), "landed in review"),
            note("out", json!({"kind": "leaves"}), "left review"),
        ]),
    )
    .await;
    set_rules(
        &td,
        "flow",
        "ready",
        json!([note("in", json!({"kind": "lands"}), "landed in ready")]),
    )
    .await;
    switch(&td, "flow").await;
    let a = new_card(&td, "flow", "a").await;
    move_to(&td, &a, "review").await;
    settle(&td).await;
    move_to(&td, &a, "ready").await;
    settle(&td).await;
    let c = comments(&td, &a).await;
    assert_eq!(
        bodies(&c),
        vec!["landed in review", "left review", "landed in ready"]
    );
    // The actor names the column: two rules called `in` stay apart.
    assert_eq!(c[0].0, "rule:flow/review/in");
    assert_eq!(c[2].0, "rule:flow/ready/in");
    // The run log has the rule ID and the column.
    let r = runs(&td, "flow").await;
    let out: Vec<&Value> = r.iter().filter(|r| r["rule"] == "review/out").collect();
    assert_eq!(out.len(), 1);
    assert_eq!(out[0]["column"], "review");
    assert!(out[0]["rule_id"].as_str().is_some_and(|s| !s.is_empty()));
    assert_eq!(out[0]["state"], "done");
}

#[tokio::test]
async fn without_the_switch_the_column_rules_run_nothing() {
    let td = setup().await;
    set_rules(
        &td,
        "flow",
        "review",
        json!([note("in", json!({"kind": "lands"}), "landed")]),
    )
    .await;
    let a = new_card(&td, "flow", "a").await;
    move_to(&td, &a, "review").await;
    settle(&td).await;
    assert!(comments(&td, &a).await.is_empty());
    assert!(runs(&td, "flow").await.is_empty());
    // A project file or the legacy migrate does not leave `columns`.
    switch(&td, "flow").await;
    let flow = state(&td)
        .db
        .write("operator", |t| {
            terrariumd::store::projects::set_board_flow(
                t,
                "flow",
                terrarium_core::config::BoardFlow::Code,
            )?;
            Ok(terrariumd::store::projects::board_flow(t, "flow")?)
        })
        .await
        .unwrap();
    assert_eq!(flow, terrarium_core::config::BoardFlow::Columns);
    // An event before the switch starts no run: the move above is older.
    settle(&td).await;
    assert!(runs(&td, "flow").await.is_empty());
}

#[tokio::test]
async fn creation_lands_and_count_reached_fires_once_at_the_count() {
    let td = setup().await;
    set_rules(
        &td,
        "flow",
        "review",
        json!([{"name": "two", "trigger": {"kind": "count_reached", "count": 2},
                "actions": [{"verb": "mail", "args": {"to": ["operator"], "subject": "two in review"}}]}]),
    )
    .await;
    set_rules(
        &td,
        "flow",
        "backlog",
        json!([note("new", json!({"kind": "lands"}), "new card")]),
    )
    .await;
    switch(&td, "flow").await;
    let cards = [
        new_card(&td, "flow", "a").await,
        new_card(&td, "flow", "b").await,
        new_card(&td, "flow", "c").await,
    ];
    for c in &cards {
        move_to(&td, c, "review").await;
    }
    settle(&td).await;
    for c in &cards {
        assert_eq!(bodies(&comments(&td, c).await), vec!["new card"]);
    }
    let r = runs(&td, "flow").await;
    let two: Vec<&Value> = r.iter().filter(|r| r["rule"] == "review/two").collect();
    assert_eq!(two.len(), 1, "{r:?}");
    assert!(two[0].get("card").is_none_or(Value::is_null));
}

#[tokio::test]
async fn archived_runs_the_rules_of_the_cards_column() {
    let td = setup().await;
    set_rules(
        &td,
        "flow",
        "review",
        json!([{"name": "gone", "trigger": {"kind": "archived"},
                "actions": [{"verb": "mail", "args": {"to": ["operator"], "subject": "{{card.id}} archived"}}]}]),
    )
    .await;
    switch(&td, "flow").await;
    let a = new_card(&td, "flow", "a").await;
    move_to(&td, &a, "review").await;
    td.delete(&format!("/api/cards/{a}")).await.ok();
    settle(&td).await;
    let r = runs(&td, "flow").await;
    let gone: Vec<&Value> = r.iter().filter(|r| r["rule"] == "review/gone").collect();
    assert_eq!(gone.len(), 1, "{r:?}");
    assert_eq!(gone[0]["card"], a.as_str());
}

#[tokio::test]
async fn a_timer_fires_once_per_slot() {
    let td = setup().await;
    set_rules(
        &td,
        "flow",
        "backlog",
        json!([{"name": "tick", "trigger": {"kind": "timer", "every_seconds": 3600},
                "actions": [{"verb": "mail", "args": {"to": ["operator"], "subject": "hourly"}}]}]),
    )
    .await;
    switch(&td, "flow").await;
    let db = state(&td).db.clone();
    let projects = rules_engine::load(&db, &mut rules_engine::Cache::default())
        .await
        .unwrap();
    let now = chrono::Utc::now();
    let mut fired = HashMap::new();
    // The live engine may have fired it already; then this writes none.
    let first = rules_engine::fire_timers(&db, &projects, &mut fired, now)
        .await
        .unwrap();
    assert!(first <= 1);
    let again = rules_engine::fire_timers(&db, &projects, &mut HashMap::new(), now)
        .await
        .unwrap();
    assert_eq!(again, 0, "the event log holds the slot across a restart");
    settle(&td).await;
    let r = runs(&td, "flow").await;
    let tick: Vec<&Value> = r.iter().filter(|r| r["rule"] == "backlog/tick").collect();
    assert_eq!(tick.len(), 1, "{r:?}");
}

#[tokio::test]
async fn an_explicit_max_per_hour_lifts_the_card_cap_and_the_default_keeps_ten() {
    let td = setup().await;
    let mut capped = note("capped", json!({"kind": "lands"}), "capped");
    capped["actions"] = json!([{"verb": "card.label", "args": {"add": ["seen"]}}]);
    let mut wide = capped.clone();
    wide["name"] = "wide".into();
    wide["max_per_hour"] = 600.into();
    set_rules(&td, "flow", "review", json!([capped, wide])).await;
    switch(&td, "flow").await;
    let a = new_card(&td, "flow", "a").await;
    for _ in 0..12 {
        move_to(&td, &a, "review").await;
        move_to(&td, &a, "ready").await;
    }
    settle(&td).await;
    let r = runs(&td, "flow").await;
    let ran = |name: &str| {
        r.iter()
            .filter(|x| x["rule"] == name && x["state"] != "skipped")
            .count()
    };
    let skipped = |name: &str| {
        r.iter()
            .filter(|x| x["rule"] == name && x["state"] == "skipped")
            .count()
    };
    assert_eq!(ran("review/capped"), 10);
    assert_eq!(skipped("review/capped"), 2);
    assert_eq!(ran("review/wide"), 12);
    assert_eq!(skipped("review/wide"), 0);
}

fn board(ready: Option<&str>) -> Value {
    let mut c = vec![json!({"name": "backlog"})];
    match ready {
        Some("ready") => c.push(json!({"name": "ready"})),
        Some(n) => c.push(json!({"name": n, "rename_from": "ready"})),
        None => {}
    }
    c.extend([
        json!({"name": "in_progress", "role": "working"}),
        json!({"name": "validating", "role": "validating"}),
        json!({"name": "review", "role": "review"}),
        json!({"name": "merge_queue", "role": "merge_queue"}),
        json!({"name": "accepted", "role": "accepted"}),
        json!({"name": "blocked", "role": "blocked"}),
        json!({"name": "done"}),
    ]);
    json!({"columns": c})
}

#[tokio::test]
async fn a_rename_rewrites_the_rules_and_a_removal_is_refused_while_named() {
    let td = setup().await;
    set_rules(
        &td,
        "flow",
        "review",
        json!([{"name": "back", "trigger": {"kind": "lands"},
                "if_card": [{"not_column": ["ready"], "column": ["@review"]}],
                "actions": [{"verb": "card.move", "args": {"to": "ready"}}]},
               {"name": "role", "trigger": {"kind": "lands"},
                "actions": [{"verb": "card.move", "args": {"to": "@accepted"}}]}]),
    )
    .await;
    let before = options(&td, "flow", "review").await;
    let b = "/api/projects/flow/board";
    td.patch(b, board(Some("queue"))).await.ok();
    let after = options(&td, "flow", "review").await;
    let back = &after["rules"][0];
    assert_eq!(back["id"], before["rules"][0]["id"]);
    assert_eq!(back["if_card"][0]["not_column"], json!(["queue"]));
    // `@review` is a role: it stays.
    assert_eq!(back["if_card"][0]["column"], json!(["@review"]));
    assert_eq!(back["actions"][0]["args"]["to"], "queue");
    assert_eq!(after["rules"][1], before["rules"][1]);
    assert_eq!(
        after["version"].as_i64().unwrap(),
        before["version"].as_i64().unwrap() + 1
    );
    // The removal of `queue` is refused while `review/back` names it.
    let r = td.patch(b, board(None)).await;
    r.err(409, "conflict");
    let d = &r.body["error"]["details"];
    assert_eq!(d["reason"], "column_in_use", "{}", r.body);
    assert_eq!(d["rules"][0]["rule"], "back");
    assert_eq!(options(&td, "flow", "queue").await["column"], "queue");
    // Without the rule the removal goes.
    set_rules(&td, "flow", "review", json!([])).await;
    td.patch(b, board(None)).await.ok();
}
