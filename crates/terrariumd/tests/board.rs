//! The kanban board and its rules (PLAN §13).
#![allow(clippy::unwrap_used, clippy::expect_used)]

mod common;

use common::TestDaemon;
use serde_json::{Value, json};

async fn setup() -> TestDaemon {
    let td = TestDaemon::start().await;
    td.post(
        "/api/projects",
        json!({"name": "proj", "card_prefix": "TER"}),
    )
    .await
    .ok();
    td.agent("proj", "lead", json!({"roles": ["project_admin"]}))
        .await;
    td.agent("proj", "w1", json!({})).await;
    td.agent("proj", "w2", json!({})).await;
    td.project("other").await;
    td.agent("other", "x", json!({})).await;
    td
}

async fn card(td: &TestDaemon, agent: &str, body: Value) -> String {
    td.post_as(agent, "/api/projects/proj/cards", body)
        .await
        .ok()["id"]
        .as_str()
        .unwrap()
        .to_owned()
}

#[tokio::test]
async fn board_shape_and_card_ids() {
    let td = setup().await;
    let b = td.get("/api/projects/proj/board").await;
    let b = b.ok();
    assert_eq!(b["card_prefix"], "TER");
    let cols: Vec<&str> = b["columns"]
        .as_array()
        .unwrap()
        .iter()
        .map(|c| c["name"].as_str().unwrap())
        .collect();
    assert_eq!(
        cols,
        [
            "backlog",
            "ready",
            "in_progress",
            "review",
            "blocked",
            "done"
        ]
    );
    let a = card(&td, "proj/w1", json!({"title": "One", "labels": ["x"]})).await;
    let b2 = card(&td, "proj/w1", json!({"title": "Two", "column": "ready"})).await;
    assert_eq!(a, "TER-1");
    assert_eq!(b2, "TER-2");
    let c = td.get("/api/cards/ter-1").await;
    let c = c.ok();
    assert_eq!(c["column"], "backlog");
    assert_eq!(c["created_by_name"], "proj/w1");
    assert_eq!(c["labels"], json!(["x"]));
    let b = td.get("/api/projects/proj/board").await;
    assert_eq!(b.ok()["cards"].as_array().unwrap().len(), 2);
    // Another project's agent cannot see the board.
    td.get_as("other/x", "/api/projects/proj/board")
        .await
        .err(403, "forbidden");
    td.post_as(
        "other/x",
        "/api/projects/proj/cards",
        json!({"title": "no"}),
    )
    .await
    .err(403, "forbidden");
    td.post("/api/projects/proj/cards", json!({"title": " "}))
        .await
        .err(422, "invalid");
    // List with filters and paging.
    let l = td
        .get("/api/projects/proj/cards?column=ready&limit=10")
        .await;
    assert_eq!(l.ok().as_array().unwrap().len(), 1);
    let l = td.get("/api/projects/proj/cards?label=x").await;
    assert_eq!(l.ok()[0]["id"], "TER-1");
    let l = td.get("/api/projects/proj/cards?limit=1").await;
    assert_eq!(l.next_cursor().as_deref(), Some("1"));
}

