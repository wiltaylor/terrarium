//! The verbs of the board rules (TER-169, docs/api.md §19.9): one test
//! for each verb, run by the engine as the rule actor; a card held by the
//! GitHub author gate refused by every verb; a rule actor without
//! `project_admin` cannot queue a merge; and a run of only no-ops ends as
//! `noop` (the TER-168 review asked for that path).
#![allow(clippy::unwrap_used, clippy::expect_used)]

mod common;

use std::time::Duration;

use common::TestDaemon;
use serde_json::{Map, Value, json};
use terrarium_core::config::{CompactMaxAge, IdleStop, Policy};
use terrarium_core::rules_file::{Action, VERBS};
use terrariumd::actor::{Actor, AgentActor};
use terrariumd::api::Shared;
use terrariumd::error::Code;
use terrariumd::events::EventDraft;
use terrariumd::rules_engine;
use terrariumd::store::jobs::{self, BatchCard, Job, JobEnd, JobKind, NewJob};

fn state(td: &TestDaemon) -> Shared {
    td.daemon.as_ref().unwrap().state.clone()
}

const SOURCE: &str = "source:test/ping";

/// Write an event of `kind` about `card` with `data`, as an event source
/// does. Returns its `seq`.
async fn emit(td: &TestDaemon, kind: &str, project: &str, card: &str, data: Value) -> i64 {
    let (kind, project, card) = (kind.to_owned(), project.to_owned(), card.to_owned());
    state(td)
        .db
        .write(SOURCE, move |t| {
            Ok(t.emit(
                EventDraft::new(kind.clone(), kind, data)
                    .project(project)
                    .card(card),
            )?
            .seq)
        })
        .await
        .unwrap()
}

/// A `test.ping` about `card` with `data.n = n`.
async fn ping(td: &TestDaemon, project: &str, card: &str, n: i64) -> i64 {
    emit(td, "test.ping", project, card, json!({"n": n})).await
}

/// A `job.verdict` of `job` for `card`, as the job queue writes it.
async fn verdict(td: &TestDaemon, job: &Job, card: &str, verdict: &str) -> i64 {
    emit(
        td,
        "job.verdict",
        &job.project,
        card,
        json!({"project": job.project, "job": job.id, "card": card, "kind": job.kind,
               "verdict": verdict, "attempt": job.attempt}),
    )
    .await
}

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

