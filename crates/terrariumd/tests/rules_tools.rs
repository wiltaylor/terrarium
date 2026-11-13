//! The tools of the board rules (TER-171, docs/api.md §19.12): the
//! migration of a project to `board_flow = rules` on a board without the
//! columns validating and accepted, the lint, the effective rules, one rule
//! on or off, the dry run with the pack, and the rights.
#![allow(clippy::unwrap_used, clippy::expect_used)]

use crate::common;

use std::time::Duration;

use common::TestDaemon;
use serde_json::{Value, json};
use terrariumd::api::Shared;
use terrariumd::events::EventDraft;
use terrariumd::rules_engine;
use terrariumd::store::jobs::{self, Job, JobEnd, JobKind, NewJob};

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
    panic!(
        "the rule engine did not settle: {:?}",
        engine_state(td).await
    );
}

/// The audit rows of `action`, newest first.
async fn audit_rows(td: &TestDaemon, action: &str) -> Vec<Value> {
    let r = td.get("/api/audit?order=desc&limit=200").await;
    r.ok()
        .as_array()
        .unwrap()
        .iter()
        .filter(|e| e["action"] == action)
        .cloned()
        .collect()
}

/// Project `p` with a repo `app` in `next_repos`, and a board without the
/// columns validating and accepted (the board of a project made before
/// the column roles, TER-165).
async fn lab() -> TestDaemon {
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
    let cols: Vec<Value> = [
        "backlog",
        "ready",
        "in_progress",
        "review",
        "blocked",
        "done",
    ]
    .iter()
    .map(|n| json!({"name": n}))
    .collect();
    let r = td
        .patch("/api/projects/p/board", json!({"columns": cols}))
        .await;
    assert_eq!(r.status, 200, "{}", r.body);
    td
}

/// A merge job of `card`, started and passed.
async fn passed_merge(td: &TestDaemon, card: &str) -> Job {
    let card = card.to_owned();
    state(td)
        .db
        .write("operator", move |t| {
            let job = jobs::create(
                t,
                NewJob {
                    project: "p".into(),
                    branch: card.to_lowercase(),
                    card,
                    kind: JobKind::Merge,
                    repo: "app".into(),
                    commit: None,
                    by_agent: None,
                    batch: Vec::new(),
                    planned_secs: None,
                },
            )?;
            jobs::start(t, &job.id, "machine", None)?;
            let end = JobEnd {
                passed: true,
                exit_code: Some(0),
                ..JobEnd::default()
            };
            Ok(jobs::finish(t, &job.id, &end)?.unwrap())
        })
        .await
        .unwrap()
}

/// The `job.verdict` of `job` for `card`, as the job queue writes it.
async fn verdict(td: &TestDaemon, job: &Job, card: &str, verdict: &str) -> i64 {
    let (project, data) = (
        job.project.clone(),
        json!({"project": job.project, "job": job.id, "card": card, "kind": job.kind,
               "verdict": verdict, "attempt": job.attempt}),
    );
    let card = card.to_owned();
    state(td)
        .db
        .write("source:test/jobs", move |t| {
            Ok(t.emit(
                EventDraft::new("job.verdict", "job.verdict", data)
                    .project(project)
                    .card(card),
            )?
            .seq)
        })
        .await
        .unwrap()
}

fn change_of<'a>(body: &'a Value, rule: &str) -> Option<&'a Value> {
    body["changes"]
        .as_array()
        .unwrap()
        .iter()
        .find(|c| c["rule"] == rule)
}

fn codes(body: &Value) -> Vec<(String, String)> {
    body["findings"]
        .as_array()
        .unwrap()
        .iter()
        .map(|f| {
            (
                f["code"].as_str().unwrap().to_owned(),
                f["column"].as_str().unwrap().to_owned(),
            )
        })
        .collect()
}

