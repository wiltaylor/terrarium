//! The column rules in the engine (TER-1113, kanban rework C2a): a project
//! with `board_flow = columns` runs the rules of its board columns on their
//! triggers, in the order of TER-1112 Q3, with the loop guards of the
//! daemon; a project without the switch runs none of them; a column rename
//! rewrites the rules that name it, and a removal is refused while a rule
//! names it (TER-1159). The follow-ups of the review (TER-1199): the
//! removal check on the board after the change, the timers by rule ID, the
//! R3 lockout at each entry, `wait_met`, and `to_count` only on `columns`.
//! A role change that leaves a rule's `@role` naming no column is refused
//! (TER-1210).
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
            terrariumd::store::projects::board_flow(t, "flow")
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
    // The audit row of the PATCH lists the rewrite (TER-1199).
    let audit = td
        .get("/api/audit?action=board.columns&order=desc&limit=5")
        .await
        .ok()
        .clone();
    let renamed = &audit[0]["detail"]["renamed_in_rules"];
    assert_eq!(renamed.as_array().map(Vec::len), Some(1), "{audit}");
    assert_eq!(renamed[0]["column"], "review");
    assert_eq!(renamed[0]["rule"], "back");
    assert_eq!(renamed[0]["before"]["actions"][0]["args"]["to"], "ready");
    assert_eq!(renamed[0]["after"]["actions"][0]["args"]["to"], "queue");
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

/// The board of [`board`] without `accepted`, and with `extra` before
/// `blocked`.
fn board_without_accepted(extra: Option<Value>) -> Value {
    let mut b = board(Some("ready"));
    let cols = b["columns"].as_array_mut().unwrap();
    cols.retain(|c| c["name"] != "accepted");
    if let Some(e) = extra {
        let at = cols.iter().position(|c| c["name"] == "blocked").unwrap();
        cols.insert(at, e);
    }
    b
}

#[tokio::test]
async fn a_removal_checks_the_roles_of_the_board_after_the_change() {
    let td = setup().await;
    set_rules(
        &td,
        "flow",
        "review",
        json!([{"name": "role", "trigger": {"kind": "lands"},
                "actions": [{"verb": "card.move", "args": {"to": "@accepted"}}]}]),
    )
    .await;
    let b = "/api/projects/flow/board";
    // `accepted` is the only column with the role `accepted`: `@accepted`
    // would name no column after the change.
    let r = td.patch(b, board_without_accepted(None)).await;
    r.err(409, "conflict");
    let d = &r.body["error"]["details"];
    assert_eq!(d["reason"], "column_in_use", "{}", r.body);
    assert_eq!(d["columns"], json!(["accepted"]));
    assert_eq!(d["rules"][0]["rule"], "role");
    assert_eq!(options(&td, "flow", "accepted").await["column"], "accepted");
    // Another column takes the role in the same change: `@accepted` still
    // resolves, so the removal goes, and the rule keeps its reference.
    td.patch(
        b,
        board_without_accepted(Some(json!({"name": "landed", "role": "accepted"}))),
    )
    .await
    .ok();
    let o = options(&td, "flow", "review").await;
    assert_eq!(o["rules"][0]["actions"][0]["args"]["to"], "@accepted");
}

/// The board of [`board`] with the column `review` named `qa` and with the
/// role `role`, and with `extra` before it. `qa` exists already.
fn board_qa(role: Option<&str>, extra: Option<Value>) -> Value {
    let mut b = board(Some("ready"));
    let cols = b["columns"].as_array_mut().unwrap();
    let at = cols.iter().position(|c| c["name"] == "review").unwrap();
    cols[at] = json!({"name": "qa", "role": role});
    if let Some(e) = extra {
        cols.insert(at, e);
    }
    b
}