#[tokio::test]
async fn members_move_only_their_own_cards() {
    let td = setup().await;
    let id = card(&td, "proj/lead", json!({"title": "Work"})).await;
    // Unassigned and made by someone else: not yours.
    td.post_as(
        "proj/w1",
        &format!("/api/cards/{id}/move"),
        json!({"column": "ready"}),
    )
    .await
    .err(403, "forbidden");
    // Claim it: assigned to w1 and in_progress.
    let claimed = td
        .post_empty_as("proj/w1", &format!("/api/cards/{id}/claim"))
        .await;
    let claimed = claimed.ok();
    assert_eq!(claimed["column"], "in_progress");
    assert_eq!(claimed["assignee_name"], "proj/w1");
    // w2 cannot move, claim, or reassign w1's card.
    td.post_as(
        "proj/w2",
        &format!("/api/cards/{id}/move"),
        json!({"column": "review"}),
    )
    .await
    .err(403, "forbidden");
    td.patch_as(
        "proj/w2",
        &format!("/api/cards/{id}"),
        json!({"column": "review"}),
    )
    .await
    .err(403, "forbidden");
    td.post_empty_as("proj/w2", &format!("/api/cards/{id}/claim"))
        .await
        .err(409, "conflict");
    td.post_as(
        "proj/w2",
        &format!("/api/cards/{id}/assign"),
        json!({"assignee": "w2"}),
    )
    .await
    .err(403, "forbidden");
    // But it may comment.
    let c = td
        .post_as(
            "proj/w2",
            &format!("/api/cards/{id}/comments"),
            json!({"body": "looks good"}),
        )
        .await;
    assert_eq!(c.status, 201);
    // The owner moves it; the admin reassigns it.
    td.post_as(
        "proj/w1",
        &format!("/api/cards/{id}/move"),
        json!({"column": "review", "comment": "check the tests"}),
    )
    .await
    .ok();
    let r = td
        .post_as(
            "proj/lead",
            &format!("/api/cards/{id}/assign"),
            json!({"assignee": "w2"}),
        )
        .await;
    assert_eq!(r.ok()["assignee_name"], "proj/w2");
    // Assignees come from the card's project.
    td.post(
        &format!("/api/cards/{id}/assign"),
        json!({"assignee": "other/x"}),
    )
    .await
    .err(422, "invalid");
    // The reassign is audited.
    let audit = td.get("/api/audit?action=card.reassign").await;
    assert_eq!(audit.ok().as_array().unwrap().len(), 1);
    // The assignee's event stream has the assignment.
    let w2 = td.get("/api/agents/proj%2Fw2").await.ok().clone();
    let ev = td
        .get(&format!(
            "/api/agents/{}/events?kind=card",
            w2["id"].as_str().unwrap()
        ))
        .await;
    assert!(
        ev.ok()
            .as_array()
            .unwrap()
            .iter()
            .any(|e| e["data"]["change"] == "assigned")
    );
    // The card history.
    let hist = td.get(&format!("/api/cards/{id}/events")).await;
    let changes: Vec<&str> = hist
        .ok()
        .as_array()
        .unwrap()
        .iter()
        .map(|e| e["data"]["change"].as_str().unwrap())
        .collect();
    assert_eq!(
        changes,
        [
            "created",
            "claimed",
            "moved",
            "commented",
            "moved",
            "assigned"
        ]
    );
    assert!(hist.body[0]["data"]["card_id"] == id.as_str());
}

#[tokio::test]
async fn dependencies_block_in_progress() {
    let td = setup().await;
    let dep = card(&td, "proj/w1", json!({"title": "First"})).await;
    let id = card(
        &td,
        "proj/w2",
        json!({"title": "Second", "depends_on": [dep]}),
    )
    .await;
    let r = td
        .post_empty_as("proj/w2", &format!("/api/cards/{id}/claim"))
        .await;
    r.err(409, "dependencies_unmet");
    assert_eq!(r.body["error"]["details"]["blocked_by"], json!([dep]));
    // The failed claim changed nothing.
    let c = td.get(&format!("/api/cards/{id}")).await;
    assert!(c.ok()["assignee"].is_null());
    assert_eq!(c.body["blocked_by"], json!([dep]));
    // A cycle is refused.
    td.patch(&format!("/api/cards/{dep}"), json!({"depends_on": [id]}))
        .await
        .err(422, "invalid");

    td.post_empty_as("proj/w1", &format!("/api/cards/{dep}/claim"))
        .await
        .ok();
    td.post_as(
        "proj/w1",
        &format!("/api/cards/{dep}/move"),
        json!({"column": "done"}),
    )
    .await
    .ok();
    // The dependent card raises `unblocked`.
    let ev = td.get(&format!("/api/cards/{id}/events")).await;
    assert!(
        ev.ok()
            .as_array()
            .unwrap()
            .iter()
            .any(|e| e["data"]["change"] == "unblocked")
    );
    let ok = td
        .post_empty_as("proj/w2", &format!("/api/cards/{id}/claim"))
        .await;
    assert_eq!(ok.ok()["column"], "in_progress");
}

