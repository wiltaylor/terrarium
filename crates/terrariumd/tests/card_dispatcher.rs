//! Dispatcher tests run real transactions with a fake harness.
#![allow(clippy::unwrap_used, clippy::expect_used)]
use crate::common::TestDaemon;
use serde_json::{Value, json};
use terrariumd::card_dispatcher::dispatch;

async fn setup(limit: i64, concurrent: u32, daily: u32) -> TestDaemon {
    let td = TestDaemon::start().await;
    park_timer(&td);
    td.project("p").await;
    td.patch(
        "/api/projects/p/board",
        json!({"columns": [
            {"name":"backlog"}, {"name":"ready"},
            {"name":"coding", "role":"working", "wip_limit":limit},
            {"name":"review", "role":"review"},
            {"name":"merge_queue", "role":"merge_queue"},
            {"name":"merged", "role":"accepted"},
            {"name":"blocked", "role":"blocked"}, {"name":"done"}
        ]}),
    )
    .await
    .ok();
    for name in ["card-small", "card-impl", "card-impl-hard"] {
        td.post(
            "/api/templates",
            json!({"name":name,"project":"p",
            "spec":{"harness":"codex"}}),
        )
        .await
        .ok();
    }
    td.agent(
        "p",
        "lead",
        json!({"roles":["project_admin"],"policy":{
            "spawn":{"templates":["project:*"],"bound":true,
                "max_concurrent":concurrent,"max_per_day":daily}
        }}),
    )
    .await;
    td
}

/// TER-475: the daemon's timer runs `card_dispatcher::pass` and
/// `review_dispatcher::pass` on each tick (15 s by default). Under load a
/// test outlives a tick, and the timer dispatches between the test's own
/// steps: a manual `dispatch` then finds nothing, or a retry count moves
/// by one. The tests drive every pass by hand, so the timer sleeps for a
/// day. Set before any dispatcher is enabled, so an earlier tick is a
/// no-op.
fn park_timer(td: &TestDaemon) {
    let s = state(td);
    s.lifecycle.set_tuning(terrariumd::lifecycle::Tuning {
        tick: std::time::Duration::from_secs(24 * 60 * 60),
        ..s.lifecycle.tuning()
    });
}

fn state(td: &TestDaemon) -> terrariumd::api::Shared {
    td.daemon.as_ref().unwrap().state.clone()
}

async fn card(td: &TestDaemon, kind: &str) -> String {
    td.post(
        "/api/projects/p/cards",
        json!({"title":"Task", "column":"ready",
        "labels":[format!("type:{kind}")]}),
    )
    .await
    .ok()["id"]
        .as_str()
        .unwrap()
        .into()
}

async fn enabled(td: &TestDaemon) {
    td.patch("/api/projects/p", json!({"card_dispatcher":"lead"}))
        .await
        .ok();
}

async fn review_setup() -> (TestDaemon, String) {
    let td = setup(2, 4, 20).await;
    td.post(
        "/api/templates",
        json!({"name":"card-review","project":"p","spec":{"harness":"codex"}}),
    )
    .await
    .ok();
    td.patch("/api/projects/p", json!({"review_dispatcher":"lead"}))
        .await
        .ok();
    let id = card(&td, "small").await;
    td.post(&format!("/api/cards/{id}/move"), json!({"column":"review"}))
        .await
        .ok();
    (td, id)
}

#[tokio::test]
async fn review_candidate_query_uses_only_indexed_searches() {
    let (td, _) = review_setup().await;
    let plan = state(&td)
        .db
        .read(|c| {
            let mut q = c.prepare(&format!(
                "EXPLAIN QUERY PLAN {}",
                terrariumd::review_dispatcher::CANDIDATES
            ))?;
            Ok(q.query_map(
                rusqlite::params![
                    "p",
                    "review",
                    0,
                    -1,
                    "",
                    terrariumd::review_dispatcher::CANDIDATE_PAGE
                ],
                |r| r.get::<_, String>(3),
            )?
            .collect::<Result<Vec<_>, _>>()?)
        })
        .await
        .unwrap();
    assert!(plan.iter().any(|s| s.contains("cards_column")), "{plan:?}");
    // Only the per-card label `json_each` virtual tables may scan; every
    // table access, the cards lookup included, must stay an index search.
    assert!(
        plan.iter()
            .filter(|s| s.contains("SCAN"))
            .all(|s| s.starts_with("SCAN github_label") || s.starts_with("SCAN hold_label")),
        "{plan:?}"
    );
}

async fn review_card(td: &TestDaemon, title: &str, labels: Value) -> String {
    let id = ready_card(
        td,
        json!({"title":title, "column":"ready", "labels":labels}),
    )
    .await;
    td.post(&format!("/api/cards/{id}/move"), json!({"column":"review"}))
        .await
        .ok();
    id
}

/// The ids that one page of the review candidate query returns.
async fn review_page(td: &TestDaemon) -> Vec<String> {
    state(td)
        .db
        .read(|c| {
            let mut q = c.prepare(terrariumd::review_dispatcher::CANDIDATES)?;
            let ids = q.query_map(
                rusqlite::params![
                    "p",
                    "review",
                    0,
                    -1,
                    "",
                    terrariumd::review_dispatcher::CANDIDATE_PAGE
                ],
                |r| r.get::<_, String>(0),
            )?;
            Ok(ids.collect::<Result<Vec<_>, _>>()?)
        })
        .await
        .unwrap()
}

async fn review_picked(td: &TestDaemon) -> Vec<String> {
    state(td)
        .db
        .read(|c| {
            let mut q = c.prepare("SELECT card FROM card_reviews ORDER BY card")?;
            let ids = q.query_map([], |r| r.get::<_, String>(0))?;
            Ok(ids.collect::<Result<Vec<_>, _>>()?)
        })
        .await
        .unwrap()
}

