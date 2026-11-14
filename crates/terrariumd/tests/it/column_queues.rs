//! The column queues (TER-1114, kanban rework C3): on a `columns` board a
//! move into a full column is accepted and the card waits in the column's
//! queue; the `lands` rules run when it gets a slot (`wip`, `job`, or
//! `batch` release). Claims, card waits and ETAs follow a queued card.
//! Every other board keeps the `409 wip_limit`.
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

async fn setup() -> TestDaemon {
    let td = TestDaemon::start().await;
    td.project("flow").await;
    td
}

async fn switch(td: &TestDaemon) {
    state(td)
        .db
        .write("operator", |t| {
            terrariumd::store::projects::set_columns_flow(t, "flow")?;
            Ok(())
        })
        .await
        .unwrap();
}

async fn sql(td: &TestDaemon, q: &'static str) {
    state(td)
        .db
        .write("operator", move |t| {
            t.execute_batch(q)?;
            Ok(())
        })
        .await
        .unwrap();
}

async fn set_wip(td: &TestDaemon, column: &'static str, wip: i64) {
    state(td)
        .db
        .write("operator", move |t| {
            t.execute(
                "UPDATE board_columns SET wip_limit = ?2 WHERE project = 'flow' AND name = ?1",
                rusqlite::params![column, wip],
            )?;
            Ok(())
        })
        .await
        .unwrap();
}

fn path(column: &str) -> String {
    format!("/api/projects/flow/board/columns/{column}/options")
}

/// Set the queue and the rules of a column with the options `PUT`.
async fn set_options(td: &TestDaemon, column: &str, queue: Value, rules: Value) {
    let o = td.get(&path(column)).await.ok().clone();
    td.put(
        &path(column),
        json!({"base_version": o["version"], "checks": o["checks"], "queue": queue,
               "rules": rules}),
    )
    .await
    .ok();
}

fn note(name: &str, trigger: &str, text: &str) -> Value {
    json!({"name": name, "trigger": {"kind": trigger},
           "actions": [{"verb": "card.comment", "args": {"text": text}}]})
}

async fn new_card(td: &TestDaemon, title: &str) -> String {
    td.post("/api/projects/flow/cards", json!({"title": title}))
        .await
        .ok()["id"]
        .as_str()
        .unwrap()
        .to_owned()
}

async fn move_to(td: &TestDaemon, card: &str, column: &str) -> Value {
    td.post(
        &format!("/api/cards/{card}/move"),
        json!({"column": column}),
    )
    .await
    .ok()
    .clone()
}

async fn card(td: &TestDaemon, id: &str) -> Value {
    td.get(&format!("/api/cards/{id}")).await.ok().clone()
}

async fn bodies(td: &TestDaemon, id: &str) -> Vec<String> {
    card(td, id).await["comments"]
        .as_array()
        .unwrap()
        .iter()
        .map(|c| c["body"].as_str().unwrap().to_owned())
        .collect()
}

async fn column(td: &TestDaemon, name: &str) -> Value {
    let b = td.get("/api/projects/flow/board").await.ok().clone();
    b["columns"]
        .as_array()
        .unwrap()
        .iter()
        .find(|c| c["name"] == name)
        .unwrap()
        .clone()
}

async fn queue_rows(td: &TestDaemon) -> i64 {
    state(td)
        .db
        .read(|c| Ok(c.query_row("SELECT count(*) FROM card_queue", [], |r| r.get(0))?))
        .await
        .unwrap()
}

/// Wait until the engine read the whole log and ran every pending row.
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
            "the rule engine did not settle"
        );
        tokio::time::sleep(Duration::from_millis(30)).await;
    }
}

async fn sweep(td: &TestDaemon) {
    state(td)
        .db
        .write("system", terrariumd::store::board::queues::sweep_all)
        .await
        .unwrap();
}

#[tokio::test]
async fn without_the_switch_a_full_column_still_refuses() {
    let td = setup().await;
    set_wip(&td, "review", 1).await;
    let a = new_card(&td, "a").await;
    let b = new_card(&td, "b").await;
    move_to(&td, &a, "review").await;
    td.post(&format!("/api/cards/{b}/move"), json!({"column": "review"}))
        .await
        .err(409, "wip_limit");
    assert_eq!(card(&td, &b).await["column"], "backlog");
    assert_eq!(queue_rows(&td).await, 0);
    assert!(column(&td, "review").await.get("queued").is_none());
}