#[tokio::test]
async fn a_role_change_that_leaves_a_dangling_role_ref_is_refused() {
    let td = setup().await;
    set_rules(
        &td,
        "flow",
        "accepted",
        json!([{"name": "back", "trigger": {"kind": "lands"},
                "actions": [{"verb": "card.move", "args": {"to": "@review"}}]}]),
    )
    .await;
    let b = "/api/projects/flow/board";
    // The column with the role `review` gets another name: `@review` is a
    // role and stays.
    let mut renamed = board_qa(Some("review"), None);
    for c in renamed["columns"].as_array_mut().unwrap() {
        if c["name"] == "qa" {
            c["rename_from"] = "review".into();
        }
    }
    td.patch(b, renamed).await.ok();
    let o = options(&td, "flow", "accepted").await;
    assert_eq!(o["rules"][0]["actions"][0]["args"]["to"], "@review");
    let before = td.get(b).await.ok().clone();
    // The role `review` is cleared and no column is removed: `@review`
    // would name no column, neither by role nor by name (TER-1210).
    let r = td.patch(b, board_qa(None, None)).await;
    r.err(409, "conflict");
    let d = &r.body["error"]["details"];
    assert_eq!(d["reason"], "role_in_use", "{}", r.body);
    assert_eq!(d["roles"], json!(["review"]));
    assert_eq!(d["rules"][0]["column"], "accepted");
    assert_eq!(d["rules"][0]["rule"], "back");
    assert_eq!(d["rules"][0]["names"], "@review");
    // Nothing changed.
    assert_eq!(td.get(b).await.ok().clone(), before);
    // The role moves to another column in the same change: `@review`
    // follows it, so the change goes and the rule keeps its reference.
    td.patch(
        b,
        board_qa(None, Some(json!({"name": "check", "role": "review"}))),
    )
    .await
    .ok();
    let o = options(&td, "flow", "accepted").await;
    assert_eq!(o["rules"][0]["actions"][0]["args"]["to"], "@review");
    // A rule without the reference: the role may go.
    set_rules(&td, "flow", "accepted", json!([])).await;
    td.patch(b, board_qa(None, Some(json!({"name": "check"}))))
        .await
        .ok();
}

#[tokio::test]
async fn a_role_clear_that_moves_a_role_ref_to_another_column_by_name_is_refused() {
    let td = setup().await;
    set_rules(
        &td,
        "flow",
        "accepted",
        json!([{"name": "back", "trigger": {"kind": "lands"},
                "actions": [{"verb": "card.move", "args": {"to": "@review"}}]}]),
    )
    .await;
    let b = "/api/projects/flow/board";
    // The column with the role `review` is renamed to `qa`, then a new
    // column is named `review`.
    let mut renamed = board_qa(Some("review"), None);
    for c in renamed["columns"].as_array_mut().unwrap() {
        if c["name"] == "qa" {
            c["rename_from"] = "review".into();
        }
    }
    td.patch(b, renamed).await.ok();
    td.patch(b, board_qa(Some("review"), Some(json!({"name": "review"}))))
        .await
        .ok();
    let o = options(&td, "flow", "accepted").await;
    assert_eq!(o["rules"][0]["actions"][0]["args"]["to"], "@review");
    let before = td.get(b).await.ok().clone();
    // The role is cleared on `qa`: `@review` would resolve by name to the
    // column `review`, not to `qa` (TER-1246).
    let r = td
        .patch(b, board_qa(None, Some(json!({"name": "review"}))))
        .await;
    r.err(409, "conflict");
    let d = &r.body["error"]["details"];
    assert_eq!(d["reason"], "role_in_use", "{}", r.body);
    assert_eq!(d["roles"], json!(["review"]));
    assert_eq!(d["rules"][0]["rule"], "back");
    assert_eq!(d["rules"][0]["names"], "@review");
    let msg = r.body["error"]["message"].as_str().unwrap();
    assert!(!msg.contains("  "), "{msg}");
    assert_eq!(td.get(b).await.ok().clone(), before);
    // The role moves to the column named `review`: allowed.
    td.patch(
        b,
        board_qa(None, Some(json!({"name": "review", "role": "review"}))),
    )
    .await
    .ok();
    let o = options(&td, "flow", "accepted").await;
    assert_eq!(o["rules"][0]["actions"][0]["args"]["to"], "@review");
}

#[tokio::test]
async fn a_role_clear_with_a_rename_of_that_column_keeps_the_ref_on_it() {
    let td = setup().await;
    set_rules(
        &td,
        "flow",
        "accepted",
        json!([{"name": "back", "trigger": {"kind": "lands"},
                "actions": [{"verb": "card.move", "args": {"to": "@review"}}]}]),
    )
    .await;
    let b = "/api/projects/flow/board";
    // The column `review` (role `review`) is renamed to `qa` and loses the
    // role in one change: `@review` is rewritten to `@qa`, which names the
    // same column by name (TER-1159, TER-1210 R5).
    let mut renamed = board_qa(None, None);
    for c in renamed["columns"].as_array_mut().unwrap() {
        if c["name"] == "qa" {
            c["rename_from"] = "review".into();
        }
    }
    td.patch(b, renamed).await.ok();
    let o = options(&td, "flow", "accepted").await;
    assert_eq!(o["rules"][0]["actions"][0]["args"]["to"], "@qa");
}

