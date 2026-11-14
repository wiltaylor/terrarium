//! The conversion of the boards to column rules (TER-1117, kanban rework
//! C5, docs/api.md §19.14): one parity test per board kind (a `rules`
//! board and a `code` board run the same scenario before and after the
//! conversion, with the same moves, labels, comments, groups, merges and
//! version cut, and after it every write comes from a column rule), the
//! dispatchers and the merge queue rules, the dry run, the second run, the
//! revert, the upgrade at the start, and the standard board.
#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::time::Duration;

use serde_json::{Value, json};
use terrariumd::api::Shared;
use terrariumd::rules_engine;
use terrariumd::store::jobs::{self, BatchCard, Job, JobEnd, JobKind, NewJob};

use crate::common::{self, TestDaemon};

fn state(td: &TestDaemon) -> Shared {
    td.daemon.as_ref().unwrap().state.clone()
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
    panic!("the rule engine did not settle: {:?}", engine_state(td).await);
}

/// Project `p` with repo `app`, a build target and the merge queue set,
/// on `flow` (`code` or `rules`).
async fn lab_with(td: &TestDaemon, flow: &str) {
    td.project("p").await;
    let r = td
        .post(
            "/api/projects/p/repos",
            json!({"name": "app", "url": common::branch_remote()}),
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
    if flow == "rules" {
        let r = td
            .patch("/api/projects/p", json!({"board_flow": "rules"}))
            .await;
        assert_eq!(r.status, 200, "{}", r.body);
    }
}

async fn lab(flow: &str) -> TestDaemon {
    let td = TestDaemon::start().await;
    lab_with(&td, flow).await;
    td
}

async fn new_card(td: &TestDaemon, body: Value) -> String {
    let r = td.post("/api/projects/p/cards", body).await;
    assert_eq!(r.status, 201, "{}", r.body);
    r.body["id"].as_str().unwrap().to_owned()
}

async fn card(td: &TestDaemon, id: &str) -> Value {
    td.get(&format!("/api/cards/{id}")).await.ok().clone()
}

/// A merge job of `cards` (a batch for two or more), queued as the
/// operator, as `card merge` does.
async fn merge(td: &TestDaemon, cards: &[&str]) -> Job {
    let batch: Vec<BatchCard> = if cards.len() > 1 {
        cards
            .iter()
            .map(|c| BatchCard {
                card: (*c).to_owned(),
                branch: c.to_lowercase(),
                by_agent: None,
                commit: None,
                outcome: None,
                note: None,
            })
            .collect()
    } else {
        Vec::new()
    };
    let first = cards[0].to_owned();
    let j = state(td)
        .db
        .write("operator", move |t| {
            jobs::create(
                t,
                NewJob {
                    project: "p".into(),
                    branch: first.to_lowercase(),
                    card: first,
                    kind: JobKind::Merge,
                    repo: "app".into(),
                    commit: None,
                    by_agent: None,
                    batch,
                    planned_secs: None,
                },
            )
        })
        .await
        .unwrap();
    settle(td).await;
    j
}

/// Start `job` and end it through the runner's end path (`jobs::on_end`),
/// so the coded flow of a `code` board acts: passed, or red.
async fn end_job(td: &TestDaemon, job: &Job, passed: bool) {
    let st = state(td);
    let id = job.id.clone();
    let started = st
        .db
        .write("system", move |t| jobs::start(t, &id, "machine", None))
        .await
        .unwrap();
    assert!(started, "{job:?}");
    let end = if passed {
        JobEnd {
            passed: true,
            exit_code: Some(0),
            ..JobEnd::default()
        }
    } else {
        JobEnd {
            passed: false,
            outcome: Some("red".into()),
            exit_code: Some(1),
            tail: Some("test a ... FAILED\nfailures: a".into()),
            ..JobEnd::default()
        }
    };
    terrariumd::jobs::finish_for_test(&st, &job.id, end).await;
    settle(td).await;
}

async fn open_job(td: &TestDaemon) -> Job {
    let open: Vec<Job> = state(td)
        .db
        .read(|c| jobs::list(c, "p", None))
        .await
        .unwrap()
        .into_iter()
        .filter(|j| j.state.is_open())
        .collect();
    assert_eq!(open.len(), 1, "{open:?}");
    open[0].clone()
}

/// What a card shows after the scenario, without IDs and authors: its
/// column, its labels, the kind of each comment, the start of its body,
/// and the kind of its group.
async fn trace(td: &TestDaemon, id: &str) -> Value {
    let c = card(td, id).await;
    let mut labels: Vec<String> = c["labels"]
        .as_array()
        .unwrap()
        .iter()
        .map(|l| l.as_str().unwrap().to_owned())
        .collect();
    labels.sort();
    let kinds: Vec<&str> = c["comments"]
        .as_array()
        .unwrap()
        .iter()
        .map(|c| {
            let b = c["body"].as_str().unwrap();
            if b.contains("The group gets a fix card") {
                "fix"
            } else if b.contains("runs once more") || b.contains("runs again") {
                "rerun"
            } else if b.contains(" queued") {
                "queued"
            } else if b.contains(" passed") {
                "passed"
            } else if b.contains(" failed") {
                "failed"
            } else {
                "other"
            }
        })
        .collect();
    let group = c["group"]["key"]
        .as_str()
        .map(|k| k.split(':').next().unwrap().to_owned());
    let body = c["body"].as_str().unwrap_or_default();
    json!({"column": c["column"], "assignee": c["assignee"], "labels": labels,
           "comments": kinds, "failure_block": body.starts_with('>') || body.contains("FAILED"),
           "group": group})
}

/// The scenario of the parity tests: two cards merge as one batch and
/// pass; four more pass (six accepted: the cut of a `rules` board); one
/// card is red twice (a rerun, then the failure back to ready).
async fn scenario(td: &TestDaemon) -> Vec<Value> {
    let mut ids = Vec::new();
    for t in ["a", "b", "c", "d", "e", "f", "g"] {
        ids.push(
            new_card(
                td,
                json!({"title": t, "column": "review", "repo": "app",
                       "labels": ["version:vnext"]}),
            )
            .await,
        );
    }
    settle(td).await;
    let j = merge(td, &[&ids[0], &ids[1]]).await;
    end_job(td, &j, true).await;
    let j = merge(td, &[&ids[2], &ids[3], &ids[4], &ids[5]]).await;
    end_job(td, &j, true).await;
    let j = merge(td, &[&ids[6]]).await;
    end_job(td, &j, false).await;
    let again = open_job(td).await;
    assert_ne!(again.id, j.id, "a rerun");
    end_job(td, &again, false).await;
    let mut out = Vec::new();
    for id in &ids {
        out.push(trace(td, id).await);
    }
    out
}

async fn convert(td: &TestDaemon) -> Value {
    let r = td
        .post("/api/projects/p/board/convert", json!({"action": "apply"}))
        .await;
    assert_eq!(r.status, 200, "{}", r.body);
    r.body.clone()
}

/// Every write of every card after the conversion came from the test (the
/// operator) or a column rule (`rule:p/<column>/<rule>`): none from the
/// coded flow, the pack or `system`.
async fn only_column_rule_writes(td: &TestDaemon) {
    let rows: Vec<(String, String, String)> = state(td)
        .db
        .read(|c| {
            let mut st = c.prepare(
                "SELECT COALESCE(card, ''), COALESCE(actor, ''),
                        COALESCE(json_extract(payload, '$.change'), '')
                   FROM events WHERE kind = 'card' AND project = 'p' ORDER BY seq",
            )?;
            let v = st
                .query_map([], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)))?
                .collect::<Result<Vec<_>, _>>()?;
            Ok(v)
        })
        .await
        .unwrap();
    let mut seen = 0;
    for (card, actor, change) in rows {
        if !matches!(change.as_str(), "moved" | "commented" | "edited" | "assigned") {
            continue;
        }
        seen += 1;
        let column_rule = actor
            .strip_prefix("rule:p/")
            .is_some_and(|rest| rest.matches('/').count() == 1);
        assert!(
            actor == "operator" || column_rule,
            "{card}: {change} by {actor}"
        );
    }
    assert!(seen > 0);
}