#[tokio::test]
async fn a_full_column_queues_and_lands_runs_when_the_card_gets_its_slot() {
    let td = setup().await;
    set_wip(&td, "review", 1).await;
    set_options(
        &td,
        "review",
        json!({"release": "wip"}),
        json!([
            note("in", "lands", "landed in review"),
            note("out", "leaves", "left review"),
            note("gone", "archived", "archived in review"),
        ]),
    )
    .await;
    switch(&td).await;
    let a = new_card(&td, "a").await;
    let b = new_card(&td, "b").await;
    let c = new_card(&td, "c").await;
    let d = new_card(&td, "d").await;
    assert!(move_to(&td, &a, "review").await.get("queued").is_none());
    let mb = move_to(&td, &b, "review").await;
    assert_eq!(mb["column"], "review");
    assert_eq!(mb["queued"]["column"], "review");
    assert_eq!(mb["queued"]["place"], 1);
    let mc = move_to(&td, &c, "review").await;
    assert_eq!(mc["queued"]["place"], 2);
    move_to(&td, &d, "review").await;
    let col = column(&td, "review").await;
    assert_eq!(col["count"], 4);
    assert_eq!(col["queued"], 3);
    settle(&td).await;
    assert_eq!(bodies(&td, &a).await, vec!["landed in review"]);
    assert!(bodies(&td, &b).await.is_empty());

    // C leaves the queue: it never landed, so `leaves` does not run, and B
    // stays first.
    move_to(&td, &c, "ready").await;
    // D is archived in the queue: no `archived` rule runs for it.
    td.delete(&format!("/api/cards/{d}")).await.ok();
    assert_eq!(card(&td, &b).await["queued"]["place"], 1);

    // A leaves: B gets the slot, and its `lands` rule runs then.
    move_to(&td, &a, "ready").await;
    settle(&td).await;
    let kb = card(&td, &b).await;
    assert!(kb.get("queued").is_none(), "{kb}");
    assert_eq!(bodies(&td, &b).await, vec!["landed in review"]);
    assert_eq!(
        bodies(&td, &a).await,
        vec!["landed in review", "left review"]
    );
    assert!(bodies(&td, &c).await.is_empty());
    assert!(bodies(&td, &d).await.is_empty());
    assert_eq!(queue_rows(&td).await, 0);
    assert!(column(&td, "review").await.get("queued").is_none());

    // The `admitted` event comes after the `moved` event that freed the slot.
    let kinds: Vec<String> = state(&td)
        .db
        .read(move |c| {
            let mut st = c.prepare(
                "SELECT json_extract(payload, '$.change') || ':' || coalesce(card, '')
                   FROM events WHERE kind = 'card' ORDER BY seq",
            )?;
            let v = st
                .query_map([], |r| r.get::<_, String>(0))?
                .collect::<Result<Vec<_>, _>>()?;
            Ok(v)
        })
        .await
        .unwrap();
    let moved = kinds
        .iter()
        .rposition(|k| *k == format!("moved:{a}"))
        .unwrap();
    let admitted = kinds
        .iter()
        .position(|k| *k == format!("admitted:{b}"))
        .unwrap();
    assert!(moved < admitted, "{kinds:?}");
}

#[tokio::test]
async fn a_claim_waits_in_the_working_queue_and_its_eta_starts_with_the_slot() {
    let td = setup().await;
    set_wip(&td, "in_progress", 1).await;
    switch(&td).await;
    let a = new_card(&td, "a").await;
    let b = new_card(&td, "b").await;
    td.post_empty(&format!("/api/cards/{a}/claim")).await.ok();
    let kb = td
        .post_empty(&format!("/api/cards/{b}/claim"))
        .await
        .ok()
        .clone();
    assert_eq!(kb["column"], "in_progress");
    assert_eq!(kb["assignee"], "operator");
    assert_eq!(kb["queued"]["place"], 1);
    // The ETA of a waiting card is kept for its admission.
    let kb = td
        .patch(&format!("/api/cards/{b}"), json!({"eta_at": "30m"}))
        .await
        .ok()
        .clone();
    assert!(kb.get("eta_at").is_none(), "{kb}");
    assert_eq!(kb["queued"]["held_eta"], "30m");
    // The card reminder reads only admitted cards.
    let working = state(&td)
        .db
        .read(|c| terrariumd::store::board::working_cards_of(c, "operator"))
        .await
        .unwrap();
    assert_eq!(
        working.iter().map(|w| w.0.clone()).collect::<Vec<_>>(),
        vec![a.clone()]
    );

    move_to(&td, &a, "review").await;
    let kb = card(&td, &b).await;
    assert!(kb.get("queued").is_none(), "{kb}");
    let eta = chrono::DateTime::parse_from_rfc3339(kb["eta_at"].as_str().unwrap()).unwrap();
    let left = eta.signed_duration_since(chrono::Utc::now()).num_minutes();
    assert!((28..=30).contains(&left), "{kb}");
    assert_eq!(kb["eta_set_by"], "operator");
}