#[tokio::test]
async fn migration_on_a_board_without_validating_and_accepted() {
    let td = lab().await;

    // The plan writes nothing.
    let plan = td
        .post("/api/projects/p/rules/migrate", json!({}))
        .await
        .ok()
        .clone();
    assert_eq!(plan["applied"], false);
    assert_eq!(plan["board_flow"], "code");
    assert_eq!(plan["changed"], true);
    assert_eq!(change_of(&plan, "merge-pass").unwrap()["change"], "mapped");
    assert_eq!(change_of(&plan, "agent-park").unwrap()["change"], "mapped");
    assert!(change_of(&plan, "validate-pass").is_none());
    assert!(
        codes(&plan).iter().all(|(c, _)| c != "missing_column"),
        "{plan}"
    );
    let r = td.get("/api/projects/p/rules").await.ok().clone();
    assert_eq!(r["exists"], false);
    assert_eq!(td.get("/api/projects/p").await.ok()["board_flow"], "code");

    // The pack alone, without the migration, would fail merge-pass here.
    let unmapped = td
        .post(
            "/api/projects/p/rules/check",
            json!({"text": terrarium_core::rules_file::DEFAULT_PACK}),
        )
        .await;
    unmapped.err(422, "invalid");

    // Apply: the overrides and the switch in one write, with the audit.
    let done = td
        .post("/api/projects/p/rules/migrate", json!({"apply": true}))
        .await
        .ok()
        .clone();
    assert_eq!(done["applied"], true);
    assert_eq!(done["board_flow"], "rules");
    assert_eq!(done["text"], plan["text"]);
    let project = td.get("/api/projects/p").await.ok().clone();
    assert_eq!(project["board_flow"], "rules");
    let r = td.get("/api/projects/p/rules").await.ok().clone();
    assert_eq!(r["text"], plan["text"]);
    assert_eq!(r["hash"], done["hash"]);
    let rows = audit_rows(&td, "rules.migrate").await;
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0]["detail"]["previous_board_flow"], "code");

    // The rules that run: the pack, with the overrides in its order.
    let eff = td.get("/api/projects/p/rules/effective").await.ok().clone();
    assert_eq!(eff["board_flow"], "rules");
    let rules = eff["rules"].as_array().unwrap();
    let origin = |n: &str| {
        rules
            .iter()
            .find(|r| r["name"] == n)
            .map(|r| r["origin"].as_str().unwrap().to_owned())
    };
    assert_eq!(origin("merge-pass").as_deref(), Some("override"));
    assert_eq!(origin("job-fail-red").as_deref(), Some("pack"));
    assert!(
        rules
            .iter()
            .all(|r| r["wcl"].as_str().unwrap().starts_with("rule \""))
    );

    // The lint of the stored rules: no missing column.
    let lint = td
        .post("/api/projects/p/rules/lint", json!({}))
        .await
        .ok()
        .clone();
    assert!(
        codes(&lint).iter().all(|(c, _)| c != "missing_column"),
        "{lint}"
    );

    // A second migration changes nothing.
    let again = td
        .post("/api/projects/p/rules/migrate", json!({"apply": true}))
        .await
        .ok()
        .clone();
    assert_eq!(again["changed"], false);
    assert_eq!(again["hash"], done["hash"]);

    // A passed merge: the mapped merge-pass takes the labels off, writes
    // the end comment, and frees the card, which stays in review (the
    // board has no accepted column). The run is done, not failed.
    let r = td
        .post(
            "/api/projects/p/cards",
            json!({"title": "c", "column": "review", "labels": ["validation:running"]}),
        )
        .await;
    assert_eq!(r.status, 201, "{}", r.body);
    let card = r.body["id"].as_str().unwrap().to_owned();
    let job = passed_merge(&td, &card).await;
    settle(&td).await;
    verdict(&td, &job, &card, "passed").await;
    settle(&td).await;
    let c = td.get(&format!("/api/cards/{card}")).await.ok().clone();
    assert_eq!(c["column"], "review");
    let labels: Vec<&str> = c["labels"]
        .as_array()
        .unwrap()
        .iter()
        .map(|l| l.as_str().unwrap())
        .collect();
    assert!(!labels.iter().any(|l| l.starts_with("validation:")), "{c}");
    let comments: Vec<&str> = c["comments"]
        .as_array()
        .unwrap()
        .iter()
        .map(|x| x["body"].as_str().unwrap())
        .collect();
    assert!(
        comments.iter().any(|b| b.contains(job.id.as_str())),
        "{comments:?}"
    );
    let runs = td
        .get("/api/projects/p/rules/runs?rule=merge-pass")
        .await
        .ok()
        .clone();
    let states: Vec<&str> = runs["runs"]
        .as_array()
        .unwrap()
        .iter()
        .map(|r| r["state"].as_str().unwrap())
        .collect();
    assert_eq!(states, vec!["done"], "{runs}");
}

