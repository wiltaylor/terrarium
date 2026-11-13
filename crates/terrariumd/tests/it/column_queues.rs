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
            terrariumd::store::jobs::finish(t, "JC", &end)?;
            Ok(())
        })
        .await
        .unwrap();
    assert!(card(&td, &d).await.get("queued").is_none());

    // D leaves before any job: E gets the slot.
    let e = new_card(&td, "e").await;
    move_to(&td, &e, "validating").await;
    assert_eq!(card(&td, &e).await["queued"]["place"], 1);
    move_to(&td, &d, "review").await;
    assert!(card(&td, &e).await.get("queued").is_none());
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

/// The stored `eta_at` and `eta_set_by` of card `id`.
async fn db_eta(td: &TestDaemon, id: &str) -> (Option<String>, Option<String>) {
    let id = id.to_owned();
    state(td)
        .db
        .read(move |c| {
            Ok(c.query_row(
                "SELECT eta_at, eta_set_by FROM cards WHERE id = ?1",
                [id],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )?)
        })
        .await
        .unwrap()
}

/// TER-1204 (rev-1114 findings 1 and 2): with `clear_eta_on_enter = false`
/// a card that waits in the queue has no ETA, so no `eta_overdue` fires
/// while it waits. The kept ETA is held and set again at its admission. A
/// held ETA set on another actor's card is audited.
#[tokio::test]
async fn a_waiting_card_holds_a_kept_eta_and_another_actors_change_is_audited() {
    let td = setup().await;
    td.agent("flow", "w1", json!({})).await;
    set_wip(&td, "review", 1).await;
    let o = td.get(&path("review")).await.ok().clone();
    let mut checks = o["checks"].clone();
    checks["clear_eta_on_enter"] = json!(false);
    td.put(
        &path("review"),
        json!({"base_version": o["version"], "checks": checks,
               "queue": {"release": "wip"}, "rules": []}),
    )
    .await
    .ok();
    switch(&td).await;
    let a = new_card(&td, "a").await;
    let b = new_card(&td, "b").await;
    let c = new_card(&td, "c").await;
    move_to(&td, &a, "review").await;

    // B: claimed by w1 with an ETA, then into the full review column.
    for id in [&b, &c] {
        td.post_empty_as("flow/w1", &format!("/api/cards/{id}/claim"))
            .await
            .ok();
    }
    let kb = td
        .patch_as(
            "flow/w1",
            &format!("/api/cards/{b}"),
            json!({"eta_at": "2h"}),
        )
        .await
        .ok()
        .clone();
    let eta = kb["eta_at"].as_str().unwrap().to_owned();
    let mb = td
        .post_as(
            "flow/w1",
            &format!("/api/cards/{b}/move"),
            json!({"column": "review"}),
        )
        .await
        .ok()
        .clone();
    assert_eq!(mb["queued"]["place"], 1, "{mb}");
    assert!(mb.get("eta_at").is_none(), "{mb}");
    assert_eq!(mb["queued"]["held_eta"], json!(eta), "{mb}");
    let (raw, _) = db_eta(&td, &b).await;
    assert!(raw.is_none(), "a waiting card has no eta_at: {raw:?}");

    // A leaves: B is admitted with its kept ETA.
    move_to(&td, &a, "ready").await;
    let kb = card(&td, &b).await;
    assert!(kb.get("queued").is_none(), "{kb}");
    // The review column shows the review ETA: read the stored one.
    let (at, by) = db_eta(&td, &b).await;
    assert_eq!(at.as_deref(), Some(eta.as_str()), "{kb}");
    assert_eq!(by.as_deref(), kb["assignee"].as_str(), "{kb}");

    // C waits; w1's own held ETA is not audited, the operator's is.
    let mc = td
        .post_as(
            "flow/w1",
            &format!("/api/cards/{c}/move"),
            json!({"column": "review"}),
        )
        .await
        .ok()
        .clone();
    assert_eq!(mc["queued"]["place"], 1, "{mc}");
    td.patch_as(
        "flow/w1",
        &format!("/api/cards/{c}"),
        json!({"eta_at": "3h"}),
    )
    .await
    .ok();
    let audit = td.get("/api/audit?action=card.eta").await.ok().clone();
    assert!(audit.as_array().unwrap().is_empty(), "{audit}");
    td.patch(&format!("/api/cards/{c}"), json!({"eta_at": "1h"}))
        .await
        .ok();
    let audit = td.get("/api/audit?action=card.eta").await.ok().clone();
    assert_eq!(audit.as_array().unwrap().len(), 1, "{audit}");
    assert_eq!(card(&td, &c).await["queued"]["held_eta"], "1h");
}

