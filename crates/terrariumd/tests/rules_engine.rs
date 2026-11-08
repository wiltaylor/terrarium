//! The board rule engine (TER-168, docs/api.md §19.9): the durable cursor
//! and the action outbox (a restart between the plan and the run loses no
//! action), the loop guard, the dry run (it writes nothing), the rule
//! actor's rights, the ordered actions (the first failure stops the
//! list), the per-card order, the conditions on the current state, and
//! the column check after a board change.
//!
//! `card.comment` is the test verb here; tests/rules_verbs.rs tests each
//! verb (TER-169).
#![allow(clippy::unwrap_used, clippy::expect_used)]

mod common;

use std::time::Duration;

use common::TestDaemon;
use serde_json::{Value, json};
use terrariumd::api::Shared;
use terrariumd::error::Code;
use terrariumd::events::{EventDraft, EventHub};
use terrariumd::rules_engine;
use terrariumd::store::board;

fn state(td: &TestDaemon) -> Shared {
    td.daemon.as_ref().unwrap().state.clone()
}

const SOURCE: &str = "source:test/ping";

/// Write a `test.ping` event about `card` with `data.n = n`, as an event
/// source does. Returns its `seq`.
async fn ping(td: &TestDaemon, project: &str, card: &str, n: i64) -> i64 {
    let (project, card) = (project.to_owned(), card.to_owned());
    state(td)
        .db
        .write(SOURCE, move |t| {
            Ok(t.emit(
                EventDraft::new("test.ping", format!("ping {n}"), json!({"n": n}))
                    .project(project)
                    .card(card),
            )?
            .seq)
        })
        .await
        .unwrap()
}

/// The engine's cursor, the last event `seq`, and the pending rows.
async fn engine_state(td: &TestDaemon) -> (i64, i64, i64) {
    state(td)
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
        .unwrap()
}

/// Wait until the engine read the whole log and ran every pending row,
/// twice in a row (a run writes events that the engine reads again).
async fn settle(td: &TestDaemon) {
    let mut quiet = 0;
    for _ in 0..1000 {
        let (cursor, last, pending) = engine_state(td).await;
        if cursor >= last && pending == 0 {
            quiet += 1;
            if quiet >= 3 {
                return;
            }
        } else {
            quiet = 0;
        }
        tokio::time::sleep(Duration::from_millis(30)).await;
    }
    panic!(
        "the rule engine did not settle: {:?}",
        engine_state(td).await
    );
}

async fn put_rules(td: &TestDaemon, project: &str, text: &str) {
    td.put(
        &format!("/api/projects/{project}/rules"),
        json!({"text": text}),
    )
    .await
    .ok();
}

async fn new_card(td: &TestDaemon, project: &str, body: Value) -> String {
    td.post(&format!("/api/projects/{project}/cards"), body)
        .await
        .ok()["id"]
        .as_str()
        .unwrap()
        .to_owned()
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

const TWO_STEPS: &str = r#"rule "note" {
  on = ["test.ping"]
  card_comment { text = "first {{event.data.n}}" }
  card_comment { text = "second on {{card.id}} by {{rule.name}}" }
}
"#;