async fn assert_no_legacy_runs(td: &TestDaemon) {
    let r = td.get("/api/projects/p/rules/runs?limit=500").await;
    for run in r.ok()["runs"].as_array().unwrap() {
        assert!(run["column"].is_string(), "a legacy rule ran: {run}");
    }
}

#[tokio::test]
async fn a_converted_rules_board_does_what_the_pack_did() {
    let before = lab("rules").await;
    let want = scenario(&before).await;
    // The pack cut a version at six accepted cards.
    assert_eq!(want[0]["group"], "version", "{want:?}");
    assert_eq!(want[6]["column"], "ready", "{want:?}");

    let after = lab("rules").await;
    let out = convert(&after).await;
    assert_eq!(out["result"], "converted", "{out}");
    let p = after.get("/api/projects/p").await.ok().clone();
    assert_eq!(p["board_flow"], "columns");
    let got = scenario(&after).await;
    assert_eq!(got, want);
    only_column_rule_writes(&after).await;
    assert_no_legacy_runs(&after).await;
}

#[tokio::test]
async fn a_converted_code_board_does_what_the_coded_flow_did() {
    let before = lab("code").await;
    let want = scenario(&before).await;
    // No cut on a code board: its lead cuts by hand.
    assert!(want[0]["group"].is_null(), "{want:?}");
    assert_eq!(want[6]["column"], "ready", "{want:?}");

    let after = lab("code").await;
    let out = convert(&after).await;
    assert_eq!(out["result"], "converted", "{out}");
    let report = out["plan"]["report"].as_array().unwrap();
    for skipped in ["version-cut", "triage-hourly"] {
        assert!(
            report.iter().all(|e| e["rule"] != skipped),
            "{skipped} is not a rule of the coded flow: {out}"
        );
    }
    // The validating column keeps the coded flow's validate move.
    assert!(report.iter().any(|e| e["rule"] == "validate-queued-move"), "{out}");
    let got = scenario(&after).await;
    assert_eq!(got, want);
    only_column_rule_writes(&after).await;
    assert_no_legacy_runs(&after).await;
}

