//! The `review.start` rule action (TER-1214, kanban rework C4b, D23): on a
//! columns board a column rule starts the review agent that the
//! `review_dispatcher` setting starts on other boards, with its rounds,
//! its round limit mail and its verdict; the setting is refused there.
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

/// A project `p` on column rules with a review column, the `card-review`
/// template, and a lead that may spawn bound agents.
async fn setup() -> TestDaemon {
    let td = TestDaemon::start().await;
    // The dispatcher timers of the settings sleep: only the rules act.
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
            {"name":"coding", "role":"working", "wip_limit":4},
            {"name":"review", "role":"review"}, {"name":"merge_queue"},
            {"name":"blocked", "role":"blocked"}, {"name":"done"}
        ]}),
    )
    .await
    .ok();
    td.post(
        "/api/templates",
        json!({"name":"card-review","project":"p","spec":{"harness":"codex"}}),
    )
    .await
    .ok();
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

fn review_rule(args: Value) -> Value {
    json!([{
        "name": "review", "trigger": {"kind": "lands"},
        "actions": [{"verb": "review.start", "args": args}]
    }])
}

async fn new_card(td: &TestDaemon, column: &str) -> String {
    td.post(
        "/api/projects/p/cards",
        json!({"title": "Task", "column": column}),
    )
    .await
    .ok()["id"]
        .as_str()
        .unwrap()
        .to_owned()
}