#[tokio::test]
async fn a_restart_between_plan_and_run_loses_no_action() {
    let mut td = TestDaemon::start().await;
    td.project("terra").await;
    put_rules(&td, "terra", TWO_STEPS).await;
    let card = new_card(&td, "terra", json!({"title": "c"})).await;
    settle(&td).await;

    // The daemon stops. Two events come; the engine plans them (the
    // outbox rows and the cursor in one write) and stops before it runs
    // them. The second run had done its first action already.
    td.daemon.take().unwrap().shutdown();
    tokio::time::sleep(Duration::from_millis(50)).await;
    let path = td.state.join("terrarium.db");
    {
        let conn = terrariumd::db::open(&path).unwrap();
        for n in [1, 2] {
            terrariumd::events::insert(
                &conn,
                SOURCE,
                EventDraft::new("test.ping", "down", json!({"n": n}))
                    .project("terra")
                    .card(card.clone()),
            )
            .unwrap();
        }
        let db = terrariumd::db::Db::new(conn, EventHub::default());
        let mut cache = rules_engine::Cache::default();
        let projects = rules_engine::load(&db, &mut cache).await.unwrap();
        let (written, more) = rules_engine::plan_pass(&db, projects, chrono::Utc::now())
            .await
            .unwrap();
        assert_eq!((written, more), (2, false));
        let (cursor, last, pending) = db
            .read(|c| {
                Ok((
                    rules_engine::cursor(c)?.unwrap(),
                    terrariumd::events::last_seq(c)?,
                    c.query_row(
                        "SELECT count(*) FROM rule_runs WHERE state = 'pending'",
                        [],
                        |r| r.get::<_, i64>(0),
                    )?,
                ))
            })
            .await
            .unwrap();
        assert_eq!((cursor, pending), (last, 2), "planned, not run");
        // The crash came after the first action of the second run.
        db.write("system", |t| {
            t.execute(
                "UPDATE rule_runs SET step = 1 WHERE id =
                   (SELECT id FROM rule_runs ORDER BY event_seq DESC LIMIT 1)",
                [],
            )?;
            Ok(())
        })
        .await
        .unwrap();
        let card_id = card.clone();
        let written: i64 = db
            .read(move |c| {
                Ok(c.query_row(
                    "SELECT count(*) FROM card_comments WHERE card = ?1",
                    [card_id],
                    |r| r.get(0),
                )?)
            })
            .await
            .unwrap();
        assert_eq!(written, 0, "the plan ran no action");
    }

    td.boot(None, None).await;
    settle(&td).await;
    let c = comments(&td, &card).await;
    // IDs and the rule name are made by the daemon: not quoted (TER-170).
    let second = format!("second on {card} by note");
    assert_eq!(
        bodies(&c),
        vec!["first `1`", second.as_str(), second.as_str()],
        "each planned action ran once, the second run from its next action"
    );
    let r = runs(&td, "terra").await;
    assert_eq!(r.len(), 2, "no event was planned twice: {r:?}");
    assert!(r.iter().all(|r| r["state"] == "done"), "{r:?}");
    assert_eq!(r[0]["step"], 2);
}

const PING_PONG: &str = r#"rule "ping" {
  on = ["card"]
  when "data.change" { eq = "commented" }
  when "data.comment" { glob = "ping*" }
  card_comment { text = "pong" }
}

rule "pong" {
  on = ["card"]
  when "data.change" { eq = "commented" }
  when "data.comment" { glob = "pong*" }
  card_comment { text = "ping" }
}
"#;

#[tokio::test]
async fn a_loop_between_two_rules_stops_at_the_chain_depth() {
    let td = TestDaemon::start().await;
    td.project("terra").await;
    put_rules(&td, "terra", PING_PONG).await;
    let card = new_card(&td, "terra", json!({"title": "c"})).await;
    td.post(
        &format!("/api/cards/{card}/comments"),
        json!({"body": "ping"}),
    )
    .await
    .ok();
    settle(&td).await;

    // The operator's ping (depth 0), then four rule comments (depth 1 to
    // 4). The event at depth 4 starts no run.
    let c = comments(&td, &card).await;
    assert_eq!(bodies(&c), vec!["ping", "pong", "ping", "pong", "ping"]);
    assert_eq!(c[1].0, "rule:terra/ping");
    assert_eq!(c[2].0, "rule:terra/pong");
    let r = runs(&td, "terra").await;
    let done = r.iter().filter(|r| r["state"] == "done").count();
    assert_eq!(done, 4, "{r:?}");
    let last = r.last().unwrap();
    assert_eq!(last["state"], "skipped", "{r:?}");
    assert_eq!(last["rule"], "ping");
    assert!(
        last["reason"].as_str().unwrap().contains("chain depth 4"),
        "{last:?}"
    );
    // Nothing more runs later.
    tokio::time::sleep(Duration::from_millis(300)).await;
    settle(&td).await;
    assert_eq!(comments(&td, &card).await.len(), 5);
}