#[tokio::test]
async fn lint_finds_a_missing_and_a_stuck_column() {
    let td = lab().await;
    // Switched by hand, without the migration: the pack names accepted.
    td.patch("/api/projects/p", json!({"board_flow": "rules"}))
        .await
        .ok();
    let lint = td
        .post("/api/projects/p/rules/lint", json!({}))
        .await
        .ok()
        .clone();
    assert!(
        codes(&lint).contains(&("missing_column".into(), "@accepted".into())),
        "{lint}"
    );
    // With the pack's passes off, the failures move cards into ready and
    // no rule moves one out (to-blocked runs only in in_progress): ready
    // is stuck. The blocked column is left out by design.
    let text = r#"rule "validate-pass" { enabled = false }
rule "merge-pass" { enabled = false }
rule "to-blocked" {
  on = ["test.ping"]
  if_card { column = ["in_progress"] }
  card_move { to = "blocked"  comment = "x" }
}
"#;
    let lint = td
        .post("/api/projects/p/rules/lint", json!({"text": text}))
        .await
        .ok()
        .clone();
    assert_eq!(lint["source"], "text");
    let c = codes(&lint);
    assert!(
        c.contains(&("stuck_column".into(), "ready".into())),
        "{lint}"
    );
    assert!(!c.iter().any(|(_, col)| col == "blocked"), "{lint}");
    // A bad text is 422.
    td.post(
        "/api/projects/p/rules/lint",
        json!({"text": "rule \"x\" {"}),
    )
    .await
    .err(422, "invalid");
}

#[tokio::test]
async fn enable_and_disable_edit_the_file() {
    let td = lab().await;
    td.post("/api/projects/p/rules/migrate", json!({"apply": true}))
        .await
        .ok();
    // A pack rule that the file does not name: off appends the short form.
    let r = td
        .post("/api/projects/p/rules/job-cancelled/disable", json!({}))
        .await
        .ok()
        .clone();
    assert_eq!(r["changed"], true);
    let text = td.get("/api/projects/p/rules").await.ok()["text"]
        .as_str()
        .unwrap()
        .to_owned();
    assert!(
        text.ends_with("rule \"job-cancelled\" { enabled = false }\n"),
        "{text}"
    );
    let eff = td.get("/api/projects/p/rules/effective").await.ok().clone();
    let jc = eff["rules"]
        .as_array()
        .unwrap()
        .iter()
        .find(|r| r["name"] == "job-cancelled")
        .unwrap()
        .clone();
    assert_eq!(jc["enabled"], false);
    assert_eq!(jc["origin"], "override");
    // Off again: no change.
    let r = td
        .post("/api/projects/p/rules/job-cancelled/disable", json!({}))
        .await
        .ok()
        .clone();
    assert_eq!(r["changed"], false);
    // On: the short form goes, the pack rule runs again.
    td.post("/api/projects/p/rules/job-cancelled/enable", json!({}))
        .await
        .ok();
    let after = td.get("/api/projects/p/rules").await.ok()["text"]
        .as_str()
        .unwrap()
        .to_owned();
    assert!(!after.contains("job-cancelled"), "{after}");
    // A mapped override: off sets its field.
    td.post("/api/projects/p/rules/merge-pass/disable", json!({}))
        .await
        .ok();
    let r = td.get("/api/projects/p/rules").await.ok().clone();
    let mp = r["rules"]
        .as_array()
        .unwrap()
        .iter()
        .find(|x| x["name"] == "merge-pass")
        .unwrap()
        .clone();
    assert_eq!(mp["enabled"], false);
    assert!(!mp["actions"].as_array().unwrap().is_empty());
    // Unknown rule: 404. Each change is a rules.put in the audit.
    td.post("/api/projects/p/rules/nope/disable", json!({}))
        .await
        .err(404, "not_found");
    assert_eq!(audit_rows(&td, "rules.put").await.len(), 4);
    // The answer has the lint of the rules that run after the change: an
    // enable of the short form of merge-pass lets the unmapped pack rule
    // run, which names the missing @accepted (TER-226).
    td.put(
        "/api/projects/p/rules",
        json!({"text": "rule \"merge-pass\" { enabled = false }\n"}),
    )
    .await
    .ok();
    let r = td
        .post("/api/projects/p/rules/merge-pass/enable", json!({}))
        .await
        .ok()
        .clone();
    assert_eq!(r["changed"], true);
    assert!(
        codes(&r).contains(&("missing_column".into(), "@accepted".into())),
        "{r}"
    );
    let r = td
        .post("/api/projects/p/rules/merge-pass/enable", json!({}))
        .await
        .ok()
        .clone();
    assert_eq!(r["changed"], false);
    assert!(r["findings"].is_array(), "{r}");
}