#[tokio::test]
async fn a_met_card_wait_queues_the_card_in_a_full_column() {
    let td = setup().await;
    set_wip(&td, "review", 1).await;
    // On a `columns` board a column rule runs the action of a met wait.
    set_options(
        &td,
        "backlog",
        json!({"release": "wip"}),
        json!([{"name": "wait", "trigger": {"kind": "wait_met"},
                "actions": [{"verb": "card.wait_action", "args": {}}]}]),
    )
    .await;
    switch(&td).await;
    let a = new_card(&td, "a").await;
    let dep = new_card(&td, "dep").await;
    let w = new_card(&td, "w").await;
    move_to(&td, &a, "review").await;
    td.put(
        &format!("/api/cards/{w}/wait-on"),
        json!({"dependencies": [{"card": dep}], "action": {"move_to": "review"}}),
    )
    .await
    .ok();
    td.post(
        &format!("/api/cards/{dep}/move"),
        json!({"column": "done", "comment": "done"}),
    )
    .await
    .ok();
    settle(&td).await;
    let kw = card(&td, &w).await;
    assert_eq!(kw["column"], "review", "{kw}");
    assert_eq!(kw["queued"]["place"], 1);
}

#[tokio::test]
async fn the_job_release_admits_one_card_per_job_and_frees_the_slot_of_a_lost_job() {
    let td = setup().await;
    set_options(
        &td,
        "validating",
        json!({"release": "job"}),
        json!([note("in", "lands", "landed in validating")]),
    )
    .await;
    switch(&td).await;
    let a = new_card(&td, "a").await;
    let b = new_card(&td, "b").await;
    let c = new_card(&td, "c").await;
    let d = new_card(&td, "d").await;
    assert!(move_to(&td, &a, "validating").await.get("queued").is_none());
    assert_eq!(move_to(&td, &b, "validating").await["queued"]["place"], 1);
    assert_eq!(move_to(&td, &c, "validating").await["queued"]["place"], 2);
    settle(&td).await;
    assert_eq!(bodies(&td, &a).await, vec!["landed in validating"]);
    assert!(bodies(&td, &b).await.is_empty());

    // A job of A is queued: the slot stays with A, also through the sweep.
    let qa = a.clone();
    state(&td)
        .db
        .write("operator", move |t| {
            t.execute(
                "INSERT INTO jobs (id, project, card, kind, repo, branch, state, attempt,
                                   created_by, created_at)
                 VALUES ('JA', 'flow', ?1, 'validate', 'app', 'x', 'queued', 1, 'operator',
                         strftime('%Y-%m-%dT%H:%M:%fZ', 'now'))",
                [qa],
            )?;
            Ok(())
        })
        .await
        .unwrap();
    sweep(&td).await;
    assert_eq!(card(&td, &b).await["queued"]["place"], 1);

    // The job ends (a cancel): B gets the slot. A stays in the column.
    state(&td)
        .db
        .write("operator", |t| {
            terrariumd::store::jobs::cancel(t, "JA")?;
            Ok(())
        })
        .await
        .unwrap();
    assert!(card(&td, &b).await.get("queued").is_none());
    assert_eq!(card(&td, &a).await["column"], "validating");
    assert_eq!(card(&td, &c).await["queued"]["place"], 1);
    settle(&td).await;
    assert_eq!(bodies(&td, &b).await, vec!["landed in validating"]);

    // B's job runs and is lost (a daemon restart): it waits in the job
    // queue again, so it still holds the slot.
    job(&td, "JB", &b, "running").await;
    state(&td)
        .db
        .write("operator", |t| {
            terrariumd::store::jobs::requeue_lost(t, "JB")?;
            Ok(())
        })
        .await
        .unwrap();
    sweep(&td).await;
    assert_eq!(card(&td, &c).await["queued"]["place"], 1);
    // The lost job is cancelled: no slot leak, C gets the slot.
    state(&td)
        .db
        .write("operator", |t| {
            terrariumd::store::jobs::cancel(t, "JB")?;
            Ok(())
        })
        .await
        .unwrap();
    assert!(card(&td, &c).await.get("queued").is_none());

    // C's job fails: D gets the slot.
    move_to(&td, &d, "validating").await;
    assert_eq!(card(&td, &d).await["queued"]["place"], 1);
    job(&td, "JC", &c, "running").await;
    state(&td)
        .db
        .write("operator", |t| {
            let end = terrariumd::store::jobs::JobEnd {
                passed: false,
                outcome: Some("failed".into()),
                exit_code: Some(1),
                tail: None,
                head: None,
                batch: None,
            };
            let j = terrariumd::store::jobs::finish(t, "JC", &end)?.unwrap();
            terrariumd::jobs::emit_verdicts(t, &j)?;
            Ok(())
        })
        .await
        .unwrap();
    // The rules act on the failure first (no rerun here, TER-1250).
    assert_eq!(card(&td, &d).await["queued"]["place"], 1);
    settle(&td).await;
    sweep(&td).await;
    assert!(card(&td, &d).await.get("queued").is_none());

    // D leaves before any job: E gets the slot.
    let e = new_card(&td, "e").await;
    move_to(&td, &e, "validating").await;
    assert_eq!(card(&td, &e).await["queued"]["place"], 1);
    move_to(&td, &d, "review").await;
    assert!(card(&td, &e).await.get("queued").is_none());
}