/// Wait until the engine read the whole log and ran every pending row.
async fn settle(td: &TestDaemon) {
    let mut quiet = 0;
    for _ in 0..2000 {
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
    let r = td
        .put(
            &format!("/api/projects/{project}/rules"),
            json!({"text": text}),
        )
        .await;
    assert_eq!(r.status, 200, "{}", r.body);
}

async fn new_card(td: &TestDaemon, project: &str, body: Value) -> String {
    let r = td.post(&format!("/api/projects/{project}/cards"), body).await;
    assert_eq!(r.status, 201, "{}", r.body);
    r.body["id"].as_str().unwrap().to_owned()
}

async fn card(td: &TestDaemon, id: &str) -> Value {
    td.get(&format!("/api/cards/{id}")).await.ok().clone()
}

fn bodies(card: &Value) -> Vec<String> {
    card["comments"]
        .as_array()
        .unwrap()
        .iter()
        .map(|c| c["body"].as_str().unwrap().to_owned())
        .collect()
}

/// The run log of a project, in event order.
async fn runs(td: &TestDaemon, project: &str) -> Vec<Value> {
    let mut v = td
        .get(&format!("/api/projects/{project}/rules/runs?limit=500"))
        .await
        .ok()["runs"]
        .as_array()
        .unwrap()
        .clone();
    v.sort_by_key(|r| r["event_seq"].as_i64());
    v
}

/// The runs of rule `rule`, oldest first.
async fn runs_of(td: &TestDaemon, project: &str, rule: &str) -> Vec<Value> {
    runs(td, project)
        .await
        .into_iter()
        .filter(|r| r["rule"] == rule)
        .collect()
}

fn states(r: &[Value]) -> Vec<&str> {
    r.iter().map(|r| r["state"].as_str().unwrap()).collect()
}

/// Project `p` with a repo `app` that has `next`, and a job queue whose
/// target's host agent is offline: a queued job stays queued.
async fn queue_lab() -> TestDaemon {
    let td = TestDaemon::start().await;
    td.project("p").await;
    let r = td
        .post(
            "/api/projects/p/repos",
            json!({"name": "app", "url": "https://git.example/x/app.git"}),
        )
        .await;
    assert_eq!(r.status, 201, "{}", r.body);
    td.post("/api/hosts", json!({"name": "helios"})).await.ok();
    td.post(
        "/api/targets",
        json!({"name": "p-build", "kind": "host", "host": "helios", "project": "p"}),
    )
    .await
    .ok();
    let r = td
        .put(
            "/api/projects/p/delivery",
            json!({"target": "p-build", "recipe": "true", "next_repos": ["app"]}),
        )
        .await;
    assert_eq!(r.status, 200, "{}", r.body);
    td
}

/// The jobs of project `p`, oldest first.
async fn all_jobs(td: &TestDaemon) -> Vec<Job> {
    let mut v = state(td)
        .db
        .read(|c| jobs::list(c, "p", None))
        .await
        .unwrap();
    v.sort_by(|a, b| a.id.cmp(&b.id));
    v
}

/// A job of `cards` (a batch for two or more), queued as the operator.
async fn store_job(td: &TestDaemon, kind: JobKind, cards: &[&str]) -> Job {
    let batch: Vec<BatchCard> = if cards.len() > 1 {
        cards
            .iter()
            .map(|c| BatchCard {
                card: (*c).to_owned(),
                branch: c.to_lowercase(),
                by_agent: None,
                outcome: None,
                note: None,
            })
            .collect()
    } else {
        Vec::new()
    };
    let first = cards[0].to_owned();
    state(td)
        .db
        .write("operator", move |t| {
            jobs::create(
                t,
                NewJob {
                    project: "p".into(),
                    branch: first.to_lowercase(),
                    card: first,
                    kind,
                    repo: "app".into(),
                    commit: None,
                    by_agent: None,
                    batch,
                },
            )
        })
        .await
        .unwrap()
}

/// Start and end `job` as failed with `outcome`.
async fn fail_job(td: &TestDaemon, job: &Job, outcome: &str) -> Job {
    let (id, outcome) = (job.id.clone(), outcome.to_owned());
    state(td)
        .db
        .write("system", move |t| {
            jobs::start(t, &id, "machine")?;
            let end = JobEnd {
                passed: false,
                outcome: Some(outcome),
                exit_code: Some(1),
                tail: Some("test a ... FAILED\nfailures: a".into()),
                ..JobEnd::default()
            };
            Ok(jobs::finish(t, &id, &end)?.unwrap())
        })
        .await
        .unwrap()
}

// ------------------------------------------------------------ card verbs

const MOVE: &str = r#"rule "to-ready" {
  on = ["test.ping"]
  when "data.n" { eq = "1" }
  card_move { to = "ready"  comment = "ready by {{rule.name}}" }
}

rule "guarded" {
  on = ["test.ping"]
  when "data.n" { eq = "2" }
  card_move { to = "review"  from = ["backlog"] }
}

rule "top" {
  on = ["test.ping"]
  when "data.n" { eq = "3" }
  card_move { to = "@review"  from = ["ready"]  position = "top" }
}
"#;

#[tokio::test]
async fn card_move_takes_a_precondition_and_a_position() {
    let td = TestDaemon::start().await;
    td.project("p").await;
    put_rules(&td, "p", MOVE).await;
    let a = new_card(&td, "p", json!({"title": "a"})).await;
    let b = new_card(&td, "p", json!({"title": "b", "column": "review"})).await;
    settle(&td).await;

    ping(&td, "p", &a, 1).await;
    settle(&td).await;
    let c = card(&td, &a).await;
    assert_eq!(c["column"], "ready");
    assert_eq!(bodies(&c), vec!["ready by `to-ready`"]);
    assert_eq!(c["comments"][0]["author"], "rule:p/to-ready");

    // Not in `from`: a no-op.
    ping(&td, "p", &a, 2).await;
    settle(&td).await;
    assert_eq!(card(&td, &a).await["column"], "ready");
    let g = runs_of(&td, "p", "guarded").await;
    assert_eq!(states(&g), vec!["noop"], "{g:?}");
    assert!(
        g[0]["steps"][0]["detail"]["reason"]
            .as_str()
            .unwrap()
            .contains("not in backlog"),
        "{g:?}"
    );

    // `@review` by role, at the top.
    ping(&td, "p", &a, 3).await;
    settle(&td).await;
    let c = card(&td, &a).await;
    assert_eq!(c["column"], "review");
    assert_eq!(c["position"], 0);
    assert_eq!(card(&td, &b).await["position"], 1);
    // In `to` already: a no-op.
    ping(&td, "p", &a, 1).await;
    settle(&td).await;
    let r = runs_of(&td, "p", "to-ready").await;
    assert_eq!(states(&r), vec!["done", "done"], "the move to ready again");
    assert_eq!(card(&td, &a).await["column"], "ready");
}