/// The rows of every table, and the last event: what a write changes.
async fn digest(td: &TestDaemon) -> Vec<(String, i64)> {
    state(td)
        .db
        .read(|c| {
            let mut st = c.prepare(
                "SELECT name FROM sqlite_master WHERE type = 'table' AND name NOT LIKE 'sqlite_%'
                  ORDER BY name",
            )?;
            let names = st
                .query_map([], |r| r.get::<_, String>(0))?
                .collect::<Result<Vec<_>, _>>()?;
            let mut out = Vec::new();
            for n in names {
                let count: i64 =
                    c.query_row(&format!("SELECT count(*) FROM \"{n}\""), [], |r| r.get(0))?;
                out.push((n, count));
            }
            out.push(("events.seq".into(), terrariumd::events::last_seq(c)?));
            let cursor = rules_engine::cursor(c)?.unwrap_or(-1);
            out.push(("rule_cursor".into(), cursor));
            Ok(out)
        })
        .await
        .unwrap()
}

const DRY: &str = r#"rule "note" {
  on = ["test.ping"]
  card_comment { text = "first {{event.data.n}} on {{card.title}}" }
  card_move { to = "review"  comment = "moved by {{rule.name}}" }
}

rule "only-quiet" {
  on = ["test.ping"]
  if_card { has_labels = ["quiet"] }
  card_comment { text = "quiet" }
}

rule "other-kind" {
  on = ["test.pong"]
  card_comment { text = "never" }
}
"#;

#[tokio::test]
async fn the_dry_run_plans_and_writes_nothing() {
    let td = TestDaemon::start().await;
    td.project("dry").await;
    let card = new_card(&td, "dry", json!({"title": "Dry card"})).await;
    let s1 = ping(&td, "dry", &card, 1).await;
    let s2 = ping(&td, "dry", &card, 2).await;
    settle(&td).await;

    // No stored rules: the dry run needs a text.
    td.post("/api/projects/dry/rules/dry-run", json!({"seq": s1}))
        .await
        .err(404, "not_found");
    td.post(
        "/api/projects/dry/rules/dry-run",
        json!({"seq": s1, "from": s1, "text": DRY}),
    )
    .await
    .err(422, "invalid");

    let before = digest(&td).await;
    let r = td
        .post(
            "/api/projects/dry/rules/dry-run",
            json!({"seq": s1, "text": DRY}),
        )
        .await;
    let body = r.ok().clone();
    assert_eq!(body["source"], "text");
    assert_eq!(body["events"], 1);
    let plans = body["plans"].as_array().unwrap();
    assert_eq!(plans.len(), 2, "{plans:?}");
    assert_eq!(plans[0]["rule"], "note");
    assert_eq!(plans[0]["outcome"], "run");
    assert_eq!(plans[0]["card"], card.as_str());
    assert_eq!(plans[0]["actor"], "rule:dry/note");
    let acts = plans[0]["actions"].as_array().unwrap();
    assert_eq!(acts[0]["verb"], "card.comment");
    assert_eq!(acts[0]["args"]["text"], "first `1` on `Dry card`");
    assert_eq!(acts[0]["implemented"], true);
    assert_eq!(acts[1]["verb"], "card.move");
    assert_eq!(acts[1]["args"]["to"], "review");
    assert_eq!(acts[1]["args"]["comment"], "moved by note");
    assert_eq!(acts[1]["implemented"], true);
    assert_eq!(plans[1]["rule"], "only-quiet");
    assert_eq!(plans[1]["outcome"], "skipped");
    assert!(
        plans[1]["reason"]
            .as_str()
            .unwrap()
            .contains("has no label quiet"),
        "{plans:?}"
    );

    assert_eq!(digest(&td).await, before, "the dry run wrote nothing");

    // A range, with the stored rules.
    put_rules(&td, "dry", DRY).await;
    settle(&td).await;
    let before_range = digest(&td).await;
    let r = td
        .post(
            "/api/projects/dry/rules/dry-run",
            json!({"from": s1, "to": s2}),
        )
        .await;
    let body = r.ok().clone();
    assert_eq!(body["source"], "stored");
    assert_eq!(body["events"], 2);
    let plans = body["plans"].as_array().unwrap();
    assert_eq!(plans.len(), 4, "{plans:?}");
    assert_eq!(plans[2]["event"], s2);
    assert_eq!(
        plans[2]["actions"][0]["args"]["text"],
        "first `2` on `Dry card`"
    );
    assert_eq!(digest(&td).await, before_range, "the dry run wrote nothing");
    assert!(
        comments(&td, &card).await.is_empty(),
        "the old events ran nothing: they came before the rules"
    );
}