#[tokio::test]
async fn wip_limits_and_columns() {
    let td = setup().await;
    let cols = json!({"columns": [
        {"name": "backlog"},
        {"name": "ready"},
        {"name": "in_progress", "wip_limit": 1},
        {"name": "qa", "rename_from": "review"},
        {"name": "blocked"},
        {"name": "done"}
    ]});
    td.patch_as("proj/w1", "/api/projects/proj/board", cols.clone())
        .await
        .err(403, "forbidden");
    let b = td
        .patch_as("proj/lead", "/api/projects/proj/board", cols)
        .await;
    let b = b.ok();
    assert_eq!(b["columns"][2]["wip_limit"], 1);
    assert_eq!(b["columns"][3]["name"], "qa");
    // Required columns stay.
    td.patch(
        "/api/projects/proj/board",
        json!({"columns": [{"name": "backlog"}, {"name": "done"}]}),
    )
    .await
    .err(422, "invalid");

    let a = card(&td, "proj/w1", json!({"title": "A"})).await;
    let b = card(&td, "proj/w2", json!({"title": "B"})).await;
    td.post_empty_as("proj/w1", &format!("/api/cards/{a}/claim"))
        .await
        .ok();
    let r = td
        .post_empty_as("proj/w2", &format!("/api/cards/{b}/claim"))
        .await;
    r.err(409, "wip_limit");
    assert_eq!(r.body["error"]["details"]["wip_limit"], 1);
    // A column with cards cannot go.
    td.patch(
        "/api/projects/proj/board",
        json!({"columns": [
            {"name": "backlog"}, {"name": "in_progress"}, {"name": "blocked"}, {"name": "done"}
        ]}),
    )
    .await
    .ok();
    td.post(&format!("/api/cards/{b}/move"), json!({"column": "qa"}))
        .await
        .err(422, "invalid");
}

#[tokio::test]
async fn blocked_needs_a_comment_and_mails() {
    let td = setup().await;
    let id = card(&td, "proj/w1", json!({"title": "Hard"})).await;
    td.post_as(
        "proj/lead",
        &format!("/api/cards/{id}/assign"),
        json!({"assignee": "w2"}),
    )
    .await
    .ok();
    td.post_as(
        "proj/w2",
        &format!("/api/cards/{id}/move"),
        json!({"column": "blocked"}),
    )
    .await
    .err(422, "comment_required");
    td.patch_as(
        "proj/w2",
        &format!("/api/cards/{id}"),
        json!({"column": "blocked", "comment": "  "}),
    )
    .await
    .err(422, "comment_required");
    let c = td
        .patch_as(
            "proj/w2",
            &format!("/api/cards/{id}"),
            json!({"column": "blocked", "comment": "need DB access"}),
        )
        .await;
    let c = c.ok();
    assert_eq!(c["column"], "blocked");
    assert_eq!(c["comments"][0]["body"], "need DB access");
    // The operator and the creator (w1) get mail.
    let ops = td.get("/api/mail?to=operator").await;
    let ops = ops.ok().as_array().unwrap().clone();
    assert_eq!(ops.len(), 1);
    assert_eq!(ops[0]["from"], "project:proj");
    assert!(
        ops[0]["subject"]
            .as_str()
            .unwrap()
            .starts_with(&format!("{id} blocked"))
    );
    assert!(ops[0]["body"].as_str().unwrap().contains("need DB access"));
    let w1 = td.mail_of("proj/w1").await;
    assert_eq!(w1.len(), 1);
    assert_eq!(w1[0]["thread_id"], ops[0]["thread_id"]);
}