const LABEL: &str = r#"rule "label" {
  on = ["test.ping"]
  card_label { add = ["x", "{{event.data.extra}}"]  remove = ["old"] }
}
"#;

/// `card.label`, and the `noop` end of a run whose only action changed
/// nothing.
#[tokio::test]
async fn card_label_adds_and_removes_and_a_second_run_is_a_noop() {
    let td = TestDaemon::start().await;
    td.project("p").await;
    put_rules(&td, "p", LABEL).await;
    let a = new_card(&td, "p", json!({"title": "a", "labels": ["old", "keep"]})).await;
    settle(&td).await;

    emit(&td, "test.ping", "p", &a, json!({"extra": "y"})).await;
    settle(&td).await;
    let mut labels: Vec<String> =
        serde_json::from_value(card(&td, &a).await["labels"].clone()).unwrap();
    labels.sort();
    assert_eq!(labels, vec!["keep", "x", "y"]);

    emit(&td, "test.ping", "p", &a, json!({"extra": "y"})).await;
    settle(&td).await;
    let r = runs_of(&td, "p", "label").await;
    assert_eq!(states(&r), vec!["done", "noop"], "{r:?}");
    assert_eq!(r[1]["steps"][0]["ok"], true);
    assert_eq!(r[1]["steps"][0]["changed"], false);
    // A placeholder without a value adds no empty label.
    emit(&td, "test.ping", "p", &a, json!({})).await;
    settle(&td).await;
    let r = runs_of(&td, "p", "label").await;
    assert_eq!(r[2]["state"], "noop", "{r:?}");
}

const COMMENT: &str = r#"rule "note" {
  on = ["test.ping"]
  card_comment { text = "seen {{event.data.n}} on {{card.title}}" }
}
"#;

#[tokio::test]
async fn card_comment_quotes_its_values() {
    let td = TestDaemon::start().await;
    td.project("p").await;
    put_rules(&td, "p", COMMENT).await;
    let a = new_card(&td, "p", json!({"title": "a `b`"})).await;
    settle(&td).await;
    ping(&td, "p", &a, 4).await;
    settle(&td).await;
    let c = card(&td, &a).await;
    assert_eq!(bodies(&c), vec!["seen `4` on `a 'b'`"]);
    assert_eq!(c["comments"][0]["author"], "rule:p/note");
}

const ASSIGN: &str = r#"rule "give" {
  on = ["test.ping"]
  when "data.n" { eq = "1" }
  card_assign { who = "w1" }
}

rule "free" {
  on = ["test.ping"]
  when "data.n" { eq = "2" }
  card_assign { who = "none" }
}

rule "empty" {
  on = ["test.ping"]
  when "data.n" { eq = "3" }
  card_assign { who = "{{event.data.who}}" }
}
"#;

#[tokio::test]
async fn card_assign_gives_frees_and_refuses_an_empty_name() {
    let td = TestDaemon::start().await;
    td.project("p").await;
    td.agent("p", "w1", json!({})).await;
    put_rules(&td, "p", ASSIGN).await;
    let a = new_card(&td, "p", json!({"title": "a"})).await;
    settle(&td).await;

    ping(&td, "p", &a, 1).await;
    settle(&td).await;
    assert_eq!(card(&td, &a).await["assignee_name"], "w1");
    ping(&td, "p", &a, 1).await;
    settle(&td).await;
    assert_eq!(
        states(&runs_of(&td, "p", "give").await),
        vec!["done", "noop"]
    );

    // A placeholder without a value does not unassign.
    ping(&td, "p", &a, 3).await;
    settle(&td).await;
    let e = runs_of(&td, "p", "empty").await;
    assert_eq!(e[0]["state"], "failed", "{e:?}");
    assert!(
        e[0]["reason"].as_str().unwrap().contains("is empty"),
        "{e:?}"
    );
    assert_eq!(card(&td, &a).await["assignee_name"], "w1");

    ping(&td, "p", &a, 2).await;
    settle(&td).await;
    assert!(card(&td, &a).await["assignee"].is_null());
}

const PREPEND: &str = r#"rule "block" {
  on = ["test.ping"]
  card_prepend { text = "{{event.data.block}}" }
}
"#;