/// A board write on a card as an actor.
type CardWrite = fn(
    &mut terrariumd::db::Tx<'_>,
    &terrariumd::actor::Actor,
    &str,
) -> terrariumd::error::ApiResult<board::Card>;

#[tokio::test]
async fn the_rule_actor_is_a_project_admin_of_its_project_only() {
    let td = TestDaemon::start().await;
    td.project("terra").await;
    td.project("other").await;
    let held = new_card(
        &td,
        "terra",
        json!({"title": "From GitHub", "labels": ["github", "hold"]}),
    )
    .await;
    let mine = new_card(&td, "terra", json!({"title": "mine"})).await;
    let theirs = new_card(&td, "other", json!({"title": "theirs"})).await;

    let actor = rules_engine::rule_actor("terra", "r");
    assert!(!actor.is_operator());
    assert!(terrariumd::policy::is_admin_of(&actor, "terra"));
    assert!(!terrariumd::policy::is_admin_of(&actor, "other"));
    assert!(!actor.agent().unwrap().is_sysadmin());

    let st = state(&td);
    let try_as = |card: String, f: CardWrite| {
        let st = st.clone();
        async move {
            st.db
                .write(rules_engine::actor_label("terra", "r"), move |t| {
                    f(t, &rules_engine::rule_actor("terra", "r"), &card)
                })
                .await
        }
    };
    fn to_ready(
        t: &mut terrariumd::db::Tx<'_>,
        a: &terrariumd::actor::Actor,
        id: &str,
    ) -> terrariumd::error::ApiResult<board::Card> {
        board::move_card(t, a, id, "ready", None, None)
    }
    fn note(
        t: &mut terrariumd::db::Tx<'_>,
        a: &terrariumd::actor::Actor,
        id: &str,
    ) -> terrariumd::error::ApiResult<board::Card> {
        board::add_comment(t, a, id, "a note")
    }
    // The GitHub author gate holds for the rule actor.
    let e = try_as(held.clone(), to_ready).await.unwrap_err();
    assert!(matches!(e.code, Code::Forbidden), "{e:?}");
    assert!(e.message.contains("GitHub author gate"), "{e:?}");
    // The project gate: no write in another project.
    let e = try_as(theirs.clone(), note).await.unwrap_err();
    assert!(matches!(e.code, Code::Forbidden), "{e:?}");
    // In its own project it writes, as itself.
    try_as(mine.clone(), to_ready).await.unwrap();
    try_as(mine.clone(), note).await.unwrap();
    let c = comments(&td, &mine).await;
    assert_eq!(c[0].0, "rule:terra/r");

    // Through the engine: the writes and events carry the rule actor and
    // the cause.
    put_rules(&td, "terra", TWO_STEPS).await;
    let seq = ping(&td, "terra", &mine, 7).await;
    settle(&td).await;
    let evs = td
        .get(&format!("/api/cards/{mine}/events?limit=50"))
        .await
        .ok()
        .clone();
    let evs = evs.as_array().or(evs["events"].as_array()).unwrap().clone();
    let by_rule: Vec<&Value> = evs
        .iter()
        .filter(|e| e["data"]["change"] == "commented" && e["actor"] == "rule:terra/note")
        .collect();
    assert_eq!(by_rule.len(), 2, "{evs:?}");
    assert_eq!(by_rule[0]["data"]["cause"]["rule"], "terra/note");
    assert_eq!(by_rule[0]["data"]["cause"]["event"], seq);
    assert_eq!(by_rule[0]["data"]["cause"]["depth"], 1);
}

const FLOW: &str = r#"rule "first" {
  on = ["test.ping"]
  card_comment { text = "first {{event.data.n}}" }
}