#[tokio::test]
async fn wait_met_runs_the_rules_of_the_cards_column() {
    let td = setup().await;
    set_rules(
        &td,
        "flow",
        "ready",
        json!([note(
            "met",
            json!({"kind": "wait_met"}),
            "deps met in ready"
        )]),
    )
    .await;
    set_rules(
        &td,
        "flow",
        "backlog",
        json!([note(
            "met",
            json!({"kind": "wait_met"}),
            "deps met in backlog"
        )]),
    )
    .await;
    switch(&td, "flow").await;
    let a = new_card(&td, "flow", "a").await;
    let b = new_card(&td, "flow", "b").await;
    move_to(&td, &b, "ready").await;
    td.patch(
        &format!("/api/cards/{b}"),
        json!({"wait": {"dependencies": [{"card": a, "target": "review"}], "mode": "any"}}),
    )
    .await
    .ok();
    settle(&td).await;
    assert!(comments(&td, &b).await.is_empty());
    move_to(&td, &a, "review").await;
    settle(&td).await;
    assert_eq!(bodies(&comments(&td, &b).await), vec!["deps met in ready"]);
    assert!(comments(&td, &a).await.is_empty());
    let r = runs(&td, "flow").await;
    let met: Vec<&Value> = r.iter().filter(|r| r["rule"] == "ready/met").collect();
    assert_eq!(met.len(), 1, "{r:?}");
    assert_eq!(met[0]["card"], b.as_str());
    assert!(r.iter().all(|r| r["rule"] != "backlog/met"), "{r:?}");
}

/// Write a `rule.timer` event as the engine does (actor `system`).
async fn timer_event(td: &TestDaemon, data: Value) {
    state(td)
        .db
        .write("system", move |t| {
            t.emit(
                terrariumd::events::EventDraft::new(rules_engine::TIMER_KIND, "test timer", data)
                    .project("flow"),
            )?;
            Ok(())
        })
        .await
        .unwrap();
}

#[tokio::test]
async fn a_timer_event_runs_only_the_timer_rule_with_its_id() {
    let td = setup().await;
    set_rules(
        &td,
        "flow",
        "review",
        json!([note("land", json!({"kind": "lands"}), "landed"),
               {"name": "tick", "trigger": {"kind": "timer", "every_seconds": 3600},
                "actions": [{"verb": "mail", "args": {"to": ["operator"], "subject": "tick"}}]}]),
    )
    .await;
    switch(&td, "flow").await;
    settle(&td).await;
    let o = options(&td, "flow", "review").await;
    let land_id = o["rules"][0]["id"].as_str().unwrap().to_owned();
    let tick_id = o["rules"][1]["id"].as_str().unwrap().to_owned();
    let count = |r: &[Value], name: &str| r.iter().filter(|x| x["rule"] == name).count();
    // The live engine may have fired the slot of `tick` already.
    let ticks = count(&runs(&td, "flow").await, "review/tick");
    // By ID: the timer rule runs, also under a stale name (a rename
    // between the write and the plan).
    timer_event(
        &td,
        json!({"rule": "old/tick", "rule_id": tick_id, "slot": "x"}),
    )
    .await;
    // By name only (the shape of a legacy timer): nothing runs.
    timer_event(&td, json!({"rule": "review/tick"})).await;
    timer_event(&td, json!({"rule": "review/land"})).await;
    // The ID of a rule whose trigger is not `timer`: nothing runs.
    timer_event(&td, json!({"rule": "review/land", "rule_id": land_id})).await;
    settle(&td).await;
    let r = runs(&td, "flow").await;
    assert_eq!(count(&r, "review/tick"), ticks + 1, "{r:?}");
    assert_eq!(count(&r, "review/land"), 0, "{r:?}");
}