#[tokio::test]
async fn the_dry_run_plans_the_pack_of_a_rules_project() {
    let td = lab().await;
    // Code flow, no rules: nothing to plan.
    td.post("/api/projects/p/rules/dry-run", json!({"from": 1}))
        .await
        .err(404, "not_found");
    td.post("/api/projects/p/rules/migrate", json!({"apply": true}))
        .await
        .ok();
    let r = td
        .post(
            "/api/projects/p/cards",
            json!({"title": "c", "column": "in_progress"}),
        )
        .await;
    let card = r.body["id"].as_str().unwrap().to_owned();
    let mv = td
        .post(
            &format!("/api/cards/{card}/move"),
            json!({"column": "review"}),
        )
        .await;
    assert_eq!(mv.status, 200, "{}", mv.body);
    let seq = state(&td)
        .db
        .read(terrariumd::events::last_seq)
        .await
        .unwrap();
    let plan = td
        .post(
            "/api/projects/p/rules/dry-run",
            json!({"from": seq - 5, "to": seq}),
        )
        .await
        .ok()
        .clone();
    let rules: Vec<&str> = plan["plans"]
        .as_array()
        .unwrap()
        .iter()
        .map(|p| p["rule"].as_str().unwrap())
        .collect();
    assert!(rules.contains(&"agent-park"), "{plan}");
    // With `text`, the pack runs too, as in the lint with `text` (TER-226).
    let plan = td
        .post(
            "/api/projects/p/rules/dry-run",
            json!({"from": seq - 5, "to": seq, "text": ""}),
        )
        .await
        .ok()
        .clone();
    assert_eq!(plan["source"], "text");
    let rules: Vec<&str> = plan["plans"]
        .as_array()
        .unwrap()
        .iter()
        .map(|p| p["rule"].as_str().unwrap())
        .collect();
    assert!(rules.contains(&"agent-park"), "{plan}");
}

#[tokio::test]
async fn rights_plan_as_a_reader_apply_as_an_admin() {
    let td = lab().await;
    let lead = td
        .agent("p", "lead", json!({"roles": ["project_admin"]}))
        .await["id"]
        .as_str()
        .unwrap()
        .to_owned();
    let worker = td.agent("p", "worker", json!({})).await["id"]
        .as_str()
        .unwrap()
        .to_owned();
    td.project("other").await;
    let stranger = td.agent("other", "w", json!({})).await["id"]
        .as_str()
        .unwrap()
        .to_owned();

    td.post_as(&worker, "/api/projects/p/rules/migrate", json!({}))
        .await
        .ok();
    td.post_as(&worker, "/api/projects/p/rules/lint", json!({}))
        .await
        .ok();
    td.get_as(&worker, "/api/projects/p/rules/effective")
        .await
        .ok();
    td.post_as(
        &worker,
        "/api/projects/p/rules/migrate",
        json!({"apply": true}),
    )
    .await
    .err(403, "forbidden");
    td.post_as(
        &worker,
        "/api/projects/p/rules/agent-park/disable",
        json!({}),
    )
    .await
    .err(403, "forbidden");
    td.get_as(&stranger, "/api/projects/p/rules/effective")
        .await
        .err(403, "forbidden");
    td.post_as(&stranger, "/api/projects/p/rules/lint", json!({}))
        .await
        .err(403, "forbidden");

    let r = td
        .post_as(
            &lead,
            "/api/projects/p/rules/migrate",
            json!({"apply": true}),
        )
        .await
        .ok()
        .clone();
    assert_eq!(r["board_flow"], "rules");
    let rows = audit_rows(&td, "rules.migrate").await;
    assert_eq!(rows[0]["actor"], format!("agent:{lead}"));
    // A stale base_hash is 409.
    td.post_as(
        &lead,
        "/api/projects/p/rules/migrate",
        json!({"apply": true, "base_hash": "abc"}),
    )
    .await
    .err(409, "conflict");
}