rule "second" {
  on = ["test.ping"]
  if_card { lacks_labels = ["quiet"] }
  card_comment { text = "second {{event.data.n}}" }
}

rule "stops" {
  on = ["test.ping"]
  when "data.n" { eq = "1" }
  card_comment { text = "before" }
  card_move { to = "blocked" }
  card_comment { text = "after" }
}
"#;

#[tokio::test]
async fn ordered_actions_per_card_order_and_conditions() {
    let td = TestDaemon::start().await;
    td.project("flow").await;
    put_rules(&td, "flow", FLOW).await;
    let a = new_card(&td, "flow", json!({"title": "a"})).await;
    let q = new_card(&td, "flow", json!({"title": "q", "labels": ["quiet"]})).await;
    settle(&td).await;
    ping(&td, "flow", &a, 1).await;
    ping(&td, "flow", &a, 2).await;
    ping(&td, "flow", &q, 3).await;
    settle(&td).await;

    // Event order, then rule order, on each card. The run of `stops`
    // ended at its failed action: "after" never ran.
    let ca = comments(&td, &a).await;
    assert_eq!(
        bodies(&ca),
        vec![
            "first `1`",
            "second `1`",
            "before",
            "first `2`",
            "second `2`"
        ]
    );
    assert_eq!(ca[0].0, "rule:flow/first");
    assert_eq!(ca[2].0, "rule:flow/stops");
    let cq = comments(&td, &q).await;
    assert_eq!(bodies(&cq), vec!["first `3`"]);

    let r = runs(&td, "flow").await;
    let stops: Vec<&Value> = r.iter().filter(|r| r["rule"] == "stops").collect();
    assert_eq!(stops.len(), 1);
    assert_eq!(stops[0]["state"], "failed");
    assert_eq!(stops[0]["step"], 1);
    let reason = stops[0]["reason"].as_str().unwrap();
    assert!(reason.contains("action 2 (card.move)"), "{reason}");
    assert!(reason.contains("needs a comment"), "{reason}");
    assert_eq!(stops[0]["steps"][1]["ok"], false);
    let quiet: Vec<&Value> = r
        .iter()
        .filter(|r| r["rule"] == "second" && r["card"] == q.as_str())
        .collect();
    assert_eq!(quiet[0]["state"], "skipped");
    assert!(
        quiet[0]["reason"]
            .as_str()
            .unwrap()
            .contains("has the label quiet"),
        "{quiet:?}"
    );
    // The failed action wrote nothing: the card did not move.
    let card = td.get(&format!("/api/cards/{a}")).await.ok().clone();
    assert_eq!(card["column"], "backlog");
}

const COLS: &str = r#"rule "col" {
  on = ["test.ping"]
  if_card { not_column = ["review"] }
  card_comment { text = "not in review" }
}
"#;

#[tokio::test]
async fn a_column_removed_after_the_check_fails_the_run() {
    let td = TestDaemon::start().await;
    td.project("cols").await;
    put_rules(&td, "cols", COLS).await;
    let card = new_card(&td, "cols", json!({"title": "c"})).await;
    ping(&td, "cols", &card, 1).await;
    settle(&td).await;
    assert_eq!(bodies(&comments(&td, &card).await), vec!["not in review"]);

    // The board loses the column that the rule names.
    state(&td)
        .db
        .write("operator", |t| {
            t.execute(
                "DELETE FROM board_columns WHERE project = 'cols' AND name = 'review'",
                [],
            )?;
            Ok(())
        })
        .await
        .unwrap();
    ping(&td, "cols", &card, 2).await;
    settle(&td).await;
    assert_eq!(comments(&td, &card).await.len(), 1, "the run did not act");
    let r = runs(&td, "cols").await;
    let last = r.last().unwrap();
    assert_eq!(last["state"], "failed");
    assert!(
        last["reason"]
            .as_str()
            .unwrap()
            .contains("column `review` is not on the board of project `cols`"),
        "{last:?}"
    );
}