/// A failed job that a column rule runs again keeps the `job` slot until
/// the rerun ends (TER-1250): the next card is not admitted in the gap
/// between the end of attempt 1 and the rerun.
#[tokio::test]
async fn a_rerun_keeps_the_job_slot_until_it_ends() {
    let td = setup().await;
    set_options(
        &td,
        "validating",
        json!({"release": "job"}),
        json!([{"name": "again", "trigger": {"kind": "job_result", "events": ["verdict"]},
                "actions": [{"verb": "job.rerun"}]}]),
    )
    .await;
    switch(&td).await;
    let a = new_card(&td, "a").await;
    let b = new_card(&td, "b").await;
    assert!(move_to(&td, &a, "validating").await.get("queued").is_none());
    assert_eq!(move_to(&td, &b, "validating").await["queued"]["place"], 1);

    // A's job fails: its end and its verdict, as `finish_job` writes them.
    job(&td, "JA", &a, "running").await;
    state(&td)
        .db
        .write("system", |t| {
            let end = terrariumd::store::jobs::JobEnd {
                passed: false,
                outcome: Some("failed".into()),
                exit_code: Some(1),
                tail: None,
                head: None,
                batch: None,
            };
            let j = terrariumd::store::jobs::finish(t, "JA", &end)?.unwrap();
            terrariumd::jobs::emit_verdicts(t, &j)?;
            Ok(())
        })
        .await
        .unwrap();
    assert_eq!(card(&td, &b).await["queued"]["place"], 1);
    sweep(&td).await;
    assert_eq!(card(&td, &b).await["queued"]["place"], 1);

    // The rule queues the rerun: A still holds the slot.
    settle(&td).await;
    sweep(&td).await;
    let qa = a.clone();
    let rerun: String = state(&td)
        .db
        .read(move |c| {
            Ok(c.query_row(
                "SELECT id FROM jobs WHERE card = ?1 AND attempt = 2 AND state = 'queued'",
                [qa],
                |r| r.get(0),
            )?)
        })
        .await
        .unwrap();
    assert_eq!(card(&td, &b).await["queued"]["place"], 1);

    // The rerun passes: B gets the slot.
    state(&td)
        .db
        .write("system", move |t| {
            t.execute(
                "UPDATE jobs SET state = 'running', started_at = ?2 WHERE id = ?1",
                rusqlite::params![rerun, terrariumd::db::now()],
            )?;
            let end = terrariumd::store::jobs::JobEnd {
                passed: true,
                outcome: Some("passed".into()),
                exit_code: Some(0),
                tail: None,
                head: None,
                batch: None,
            };
            terrariumd::store::jobs::finish(t, &rerun, &end)?.unwrap();
            Ok(())
        })
        .await
        .unwrap();
    assert!(card(&td, &b).await.get("queued").is_none());
}