#[tokio::test]
async fn review_held_cards_cannot_starve_the_candidate_list() {
    let td = setup(2, 4, 20).await;
    td.post(
        "/api/templates",
        json!({"name":"card-review","project":"p","spec":{"harness":"codex"}}),
    )
    .await
    .ok();
    td.patch("/api/projects/p", json!({"review_dispatcher":"lead"}))
        .await
        .ok();
    let page = terrariumd::review_dispatcher::CANDIDATE_PAGE;
    // Cards with the labels `github` and `hold` (any ASCII case, spaces
    // around) are dropped by the SQL itself: a full page of them returns
    // no rows, so the LIMIT window is not spent on them.
    for i in 0..page {
        let labels = if i % 2 == 0 {
            json!(["github", "hold"])
        } else {
            json!([" GitHub ", "HOLD "])
        };
        review_card(&td, "Task", labels).await;
    }
    assert_eq!(review_page(&td).await, Vec::<String>::new());
    // Cards held only by a `GitHub #<n>:` title (one-digit numbers too)
    // pass the SQL and are skipped in Rust. More than a page of them is
    // walked page by page.
    let mut title_held = Vec::new();
    for i in 0..page + 1 {
        let title = format!("GitHub #{}: imported", i % 10);
        title_held.push(review_card(&td, &title, json!(["hold"])).await);
    }
    assert_eq!(review_page(&td).await, title_held[..page as usize]);
    assert!(
        !terrariumd::review_dispatcher::dispatch(&state(&td), "p")
            .await
            .unwrap()
    );
    // Titles that `github_title` rejects are no GitHub cards: with a
    // `hold` label alone they are not held and are offered for review.
    let mut free = Vec::new();
    for title in ["GitHub #12 fix: crash", "Git Hub #12: x", "GitHub #1 2: x"] {
        free.push(review_card(&td, title, json!(["hold"])).await);
    }
    for _ in &free {
        assert!(
            terrariumd::review_dispatcher::dispatch(&state(&td), "p")
                .await
                .unwrap()
        );
    }
    free.sort();
    assert_eq!(review_picked(&td).await, free);
}

#[tokio::test]
async fn review_owner_error_keeps_the_verdict_and_records_the_reason() {
    // An owner that lost project admin, or was deleted, must not reject
    // the reviewer's comment: the findings stay on the card.
    for broken in ["demoted", "deleted"] {
        let (td, id) = review_setup().await;
        assert!(
            terrariumd::review_dispatcher::dispatch(&state(&td), "p")
                .await
                .unwrap()
        );
        let agent = reviewer(&td, &id).await;
        if broken == "demoted" {
            let lead = lead_id(&td).await;
            td.put(&format!("/api/agents/{lead}/roles"), json!({"roles":[]}))
                .await
                .ok();
        } else {
            state(&td)
                .db
                .write("operator", |t| {
                    t.execute("UPDATE projects SET review_dispatcher = 'ghost'", [])?;
                    Ok(())
                })
                .await
                .unwrap();
        }
        let body = "Review verdict: accept\nF1: Kept findings.".to_owned();
        td.post_as(
            &agent,
            &format!("/api/cards/{id}/comments"),
            json!({"body":body}),
        )
        .await
        .ok();
        let c = get(&td, &id).await;
        assert_eq!(c["column"], "review");
        assert!(
            c["comments"]
                .as_array()
                .unwrap()
                .iter()
                .any(|x| x["body"] == body),
            "{broken}: {}",
            c["comments"]
        );
        assert!(
            c["comments"].as_array().unwrap().iter().any(|x| x["body"]
                .as_str()
                .unwrap()
                .contains("review dispatcher owner is no longer valid")),
            "{broken}: {}",
            c["comments"]
        );
        // Like a project without a dispatcher, the round stays open.
        let key = id.clone();
        let applied: i64 = state(&td)
            .db
            .read(move |c| {
                Ok(c.query_row(
                    "SELECT applied FROM card_reviews WHERE card = ?1",
                    [&key],
                    |r| r.get(0),
                )?)
            })
            .await
            .unwrap();
        assert_eq!(applied, 0, "{broken}");
    }
}

#[tokio::test]
async fn dispatch_candidate_query_uses_only_indexed_searches() {
    let td = setup(2, 4, 20).await;
    enabled(&td).await;
    let first = card(&td, "small").await;
    let second = card(&td, "impl").await;
    state(&td)
        .db
        .write("system", move |t| {
            t.execute(
                "WITH RECURSIVE n(i) AS (VALUES(1) UNION ALL SELECT i + 1 FROM n WHERE i < 2048)
            INSERT INTO conflict_fixes(job, card, project, template, task, done, retry_at)
            SELECT 'job-' || i, CASE WHEN i % 2 = 0 THEN ?1 ELSE ?2 END,
                'p', 'card-impl', 'repair', i % 3 = 0,
                CASE WHEN i % 4 = 0 THEN unixepoch() + 3600 ELSE i % 16 END FROM n",
                rusqlite::params![first, second],
            )?;
            Ok(())
        })
        .await
        .unwrap();
    let plan = state(&td)
        .db
        .read(|c| {
            let mut plan = Vec::new();
            let queries: [(&str, &[&dyn rusqlite::ToSql]); 3] = [
                (
                    terrariumd::card_dispatcher::CANDIDATES,
                    rusqlite::params![
                        "p",
                        "ready",
                        0,
                        -1,
                        "",
                        terrariumd::card_dispatcher::CANDIDATE_PAGE
                    ],
                ),
                (
                    terrariumd::card_dispatcher::WORKING_WIP,
                    rusqlite::params!["p"],
                ),
                (terrariumd::conflict_fixes::PENDING, &[]),
            ];
            for (sql, params) in queries {
                let mut q = c.prepare(&format!("EXPLAIN QUERY PLAN {sql}"))?;
                let rows = q.query_map(params, |r| r.get::<_, String>(3))?;
                plan.extend(rows.collect::<Result<Vec<_>, _>>()?);
            }
            Ok(plan)
        })
        .await
        .unwrap();
    assert!(plan.iter().any(|s| s.contains("cards_column")), "{plan:?}");
    assert!(plan.iter().all(|s| !s.contains("SCAN")), "{plan:?}");
    assert!(plan.iter().all(|s| !s.contains("TEMP B-TREE")), "{plan:?}");
    for index in ["conflict_fixes_card_pending", "conflict_fixes_pending"] {
        assert!(
            plan.iter()
                .any(|s| s.contains("SEARCH") && s.contains(index)),
            "{index}: {plan:?}"
        );
    }
}

async fn ready_card(td: &TestDaemon, body: Value) -> String {
    td.post("/api/projects/p/cards", body).await.ok()["id"]
        .as_str()
        .unwrap()
        .into()
}

/// A held GitHub card above an eligible card is skipped, not dispatched.
/// The held card carries a usable type label, so only the hold gate
/// skips it: without `github_gate::is_held` the dispatcher would pick it.
#[tokio::test]
async fn held_card_is_skipped_and_next_card_dispatched() {
    let td = setup(2, 3, 10).await;
    let held = ready_card(
        &td,
        json!({"title": "GitHub #7: held",
               "labels": ["github", "hold", "type:small"],
               "column": "ready"}),
    )
    .await;
    let next = card(&td, "small").await;
    enabled(&td).await;
    assert!(dispatch(&state(&td), "p").await.unwrap());
    let c = get(&td, &held).await;
    assert_eq!(c["column"], "ready");
    assert!(c["agent"].is_null());
    assert_eq!(get(&td, &next).await["column"], "coding");
}