#[tokio::test]
async fn the_legacy_version_timer_does_not_scan_the_column_rules() {
    let td = setup().await;
    // A `version.cut` that the legacy timer would run by its name: one
    // accepted `vnext` card is more than `min_cards`.
    set_rules(
        &td,
        "flow",
        "review",
        json!([{"name": "cut", "trigger": {"kind": "archived"},
                "actions": [{"verb": "version.cut",
                             "args": {"min_cards": 1, "max_wait_minutes": 240}}]}]),
    )
    .await;
    let a = td
        .post(
            "/api/projects/flow/cards",
            json!({"title": "a", "column": "accepted", "labels": ["version:vnext"]}),
        )
        .await
        .ok()["id"]
        .as_str()
        .unwrap()
        .to_owned();
    switch(&td, "flow").await;
    let db = state(&td).db.clone();
    let projects = rules_engine::load(&db, &mut rules_engine::Cache::default())
        .await
        .unwrap();
    let written =
        rules_engine::fire_timers(&db, &projects, &mut HashMap::new(), chrono::Utc::now())
            .await
            .unwrap();
    assert_eq!(written, 0);
    settle(&td).await;
    let timers: i64 = db
        .read(|c| {
            Ok(c.query_row(
                "SELECT count(*) FROM events WHERE project = 'flow' AND kind = 'rule.timer'",
                [],
                |r| r.get(0),
            )?)
        })
        .await
        .unwrap();
    assert_eq!(timers, 0);
    assert!(runs(&td, "flow").await.is_empty());
    assert!(td.get(&format!("/api/cards/{a}")).await.ok()["group"].is_null());
}

#[tokio::test]
async fn no_entry_but_the_upgrade_sets_columns() {
    let td = setup().await;
    // The project settings.
    td.patch("/api/projects/flow", json!({"board_flow": "columns"}))
        .await
        .err(400, "bad_request");
    // The board PATCH has no such field.
    let mut b = board(Some("ready"));
    b["board_flow"] = "columns".into();
    td.patch("/api/projects/flow/board", b)
        .await
        .err(400, "bad_request");
    assert_eq!(
        td.get("/api/projects/flow").await.ok()["board_flow"],
        "code"
    );
    // The fleet import: a fleet file gives no `board_flow` at all (only a
    // project file does, and there `columns` does not read: the core test
    // `a_project_file_cannot_give_the_columns_flow`).
    let fleet = "name = \"colfleet\"\n[[projects]]\nname = \"fc\"\nboard_flow = \"columns\"\n\
                 [[projects.agents]]\nname = \"a\"\nharness = \"claude\"\n";
    let r = td.post("/api/fleet/apply", json!({"content": fleet})).await;
    r.err(422, "invalid");
    assert!(
        r.body["error"]["message"]
            .as_str()
            .is_some_and(|m| m.contains("board_flow")),
        "{}",
        r.body
    );
    td.get("/api/projects/fc").await.err(404, "not_found");
    // The legacy migrate tool on a converted board.
    switch(&td, "flow").await;
    let r = td
        .post("/api/projects/flow/rules/migrate", json!({"apply": true}))
        .await;
    r.err(409, "conflict");
    assert_eq!(
        td.get("/api/projects/flow").await.ok()["board_flow"],
        "columns"
    );
}

#[tokio::test]
async fn to_count_is_written_only_on_a_columns_project() {
    let td = setup().await;
    td.project("plain").await;
    switch(&td, "flow").await;
    for p in ["flow", "plain"] {
        let a = new_card(&td, p, "a").await;
        move_to(&td, &a, "review").await;
    }
    let rows: Vec<(String, String, Option<i64>)> = state(&td)
        .db
        .read(|c| {
            let mut st = c.prepare(
                "SELECT project, json_extract(payload, '$.change'),
                        json_extract(payload, '$.to_count')
                   FROM events WHERE kind = 'card'
                    AND json_extract(payload, '$.change') IN ('created', 'moved')
                  ORDER BY seq",
            )?;
            let v = st
                .query_map([], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)))?
                .collect::<Result<Vec<_>, _>>()?;
            Ok(v)
        })
        .await
        .unwrap();
    let of = |p: &str| -> Vec<(String, Option<i64>)> {
        rows.iter()
            .filter(|r| r.0 == p)
            .map(|r| (r.1.clone(), r.2))
            .collect()
    };
    assert_eq!(
        of("flow"),
        vec![
            ("created".to_owned(), Some(1)),
            ("moved".to_owned(), Some(1))
        ]
    );
    assert_eq!(
        of("plain"),
        vec![("created".to_owned(), None), ("moved".to_owned(), None)]
    );
}