/// End job `id` as failed; `verdict`: also write its `job.verdict`.
async fn fail_job(td: &TestDaemon, id: &str, verdict: bool) {
    let id = id.to_owned();
    state(td)
        .db
        .write("system", move |t| {
            let end = terrariumd::store::jobs::JobEnd {
                passed: false,
                outcome: Some("failed".into()),
                exit_code: Some(1),
                tail: None,
                head: None,
                batch: None,
            };
            let j = terrariumd::store::jobs::finish(t, &id, &end)?.unwrap();
            if verdict {
                terrariumd::jobs::emit_verdicts(t, &j)?;
            }
            Ok(())
        })
        .await
        .unwrap();
}

/// Start queued job `id` (a rerun).
async fn start_job(td: &TestDaemon, id: &str) {
    let id = id.to_owned();
    state(td)
        .db
        .write("system", move |t| {
            t.execute(
                "UPDATE jobs SET state = 'running', started_at = ?2 WHERE id = ?1",
                rusqlite::params![id, terrariumd::db::now()],
            )?;
            Ok(())
        })
        .await
        .unwrap();
}

/// The queued attempt `attempt` of the job of card `card`.
async fn rerun_id(td: &TestDaemon, card: &str, attempt: i64) -> String {
    let card = card.to_owned();
    state(td)
        .db
        .read(move |c| {
            Ok(c.query_row(
                "SELECT id FROM jobs WHERE card = ?1 AND attempt = ?2 AND state = 'queued'",
                rusqlite::params![card, attempt],
                |r| r.get(0),
            )?)
        })
        .await
        .unwrap()
}

/// Run `before`, the sweep, then `after` in one write, so the rule engine
/// cannot act in between. Does card `waiter` still wait after the sweep?
async fn waits_after_sweep<B, A>(td: &TestDaemon, waiter: &str, before: B, after: A) -> bool
where
    B: FnOnce(&rusqlite::Connection) -> rusqlite::Result<()> + Send + 'static,
    A: FnOnce(&rusqlite::Connection) -> rusqlite::Result<()> + Send + 'static,
{
    let waiter = waiter.to_owned();
    state(td)
        .db
        .write("system", move |t| {
            before(t)?;
            terrariumd::store::board::queues::sweep_all(t)?;
            let waits: bool = t.query_row(
                "SELECT coalesce((SELECT admitted_at IS NULL FROM card_queue WHERE card = ?1), 0)",
                [&waiter],
                |r| r.get(0),
            )?;
            after(t)?;
            Ok(waits)
        })
        .await
        .unwrap()
}

/// Move the end of job `job` 11 minutes back and the admission of the
/// admitted card 20 minutes back: past the 10 minutes of `RERUN_SETTLED`.
fn aged(job: &'static str) -> impl Fn(&rusqlite::Connection) -> rusqlite::Result<()> + Copy {
    move |c| {
        c.execute(
            "UPDATE jobs SET ended_at = strftime('%Y-%m-%dT%H:%M:%fZ', 'now', '-660 seconds')
              WHERE id = ?1",
            [job],
        )?;
        c.execute(
            "UPDATE card_queue
                SET admitted_at = strftime('%Y-%m-%dT%H:%M:%fZ', 'now', '-1200 seconds')
              WHERE admitted_at IS NOT NULL",
            [],
        )?;
        Ok(())
    }
}

/// A pending rule run of event `seq`, last active `age` seconds ago.
fn pending_run(
    seq: i64,
    age: i64,
) -> impl Fn(&rusqlite::Connection) -> rusqlite::Result<()> + Copy {
    move |c| {
        c.execute(
            "INSERT INTO rule_runs (id, project, rule, rule_hash, ord, event_seq, card, lane,
                                    rule_json, cause, state, created_at, updated_at)
             VALUES ('RR-TER-1254', 'flow', 'held', 'h', 0, ?1, NULL, 'flow', '{}', '{}',
                     'pending', strftime('%Y-%m-%dT%H:%M:%fZ', 'now', ?2),
                     strftime('%Y-%m-%dT%H:%M:%fZ', 'now', ?2))",
            rusqlite::params![seq, format!("-{age} seconds")],
        )
        .map(|_| ())
    }
}

fn drop_run(c: &rusqlite::Connection) -> rusqlite::Result<()> {
    c.execute("DELETE FROM rule_runs WHERE id = 'RR-TER-1254'", [])
        .map(|_| ())
}

fn set_cursor(seq: i64) -> impl Fn(&rusqlite::Connection) -> rusqlite::Result<()> + Copy {
    move |c| {
        c.execute("UPDATE rule_cursor SET seq = ?1 WHERE id = 1", [seq])
            .map(|_| ())
    }
}