const FEW: &str = r#"rule "few" {
  on = ["test.ping"]
  max_per_hour = 2
  card_comment { text = "few {{event.data.n}}" }
}
"#;

const MANY: &str = r#"rule "many" {
  on = ["test.ping"]
  card_comment { text = "many {{event.data.n}}" }
}
"#;

/// The rate limits of the loop guard: they stop a loop that the chain
/// depth does not see (one that goes through an agent, depth 0 again).
#[tokio::test]
async fn the_rate_limits_stop_a_rule_per_hour_and_per_card() {
    let td = TestDaemon::start().await;

    // max_per_hour = 2: the third event of the hour starts no run, also
    // on another card.
    td.project("few").await;
    put_rules(&td, "few", FEW).await;
    let mut cards = Vec::new();
    for t in ["a", "b", "c"] {
        cards.push(new_card(&td, "few", json!({"title": t})).await);
    }
    settle(&td).await;
    for (n, card) in cards.iter().enumerate() {
        ping(&td, "few", card, n as i64 + 1).await;
    }
    settle(&td).await;
    assert_eq!(bodies(&comments(&td, &cards[0]).await), vec!["few `1`"]);
    assert_eq!(bodies(&comments(&td, &cards[1]).await), vec!["few `2`"]);
    assert!(comments(&td, &cards[2]).await.is_empty());
    let mut r = runs(&td, "few").await;
    r.sort_by_key(|r| r["event_seq"].as_i64());
    let states: Vec<&str> = r.iter().map(|r| r["state"].as_str().unwrap()).collect();
    assert_eq!(states, vec!["done", "done", "skipped"], "{r:?}");
    assert_eq!(r[2]["card"], cards[2].as_str());
    let reason = r[2]["reason"].as_str().unwrap();
    assert!(
        reason.starts_with("rate limit: 2 runs in an hour"),
        "{reason}"
    );

    // The default limit (30) holds, but one card takes at most 10 runs of
    // a rule in an hour.
    td.project("many").await;
    put_rules(&td, "many", MANY).await;
    let card = new_card(&td, "many", json!({"title": "busy"})).await;
    let other = new_card(&td, "many", json!({"title": "quiet"})).await;
    settle(&td).await;
    for n in 1..=11 {
        ping(&td, "many", &card, n).await;
    }
    ping(&td, "many", &other, 12).await;
    settle(&td).await;
    let c = comments(&td, &card).await;
    assert_eq!(c.len(), 10, "{c:?}");
    assert_eq!(c[9].1, "many `10`");
    assert_eq!(
        bodies(&comments(&td, &other).await),
        vec!["many `12`"],
        "another card still runs"
    );
    let r = runs(&td, "many").await;
    let skipped: Vec<&Value> = r.iter().filter(|r| r["state"] == "skipped").collect();
    assert_eq!(skipped.len(), 1, "{r:?}");
    assert_eq!(skipped[0]["card"], card.as_str());
    let reason = skipped[0]["reason"].as_str().unwrap();
    assert!(
        reason.contains(&format!("card rate limit: 10 runs for {card} in an hour")),
        "{reason}"
    );
    assert_eq!(r.iter().filter(|r| r["state"] == "done").count(), 11);
}