#[tokio::test]
async fn card_prepend_keeps_the_lines_and_runs_once() {
    let td = TestDaemon::start().await;
    td.project("p").await;
    put_rules(&td, "p", PREPEND).await;
    let a = new_card(&td, "p", json!({"title": "a", "body": "The goal."})).await;
    settle(&td).await;
    let block = "Validation failed: job 1.\nOutput tail:\n\n    test a ... FAILED\n";
    emit(&td, "test.ping", "p", &a, json!({"block": block})).await;
    settle(&td).await;
    assert_eq!(
        card(&td, &a).await["body"],
        format!("{block}The goal.").as_str()
    );
    emit(&td, "test.ping", "p", &a, json!({"block": block})).await;
    settle(&td).await;
    assert_eq!(
        states(&runs_of(&td, "p", "block").await),
        vec!["done", "noop"]
    );
}

// ------------------------------------------------------------------ mail

const MAIL: &str = r#"rule "tell" {
  on = ["test.ping"]
  mail {
    to = ["operator", "{{event.data.who}}"]
    subject = "{{card.id}} needs a look"
    body = "Seen {{event.data.n}}."
  }
}
"#;

/// `mail` from the project inbox; a recipient without a value is left
/// out. The operator is notified this way (there is no `notify` verb).
#[tokio::test]
async fn mail_goes_from_the_project_inbox() {
    let td = TestDaemon::start().await;
    td.project("p").await;
    let w1 = td.agent("p", "w1", json!({})).await["id"]
        .as_str()
        .unwrap()
        .to_owned();
    put_rules(&td, "p", MAIL).await;
    let a = new_card(&td, "p", json!({"title": "a"})).await;
    settle(&td).await;
    emit(&td, "test.ping", "p", &a, json!({"n": 1, "who": "w1"})).await;
    emit(&td, "test.ping", "p", &a, json!({"n": 2})).await;
    settle(&td).await;
    let subject = format!("`{a}` needs a look");
    let sent: Vec<(String, String, String)> = state(&td)
        .db
        .read(move |c| {
            let mut st = c.prepare(
                "SELECT from_agent, to_agent, body FROM messages WHERE subject = ?1 ORDER BY created_at, id",
            )?;
            let rows = st
                .query_map([subject], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)))?
                .collect::<Result<Vec<_>, _>>()?;
            Ok(rows)
        })
        .await
        .unwrap();
    assert_eq!(sent.len(), 3, "{sent:?}");
    assert!(sent.iter().all(|m| m.0 == "project:p"), "{sent:?}");
    let mut first: Vec<&str> = sent
        .iter()
        .filter(|m| m.2 == "Seen `1`.")
        .map(|m| m.1.as_str())
        .collect();
    first.sort_unstable();
    let mut want = vec!["operator", w1.as_str()];
    want.sort_unstable();
    assert_eq!(first, want, "{sent:?}");
    let second: Vec<&str> = sent
        .iter()
        .filter(|m| m.2 == "Seen `2`.")
        .map(|m| m.1.as_str())
        .collect();
    assert_eq!(second, vec!["operator"], "{sent:?}");
}

// ------------------------------------------------------------- job verbs

const QUEUE: &str = r#"rule "validate" {
  on = ["test.ping"]
  when "data.n" { eq = "1" }
  job_queue { kind = "validate" }
}

rule "merge" {
  on = ["test.ping"]
  when "data.n" { eq = "2" }
  job_queue { kind = "merge" }
}

rule "batch" {
  on = ["test.ping"]
  when "data.n" { eq = "3" }
  job_queue { kind = "batch"  from = "@accepted"  limit = 2 }
}
"#;