/// Held cards and cards without a usable type label must not starve the
/// bounded page: the card after a full page of skips is still dispatched.
/// The held cards carry a usable type label, so each skip kind is a skip
/// for exactly one reason: the hold gate, or a missing template.
#[tokio::test]
async fn skips_beyond_one_page_do_not_starve_a_later_card() {
    let td = setup(2, 4, 20).await;
    for i in 0..terrariumd::card_dispatcher::CANDIDATE_PAGE + 1 {
        let body = if i % 2 == 0 {
            json!({"title": format!("GitHub #{i}: held"),
                   "labels": ["github", "hold", "type:small"],
                   "column": "ready"})
        } else {
            json!({"title": "No type label", "column": "ready"})
        };
        ready_card(&td, body).await;
    }
    let last = card(&td, "small").await;
    enabled(&td).await;
    assert!(dispatch(&state(&td), "p").await.unwrap());
    assert_eq!(get(&td, &last).await["column"], "coding");
}

/// Positions can tie (archived cards keep theirs, group cards go in at 0
/// with no shift). A tie at a page boundary must not drop the tied card,
/// so the keyset is `(position, id)`, not `position` alone.
#[tokio::test]
async fn a_tie_at_a_page_boundary_does_not_drop_the_tied_card() {
    let td = setup(2, 4, 20).await;
    let mut skips = Vec::new();
    for _ in 0..terrariumd::card_dispatcher::CANDIDATE_PAGE {
        skips.push(ready_card(&td, json!({"title": "No type label", "column": "ready"})).await);
    }
    let tied = card(&td, "small").await;
    // The eligible card ties with the last skip of the first page, and its
    // id orders after that skip, so it is the first row of the next page.
    let tied_to = skips[terrariumd::card_dispatcher::CANDIDATE_PAGE as usize - 1].clone();
    assert!(tied_to < tied, "{tied_to} must order before {tied}");
    let id = tied.clone();
    state(&td)
        .db
        .write("system", move |t| {
            t.execute(
                "UPDATE cards SET position = ?2 WHERE id = ?1",
                rusqlite::params![id, terrariumd::card_dispatcher::CANDIDATE_PAGE - 1],
            )?;
            Ok(())
        })
        .await
        .unwrap();
    enabled(&td).await;
    assert!(dispatch(&state(&td), "p").await.unwrap());
    assert_eq!(get(&td, &tied).await["column"], "coding");
    assert_eq!(get(&td, &tied_to).await["column"], "ready");
}

#[tokio::test]
async fn review_global_fallback_and_conventional_verdict() {
    let (td, id) = review_setup().await;
    td.delete("/api/templates/p%2Fcard-review").await.ok();
    td.post(
        "/api/templates",
        json!({"name":"card-review","spec":{"harness":"codex"}}),
    )
    .await
    .ok();
    let lead = lead_id(&td).await;
    td.patch(
        &format!("/api/agents/{lead}"),
        json!({"policy":{"spawn":{
            "templates":["global:*"],"bound":true,"max_concurrent":4
        }}}),
    )
    .await
    .ok();
    assert!(
        terrariumd::review_dispatcher::dispatch(&state(&td), "p")
            .await
            .unwrap()
    );
    let agent = reviewer(&td, &id).await;
    td.post_as(
        &agent,
        &format!("/api/cards/{id}/comments"),
        json!({"body":"Review verdict: accept\nChecked."}),
    )
    .await
    .ok();
    assert_eq!(get(&td, &id).await["column"], "merge_queue");
}

#[tokio::test]
async fn review_round_limit_notifies_owner_once() {
    let (td, id) = review_setup().await;
    for _ in 0..3 {
        assert!(
            terrariumd::review_dispatcher::dispatch(&state(&td), "p")
                .await
                .unwrap()
        );
        let agent = reviewer(&td, &id).await;
        terrariumd::spawn::close(
            &state(&td),
            &agent,
            terrariumd::store::spawns::DONE,
            None,
            false,
        )
        .await
        .unwrap();
    }
    assert!(
        terrariumd::review_dispatcher::dispatch(&state(&td), "p")
            .await
            .unwrap()
    );
    assert!(
        !terrariumd::review_dispatcher::dispatch(&state(&td), "p")
            .await
            .unwrap()
    );
    assert_eq!(get(&td, &id).await["review_round"], 3);
    assert_eq!(get(&td, &id).await["column"], "review");
    let count: i64 = state(&td)
        .db
        .read(|c| {
            Ok(c.query_row(
                "SELECT count(*) FROM messages WHERE subject LIKE '%review needs lead decision'",
                [],
                |r| r.get(0),
            )?)
        })
        .await
        .unwrap();
    assert_eq!(count, 1);
}

#[tokio::test]
async fn review_failed_move_retains_findings_and_notifies_owner() {
    for verdict in ["Review verdict: accept", "Review verdict: changes"] {
        let (td, id) = review_setup().await;
        let column = if verdict.ends_with("accept") {
            "merge_queue"
        } else {
            "ready"
        };
        let col = column.to_owned();
        state(&td)
            .db
            .write("operator", move |t| {
                t.execute(
                    "UPDATE board_columns SET wip_limit = 1 WHERE project = 'p' AND name = ?1",
                    [&col],
                )?;
                Ok(())
            })
            .await
            .unwrap();
        let blocker = card(&td, "small").await;
        if column == "merge_queue" {
            td.post(
                &format!("/api/cards/{blocker}/move"),
                json!({"column":column}),
            )
            .await
            .ok();
        }
        assert!(
            terrariumd::review_dispatcher::dispatch(&state(&td), "p")
                .await
                .unwrap()
        );
        let agent = reviewer(&td, &id).await;
        let body = format!("{verdict}\nF1: Retained findings.");
        td.post_as(
            &agent,
            &format!("/api/cards/{id}/comments"),
            json!({"body":body}),
        )
        .await
        .ok();
        let c = get(&td, &id).await;
        assert_eq!(c["column"], "review");
        assert!(
            c["comments"]
                .as_array()
                .unwrap()
                .iter()
                .any(|x| x["body"] == body)
        );
        assert!(
            c["comments"]
                .as_array()
                .unwrap()
                .iter()
                .any(|x| x["body"].as_str().unwrap().contains("move to"))
        );
        let count: i64 = state(&td).db.read(|c| {
            Ok(c.query_row("SELECT count(*) FROM messages WHERE subject LIKE '%review needs lead decision'", [], |r| r.get(0))?)
        }).await.unwrap();
        assert_eq!(count, 1);
    }
}