/// TER-1204 (rev-1114 finding 3): a job of the card from the previous
/// column that ends after the admission does not free the `job` slot. A
/// job queued after the admission does.
#[tokio::test]
async fn an_older_job_of_the_card_does_not_free_the_job_slot() {
    let td = setup().await;
    set_options(&td, "validating", json!({"release": "job"}), json!([])).await;
    switch(&td).await;
    let a = new_card(&td, "a").await;
    let b = new_card(&td, "b").await;
    // A job of A runs while A is still in review.
    move_to(&td, &a, "review").await;
    job(&td, "JOLD", &a, "running").await;
    tokio::time::sleep(Duration::from_millis(5)).await;
    assert!(move_to(&td, &a, "validating").await.get("queued").is_none());
    assert_eq!(move_to(&td, &b, "validating").await["queued"]["place"], 1);
    // The old job ends: the slot stays with A.
    state(&td)
        .db
        .write("operator", |t| {
            terrariumd::store::jobs::cancel(t, "JOLD")?;
            Ok(())
        })
        .await
        .unwrap();
    sweep(&td).await;
    assert_eq!(card(&td, &b).await["queued"]["place"], 1);
    // A job of A queued in validating ends: B gets the slot.
    job(&td, "JNEW", &a, "running").await;
    state(&td)
        .db
        .write("operator", |t| {
            terrariumd::store::jobs::cancel(t, "JNEW")?;
            Ok(())
        })
        .await
        .unwrap();
    assert!(card(&td, &b).await.get("queued").is_none());
}

/// TER-1204 (rev-1114 finding 4, rev-1204 finding 1): a column that
/// changes from `wip` to `batch` with a card admitted under `wip` and no
/// job puts that card back in its queue, first: it does not hold the lane,
/// and it goes with the next batch.
#[tokio::test]
async fn a_change_from_wip_to_batch_puts_a_card_with_no_job_back_in_the_queue() {
    let td = setup().await;
    set_options(&td, "merge_queue", json!({"release": "wip"}), json!([])).await;
    switch(&td).await;
    let a = new_card(&td, "a").await;
    let b = new_card(&td, "b").await;
    let c = new_card(&td, "c").await;
    assert!(
        move_to(&td, &a, "merge_queue")
            .await
            .get("queued")
            .is_none()
    );
    set_options(
        &td,
        "merge_queue",
        json!({"release": "batch", "batch_size": 2, "max_wait_seconds": 3600,
               "max_batch_cards": 2}),
        json!([]),
    )
    .await;
    assert_eq!(card(&td, &a).await["queued"]["place"], 1);
    // B makes the batch of two with A; C waits for the next one.
    move_to(&td, &b, "merge_queue").await;
    move_to(&td, &c, "merge_queue").await;
    sweep(&td).await;
    assert!(card(&td, &a).await.get("queued").is_none());
    assert!(card(&td, &b).await.get("queued").is_none());
    assert_eq!(card(&td, &c).await["queued"]["place"], 1);
}

/// TER-1204 (rev-1204 finding 3): the ETA of a job of a waiting card is
/// held for its admission, so no `eta_overdue` fires while it waits.
#[tokio::test]
async fn a_job_eta_of_a_waiting_card_is_held() {
    let td = setup().await;
    set_wip(&td, "review", 1).await;
    switch(&td).await;
    let a = new_card(&td, "a").await;
    let b = new_card(&td, "b").await;
    move_to(&td, &a, "review").await;
    assert_eq!(move_to(&td, &b, "review").await["queued"]["place"], 1);
    let at = (chrono::Utc::now() + chrono::Duration::hours(1))
        .to_rfc3339_opts(chrono::SecondsFormat::Millis, true);
    let (id, when) = (b.clone(), at.clone());
    state(&td)
        .db
        .write("operator", move |t| {
            terrariumd::store::board::set_job_eta(t, &id, Some(&when), "agent:w1")?;
            Ok(())
        })
        .await
        .unwrap();
    let (raw, _) = db_eta(&td, &b).await;
    assert!(raw.is_none(), "{raw:?}");
    assert_eq!(card(&td, &b).await["queued"]["held_eta"], json!(at));
    move_to(&td, &a, "ready").await;
    let (raw, _) = db_eta(&td, &b).await;
    assert_eq!(raw.as_deref(), Some(at.as_str()));
}