#[tokio::test]
async fn job_queue_validates_merges_and_batches() {
    let td = queue_lab().await;
    put_rules(&td, "p", QUEUE).await;
    let a = new_card(
        &td,
        "p",
        json!({"title": "a", "column": "in_progress", "labels": ["by:w1"]}),
    )
    .await;
    let b = new_card(&td, "p", json!({"title": "b", "column": "review"})).await;
    let held = new_card(
        &td,
        "p",
        json!({"title": "From GitHub", "column": "accepted", "labels": ["github", "hold"]}),
    )
    .await;
    let c = new_card(&td, "p", json!({"title": "c", "column": "accepted"})).await;
    let d = new_card(&td, "p", json!({"title": "d", "column": "accepted"})).await;
    let e = new_card(&td, "p", json!({"title": "e", "column": "accepted"})).await;
    settle(&td).await;

    ping(&td, "p", &a, 1).await;
    settle(&td).await;
    let j = all_jobs(&td).await;
    assert_eq!(j.len(), 1, "{j:?}");
    assert_eq!(j[0].kind, JobKind::Validate);
    assert_eq!(j[0].card, a);
    assert_eq!(j[0].branch, a.to_lowercase());
    assert_eq!(j[0].by_agent.as_deref(), Some("w1"));
    assert_eq!(j[0].created_by, "rule:p/validate");
    // One open job per card: the second run is a no-op.
    ping(&td, "p", &a, 1).await;
    settle(&td).await;
    assert_eq!(
        states(&runs_of(&td, "p", "validate").await),
        vec!["done", "noop"]
    );

    ping(&td, "p", &b, 2).await;
    settle(&td).await;
    let j = all_jobs(&td).await;
    assert_eq!(j.len(), 2, "{j:?}");
    assert_eq!(j[1].kind, JobKind::Merge);
    assert_eq!(j[1].card, b);
    assert!(j[1].batch.is_empty());

    // The batch: board order, the held card left out, at most 2.
    ping(&td, "p", &c, 3).await;
    settle(&td).await;
    let j = all_jobs(&td).await;
    assert_eq!(j.len(), 3, "{j:?}");
    let cards: Vec<&str> = j[2].batch.iter().map(|x| x.card.as_str()).collect();
    assert_eq!(cards, vec![c.as_str(), d.as_str()]);
    let r = runs_of(&td, "p", "batch").await;
    assert_eq!(r[0]["state"], "done", "{r:?}");
    assert_eq!(r[0]["steps"][0]["detail"]["left_out"][0]["card"], held);
    assert!(
        state(&td)
            .db
            .read(move |x| jobs::open_of_card(x, &e))
            .await
            .unwrap()
            .is_none()
    );
}

const CANCEL: &str = r#"rule "stop" {
  on = ["test.ping"]
  job_cancel {}
}
"#;

#[tokio::test]
async fn job_cancel_cancels_the_open_job_of_the_card() {
    let td = TestDaemon::start().await;
    td.project("p").await;
    put_rules(&td, "p", CANCEL).await;
    let a = new_card(&td, "p", json!({"title": "a"})).await;
    let job = store_job(&td, JobKind::Validate, &[&a]).await;
    settle(&td).await;

    ping(&td, "p", &a, 1).await;
    settle(&td).await;
    let j = all_jobs(&td).await;
    assert_eq!(j[0].state.as_str(), "cancelled", "{j:?}");
    let evs = td
        .get(&format!("/api/cards/{a}/events?limit=50"))
        .await
        .ok()
        .clone();
    let evs = evs.as_array().or(evs["events"].as_array()).unwrap().clone();
    assert!(
        evs.iter().any(|e| e["kind"] == "job.verdict"
            && e["data"]["verdict"] == "cancelled"
            && e["data"]["job"] == job.id.as_str()),
        "{evs:?}"
    );
    ping(&td, "p", &a, 2).await;
    settle(&td).await;
    assert_eq!(
        states(&runs_of(&td, "p", "stop").await),
        vec!["done", "noop"]
    );
}

const RERUN: &str = r#"rule "again" {
  on = ["job.verdict"]
  job_rerun {}
}
"#;

#[tokio::test]
async fn job_rerun_runs_a_failed_job_once_more() {
    let td = TestDaemon::start().await;
    td.project("p").await;
    put_rules(&td, "p", RERUN).await;
    let a = new_card(&td, "p", json!({"title": "a"})).await;
    let first = store_job(&td, JobKind::Validate, &[&a]).await;
    let first = fail_job(&td, &first, "red").await;
    settle(&td).await;

    verdict(&td, &first, &a, "red").await;
    settle(&td).await;
    let j = all_jobs(&td).await;
    assert_eq!(j.len(), 2, "{j:?}");
    let second = j[1].clone();
    assert_eq!(second.attempt, 2);
    assert_eq!(second.state.as_str(), "queued");
    assert_eq!(second.created_at, first.created_at, "at the front");

    // The same verdict again: the rerun runs already.
    verdict(&td, &first, &a, "red").await;
    // The rerun fails too: attempt 2 of 2.
    let second = fail_job(&td, &second, "red").await;
    verdict(&td, &second, &a, "red").await;
    // A conflict never runs again.
    let other = new_card(&td, "p", json!({"title": "o"})).await;
    let conflict = store_job(&td, JobKind::Merge, &[&other]).await;
    let conflict = fail_job(&td, &conflict, "conflict").await;
    verdict(&td, &conflict, &other, "conflict").await;
    settle(&td).await;
    let r = runs_of(&td, "p", "again").await;
    assert_eq!(states(&r), vec!["done", "noop", "noop", "noop"], "{r:?}");
    let why: Vec<&str> = r
        .iter()
        .map(|r| r["steps"][0]["detail"]["reason"].as_str().unwrap_or(""))
        .collect();
    assert!(why[1].contains("again already"), "{why:?}");
    assert!(why[2].contains("attempt 2 of 2"), "{why:?}");
    assert!(why[3].contains("conflict"), "{why:?}");
    assert_eq!(all_jobs(&td).await.len(), 3);
}