/// TER-1090: a reload retires exact generated legacy wait overrides with
/// a hash-checked write, restoring the built-ins. A later load is a no-op.
#[tokio::test]
async fn a_reload_retires_old_wait_overrides() {
    let td = lab().await;
    let cols: Vec<Value> = [
        "backlog",
        "ready",
        "in_progress",
        "review",
        "merge_queue",
        "blocked",
        "done",
    ]
    .iter()
    .map(|n| json!({"name": n}))
    .collect();
    let r = td
        .patch("/api/projects/p/board", json!({"columns": cols}))
        .await;
    assert_eq!(r.status, 200, "{}", r.body);
    // The text of a migration before TER-222 for this board.
    let columns = state(&td)
        .db
        .read(|c| Ok(terrariumd::store::rules::context(c, "p")?.columns.unwrap()))
        .await
        .unwrap();
    let ctx = terrarium_core::rules_file::Context::default();
    let old_pack = terrarium_core::rules_file::parse(
        terrarium_core::rules_tools::PRE_TER_222_WAIT_RULES,
        "old",
        &ctx,
    )
    .unwrap();
    let none = terrarium_core::rules_file::RuleSet::default();
    let old = terrarium_core::rules_tools::migrate(&old_pack, "", &none, &columns).text;
    assert!(old.contains("one_of = [\"review\"] }"), "{old}");
    let r = td.put("/api/projects/p/rules", json!({"text": old})).await;
    assert_eq!(r.status, 200, "{}", r.body);

    td.post_empty("/api/reload").await.ok();
    let text = td.get("/api/projects/p/rules").await.ok()["text"]
        .as_str()
        .unwrap()
        .to_owned();
    assert!(!text.contains("rule \"agent-park\""), "{text}");
    assert!(!text.contains("rule \"agent-unpark\""), "{text}");
    let effective = td.get("/api/projects/p/rules/effective").await.ok().clone();
    for name in ["agent-park", "agent-unpark"] {
        let rule = effective["rules"]
            .as_array()
            .unwrap()
            .iter()
            .find(|r| r["name"] == name)
            .unwrap();
        assert_eq!(rule["origin"], "pack");
        assert!(rule["wcl"].as_str().unwrap().contains("data.from_role") || name == "agent-park");
    }
    let puts = audit_rows(&td, "rules.put").await;
    assert_eq!(puts.len(), 2, "{puts:?}");

    // A second load changes nothing.
    td.post_empty("/api/reload").await.ok();
    assert_eq!(td.get("/api/projects/p/rules").await.ok()["text"], text);
    assert_eq!(audit_rows(&td, "rules.put").await.len(), 2);

    // A custom cap makes the park block unrecognized, so a reload keeps it.
    let edited = old.replacen("max_per_hour = 600", "max_per_hour = 60", 1);
    let r = td
        .put("/api/projects/p/rules", json!({"text": edited}))
        .await;
    assert_eq!(r.status, 200, "{}", r.body);
    td.post_empty("/api/reload").await.ok();
    let after = td.get("/api/projects/p/rules").await.ok()["text"]
        .as_str()
        .unwrap()
        .to_owned();
    assert!(after.contains("max_per_hour = 60"), "{after}");
    assert!(after.contains("rule \"agent-park\""), "{after}");
    assert!(!after.contains("rule \"agent-unpark\""), "{after}");
    assert_eq!(audit_rows(&td, "rules.put").await.len(), 4);
}