/// The first `job` or `job.verdict` event of job `job`.
async fn first_event(td: &TestDaemon, job: &'static str) -> i64 {
    state(td)
        .db
        .read(move |c| {
            Ok(c.query_row(
                "SELECT min(seq) FROM events WHERE kind IN ('job', 'job.verdict')
                   AND coalesce(json_extract(payload, '$.job'),
                                json_extract(payload, '$.id')) = ?1",
                [job],
                |r| r.get(0),
            )?)
        })
        .await
        .unwrap()
}

/// Column `validating` with a `job` release and `rules`, and `n` cards
/// moved into it: the first holds the slot, the others wait in order.
async fn job_column(td: &TestDaemon, rules: Value, n: usize) -> Vec<String> {
    set_options(td, "validating", json!({"release": "job"}), rules).await;
    switch(td).await;
    let mut cards = Vec::new();
    for i in 0..n {
        let c = new_card(td, &format!("c{i}")).await;
        let moved = move_to(td, &c, "validating").await;
        if i == 0 {
            assert!(moved.get("queued").is_none());
        } else {
            assert_eq!(moved["queued"]["place"], i);
        }
        cards.push(c);
    }
    cards
}

/// A failed job frees the `job` slot 10 minutes after its end without a
/// verdict, and 10 minutes after a rule run of it was last active when
/// that run is lost (TER-1254).
#[tokio::test]
async fn a_missing_verdict_or_a_lost_rule_run_frees_the_job_slot_after_10_minutes() {
    let td = setup().await;
    let cards = job_column(&td, json!([note("in", "lands", "landed")]), 3).await;
    let (a, b, c) = (&cards[0], &cards[1], &cards[2]);

    // A's job fails and no verdict is written: A holds the slot ...
    job(&td, "JA", a, "running").await;
    fail_job(&td, "JA", false).await;
    settle(&td).await;
    sweep(&td).await;
    assert_eq!(card(&td, b).await["queued"]["place"], 1);
    // ... until 10 minutes after the end.
    assert!(!waits_after_sweep(&td, b, aged("JA"), |_| Ok(())).await);
    assert_eq!(card(&td, c).await["queued"]["place"], 1);

    // B's job fails with a verdict. A rule run of its end is pending and
    // was active just now: B holds the slot, also 10 minutes after the end.
    job(&td, "JB", b, "running").await;
    fail_job(&td, "JB", true).await;
    settle(&td).await;
    let seq = first_event(&td, "JB").await;
    let held = move |c: &rusqlite::Connection| {
        pending_run(seq, 0)(c)?;
        aged("JB")(c)
    };
    assert!(waits_after_sweep(&td, c, held, drop_run).await);
    // The run was last active 11 minutes ago (lost): C gets the slot.
    let lost = move |c: &rusqlite::Connection| {
        pending_run(seq, 660)(c)?;
        aged("JB")(c)
    };
    assert!(!waits_after_sweep(&td, c, lost, drop_run).await);
    assert!(card(&td, c).await.get("queued").is_none());
}

/// After a daemon stop the sweep runs before the engine reads the log
/// (TER-1254 review): an end the rules have not read holds the slot at
/// any age, and a run that the engine resumes counts its 10 minutes from
/// the start.
#[tokio::test]
async fn after_a_daemon_stop_the_job_slot_waits_for_the_rules() {
    let td = setup().await;
    let cards = job_column(&td, json!([note("in", "lands", "landed")]), 2).await;
    let (a, b) = (&cards[0], &cards[1]);
    job(&td, "JA", a, "running").await;
    fail_job(&td, "JA", true).await;
    settle(&td).await;
    let seq = first_event(&td, "JA").await;
    let read_to: i64 = state(&td)
        .db
        .read(|c| Ok(rules_engine::cursor(c)?.unwrap()))
        .await
        .unwrap();
    assert!(read_to >= seq);

    // The cursor is before the end, which is 11 minutes old: held.
    let unread = move |c: &rusqlite::Connection| {
        set_cursor(seq - 1)(c)?;
        aged("JA")(c)
    };
    assert!(waits_after_sweep(&td, b, unread, set_cursor(read_to)).await);

    // A run left pending 45 minutes ago, and the engine starts: held.
    let resumed = move |c: &rusqlite::Connection| {
        pending_run(seq, 2700)(c)?;
        aged("JA")(c)?;
        rules_engine::resume_pending(c).unwrap();
        Ok(())
    };
    assert!(waits_after_sweep(&td, b, resumed, drop_run).await);
    assert_eq!(card(&td, b).await["queued"]["place"], 1);

    // The same run, not resumed (lost): B gets the slot.
    let lost = move |c: &rusqlite::Connection| {
        pending_run(seq, 2700)(c)?;
        aged("JA")(c)
    };
    assert!(!waits_after_sweep(&td, b, lost, drop_run).await);
    assert!(card(&td, b).await.get("queued").is_none());
}