const SPLIT: &str = r#"rule "halve" {
  on = ["job.verdict"]
  when "data.verdict" { eq = "red" }
  job_split {}
}
"#;

#[tokio::test]
async fn job_split_halves_a_red_batch_once() {
    let td = TestDaemon::start().await;
    td.project("p").await;
    put_rules(&td, "p", SPLIT).await;
    let a = new_card(&td, "p", json!({"title": "a"})).await;
    let b = new_card(&td, "p", json!({"title": "b"})).await;
    let c = new_card(&td, "p", json!({"title": "c"})).await;
    let red = store_job(&td, JobKind::Merge, &[&a, &b, &c]).await;
    let red = fail_job(&td, &red, "red").await;
    settle(&td).await;

    // One verdict for each card: one split.
    for x in [&a, &b, &c] {
        verdict(&td, &red, x, "red").await;
    }
    settle(&td).await;
    let halves: Vec<Job> = all_jobs(&td)
        .await
        .into_iter()
        .filter(|j| j.parent.as_deref() == Some(red.id.as_str()))
        .collect();
    assert_eq!(halves.len(), 2, "{halves:?}");
    let mut halves = halves;
    halves.sort_by(|x, y| x.split_path.cmp(&y.split_path));
    assert_eq!(halves[0].cards().len(), 1);
    assert_eq!(halves[0].card, a);
    let right: Vec<String> = halves[1].cards().into_iter().map(|x| x.card).collect();
    assert_eq!(right, vec![b.clone(), c.clone()]);
    let r = runs_of(&td, "p", "halve").await;
    let mut s = states(&r);
    s.sort_unstable();
    assert_eq!(s, vec!["done", "noop", "noop"], "{r:?}");

    // A red job of one card does not split.
    let d = new_card(&td, "p", json!({"title": "d"})).await;
    let one = store_job(&td, JobKind::Merge, &[&d]).await;
    let one = fail_job(&td, &one, "red").await;
    verdict(&td, &one, &d, "red").await;
    settle(&td).await;
    let r = runs_of(&td, "p", "halve").await;
    assert_eq!(r.last().unwrap()["state"], "noop");
}

const FIX: &str = r#"rule "fix" {
  on = ["job.verdict"]
  when "data.kind" { eq = "merge" }
  job_fix { labels = ["fix"] }
}
"#;

/// MEM-84: a red merge group gets one fix card, linked to its cards.
#[tokio::test]
async fn job_fix_opens_one_fix_card_for_a_red_merge_group() {
    let td = TestDaemon::start().await;
    td.project("p").await;
    put_rules(&td, "p", FIX).await;
    let a = new_card(&td, "p", json!({"title": "a"})).await;
    let b = new_card(&td, "p", json!({"title": "b"})).await;
    let red = store_job(&td, JobKind::Merge, &[&a, &b]).await;
    let red = fail_job(&td, &red, "red").await;
    settle(&td).await;

    verdict(&td, &red, &a, "red").await;
    verdict(&td, &red, &b, "red").await;
    settle(&td).await;
    // The two cards run in their own lanes: one opens the card.
    let r = runs_of(&td, "p", "fix").await;
    let mut s = states(&r);
    s.sort_unstable();
    assert_eq!(s, vec!["done", "noop"], "{r:?}");
    let done = r.iter().find(|r| r["state"] == "done").unwrap();
    let fix = done["steps"][0]["detail"]["card"]
        .as_str()
        .unwrap()
        .to_owned();
    let c = card(&td, &fix).await;
    assert_eq!(c["column"], "ready");
    let labels: Vec<String> = serde_json::from_value(c["labels"].clone()).unwrap();
    assert!(labels.contains(&format!("merge-fix:{}", red.id)), "{labels:?}");
    assert!(labels.contains(&"fix".to_owned()), "{labels:?}");
    let mut links: Vec<&str> = c["links"]
        .as_array()
        .unwrap()
        .iter()
        .filter(|l| l["kind"] == "card")
        .map(|l| l["ref"].as_str().unwrap())
        .collect();
    links.sort_unstable();
    let mut want = vec![a.as_str(), b.as_str()];
    want.sort_unstable();
    assert_eq!(links, want);
    let body = c["body"].as_str().unwrap();
    assert!(body.contains("Validation failed"), "{body}");
    assert!(body.contains(&format!("{a} {b}")), "{body}");
    assert_eq!(c["created_by"], "rule:p/fix");
}