async fn reviewer(td: &TestDaemon, id: &str) -> String {
    let id = id.to_owned();
    state(td)
        .db
        .read(move |c| {
            Ok(c.query_row(
                "SELECT agent FROM card_reviews WHERE card = ?1",
                [&id],
                |r| r.get(0),
            )?)
        })
        .await
        .unwrap()
}

#[tokio::test]
async fn review_opt_in_concurrent_spawn_and_accept() {
    let (td, id) = review_setup().await;
    td.patch("/api/projects/p", json!({"review_dispatcher":"off"}))
        .await
        .ok();
    assert!(
        !terrariumd::review_dispatcher::dispatch(&state(&td), "p")
            .await
            .unwrap()
    );
    td.patch("/api/projects/p", json!({"review_dispatcher":"lead"}))
        .await
        .ok();
    let s = state(&td);
    let (a, b) = tokio::join!(
        terrariumd::review_dispatcher::dispatch(&s, "p"),
        terrariumd::review_dispatcher::dispatch(&s, "p")
    );
    assert_eq!(
        usize::from(a.unwrap_or(false)) + usize::from(b.unwrap_or(false)),
        1
    );
    assert_eq!(get(&td, &id).await["review_round"], 1);
    // Neither a human nor the lead can impersonate this round's verdict.
    td.post(
        &format!("/api/cards/{id}/comments"),
        json!({"body":"accept"}),
    )
    .await
    .ok();
    assert_eq!(get(&td, &id).await["column"], "review");
    let agent = reviewer(&td, &id).await;
    td.post_as(
        &agent,
        &format!("/api/cards/{id}/comments"),
        json!({"body":"accept\nChecked diff and evidence."}),
    )
    .await
    .ok();
    assert_eq!(get(&td, &id).await["column"], "merge_queue");
}

#[tokio::test]
async fn review_changes_returns_top_and_lead_label_reserves_decision() {
    for lead_only in [false, true] {
        let (td, id) = review_setup().await;
        let other = card(&td, "small").await;
        if lead_only {
            td.patch(
                &format!("/api/cards/{id}"),
                json!({"labels":["review:lead"]}),
            )
            .await
            .ok();
        }
        assert!(
            terrariumd::review_dispatcher::dispatch(&state(&td), "p")
                .await
                .unwrap()
        );
        let agent = reviewer(&td, &id).await;
        td.post_as(
            &agent,
            &format!("/api/cards/{id}/comments"),
            json!({"body":"Not a verdict\naccept"}),
        )
        .await
        .ok();
        assert_eq!(get(&td, &id).await["column"], "review");
        td.post_as(
            &agent,
            &format!("/api/cards/{id}/comments"),
            json!({"body":"changes\nF1: Missing test."}),
        )
        .await
        .ok();
        let c = get(&td, &id).await;
        assert_eq!(c["column"], if lead_only { "review" } else { "ready" });
        if !lead_only {
            assert!(
                c["position"].as_i64().unwrap()
                    < get(&td, &other).await["position"].as_i64().unwrap()
            );
        }
        // Repeated verdicts cannot override the first one.
        td.post_as(
            &agent,
            &format!("/api/cards/{id}/comments"),
            json!({"body":"accept"}),
        )
        .await
        .ok();
        assert_eq!(get(&td, &id).await["column"], c["column"]);
        terrariumd::spawn::close(
            &state(&td),
            &agent,
            terrariumd::store::spawns::DONE,
            None,
            false,
        )
        .await
        .unwrap();
        assert!(
            !terrariumd::review_dispatcher::dispatch(&state(&td), "p")
                .await
                .unwrap()
        );
    }
}

#[tokio::test]
async fn review_rounds_reject_stale_verdict_and_retry_unfinished_review() {
    let (td, id) = review_setup().await;
    let s = state(&td);
    assert!(
        terrariumd::review_dispatcher::dispatch(&s, "p")
            .await
            .unwrap()
    );
    let old = reviewer(&td, &id).await;
    // A reviewer that ended without a verdict must not strand the card.
    terrariumd::spawn::close(&s, &old, terrariumd::store::spawns::DONE, None, false)
        .await
        .unwrap();
    assert!(
        terrariumd::review_dispatcher::dispatch(&s, "p")
            .await
            .unwrap()
    );
    assert_eq!(get(&td, &id).await["review_round"], 2);
    let current = reviewer(&td, &id).await;
    // Moving away invalidates the round, even if the same card returns immediately.
    td.post(
        &format!("/api/cards/{id}/move"),
        json!({"column":"backlog"}),
    )
    .await
    .ok();
    td.post(&format!("/api/cards/{id}/move"), json!({"column":"review"}))
        .await
        .ok();
    td.post_as(
        &current,
        &format!("/api/cards/{id}/comments"),
        json!({"body":"accept"}),
    )
    .await
    .ok();
    assert_eq!(get(&td, &id).await["column"], "review");
    assert!(
        !terrariumd::review_dispatcher::dispatch(&s, "p")
            .await
            .unwrap()
    );
    terrariumd::spawn::close(&s, &current, terrariumd::store::spawns::DONE, None, false)
        .await
        .unwrap();
    assert!(
        terrariumd::review_dispatcher::dispatch(&s, "p")
            .await
            .unwrap()
    );
    assert_eq!(get(&td, &id).await["review_round"], 3);
}