/// The pack's explicit hourly budget applies to a busy card too.
#[tokio::test]
async fn the_pack_matches_code_flow_after_ten_runs_for_one_card() {
    let td = TestDaemon::start().await;
    td.post("/api/hosts", json!({"name": "offline"})).await.ok();
    let mut cards = Vec::new();
    for project in ["coded", "ruled"] {
        td.project(project).await;
        td.post(
            &format!("/api/projects/{project}/repos"),
            json!({"name": "app", "url": "https://git.example/x/app.git"}),
        ).await.ok();
        let target = format!("{project}-build");
        td.post("/api/targets", json!({
            "name": target, "kind": "host", "host": "offline", "project": project
        })).await.ok();
        td.put(&format!("/api/projects/{project}/delivery"), json!({
            "target": target, "recipe": "true", "next_repos": ["app"]
        })).await.ok();
        if project == "ruled" {
            td.patch(&format!("/api/projects/{project}"), json!({"board_flow": "rules"}))
                .await.ok();
        }
        cards.push(new_card(&td, project, json!({"title": "busy", "column": "review"})).await);
    }
    settle(&td).await;
    for _ in 0..11 {
        let mut jobs = Vec::new();
        for card in &cards {
            let r = td.post(&format!("/api/cards/{card}/merge"), json!({})).await;
            jobs.push(r.ok()["job"]["id"].as_str().unwrap().to_owned());
        }
        settle(&td).await;
        for phase in ["queued", "cancelled"] {
            if phase == "cancelled" {
                for job in &jobs {
                    td.delete(&format!("/api/jobs/{job}")).await.ok();
                }
                settle(&td).await;
            }
            let coded = td.get(&format!("/api/cards/{}", cards[0])).await;
            let ruled = td.get(&format!("/api/cards/{}", cards[1])).await;
            assert_eq!(coded.ok()["labels"], ruled.ok()["labels"], "{phase}");
            assert_eq!(coded.ok()["column"], ruled.ok()["column"], "{phase}");
        }
    }
    let r = runs(&td, "ruled").await;
    for name in ["merge-queued", "job-cancelled"] {
        let matching: Vec<_> = r.iter().filter(|r| r["rule"] == name).collect();
        assert_eq!(matching.len(), 11, "{r:?}");
        assert!(matching.iter().all(|r| r["state"] == "done"), "{r:?}");
    }
}

const STATE: &str = r#"rule "open-job" {
  on = ["job"]
  if_job { state = ["queued", "running"] }
  card_comment { text = "job {{job.id}}" }
}

rule "red-job" {
  on = ["job"]
  if_job { outcome = ["red"] }
  card_comment { text = "red" }
}

rule "backlog-only" {
  on = ["test.ping"]
  if_card { column = ["backlog"]  assigned = false  archived = false }
  card_comment { text = "in backlog {{event.data.n}}" }
}
"#;

/// `if_job` (state, outcome, an event without its job) and the `if_card`
/// fields `column`, `assigned` and `archived`, on the current state.
#[tokio::test]
async fn job_and_card_conditions_read_the_current_state() {
    use terrariumd::store::jobs::{self, JobKind, NewJob};
    let td = TestDaemon::start().await;
    td.project("state").await;
    put_rules(&td, "state", STATE).await;
    let a = new_card(&td, "state", json!({"title": "a"})).await;
    let b = new_card(&td, "state", json!({"title": "b"})).await;
    let c = new_card(&td, "state", json!({"title": "c"})).await;
    td.post(&format!("/api/cards/{b}/move"), json!({"column": "ready"}))
        .await
        .ok();
    let d = td.delete(&format!("/api/cards/{c}")).await;
    assert_eq!(d.status, 204);
    settle(&td).await;

    ping(&td, "state", &a, 1).await;
    ping(&td, "state", &b, 2).await;
    ping(&td, "state", &c, 3).await;
    settle(&td).await;
    assert_eq!(bodies(&comments(&td, &a).await), vec!["in backlog `1`"]);
    assert!(comments(&td, &b).await.is_empty());
    assert!(comments(&td, &c).await.is_empty());
    let r = runs(&td, "state").await;
    let reason_of = |card: &str| {
        r.iter()
            .find(|r| r["rule"] == "backlog-only" && r["card"] == card)
            .map(|r| {
                (
                    r["state"].clone(),
                    r["reason"].as_str().unwrap_or("").to_owned(),
                )
            })
            .unwrap()
    };
    let (s, why) = reason_of(&b);
    assert_eq!(s, "skipped");
    assert!(why.contains(&format!("{b} is in ready")), "{why}");
    let (s, why) = reason_of(&c);
    assert_eq!(s, "skipped");
    assert!(why.contains(&format!("{c} is archived")), "{why}");

    // A job event: the job is queued (or running), its outcome is none.
    let card = a.clone();
    let job = state(&td)
        .db
        .write("operator", move |t| {
            jobs::create(
                t,
                NewJob {
                    project: "state".into(),
                    card,
                    kind: JobKind::Validate,
                    batch: Vec::new(),
                    planned_secs: None,
                    repo: "app".into(),
                    branch: "next".into(),
                    commit: None,
                    by_agent: None,
                },
            )
        })
        .await
        .unwrap();
    settle(&td).await;
    let ca = comments(&td, &a).await;
    let want = format!("job {}", job.id);
    assert_eq!(
        bodies(&ca).iter().filter(|b| **b == want).count(),
        1,
        "{ca:?}"
    );
    assert!(!bodies(&ca).contains(&"red"), "{ca:?}");
    let r = runs(&td, "state").await;
    let red: Vec<&Value> = r.iter().filter(|r| r["rule"] == "red-job").collect();
    assert!(!red.is_empty(), "{r:?}");
    assert!(red.iter().all(|r| r["state"] == "skipped"), "{red:?}");
    assert!(
        red[0]["reason"]
            .as_str()
            .unwrap()
            .contains("if_job 1 does not hold: the outcome is"),
        "{red:?}"
    );

    // A job event whose job is not in the store: no job, no run.
    let (project, card) = ("state".to_owned(), a.clone());
    let seq = state(&td)
        .db
        .write(SOURCE, move |t| {
            Ok(t.emit(
                EventDraft::new(
                    "job",
                    "a lost job",
                    json!({"id": "no-such-job", "card": card, "state": "queued"}),
                )
                .project(project),
            )?
            .seq)
        })
        .await
        .unwrap();
    settle(&td).await;
    let r = runs(&td, "state").await;
    let lost = r
        .iter()
        .find(|r| r["rule"] == "open-job" && r["event_seq"] == seq)
        .unwrap();
    assert_eq!(lost["state"], "skipped", "{lost:?}");
    assert!(
        lost["reason"]
            .as_str()
            .unwrap()
            .contains("if_job 1: the event has no job"),
        "{lost:?}"
    );
}