/// A rule run of lane `flow` with ID `RR-TER-1277-<name>`: pending on
/// event `seq`, last active `age` seconds ago.
fn lane_run(
    name: &'static str,
    seq: i64,
    age: i64,
) -> impl Fn(&rusqlite::Connection) -> rusqlite::Result<()> + Copy {
    move |c| {
        c.execute(
            "INSERT INTO rule_runs (id, project, rule, rule_hash, ord, event_seq, card, lane,
                                    rule_json, cause, state, created_at, updated_at)
             VALUES ('RR-TER-1277-' || ?1, 'flow', ?1, 'h', 0, ?2, NULL, 'flow', '{}', '{}',
                     'pending', strftime('%Y-%m-%dT%H:%M:%fZ', 'now', ?3),
                     strftime('%Y-%m-%dT%H:%M:%fZ', 'now', ?3))",
            rusqlite::params![name, seq, format!("-{age} seconds")],
        )
        .map(|_| ())
    }
}

fn drop_lane_runs(c: &rusqlite::Connection) -> rusqlite::Result<()> {
    c.execute("DELETE FROM rule_runs WHERE id LIKE 'RR-TER-1277-%'", [])
        .map(|_| ())
}

/// After an outage, a lane with a backlog over 10 minutes holds the slot
/// while it makes progress, whatever the age of the run of the job end
/// (TER-1277, rev-1254b minor 3). A lane that made no progress for 10
/// minutes frees it.
#[tokio::test]
async fn a_lane_backlog_holds_the_job_slot_whatever_the_age() {
    let td = setup().await;
    let cards = job_column(&td, json!([note("in", "lands", "landed")]), 2).await;
    let (a, b) = (&cards[0], &cards[1]);
    job(&td, "JA", a, "running").await;
    fail_job(&td, "JA", true).await;
    settle(&td).await;
    let seq = first_event(&td, "JA").await;

    // The run of the end has waited 45 minutes behind an earlier run of
    // its lane, which the engine works on now: held.
    let backlog = move |c: &rusqlite::Connection| {
        lane_run("head", seq - 1, 0)(c)?;
        lane_run("end", seq, 2700)(c)?;
        aged("JA")(c)
    };
    assert!(waits_after_sweep(&td, b, backlog, drop_lane_runs).await);
    assert_eq!(card(&td, b).await["queued"]["place"], 1);

    // The same lane made no progress for 11 minutes (stalled): B gets the
    // slot.
    let stalled = move |c: &rusqlite::Connection| {
        lane_run("head", seq - 1, 660)(c)?;
        lane_run("end", seq, 2700)(c)?;
        aged("JA")(c)
    };
    assert!(!waits_after_sweep(&td, b, stalled, drop_lane_runs).await);
    assert!(card(&td, b).await.get("queued").is_none());
}

/// A failed rerun holds the slot until the rules act on its own end,
/// though the first failed job is settled (TER-1254, rev-1250 minor 3).
#[tokio::test]
async fn a_second_rerun_keeps_the_job_slot_until_it_ends() {
    let td = setup().await;
    let cards = job_column(
        &td,
        json!([{"name": "again", "trigger": {"kind": "job_result", "events": ["verdict"]},
                "actions": [{"verb": "job.rerun", "args": {"max_attempts": 3}}]}]),
        2,
    )
    .await;
    let (a, b) = (&cards[0], &cards[1]);

    job(&td, "JA", a, "running").await;
    fail_job(&td, "JA", true).await;
    settle(&td).await;
    sweep(&td).await;
    let second = rerun_id(&td, a, 2).await;
    assert_eq!(card(&td, b).await["queued"]["place"], 1);

    // The second attempt fails: JA is settled, but its rerun is not.
    start_job(&td, &second).await;
    fail_job(&td, &second, true).await;
    assert_eq!(card(&td, b).await["queued"]["place"], 1);
    sweep(&td).await;
    assert_eq!(card(&td, b).await["queued"]["place"], 1);

    // The rule queues the third attempt: A still holds the slot.
    settle(&td).await;
    sweep(&td).await;
    let third = rerun_id(&td, a, 3).await;
    assert_eq!(card(&td, b).await["queued"]["place"], 1);

    // The third attempt passes: B gets the slot.
    start_job(&td, &third).await;
    state(&td)
        .db
        .write("system", move |t| {
            let end = terrariumd::store::jobs::JobEnd {
                passed: true,
                outcome: Some("passed".into()),
                exit_code: Some(0),
                tail: None,
                head: None,
                batch: None,
            };
            terrariumd::store::jobs::finish(t, &third, &end)?.unwrap();
            Ok(())
        })
        .await
        .unwrap();
    assert!(card(&td, b).await.get("queued").is_none());
}