#[tokio::test]
async fn review_caps_and_template_failure_backoff() {
    let (td, id) = review_setup().await;
    let lead = lead_id(&td).await;
    td.patch(
        &format!("/api/agents/{lead}"),
        json!({"policy":{"spawn":{
            "templates":["card-small"],"bound":true,"max_concurrent":4
        }}}),
    )
    .await
    .ok();
    assert!(
        terrariumd::review_dispatcher::dispatch(&state(&td), "p")
            .await
            .is_err()
    );
    assert!(
        !terrariumd::review_dispatcher::dispatch(&state(&td), "p")
            .await
            .unwrap()
    );
    assert_eq!(deferred_counts(&td).await.0, 1);
    assert_eq!(get(&td, &id).await["review_round"], 0);
    td.patch(
        &format!("/api/agents/{lead}"),
        json!({"policy":{"spawn":{
            "templates":["project:*"],"bound":true,"max_concurrent":1,"max_per_day":1
        }}}),
    )
    .await
    .ok();
    let key = id.clone();
    state(&td)
        .db
        .write("operator", move |t| {
            t.execute(
                "UPDATE card_dispatch_retries SET retry_at = 0 WHERE card = ?1",
                [&key],
            )?;
            Ok(())
        })
        .await
        .unwrap();
    assert!(
        terrariumd::review_dispatcher::dispatch(&state(&td), "p")
            .await
            .unwrap()
    );
    let second = card(&td, "small").await;
    td.post(
        &format!("/api/cards/{second}/move"),
        json!({"column":"review"}),
    )
    .await
    .ok();
    assert!(
        !terrariumd::review_dispatcher::dispatch(&state(&td), "p")
            .await
            .unwrap()
    );
    assert_eq!(get(&td, &second).await["review_round"], 0);
    let agent = reviewer(&td, &id).await;
    terrariumd::spawn::close(
        &state(&td),
        &agent,
        terrariumd::store::spawns::DONE,
        None,
        false,
    )
    .await
    .unwrap();
    // The daily cap still applies after the concurrent place is released.
    assert!(
        !terrariumd::review_dispatcher::dispatch(&state(&td), "p")
            .await
            .unwrap()
    );
}

#[tokio::test]
async fn review_fleet_and_file_setting_removal_disables_dispatch() {
    let td = setup(2, 4, 20).await;
    for setting in ["review_dispatcher = \"lead\"", ""] {
        td.post("/api/fleet/apply", json!({"content":format!("name = \"review-fleet\"\n[[projects]]\nname = \"p\"\n{setting}\n")})).await.ok();
        assert_eq!(
            td.get("/api/projects/p").await.ok()["review_dispatcher"],
            if setting.is_empty() {
                Value::Null
            } else {
                json!("lead")
            }
        );
    }
    for setting in [Some("lead"), None] {
        let mut def = json!({"name":"p"});
        if let Some(owner) = setting {
            def["review_dispatcher"] = json!(owner);
        }
        let defs = terrarium_core::config::Definitions {
            projects: vec![serde_json::from_value(def).unwrap()],
            ..Default::default()
        };
        let path = td.state.clone();
        state(&td)
            .db
            .write("operator", move |t| {
                terrariumd::store::sync::sync(t, &defs, &path)
            })
            .await
            .unwrap();
        assert_eq!(
            td.get("/api/projects/p").await.ok()["review_dispatcher"],
            json!(setting)
        );
    }
}

async fn get(td: &TestDaemon, id: &str) -> Value {
    td.get(&format!("/api/cards/{id}")).await.ok().clone()
}

#[tokio::test]
async fn off_by_default_and_disabled_explicitly() {
    let td = setup(2, 2, 10).await;
    let id = card(&td, "small").await;
    assert!(!dispatch(&state(&td), "p").await.unwrap());
    enabled(&td).await;
    td.patch("/api/projects/p", json!({"card_dispatcher":"off"}))
        .await
        .ok();
    assert!(!dispatch(&state(&td), "p").await.unwrap());
    assert!(get(&td, &id).await["agent"].is_null());
}

#[tokio::test]
async fn claims_in_order_with_role_based_wip_and_no_duplicate() {
    let td = setup(1, 3, 10).await;
    let first = card(&td, "impl").await;
    let second = card(&td, "hard").await;
    enabled(&td).await;
    assert!(dispatch(&state(&td), "p").await.unwrap());
    let c = get(&td, &first).await;
    assert_eq!(c["column"], "coding");
    assert_eq!(c["agent"], c["assignee"]);
    assert_eq!(c["agent_name"], first.to_ascii_lowercase());
    assert!(!dispatch(&state(&td), "p").await.unwrap());
    assert!(get(&td, &second).await["agent"].is_null());
}

#[tokio::test]
async fn concurrent_dispatchers_cannot_overfill_wip() {
    let td = setup(1, 3, 10).await;
    let first = card(&td, "small").await;
    let second = card(&td, "small").await;
    enabled(&td).await;
    let s = state(&td);
    let (a, b) = tokio::join!(dispatch(&s, "p"), dispatch(&s, "p"));
    assert_eq!(
        usize::from(a.unwrap_or(false)) + usize::from(b.unwrap_or(false)),
        1
    );
    assert_eq!(get(&td, &first).await["column"], "coding");
    assert!(get(&td, &second).await["agent"].is_null());
}

#[tokio::test]
async fn skips_unfinished_dependencies_and_unknown_types() {
    let td = setup(2, 3, 10).await;
    let dependency = card(&td, "unknown").await;
    let blocked = card(&td, "small").await;
    td.patch(
        &format!("/api/cards/{blocked}"),
        json!({"depends_on":[dependency]}),
    )
    .await
    .ok();
    let eligible = card(&td, "hard").await;
    enabled(&td).await;
    assert!(dispatch(&state(&td), "p").await.unwrap());
    assert!(get(&td, &blocked).await["agent"].is_null());
    assert_eq!(get(&td, &eligible).await["column"], "coding");
}

#[tokio::test]
async fn daily_spawn_cap_is_not_bypassed_by_daemon() {
    let td = setup(3, 3, 1).await;
    card(&td, "small").await;
    let second = card(&td, "small").await;
    enabled(&td).await;
    assert!(dispatch(&state(&td), "p").await.unwrap());
    assert!(!dispatch(&state(&td), "p").await.unwrap());
    assert!(get(&td, &second).await["agent"].is_null());
    assert_eq!(get(&td, &second).await["column"], "ready");
}

#[tokio::test]
async fn concurrent_spawn_cap_leaves_card_ready() {
    let td = setup(3, 1, 10).await;
    card(&td, "small").await;
    let second = card(&td, "small").await;
    enabled(&td).await;
    assert!(dispatch(&state(&td), "p").await.unwrap());
    assert!(!dispatch(&state(&td), "p").await.unwrap());
    assert!(get(&td, &second).await["agent"].is_null());
    assert_eq!(get(&td, &second).await["comments"], json!([]));
    assert_eq!(deferred_counts(&td).await.0, 0);
}

#[tokio::test]
async fn missing_owner_leaves_card_unclaimed() {
    let td = setup(2, 2, 10).await;
    let id = card(&td, "small").await;
    td.patch("/api/projects/p", json!({"card_dispatcher":"missing"}))
        .await
        .ok();
    assert!(!dispatch(&state(&td), "p").await.unwrap());
    assert!(get(&td, &id).await["assignee"].is_null());
    assert!(get(&td, &id).await["agent"].is_null());
}