// ----------------------------------------------------------- agent verbs

const AGENT: &str = r#"rule "park" {
  on = ["test.ping"]
  when "data.n" { eq = "1" }
  agent_park {}
}

rule "unpark" {
  on = ["test.ping"]
  when "data.n" { eq = "2" }
  agent_unpark { turn = "Back, ping {{event.data.n}}." }
}

rule "end" {
  on = ["test.ping"]
  when "data.n" { eq = "3" }
  agent_end { outcome = "done" }
}
"#;

async fn until<F: std::future::Future<Output = bool>>(what: &str, mut f: impl FnMut() -> F) {
    let start = std::time::Instant::now();
    while start.elapsed() < Duration::from_secs(60) {
        if f().await {
            return;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    panic!("timed out waiting for {what}");
}

/// `agent.park`, `agent.unpark` (with the rule's turn), and `agent.end`
/// on the agent bound to the card, with the fake supervisor.
#[tokio::test]
async fn agent_verbs_park_unpark_and_end_the_bound_agent() {
    let td = TestDaemon::start().await;
    let s = state(&td);
    s.lifecycle.set_tuning(terrariumd::lifecycle::Tuning {
        idle_stop: IdleStop::Off,
        tick: Duration::from_millis(20),
        compact_on_idle: false,
        compact_max_age: CompactMaxAge::Off,
        compact_wait: Duration::from_secs(5),
    });
    s.lifecycle.spawns.set_tuning(terrariumd::spawn::Tuning {
        done_delay: Duration::from_millis(50),
    });
    td.project("p").await;
    let r = td
        .post(
            "/api/templates",
            json!({"name": "w-claude", "project": "p", "spec": {"harness": "claude"}}),
        )
        .await;
    assert_eq!(r.status, 201, "{}", r.body);
    let lead = td
        .agent(
            "p",
            "lead",
            json!({"roles": ["project_admin"],
                   "policy": {"spawn": {"templates": ["project:*"], "max_concurrent": 2,
                                        "max_lifetime": "1h", "bound": true}}}),
        )
        .await["id"]
        .as_str()
        .unwrap()
        .to_owned();
    put_rules(&td, "p", AGENT).await;
    let a = new_card(&td, "p", json!({"title": "a", "column": "in_progress"})).await;
    let none = new_card(&td, "p", json!({"title": "no agent"})).await;
    let r = td
        .post_as(
            &lead,
            &format!("/api/agents/{lead}/spawn"),
            json!({"template": "w-claude", "task": "do it", "bind": format!("card:{a}")}),
        )
        .await;
    assert_eq!(r.status, 201, "{}", r.body);
    let eph = r.body["id"].as_str().unwrap().to_owned();
    td.wait_status(&eph, "idle").await;
    settle(&td).await;
    let row = |id: String| {
        let td = &td;
        async move { td.get(&format!("/api/spawns/{id}")).await.ok().clone() }
    };

    ping(&td, "p", &a, 1).await;
    settle(&td).await;
    let sp = row(eph.clone()).await;
    assert!(sp["parked_at"].is_string(), "{sp}");
    assert_eq!(sp["park_reason"], "rule park");
    // A card without a bound agent: a no-op.
    ping(&td, "p", &none, 1).await;
    settle(&td).await;
    assert_eq!(
        states(&runs_of(&td, "p", "park").await),
        vec!["done", "noop"]
    );

    ping(&td, "p", &a, 2).await;
    settle(&td).await;
    let sp = row(eph.clone()).await;
    assert!(sp["parked_at"].is_null(), "{sp}");
    let turns = td
        .get(&format!("/api/agents/{eph}/events?kind=agent.unparked"))
        .await
        .ok()
        .clone();
    assert_eq!(turns.as_array().unwrap().len(), 1, "{turns}");

    ping(&td, "p", &a, 3).await;
    settle(&td).await;
    let key = eph.clone();
    until("the close", || {
        let key = key.clone();
        let row = &row;
        async move { row(key).await["finished_at"].is_string() }
    })
    .await;
    assert_eq!(row(eph.clone()).await["outcome"], "done");
}

// ---------------------------------------------------- held card, rights

const HELD: &str = r#"rule "h-move" {
  on = ["test.ping"]
  card_move { to = "ready" }
}
rule "h-label" {
  on = ["test.ping"]
  card_label { add = ["x"] }
}
rule "h-comment" {
  on = ["test.ping"]
  card_comment { text = "x" }
}
rule "h-assign" {
  on = ["test.ping"]
  card_assign { who = "none" }
}
rule "h-prepend" {
  on = ["test.ping"]
  card_prepend { text = "x" }
}
rule "h-mail" {
  on = ["test.ping"]
  mail { to = ["operator"]  subject = "x" }
}
rule "h-queue" {
  on = ["test.ping"]
  job_queue { kind = "validate" }
}
rule "h-cancel" {
  on = ["test.ping"]
  job_cancel {}
}
rule "h-rerun" {
  on = ["test.ping"]
  job_rerun {}
}
rule "h-split" {
  on = ["test.ping"]
  job_split {}
}
rule "h-fix" {
  on = ["test.ping"]
  job_fix {}
}
rule "h-park" {
  on = ["test.ping"]
  agent_park {}
}
rule "h-unpark" {
  on = ["test.ping"]
  agent_unpark {}
}
rule "h-end" {
  on = ["test.ping"]
  agent_end { outcome = "cancelled" }
}
"#;

/// The GitHub author gate (TER-85): every verb refuses a held card, and
/// the card does not change.
#[tokio::test]
async fn every_verb_refuses_a_held_card() {
    let td = TestDaemon::start().await;
    td.project("p").await;
    put_rules(&td, "p", HELD).await;
    let held = new_card(
        &td,
        "p",
        json!({"title": "From GitHub", "labels": ["github", "hold"]}),
    )
    .await;
    settle(&td).await;
    let before = card(&td, &held).await;
    ping(&td, "p", &held, 1).await;
    settle(&td).await;

    let r = runs(&td, "p").await;
    let verbs: Vec<&str> = r
        .iter()
        .map(|r| r["steps"][0]["verb"].as_str().unwrap())
        .collect();
    let mut want: Vec<&str> = VERBS.iter().map(|v| v.name).collect();
    let mut got = verbs.clone();
    want.sort_unstable();
    got.sort_unstable();
    assert_eq!(got, want, "one run for each verb");
    for run in &r {
        assert_eq!(run["state"], "failed", "{run}");
        let reason = run["reason"].as_str().unwrap();
        assert!(reason.contains("held by the GitHub author gate"), "{run}");
    }
    let after = card(&td, &held).await;
    assert_eq!(after["column"], before["column"]);
    assert_eq!(after["labels"], before["labels"]);
    assert_eq!(after["body"], before["body"]);
    assert!(bodies(&after).is_empty());
    assert!(all_jobs(&td).await.is_empty());
}

/// A rule actor without `project_admin` cannot queue a merge; the rule
/// actor (capped at `project_admin`) can.
#[tokio::test]
async fn a_rule_without_admin_rights_cannot_queue_a_merge() {
    let td = queue_lab().await;
    let b = new_card(&td, "p", json!({"title": "b", "column": "accepted"})).await;
    let seq = ping(&td, "p", &b, 1).await;
    let mut args = Map::new();
    args.insert("kind".into(), json!("merge"));
    let action = Action {
        verb: "job.queue".into(),
        args,
    };
    let weak = Actor::Agent(AgentActor {
        id: rules_engine::actor_label("p", "weak"),
        name: rules_engine::actor_label("p", "weak"),
        project: "p".into(),
        roles: Vec::new(),
        policy: Policy::default(),
    });
    let (card_id, act) = (b.clone(), action.clone());
    let e = state(&td)
        .db
        .write(rules_engine::actor_label("p", "weak"), move |t| {
            rules_engine::try_verb(t, &weak, "p", "weak", seq, Some(&card_id), &act)
        })
        .await
        .unwrap_err();
    assert!(matches!(e.code, Code::Forbidden), "{e:?}");
    assert!(e.message.contains("project_admin"), "{e:?}");
    assert!(all_jobs(&td).await.is_empty());

    let (card_id, act) = (b.clone(), action);
    let done = state(&td)
        .db
        .write(rules_engine::actor_label("p", "lead"), move |t| {
            let actor = rules_engine::rule_actor("p", "lead");
            rules_engine::try_verb(t, &actor, "p", "lead", seq, Some(&card_id), &act)
        })
        .await
        .unwrap();
    assert!(done.changed);
    let j = all_jobs(&td).await;
    assert_eq!(j.len(), 1);
    assert_eq!(j[0].kind, JobKind::Merge);
}