#[tokio::test]
async fn done_is_terminal_for_members() {
    let td = setup().await;
    let id = card(&td, "proj/w1", json!({"title": "Ship"})).await;
    td.post_empty_as("proj/w1", &format!("/api/cards/{id}/claim"))
        .await
        .ok();
    td.post_as(
        "proj/w1",
        &format!("/api/cards/{id}/move"),
        json!({"column": "done"}),
    )
    .await
    .ok();
    td.post_as(
        "proj/w1",
        &format!("/api/cards/{id}/move"),
        json!({"column": "ready"}),
    )
    .await
    .err(403, "forbidden");
    td.patch_as(
        "proj/w1",
        &format!("/api/cards/{id}"),
        json!({"title": "edit"}),
    )
    .await
    .err(403, "forbidden");
    // Members still comment on a done card.
    td.post_as(
        "proj/w1",
        &format!("/api/cards/{id}/comments"),
        json!({"body": "shipped"}),
    )
    .await
    .ok();
    // A project admin reopens it, and the reopen is audited.
    let r = td
        .post_as(
            "proj/lead",
            &format!("/api/cards/{id}/move"),
            json!({"column": "ready"}),
        )
        .await;
    assert_eq!(r.ok()["column"], "ready");
    let audit = td.get("/api/audit?action=card.reopen").await;
    assert_eq!(audit.ok().as_array().unwrap().len(), 1);
    // Members cannot make a card in done.
    td.post_as(
        "proj/w1",
        "/api/projects/proj/cards",
        json!({"title": "x", "column": "done"}),
    )
    .await
    .err(403, "forbidden");
}

#[tokio::test]
async fn edit_link_archive() {
    let td = setup().await;
    let id = card(&td, "proj/w1", json!({"title": "Edit me"})).await;
    let e = td
        .patch_as(
            "proj/w1",
            &format!("/api/cards/{id}"),
            json!({"body": "more", "branch": "feat/x", "labels": ["a", "b"]}),
        )
        .await;
    let e = e.ok();
    assert_eq!(e["branch"], "feat/x");
    // Sending back the same column is no move.
    let same = td
        .patch_as(
            "proj/w1",
            &format!("/api/cards/{id}"),
            json!({"column": "backlog", "title": "Edited"}),
        )
        .await;
    assert_eq!(same.ok()["title"], "Edited");
    let hist = td.get(&format!("/api/cards/{id}/events")).await;
    assert!(
        !hist
            .ok()
            .as_array()
            .unwrap()
            .iter()
            .any(|e| e["data"]["change"] == "reordered")
    );
    assert_eq!(e["labels"], json!(["a", "b"]));
    td.patch_as(
        "proj/w2",
        &format!("/api/cards/{id}"),
        json!({"body": "mine now"}),
    )
    .await
    .err(403, "forbidden");
    let l = td
        .post_as(
            "proj/w1",
            &format!("/api/cards/{id}/links"),
            json!({"kind": "branch", "ref": "feat/x"}),
        )
        .await;
    assert_eq!(l.ok()["links"][0]["ref"], "feat/x");
    td.post_as(
        "proj/w1",
        &format!("/api/cards/{id}/links"),
        json!({"kind": "thread", "ref": "01NOPE"}),
    )
    .await
    .err(404, "not_found");
    td.post_as(
        "proj/w1",
        &format!("/api/cards/{id}/links"),
        json!({"kind": "bogus", "ref": "x"}),
    )
    .await
    .err(422, "invalid");
    // Unassign with null.
    td.patch_as(
        "proj/w1",
        &format!("/api/cards/{id}"),
        json!({"assignee": "me"}),
    )
    .await
    .ok();
    let u = td
        .patch_as(
            "proj/w1",
            &format!("/api/cards/{id}"),
            json!({"assignee": null}),
        )
        .await;
    assert!(u.ok()["assignee"].is_null());
    // Archive: the creator may, w2 may not.
    td.req(
        hyper::Method::DELETE,
        &format!("/api/cards/{id}"),
        None,
        Some("proj/w2"),
    )
    .await
    .err(403, "forbidden");
    let d = td
        .req(
            hyper::Method::DELETE,
            &format!("/api/cards/{id}"),
            None,
            Some("proj/w1"),
        )
        .await;
    assert_eq!(d.status, 204);
    let b = td.get("/api/projects/proj/board").await;
    assert!(b.ok()["cards"].as_array().unwrap().is_empty());
    let c = td.get(&format!("/api/cards/{id}")).await;
    assert_eq!(c.ok()["archived"], true);
    let arch = td.get("/api/projects/proj/cards?archived=true").await;
    assert_eq!(arch.ok().as_array().unwrap().len(), 1);
}