#[tokio::test]
async fn the_conversion_is_a_dry_run_first_then_once_then_reverts() {
    let td = lab("rules").await;
    let dry = td.get("/api/projects/p/board/conversion").await.ok().clone();
    assert!(dry["conversion"].is_null(), "{dry}");
    let planned: usize = dry["plan"]["columns"]
        .as_array()
        .unwrap()
        .iter()
        .map(|c| c["rules"].as_array().unwrap().len())
        .sum();
    assert!(planned > 0, "{dry}");
    // A dry run writes nothing.
    let p = td.get("/api/projects/p").await.ok().clone();
    assert_eq!(p["board_flow"], "rules");

    let out = convert(&td).await;
    assert_eq!(out["rules"].as_u64().unwrap() as usize, planned, "{out}");
    let again = convert(&td).await;
    assert_eq!(again["result"], "skipped", "{again}");
    let rec = td.get("/api/projects/p/board/conversion").await.ok().clone();
    assert_eq!(rec["conversion"]["previous_flow"], "rules", "{rec}");
    assert!(rec["plan"].is_null(), "{rec}");

    // Only the operator converts.
    td.agent("p", "lead", json!({"roles": ["project_admin"]})).await;
    let r = td
        .post_as("lead", "/api/projects/p/board/convert", json!({"action": "revert"}))
        .await;
    assert_eq!(r.status, 403, "{}", r.body);

    let r = td
        .post("/api/projects/p/board/convert", json!({"action": "revert"}))
        .await;
    assert_eq!(r.status, 200, "{}", r.body);
    assert_eq!(r.body["rules"].as_u64().unwrap() as usize, planned);
    let p = td.get("/api/projects/p").await.ok().clone();
    assert_eq!(p["board_flow"], "rules");
    let opts = state(&td)
        .db
        .read(|c| terrariumd::board_convert::options_of(c, "p"))
        .await
        .unwrap();
    assert!(opts.iter().all(|o| o.rules.is_empty()), "{opts:?}");
    let r = td
        .post("/api/projects/p/board/convert", json!({"action": "revert"}))
        .await;
    assert_eq!(r.status, 404, "{}", r.body);
}