const ON_OFF: &str = r#"rule "note" {
  on = ["test.ping"]
  card_comment { text = "note {{event.data.n}}" }
}
"#;

const OFF: &str = r#"rule "note" {
  on = ["test.ping"]
  enabled = false
  card_comment { text = "note {{event.data.n}}" }
}
"#;

/// A planned run of a rule that is disabled (or gone) before the run
/// starts does not act.
#[tokio::test]
async fn a_rule_disabled_after_the_plan_does_not_run_its_pending_rows() {
    let mut td = TestDaemon::start().await;
    td.project("off").await;
    put_rules(&td, "off", ON_OFF).await;
    let card = new_card(&td, "off", json!({"title": "c"})).await;
    settle(&td).await;

    // With the daemon down: plan one event, then disable the rule.
    td.daemon.take().unwrap().shutdown();
    tokio::time::sleep(Duration::from_millis(50)).await;
    let path = td.state.join("terrarium.db");
    {
        let conn = terrariumd::db::open(&path).unwrap();
        terrariumd::events::insert(
            &conn,
            SOURCE,
            EventDraft::new("test.ping", "down", json!({"n": 1}))
                .project("off")
                .card(card.clone()),
        )
        .unwrap();
        let db = terrariumd::db::Db::new(conn, EventHub::default());
        let mut cache = rules_engine::Cache::default();
        let projects = rules_engine::load(&db, &mut cache).await.unwrap();
        let (written, _) = rules_engine::plan_pass(&db, projects, chrono::Utc::now())
            .await
            .unwrap();
        assert_eq!(written, 1);
        db.write("system", |t| {
            t.execute(
                "UPDATE project_rules SET text = ?1, hash = 'disabled' WHERE project = 'off'",
                [OFF],
            )?;
            Ok(())
        })
        .await
        .unwrap();
    }

    td.boot(None, None).await;
    settle(&td).await;
    assert!(comments(&td, &card).await.is_empty(), "the run did not act");
    let r = runs(&td, "off").await;
    assert_eq!(r.len(), 1, "{r:?}");
    assert_eq!(r[0]["state"], "skipped");
    assert!(
        r[0]["reason"]
            .as_str()
            .unwrap()
            .contains("disabled or gone since the plan"),
        "{r:?}"
    );
}