/// Insert job `id` of card `card` in state `state`.
async fn job(td: &TestDaemon, id: &'static str, card: &str, state_: &'static str) {
    let card = card.to_owned();
    state(td)
        .db
        .write("operator", move |t| {
            t.execute(
                "INSERT INTO jobs (id, project, card, kind, repo, branch, state, attempt,
                                   created_by, created_at, started_at)
                 VALUES (?1, 'flow', ?2, 'validate', 'app', 'x', ?3, 1, 'operator',
                         strftime('%Y-%m-%dT%H:%M:%fZ', 'now'),
                         strftime('%Y-%m-%dT%H:%M:%fZ', 'now'))",
                rusqlite::params![id, card, state_],
            )?;
            Ok(())
        })
        .await
        .unwrap();
}

#[tokio::test]
async fn the_batch_release_admits_n_cards_or_after_the_wait() {
    let td = setup().await;
    set_options(
        &td,
        "merge_queue",
        json!({"release": "batch", "batch_size": 2, "max_wait_seconds": 60,
               "max_batch_cards": 2}),
        json!([note("in", "lands", "landed in merge_queue")]),
    )
    .await;
    switch(&td).await;
    let a = new_card(&td, "a").await;
    let b = new_card(&td, "b").await;
    let c = new_card(&td, "c").await;
    let d = new_card(&td, "d").await;
    // One card is not a batch yet.
    assert_eq!(move_to(&td, &a, "merge_queue").await["queued"]["place"], 1);
    sweep(&td).await;
    assert_eq!(card(&td, &a).await["queued"]["place"], 1);
    // The second card makes the batch: both are admitted.
    move_to(&td, &b, "merge_queue").await;
    assert!(card(&td, &a).await.get("queued").is_none());
    assert!(card(&td, &b).await.get("queued").is_none());
    // While a card of the batch is in the column, the next ones wait.
    move_to(&td, &c, "merge_queue").await;
    move_to(&td, &d, "merge_queue").await;
    assert_eq!(card(&td, &c).await["queued"]["place"], 1);
    move_to(&td, &a, "accepted").await;
    assert_eq!(card(&td, &c).await["queued"]["place"], 1);
    // The last card of the batch leaves: C and D are the next batch.
    move_to(&td, &b, "accepted").await;
    assert!(card(&td, &c).await.get("queued").is_none());
    assert!(card(&td, &d).await.get("queued").is_none());
    settle(&td).await;
    for id in [&a, &b, &c, &d] {
        assert_eq!(bodies(&td, id).await, vec!["landed in merge_queue"]);
    }

    // One card alone goes after `max_wait_seconds`.
    move_to(&td, &c, "accepted").await;
    move_to(&td, &d, "accepted").await;
    let e = new_card(&td, "e").await;
    move_to(&td, &e, "merge_queue").await;
    sweep(&td).await;
    assert_eq!(card(&td, &e).await["queued"]["place"], 1);
    sql(
        &td,
        "UPDATE card_queue SET queued_at = '2000-01-01T00:00:00.000Z' WHERE admitted_at IS NULL",
    )
    .await;
    sweep(&td).await;
    assert!(card(&td, &e).await.get("queued").is_none());
}

#[tokio::test]
async fn an_options_write_releases_waiting_cards() {
    let td = setup().await;
    set_wip(&td, "review", 1).await;
    switch(&td).await;
    let a = new_card(&td, "a").await;
    let b = new_card(&td, "b").await;
    move_to(&td, &a, "review").await;
    move_to(&td, &b, "review").await;
    assert_eq!(card(&td, &b).await["queued"]["place"], 1);
    // A queue option write releases the column at once.
    set_wip(&td, "review", 2).await;
    set_options(&td, "review", json!({"release": "job"}), json!([])).await;
    assert!(card(&td, &b).await.get("queued").is_none());
}