#[tokio::test]
async fn skips_existing_bound_agent_even_without_assignee() {
    let td = setup(2, 3, 10).await;
    let first = card(&td, "small").await;
    let second = card(&td, "small").await;
    let lead = state(&td)
        .db
        .read(|c| {
            Ok(terrariumd::store::agents::find(c, "lead", Some("p"))?
                .agent
                .id
                .to_string())
        })
        .await
        .unwrap();
    td.post_as(
        &lead,
        &format!("/api/agents/{lead}/spawn"),
        json!({"template":"card-small", "task":"Own this card",
            "bind":format!("card:{first}")}),
    )
    .await
    .ok();
    let original = get(&td, &first).await["agent"].clone();
    assert!(original.is_string());
    assert!(get(&td, &first).await["assignee"].is_null());
    enabled(&td).await;
    assert!(dispatch(&state(&td), "p").await.unwrap());
    assert_eq!(get(&td, &first).await["agent"], original);
    assert_eq!(get(&td, &second).await["column"], "coding");
}

#[tokio::test]
async fn name_conflict_does_not_claim_or_bind_the_card() {
    let td = setup(2, 3, 10).await;
    let id = card(&td, "small").await;
    td.agent("p", &id.to_ascii_lowercase(), json!({})).await;
    enabled(&td).await;
    assert!(dispatch(&state(&td), "p").await.is_err());
    let c = get(&td, &id).await;
    assert!(c["agent"].is_null());
    assert!(c["assignee"].is_null());
    assert_eq!(c["column"], "ready");
}

#[tokio::test]
async fn timer_pass_fills_only_available_places() {
    let td = setup(2, 3, 10).await;
    let first = card(&td, "small").await;
    let second = card(&td, "impl").await;
    let third = card(&td, "hard").await;
    enabled(&td).await;
    terrariumd::card_dispatcher::pass(&state(&td)).await;
    assert_eq!(get(&td, &first).await["column"], "coding");
    assert_eq!(get(&td, &second).await["column"], "coding");
    assert_eq!(get(&td, &third).await["column"], "ready");
    terrariumd::card_dispatcher::pass(&state(&td)).await;
    assert!(get(&td, &third).await["agent"].is_null());
}

async fn lead_id(td: &TestDaemon) -> String {
    state(td)
        .db
        .read(|c| {
            Ok(terrariumd::store::agents::find(c, "lead", Some("p"))?
                .agent
                .id
                .to_string())
        })
        .await
        .unwrap()
}

#[tokio::test]
async fn failures_back_off_once_and_do_not_block_later_cards() {
    let td = setup(3, 3, 20).await;
    let bad = card(&td, "small").await;
    let obstruction = td.agent("p", &bad.to_ascii_lowercase(), json!({})).await;
    let good = card(&td, "impl").await;
    enabled(&td).await;
    terrariumd::card_dispatcher::pass(&state(&td)).await;
    assert_eq!(get(&td, &good).await["column"], "coding");
    assert_eq!(get(&td, &bad).await["column"], "ready");
    assert_eq!(
        get(&td, &bad).await["comments"].as_array().unwrap().len(),
        1
    );
    assert!(!dispatch(&state(&td), "p").await.unwrap());
    for attempt in 2..=8 {
        let key = bad.clone();
        state(&td)
            .db
            .write("operator", move |t| {
                t.execute(
                    "UPDATE card_dispatch_retries SET retry_at = 0 WHERE card = ?1",
                    [&key],
                )?;
                Ok(())
            })
            .await
            .unwrap();
        assert!(dispatch(&state(&td), "p").await.is_err());
        let key = bad.clone();
        let (n, remaining): (u32, i64) = state(&td)
            .db
            .read(move |c| {
                Ok(c.query_row(
                    "SELECT failures, retry_at FROM card_dispatch_retries WHERE card = ?1",
                    [&key],
                    |r| {
                        Ok((
                            r.get(0)?,
                            r.get::<_, i64>(1)? - chrono::Utc::now().timestamp(),
                        ))
                    },
                )?)
            })
            .await
            .unwrap();
        assert_eq!(n, attempt);
        let delay = (60_i64 << (attempt - 1).min(5)).min(1800);
        assert!(
            (delay - 2..=delay).contains(&remaining),
            "{remaining} vs {delay}"
        );
    }
    assert_eq!(
        get(&td, &bad).await["comments"].as_array().unwrap().len(),
        1
    );
    let mails: i64 = state(&td).db.read(|c| Ok(c.query_row("SELECT count(*) FROM messages WHERE subject LIKE '%dispatcher could not start agent'", [], |r| r.get(0))?)).await.unwrap();
    assert_eq!(mails, 1);
    td.delete(&format!(
        "/api/agents/{}",
        obstruction["id"].as_str().unwrap()
    ))
    .await
    .ok();
    let key = bad.clone();
    state(&td)
        .db
        .write("operator", move |t| {
            t.execute(
                "UPDATE card_dispatch_retries SET retry_at = 0 WHERE card = ?1",
                [&key],
            )?;
            Ok(())
        })
        .await
        .unwrap();
    assert!(dispatch(&state(&td), "p").await.unwrap());
    assert_eq!(get(&td, &bad).await["column"], "coding");
    let remaining: i64 = state(&td)
        .db
        .read(|c| {
            Ok(
                c.query_row("SELECT count(*) FROM card_dispatch_retries", [], |r| {
                    r.get(0)
                })?,
            )
        })
        .await
        .unwrap();
    assert_eq!(remaining, 0);
}

#[tokio::test]
async fn dispatch_uses_template_targets_not_all_owner_targets() {
    let td = setup(2, 3, 10).await;
    for name in ["p-needed", "p-extra"] {
        td.post(
            "/api/targets",
            json!({"name":name,"kind":"ssh","project":"p","address":"u@h"}),
        )
        .await
        .ok();
    }
    let lead = lead_id(&td).await;
    td.patch(
        &format!("/api/agents/{lead}"),
        json!({"policy":{
            "targets":["p-needed","p-extra"],
            "spawn":{"templates":["project:*"],"bound":true,"max_concurrent":3}
        }}),
    )
    .await
    .ok();
    td.patch(
        "/api/templates/p%2Fcard-small",
        json!({"spec":{
            "harness":"codex","policy":{"targets":["p-needed"]}
        }}),
    )
    .await
    .ok();
    let id = card(&td, "small").await;
    enabled(&td).await;
    assert!(dispatch(&state(&td), "p").await.unwrap());
    let c = get(&td, &id).await;
    let a = td
        .get(&format!("/api/agents/{}", c["agent"].as_str().unwrap()))
        .await;
    assert_eq!(a.ok()["policy"]["targets"], json!(["p-needed"]));
    let manual = td
        .post_as(
            &lead,
            &format!("/api/agents/{lead}/spawn"),
            json!({"template":"card-small","task":"Compare target rights"}),
        )
        .await;
    let child = &manual.ok()["agent"];
    assert_eq!(child["policy"]["targets"], a.ok()["policy"]["targets"]);
}