#[tokio::test]
async fn the_dispatchers_and_the_merge_queue_rules_become_column_rules() {
    let td = lab("rules").await;
    td.agent("p", "lead", json!({"roles": ["project_admin"]})).await;
    let r = td
        .patch(
            "/api/projects/p",
            json!({"card_dispatcher": "lead", "review_dispatcher": "lead"}),
        )
        .await;
    assert_eq!(r.status, 200, "{}", r.body);
    let r = td
        .put(
            "/api/projects/p/rules",
            json!({"text": terrarium_core::rules_file::MERGE_QUEUE_RULES}),
        )
        .await;
    assert_eq!(r.status, 200, "{}", r.body);
    let out = convert(&td).await;
    assert_eq!(out["result"], "converted", "{out}");
    let opt = |c: &'static str| {
        let td = &td;
        async move {
            td.get(&format!("/api/projects/p/board/columns/{c}/options"))
                .await
                .ok()
                .clone()
        }
    };
    let ready = opt("ready").await;
    let dispatch = ready["rules"]
        .as_array()
        .unwrap()
        .iter()
        .find(|r| r["name"] == "dispatch")
        .unwrap()
        .clone();
    assert_eq!(dispatch["actions"][0]["verb"], "card.assign");
    assert_eq!(dispatch["actions"][0]["args"]["owner"], "lead");
    let review = opt("review").await;
    let names: Vec<&str> = review["rules"]
        .as_array()
        .unwrap()
        .iter()
        .map(|r| r["name"].as_str().unwrap())
        .collect();
    assert!(names.contains(&"review-start") && names.contains(&"review-start-timer"), "{names:?}");
    let mq = opt("merge_queue").await;
    assert_eq!(
        mq["queue"],
        json!({"release": "batch", "batch_size": 4, "max_wait_seconds": 1800,
               "max_batch_cards": 8})
    );
    assert!(
        mq["rules"]
            .as_array()
            .unwrap()
            .iter()
            .any(|r| r["name"] == "merge-queue-batch" && r["trigger"]["kind"] == "lands")
    );
    // The settings have no effect on a columns board.
    let p = td.get("/api/projects/p").await.ok().clone();
    assert_eq!(p["board_flow"], "columns");
}

#[tokio::test]
async fn the_upgrade_converts_every_board_once_at_the_start_and_mails_the_report() {
    let mut td = TestDaemon::start().await;
    lab_with(&td, "rules").await;
    td.project("q").await;
    td.agent("p", "lead", json!({"roles": ["project_admin"]})).await;
    td.web.board_columns = true;
    td.boot(None, None).await;
    for name in ["p", "q"] {
        let p = td.get(&format!("/api/projects/{name}")).await.ok().clone();
        assert_eq!(p["board_flow"], "columns", "{name}: {p}");
    }
    let mail = td.mail_of("lead").await;
    assert!(
        mail.iter()
            .any(|m| m["subject"] == "p: the board runs on column rules"),
        "{mail:?}"
    );
    let count = |td: &TestDaemon| {
        let db = state(td).db.clone();
        async move {
            db.read(|c| {
                Ok(c.query_row(
                    "SELECT count(*) FROM audit WHERE action = 'board.convert'",
                    [],
                    |r| r.get::<_, i64>(0),
                )?)
            })
            .await
            .unwrap()
        }
    };
    assert_eq!(count(&td).await, 2);
    td.boot(None, None).await;
    assert_eq!(count(&td).await, 2, "a second start converts nothing");
}

#[tokio::test]
async fn a_new_project_gets_the_standard_board_on_column_rules() {
    let mut td = TestDaemon::start().await;
    td.web.board_columns = true;
    td.boot(None, None).await;
    td.project("n").await;
    let p = td.get("/api/projects/n").await.ok().clone();
    assert_eq!(p["board_flow"], "columns", "{p}");
    let b = td.get("/api/projects/n/board").await.ok().clone();
    let cols: Vec<(&str, &Value, &Value)> = b["columns"]
        .as_array()
        .unwrap()
        .iter()
        .map(|c| (c["name"].as_str().unwrap(), &c["role"], &c["wip_limit"]))
        .collect();
    let names: Vec<&str> = cols.iter().map(|c| c.0).collect();
    assert_eq!(
        names,
        vec![
            "backlog", "research", "ready", "in_progress", "review", "merge_queue", "accepted",
            "blocked", "done"
        ]
    );
    assert_eq!(*cols[1].2, json!(2));
    let opts = td
        .get("/api/projects/n/board/columns/merge_queue/options")
        .await
        .ok()
        .clone();
    assert_eq!(opts["queue"]["release"], "batch", "{opts}");
    let backlog = td
        .get("/api/projects/n/board/columns/backlog/options")
        .await
        .ok()
        .clone();
    assert!(
        backlog["rules"]
            .as_array()
            .unwrap()
            .iter()
            .any(|r| r["name"] == "triage-hourly"),
        "{backlog}"
    );
    let acc = td
        .get("/api/projects/n/board/columns/accepted/options")
        .await
        .ok()
        .clone();
    assert!(
        acc["rules"].as_array().unwrap().iter().any(|r| r["name"] == "version-cut-timer"),
        "{acc}"
    );
}