async fn move_to(td: &TestDaemon, id: &str, column: &str) {
    td.post(&format!("/api/cards/{id}/move"), json!({"column": column}))
        .await
        .ok();
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

/// The review round of card `id` and its agent, once round `round` has
/// started (the spawn runs after the commit).
async fn wait_round(td: &TestDaemon, id: &str, round: u32) -> String {
    let start = std::time::Instant::now();
    loop {
        let card = id.to_owned();
        let row: Option<(u32, Option<String>)> = state(td)
            .db
            .read(move |c| {
                use rusqlite::OptionalExtension;
                Ok(c.query_row(
                    "SELECT round, agent FROM card_reviews WHERE card = ?1",
                    [&card],
                    |r| Ok((r.get(0)?, r.get(1)?)),
                )
                .optional()?)
            })
            .await
            .unwrap();
        if let Some((r, Some(agent))) = &row
            && *r == round
        {
            return agent.clone();
        }
        assert!(
            start.elapsed() < Duration::from_secs(30),
            "{id} has no review round {round}: {row:?}"
        );
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
}

async fn cap_mails(td: &TestDaemon) -> i64 {
    state(td)
        .db
        .read(|c| {
            Ok(c.query_row(
                "SELECT count(*) FROM messages WHERE subject LIKE '%review needs lead decision'",
                [],
                |r| r.get(0),
            )?)
        })
        .await
        .unwrap()
}

/// A card that lands in review gets a review agent from the template, as
/// the owner; its accept verdict moves the card to merge_queue as the
/// owner of the round's spawn, with no `review_dispatcher` setting.
#[tokio::test]
async fn a_lands_rule_starts_a_review_and_the_verdict_moves_the_card() {
    let td = setup().await;
    set_rules(&td, "review", review_rule(json!({"owner": "lead"}))).await;
    let id = new_card(&td, "ready").await;
    move_to(&td, &id, "review").await;
    settle(&td).await;
    let agent = wait_round(&td, &id, 1).await;
    let a = agent.clone();
    let (template, owner, bound): (String, String, String) = state(&td)
        .db
        .read(move |c| {
            Ok(c.query_row(
                "SELECT template, owner_name, bound_kind FROM spawns WHERE id = ?1",
                [&a],
                |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
            )?)
        })
        .await
        .unwrap();
    assert!(template.ends_with("card-review"), "{template}");
    assert!(owner.ends_with("lead"), "{owner}");
    assert_eq!(bound, "review");
    assert!(
        td.get("/api/projects/p").await.ok()["review_dispatcher"].is_null(),
        "no setting"
    );
    assert_eq!(card(&td, &id).await["review_round"], 1);
    let r = runs(&td).await;
    let run = r
        .iter()
        .find(|r| r["rule"] == "review/review")
        .expect("the rule ran");
    assert_eq!(run["state"], "done", "{run}");
    td.post_as(
        &agent,
        &format!("/api/cards/{id}/comments"),
        json!({"body": "Review verdict: accept\nChecked."}),
    )
    .await
    .ok();
    assert_eq!(card(&td, &id).await["column"], "merge_queue");
}

/// `max_rounds`: at the limit the card stays in review and the owner gets
/// one mail; a later landing starts nothing.
#[tokio::test]
async fn the_round_limit_keeps_the_card_and_mails_the_owner_once() {
    let td = setup().await;
    set_rules(
        &td,
        "review",
        review_rule(json!({"owner": "lead", "max_rounds": 1})),
    )
    .await;
    let id = new_card(&td, "ready").await;
    move_to(&td, &id, "review").await;
    settle(&td).await;
    let agent = wait_round(&td, &id, 1).await;
    terrariumd::spawn::close(
        &state(&td),
        &agent,
        terrariumd::store::spawns::DONE,
        None,
        false,
    )
    .await
    .unwrap();
    for _ in 0..2 {
        move_to(&td, &id, "ready").await;
        move_to(&td, &id, "review").await;
        settle(&td).await;
    }
    assert_eq!(cap_mails(&td).await, 1);
    let c = card(&td, &id).await;
    assert_eq!(c["column"], "review", "{c}");
    assert_eq!(c["review_round"], 1, "{c}");
    let r = runs(&td).await;
    let mine: Vec<&Value> = r.iter().filter(|r| r["rule"] == "review/review").collect();
    assert_eq!(mine.len(), 3, "{r:?}");
    assert!(
        mine[1].to_string().contains("round limit (1)"),
        "{}",
        mine[1]
    );
    assert_eq!(mine[2]["state"], "noop", "{}", mine[2]);
    assert!(
        mine[2].to_string().contains("no eligible card"),
        "{}",
        mine[2]
    );
    let open: i64 = state(&td)
        .db
        .read(|c| {
            Ok(c.query_row(
                "SELECT count(*) FROM spawns WHERE bound_kind = 'review' AND finished_at IS NULL",
                [],
                |r| r.get(0),
            )?)
        })
        .await
        .unwrap();
    assert_eq!(open, 0);
}

/// `from` takes the top card of the review column for a rule on another
/// column; a rule with no `from` on a card outside review is a no-op.
#[tokio::test]
async fn from_reviews_the_review_column_and_other_cards_are_a_noop() {
    let td = setup().await;
    let waiting = new_card(&td, "review").await;
    set_rules(
        &td,
        "backlog",
        review_rule(json!({"owner": "lead", "from": "@review"})),
    )
    .await;
    set_rules(&td, "ready", review_rule(json!({"owner": "lead"}))).await;
    new_card(&td, "backlog").await;
    settle(&td).await;
    wait_round(&td, &waiting, 1).await;
    let other = new_card(&td, "ready").await;
    settle(&td).await;
    let r = runs(&td).await;
    let run = r
        .iter()
        .find(|r| r["rule"] == "ready/review" && r["card"] == other.as_str())
        .expect("the rule ran");
    assert_eq!(run["state"], "noop", "{run}");
    assert!(
        run.to_string().contains("not in the review column"),
        "{run}"
    );
    assert_eq!(card(&td, &other).await["review_round"], 0);
}

/// The rule's checks: `owner` is required, `max_rounds` is 1 to 10, and
/// `from` is the review column.
#[tokio::test]
async fn review_start_checks_its_arguments() {
    let td = setup().await;
    let o = td.get(&path("review")).await.ok().clone();
    for (args, needle) in [
        (json!({}), "needs `owner`"),
        (
            json!({"owner": "lead", "max_rounds": 0}),
            "max_rounds is 1 to 10",
        ),
        (
            json!({"owner": "lead", "max_rounds": 11}),
            "max_rounds is 1 to 10",
        ),
        (
            json!({"owner": "lead", "from": "@ready"}),
            "is not the review column",
        ),
        (
            json!({"owner": "lead", "template": "card review"}),
            "is not a template name",
        ),
    ] {
        let r = td
            .put(
                &path("review"),
                json!({"base_version": o["version"], "checks": o["checks"],
                       "queue": o["queue"], "rules": review_rule(args.clone())}),
            )
            .await;
        assert_eq!(r.status.as_u16(), 422, "{args}: {}", r.body);
        assert!(r.body.to_string().contains(needle), "{args}: {}", r.body);
    }
}

/// On a columns board the `review_dispatcher` setting is refused (409),
/// `off` stays accepted, a stored owner starts nothing, and a project file
/// value is stored as `off` while another project keeps its owner.
#[tokio::test]
async fn the_review_dispatcher_setting_is_refused_on_a_columns_board() {
    let td = setup().await;
    td.patch("/api/projects/p", json!({"review_dispatcher": "lead"}))
        .await
        .err(409, "conflict");
    td.patch("/api/projects/p", json!({"review_dispatcher": "off"}))
        .await
        .ok();
    let id = new_card(&td, "review").await;
    state(&td)
        .db
        .write("operator", |t| {
            t.execute(
                "UPDATE projects SET review_dispatcher = 'lead' WHERE name = 'p'",
                [],
            )?;
            Ok(())
        })
        .await
        .unwrap();
    assert!(
        !terrariumd::review_dispatcher::dispatch(&state(&td), "p")
            .await
            .unwrap()
    );
    assert_eq!(card(&td, &id).await["review_round"], 0);
    td.project("q").await;
    let got = state(&td)
        .db
        .write("operator", |t| {
            terrariumd::store::projects::set_review_dispatcher_from_file(t, "p", Some("lead"))?;
            terrariumd::store::projects::set_review_dispatcher_from_file(t, "q", Some("lead"))?;
            Ok((
                terrariumd::store::projects::get(t, "p")?.review_dispatcher,
                terrariumd::store::projects::get(t, "q")?.review_dispatcher,
            ))
        })
        .await
        .unwrap();
    assert_eq!(got, (None, Some("lead".to_owned())));
}