#[tokio::test]
async fn paused_owner_does_not_dispatch() {
    let td = setup(2, 2, 10).await;
    let id = card(&td, "small").await;
    enabled(&td).await;
    td.post_empty(&format!("/api/agents/{}/pause", lead_id(&td).await))
        .await
        .ok();
    assert!(!dispatch(&state(&td), "p").await.unwrap());
    assert!(get(&td, &id).await["agent"].is_null());
}

#[tokio::test]
async fn accepted_dependency_allows_claim_without_waiting_for_release() {
    let td = setup(2, 2, 10).await;
    let dep = card(&td, "unknown").await;
    let id = card(&td, "small").await;
    td.patch(&format!("/api/cards/{id}"), json!({"depends_on":[dep]}))
        .await
        .ok();
    td.post(
        &format!("/api/cards/{dep}/move"),
        json!({"column":"merged"}),
    )
    .await
    .ok();
    assert_eq!(get(&td, &id).await["blocked_by"], json!([]));
    enabled(&td).await;
    assert!(dispatch(&state(&td), "p").await.unwrap());
    assert_eq!(get(&td, &id).await["column"], "coding");
}

#[tokio::test]
async fn file_removal_of_setting_disables_dispatch() {
    let td = setup(2, 2, 10).await;
    for setting in [Some("lead"), None] {
        let mut def = json!({"name":"p"});
        if let Some(owner) = setting {
            def["card_dispatcher"] = json!(owner);
        }
        let defs = terrarium_core::config::Definitions {
            projects: vec![serde_json::from_value(def).unwrap()],
            ..Default::default()
        };
        let path = td.state.clone();
        state(&td)
            .db
            .write("operator", move |t| {
                terrariumd::store::sync::sync(t, &defs, &path)
            })
            .await
            .unwrap();
        assert_eq!(
            td.get("/api/projects/p").await.ok()["card_dispatcher"],
            json!(setting)
        );
    }
    card(&td, "small").await;
    assert!(!dispatch(&state(&td), "p").await.unwrap());
}

#[tokio::test]
async fn fleet_setting_can_enable_and_disable_dispatch() {
    let td = TestDaemon::start().await;
    for setting in ["card_dispatcher = \"lead\"", ""] {
        let content = format!("name = \"dispatch\"\n[[projects]]\nname = \"p\"\n{setting}\n");
        td.post("/api/fleet/apply", json!({"content":content}))
            .await
            .ok();
        let expected = if setting.is_empty() {
            Value::Null
        } else {
            json!("lead")
        };
        assert_eq!(
            td.get("/api/projects/p").await.ok()["card_dispatcher"],
            expected
        );
    }
}

async fn deferred_counts(td: &TestDaemon) -> (i64, i64) {
    state(td)
        .db
        .read(|c| {
            Ok((
                c.query_row("SELECT count(*) FROM card_dispatch_retries", [], |r| {
                    r.get(0)
                })?,
                c.query_row(
                    "SELECT count(*) FROM messages WHERE subject = 'p: dispatcher deferred'",
                    [],
                    |r| r.get(0),
                )?,
            ))
        })
        .await
        .unwrap()
}

#[tokio::test]
async fn daily_cap_defers_project_without_card_failures_until_window_resets() {
    let td = setup(10, 10, 1).await;
    card(&td, "small").await;
    let second = card(&td, "small").await;
    let third = card(&td, "impl").await;
    enabled(&td).await;
    for _ in 0..3 {
        terrariumd::card_dispatcher::pass(&state(&td)).await;
    }
    assert_eq!(deferred_counts(&td).await, (0, 1));
    for id in [&second, &third] {
        let c = get(&td, id).await;
        assert_eq!(c["column"], "ready");
        assert_eq!(c["comments"], json!([]));
    }
    // Advance the rolling daily window without waiting or bypassing its check.
    state(&td)
        .db
        .write("operator", |t| {
            t.execute(
                "UPDATE spawns SET spawned_at = '2000-01-01T00:00:00.000Z'",
                [],
            )?;
            Ok(())
        })
        .await
        .unwrap();
    assert!(dispatch(&state(&td), "p").await.unwrap());
    assert_eq!(get(&td, &second).await["column"], "coding");
    assert!(!dispatch(&state(&td), "p").await.unwrap());
    assert_eq!(deferred_counts(&td).await, (0, 2)); // A new cap event.
}

#[tokio::test]
async fn missing_spawn_right_is_one_project_notice_and_recovers_after_repair() {
    let td = setup(3, 3, 10).await;
    let lead = lead_id(&td).await;
    td.patch(&format!("/api/agents/{lead}"), json!({"policy":{}}))
        .await
        .ok();
    let id = card(&td, "small").await;
    card(&td, "impl").await;
    enabled(&td).await;
    for _ in 0..3 {
        terrariumd::card_dispatcher::pass(&state(&td)).await;
    }
    assert_eq!(deferred_counts(&td).await, (0, 1));
    assert_eq!(get(&td, &id).await["comments"], json!([]));
    td.patch(
        &format!("/api/agents/{lead}"),
        json!({"policy":{"spawn":{
            "templates":["project:*"],"bound":true,"max_concurrent":3
        }}}),
    )
    .await
    .ok();
    assert!(dispatch(&state(&td), "p").await.unwrap());
}

#[tokio::test]
async fn misconfigured_owner_produces_one_project_notice_not_card_failures() {
    let td = setup(3, 3, 10).await;
    let id = card(&td, "small").await;
    td.patch("/api/projects/p", json!({"card_dispatcher":"missing"}))
        .await
        .ok();
    for _ in 0..3 {
        terrariumd::card_dispatcher::pass(&state(&td)).await;
    }
    assert_eq!(deferred_counts(&td).await, (0, 1));
    assert_eq!(get(&td, &id).await["comments"], json!([]));
    enabled(&td).await;
    assert!(dispatch(&state(&td), "p").await.unwrap());
}

#[tokio::test]
async fn retry_state_clears_on_moves_archives_and_direct_updates() {
    let td = setup(3, 3, 10).await;
    let mut cards = Vec::new();
    for _ in 0..3 {
        let id = card(&td, "small").await;
        td.agent("p", &id.to_ascii_lowercase(), json!({})).await;
        cards.push(id);
    }
    enabled(&td).await;
    terrariumd::card_dispatcher::pass(&state(&td)).await;
    assert_eq!(deferred_counts(&td).await.0, 3);
    td.post(
        &format!("/api/cards/{}/move", cards[0]),
        json!({"column":"backlog"}),
    )
    .await
    .ok();
    td.delete(&format!("/api/cards/{}", cards[1])).await.ok();
    let id = cards[2].clone();
    state(&td).db.write("operator", move |t| {
        t.execute("UPDATE cards SET column_id = (SELECT id FROM board_columns WHERE project = 'p' AND name = 'backlog') WHERE id = ?1", [&id])?;
        Ok(())
    }).await.unwrap();
    assert_eq!(deferred_counts(&td).await.0, 0);
}

#[tokio::test]
async fn done_dependency_uses_role_with_legacy_name_fallback() {
    let td = setup(3, 3, 10).await;
    let dep = card(&td, "unknown").await;
    let id = card(&td, "small").await;
    td.patch(&format!("/api/cards/{id}"), json!({"depends_on":[dep]}))
        .await
        .ok();
    td.post(&format!("/api/cards/{dep}/move"), json!({"column":"done"}))
        .await
        .ok();
    assert_eq!(get(&td, &id).await["blocked_by"], json!([]));
    // The current column-edit API still fixes done's name. Exercise the
    // role-based read/claim path with the schema used by custom column roles.
    state(&td).db.write("operator", |t| {
        t.execute("UPDATE board_columns SET name = 'shipped', role = 'done' WHERE project = 'p' AND name = 'done'", [])?;
        Ok(())
    }).await.unwrap();
    assert_eq!(get(&td, &id).await["blocked_by"], json!([]));
    enabled(&td).await;
    assert!(dispatch(&state(&td), "p").await.unwrap());
}

#[tokio::test]
async fn template_denials_back_off_one_card_and_allow_later_templates() {
    for target_denial in [false, true] {
        let td = setup(3, 3, 10).await;
        let lead = lead_id(&td).await;
        if target_denial {
            td.post(
                "/api/targets",
                json!({"name":"restricted","kind":"ssh","address":"u@h"}),
            )
            .await
            .ok();
            td.patch(
                "/api/templates/p%2Fcard-small",
                json!({"spec":{
                    "harness":"codex","policy":{"targets":["restricted"]}
                }}),
            )
            .await
            .ok();
        } else {
            td.patch(
                &format!("/api/agents/{lead}"),
                json!({"policy":{"spawn":{
                    "templates":["card-impl"],"bound":true,"max_concurrent":3
                }}}),
            )
            .await
            .ok();
        }
        let bad = card(&td, "small").await;
        let good = card(&td, "impl").await;
        enabled(&td).await;
        terrariumd::card_dispatcher::pass(&state(&td)).await;
        assert_eq!(get(&td, &bad).await["column"], "ready");
        assert_eq!(
            get(&td, &bad).await["comments"].as_array().unwrap().len(),
            1
        );
        assert_eq!(get(&td, &good).await["column"], "coding");
        assert_eq!(deferred_counts(&td).await, (1, 0));
        terrariumd::card_dispatcher::pass(&state(&td)).await;
        assert_eq!(
            get(&td, &bad).await["comments"].as_array().unwrap().len(),
            1
        );
    }
}

async fn lead_actor(td: &TestDaemon) -> terrariumd::actor::Actor {
    let a = state(td)
        .db
        .read(|c| Ok(terrariumd::store::agents::find(c, "lead", Some("p"))?.agent))
        .await
        .unwrap();
    terrariumd::actor::Actor::Agent(terrariumd::actor::AgentActor {
        id: a.id.to_string(),
        name: a.name,
        project: a.project,
        roles: a.roles,
        policy: a.policy,
    })
}

#[tokio::test]
async fn cap_reached_after_selection_defers_without_card_failure() {
    let td = setup(3, 1, 10).await;
    let id = card(&td, "small").await;
    enabled(&td).await;
    let who = lead_actor(&td).await;
    // Selection has a free place. A manual spawn takes it before the
    // dispatcher's spawn transaction. Exercise the transaction's cap error,
    // not the dispatcher's preflight shortcut.
    td.post_as(
        &who.address(),
        &format!("/api/agents/{}/spawn", who.address()),
        json!({"template":"card-impl", "task":"Take the available spawn place"}),
    )
    .await
    .ok();
    for _ in 0..2 {
        let error = terrariumd::spawn::spawn(
            &state(&td),
            &who,
            who.address(),
            terrariumd::spawn::SpawnBody {
                like: None,
                like_skip_missing: false,
                template: "card-small".into(),
                task: "Selected card".into(),
                name: Some(id.to_ascii_lowercase()),
                wait: false,
                timeout: None,
                project: None,
                targets: vec![],
                grants: vec![],
                bind: Some(format!("card:{id}")),
            },
            None,
            None,
        )
        .await
        .unwrap_err();
        assert_eq!(
            error.details.as_ref().unwrap()["spawn_cap"],
            "max_concurrent"
        );
        assert!(
            !terrariumd::card_dispatcher::handle_spawn_error(
                &state(&td),
                "p",
                id.clone(),
                who.clone(),
                error
            )
            .await
            .unwrap()
        );
    }
    assert_eq!(deferred_counts(&td).await, (0, 1));
    assert_eq!(get(&td, &id).await["comments"], json!([]));
    assert_eq!(get(&td, &id).await["column"], "ready");
}

#[tokio::test]
async fn selection_changed_is_not_a_card_failure() {
    let td = setup(3, 3, 10).await;
    let id = card(&td, "small").await;
    let error = terrariumd::error::ApiError::conflict("dispatcher selection changed")
        .with_details(json!({"dispatcher_selection_changed":true}));
    assert!(
        !terrariumd::card_dispatcher::handle_spawn_error(
            &state(&td),
            "p",
            id.clone(),
            lead_actor(&td).await,
            error
        )
        .await
        .unwrap()
    );
    assert_eq!(deferred_counts(&td).await, (0, 0));
    assert_eq!(get(&td, &id).await["comments"], json!([]));
    enabled(&td).await;
    assert!(dispatch(&state(&td), "p").await.unwrap());
}
