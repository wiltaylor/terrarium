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
    // `system` only sends mail; it takes no card.
    td.post(
        &format!("/api/cards/{id}/assign"),
        json!({"assignee": "system"}),
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

/// Issue #13: a patch with `"depends_on": []` removes every dependency
/// (and `"labels": []` every label). `blocked_by` follows, the edit is an
/// `edited` event, and the card can be claimed.
#[tokio::test]
async fn an_empty_list_clears_dependencies_and_labels() {
    let td = setup().await;
    let a = card(&td, "proj/w1", json!({"title": "A"})).await;
    let b = card(&td, "proj/w1", json!({"title": "B"})).await;
    let id = card(
        &td,
        "proj/w2",
        json!({"title": "C", "depends_on": [a, b], "labels": ["x", "y"]}),
    )
    .await;
    let c = td.get(&format!("/api/cards/{id}")).await;
    assert_eq!(c.ok()["blocked_by"], json!([a, b]));
    // A patch with no `depends_on` keeps them.
    let kept = td
        .patch_as(
            "proj/w2",
            &format!("/api/cards/{id}"),
            json!({"title": "C2"}),
        )
        .await;
    assert_eq!(kept.ok()["depends_on"], json!([a, b]));
    // A shorter list replaces them.
    let one = td
        .patch_as(
            "proj/w2",
            &format!("/api/cards/{id}"),
            json!({"depends_on": [b]}),
        )
        .await;
    assert_eq!(one.ok()["depends_on"], json!([b]));
    assert_eq!(one.body["blocked_by"], json!([b]));
    // The empty list removes them all.
    let cleared = td
        .patch_as(
            "proj/w2",
            &format!("/api/cards/{id}"),
            json!({"depends_on": [], "labels": []}),
        )
        .await;
    let cleared = cleared.ok();
    assert_eq!(cleared["depends_on"], json!([]));
    assert_eq!(cleared["blocked_by"], json!([]));
    assert_eq!(cleared["labels"], json!([]));
    let again = td.get(&format!("/api/cards/{id}")).await;
    assert_eq!(again.ok()["depends_on"], json!([]));
    let ev = td.get(&format!("/api/cards/{id}/events")).await;
    let last_edit = ev
        .ok()
        .as_array()
        .unwrap()
        .iter()
        .rfind(|e| e["data"]["change"] == "edited")
        .cloned()
        .unwrap();
    assert_eq!(last_edit["data"]["fields"], json!(["labels", "depends_on"]));
    // No open dependency: the claim works.
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
    assert_eq!(ops[0]["from"], "system");
    assert_eq!(ops[0]["from_kind"], "system");
    assert_eq!(ops[0]["from_name"], "system");
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
    // A reply on the thread goes to the people on it, not to `system`.
    let r = td
        .post(
            &format!("/api/mail/{}/reply", ops[0]["id"].as_str().unwrap()),
            json!({"body": "granted"}),
        )
        .await;
    let r = r.ok().as_array().unwrap().clone();
    assert_eq!(r.len(), 1, "{r:?}");
    assert_eq!(r[0]["to_name"], "proj/w1");
    // Nobody can send to `system` or as it.
    td.post("/api/mail", json!({"to": "system", "body": "x"}))
        .await
        .err(422, "invalid");
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

#[tokio::test]
async fn member_may_assign_an_unassigned_card() {
    let td = setup().await;
    let id = card(&td, "proj/w1", json!({"title": "Free"})).await;
    // A member assigns an unassigned card to a peer (PLAN §13).
    let r = td
        .post_as(
            "proj/w1",
            &format!("/api/cards/{id}/assign"),
            json!({"assignee": "w2"}),
        )
        .await;
    assert_eq!(r.ok()["assignee_name"], "proj/w2");
    // Once it is w2's, w1 may not move it on.
    td.post_as(
        "proj/w1",
        &format!("/api/cards/{id}/assign"),
        json!({"assignee": "w1"}),
    )
    .await
    .err(403, "forbidden");
}

/// After `agent rm`, the card keeps the name of its assignee and creator,
/// and the comments keep the name of their author (migration 12).
#[tokio::test]
async fn names_stay_after_agent_rm() {
    let td = setup().await;
    let id = card(&td, "proj/w1", json!({"title": "Gone soon"})).await;
    td.post_as("proj/w1", &format!("/api/cards/{id}/claim"), json!({}))
        .await
        .ok();
    td.post_as(
        "proj/w1",
        &format!("/api/cards/{id}/comments"),
        json!({"body": "on it"}),
    )
    .await
    .ok();
    let w1 = td.get("/api/agents/proj%2Fw1").await.ok().clone();
    let w1_id = w1["id"].as_str().unwrap().to_owned();
    let r = td.delete(&format!("/api/agents/{w1_id}")).await;
    assert_eq!(r.status, 204, "{}", r.body);
    td.get("/api/agents/proj%2Fw1").await.err(404, "not_found");
    let c = td.get(&format!("/api/cards/{id}")).await.ok().clone();
    assert_eq!(c["assignee"], w1_id.as_str());
    assert_eq!(c["assignee_name"], "proj/w1");
    assert_eq!(c["created_by_name"], "proj/w1");
    assert_eq!(c["comments"][0]["author_name"], "proj/w1", "{c:#}");
    // The assignee filter still takes the removed agent (the tombstone);
    // a name that never was an agent stays not found.
    for key in [w1_id.as_str(), "proj/w1", "w1"] {
        let l = td
            .get(&format!("/api/projects/proj/cards?assignee={key}"))
            .await;
        let l = l.ok().as_array().unwrap().clone();
        assert_eq!(l.len(), 1, "assignee={key}: {l:#?}");
        assert_eq!(l[0]["id"], id.as_str());
    }
    td.get("/api/projects/proj/cards?assignee=proj/nobody")
        .await
        .err(404, "not_found");
}

/// The ETA of a card (PLAN §13): set and clear with PATCH, the limits, the
/// rights of an edit, the `eta` card event, and the audit of a change to
/// another's card.
#[tokio::test]
async fn eta_set_clear_and_rules() {
    let td = setup().await;
    let id = card(&td, "proj/w1", json!({"title": "Estimate", "eta_at": "2h"})).await;
    let path = format!("/api/cards/{id}");
    let c = td.get(&path).await.ok().clone();
    let at = chrono::DateTime::parse_from_rfc3339(c["eta_at"].as_str().unwrap()).unwrap();
    let left = at.with_timezone(&chrono::Utc) - chrono::Utc::now();
    assert!(
        left.num_minutes() > 118 && left.num_minutes() <= 120,
        "{left}"
    );
    assert!(c["eta_at"].as_str().unwrap().ends_with('Z'));
    assert!(c["eta_set_at"].is_string());
    assert_eq!(c["eta_set_by_name"], "proj/w1");
    // A compound duration and an RFC 3339 time.
    let c = td
        .patch_as("proj/w1", &path, json!({"eta_at": "1h30m"}))
        .await
        .ok()
        .clone();
    assert!(c["eta_at"].is_string());
    let later = (chrono::Utc::now() + chrono::Duration::hours(3)).to_rfc3339();
    let c = td
        .patch_as("proj/w1", &path, json!({"eta_at": later}))
        .await
        .ok()
        .clone();
    let at = chrono::DateTime::parse_from_rfc3339(c["eta_at"].as_str().unwrap()).unwrap();
    assert!((at.with_timezone(&chrono::Utc) - chrono::Utc::now()).num_minutes() >= 179);
    // The limits: 422 with a clear message.
    for (bad, says) in [
        ("2000-01-01T00:00:00Z", "past"),
        ("31d", "30 days"),
        ("soon", "RFC 3339"),
        ("0m", "more than zero"),
    ] {
        let r = td.patch_as("proj/w1", &path, json!({"eta_at": bad})).await;
        let body = r.body.clone();
        r.err(422, "invalid");
        assert!(body.to_string().contains(says), "{bad}: {body}");
    }
    // Another member may not set it; the owner's edits are not audited.
    td.patch_as("proj/w2", &path, json!({"eta_at": "1h"}))
        .await
        .err(403, "forbidden");
    let audit = td.get("/api/audit?action=card.eta").await;
    assert!(audit.ok().as_array().unwrap().is_empty());
    // The operator sets it on w1's card: audited.
    td.patch(&path, json!({"eta_at": "4h"})).await.ok();
    let audit = td.get("/api/audit?action=card.eta").await.ok().clone();
    assert_eq!(audit.as_array().unwrap().len(), 1);
    // Each change is a card event with `change: eta`.
    let ev = td.get(&format!("{path}/events")).await.ok().clone();
    let etas: Vec<&Value> = ev
        .as_array()
        .unwrap()
        .iter()
        .filter(|e| e["data"]["change"] == "eta")
        .collect();
    assert_eq!(etas.len(), 4, "{ev}");
    assert!(etas[3]["summary"].as_str().unwrap().contains("ETA"));
    assert!(etas[3]["data"]["previous"].is_string());
    // Clear it: `null`. A second clear is no change and no event.
    let c = td
        .patch_as("proj/w1", &path, json!({"eta_at": null}))
        .await
        .ok()
        .clone();
    assert!(c.get("eta_at").is_none() && c.get("eta_set_by").is_none());
    td.patch_as("proj/w1", &path, json!({"eta_at": null}))
        .await
        .ok();
    let ev = td.get(&format!("{path}/events")).await.ok().clone();
    let n = ev
        .as_array()
        .unwrap()
        .iter()
        .filter(|e| e["data"]["change"] == "eta")
        .count();
    assert_eq!(n, 5);
}

/// A done card keeps its ETA and gets `done_at`; it takes no new ETA. A
/// reopen clears `done_at`.
#[tokio::test]
async fn eta_stays_on_done_and_done_at_is_set() {
    let td = setup().await;
    let id = card(&td, "proj/w1", json!({"title": "Ship"})).await;
    let path = format!("/api/cards/{id}");
    td.post_empty_as("proj/w1", &format!("{path}/claim"))
        .await
        .ok();
    let c = td
        .patch_as("proj/w1", &path, json!({"eta_at": "45m"}))
        .await
        .ok()
        .clone();
    assert!(c.get("done_at").is_none());
    let eta = c["eta_at"].clone();
    let c = td
        .post_as(
            "proj/w1",
            &format!("{path}/move"),
            json!({"column": "done"}),
        )
        .await
        .ok()
        .clone();
    assert_eq!(c["eta_at"], eta);
    assert!(c["done_at"].is_string());
    td.patch(&path, json!({"eta_at": "1h"}))
        .await
        .err(409, "conflict");
    let c = td
        .post(&format!("{path}/move"), json!({"column": "ready"}))
        .await
        .ok()
        .clone();
    assert!(c.get("done_at").is_none());
    assert_eq!(c["eta_at"], eta);
    // An archived card takes no ETA.
    td.delete(&path).await;
    td.patch(&path, json!({"eta_at": "1h"}))
        .await
        .err(409, "conflict");
}

// ---- configurable columns (PLAN §13) ----

async fn set_cols(td: &TestDaemon, cols: Value) -> common::Resp {
    td.patch("/api/projects/proj/board", json!({ "columns": cols }))
        .await
}

fn names(b: &Value) -> Vec<String> {
    b["columns"]
        .as_array()
        .unwrap()
        .iter()
        .map(|c| c["name"].as_str().unwrap().to_owned())
        .collect()
}

fn col<'a>(b: &'a Value, name: &str) -> &'a Value {
    b["columns"]
        .as_array()
        .unwrap()
        .iter()
        .find(|c| c["name"] == name)
        .unwrap()
}

#[tokio::test]
async fn new_boards_have_roles_and_titles() {
    let td = setup().await;
    let b = td.get("/api/projects/proj/board").await;
    let b = b.ok();
    assert_eq!(col(b, "in_progress")["role"], "working");
    assert_eq!(col(b, "in_progress")["title"], "in progress");
    assert_eq!(col(b, "blocked")["role"], "blocked");
    assert!(col(b, "ready").get("role").is_none());
    assert!(col(b, "backlog").get("role").is_none());
}

#[tokio::test]
async fn backlog_and_done_are_fixed() {
    let td = setup().await;
    let mid = json!([{"name": "in_progress", "role": "working"}]);
    let with = |first: Value, last: Value| {
        let mut v = vec![first];
        v.extend(mid.as_array().unwrap().iter().cloned());
        v.push(last);
        Value::Array(v)
    };
    for (cols, why) in [
        (
            json!([{"name": "in_progress", "role": "working"}, {"name": "done"}]),
            "no backlog",
        ),
        (
            json!([{"name": "backlog"}, {"name": "in_progress", "role": "working"}]),
            "no done",
        ),
        (
            json!([{"name": "in_progress", "role": "working"}, {"name": "backlog"}, {"name": "done"}]),
            "backlog not first",
        ),
        (
            json!([{"name": "backlog"}, {"name": "done"}, {"name": "in_progress", "role": "working"}]),
            "done not last",
        ),
        (
            with(
                json!({"name": "todo", "rename_from": "backlog"}),
                json!({"name": "done"}),
            ),
            "rename backlog",
        ),
        (
            with(
                json!({"name": "backlog"}),
                json!({"name": "finished", "rename_from": "done"}),
            ),
            "rename done",
        ),
        (
            with(
                json!({"name": "backlog", "wip_limit": 3}),
                json!({"name": "done"}),
            ),
            "wip on backlog",
        ),
        (
            with(
                json!({"name": "backlog"}),
                json!({"name": "done", "wip_limit": 3}),
            ),
            "wip on done",
        ),
        (
            with(
                json!({"name": "backlog", "role": "blocked"}),
                json!({"name": "done"}),
            ),
            "role on backlog",
        ),
        (
            json!([{"name": "backlog"}, {"name": "in_progress", "role": "working"}, {"name": "backlog"}, {"name": "done"}]),
            "backlog twice",
        ),
    ] {
        let r = set_cols(&td, cols).await;
        assert_eq!(r.status.as_u16(), 422, "{why}: {:?}", r.body);
    }
    // A title is allowed on a fixed column.
    let r = set_cols(
        &td,
        with(
            json!({"name": "backlog", "title": "Ideas"}),
            json!({"name": "done"}),
        ),
    )
    .await;
    let b = r.ok();
    assert_eq!(col(b, "backlog")["title"], "Ideas");
    assert_eq!(names(b), ["backlog", "in_progress", "done"]);
}

#[tokio::test]
async fn exactly_one_working_and_at_most_one_blocked() {
    let td = setup().await;
    for (cols, why) in [
        (
            json!([{"name": "backlog"}, {"name": "in_progress", "role": null}, {"name": "blocked"}, {"name": "done"}]),
            "no working",
        ),
        (
            json!([{"name": "backlog"}, {"name": "ready", "role": "working"}, {"name": "in_progress"}, {"name": "blocked"}, {"name": "done"}]),
            "two working",
        ),
        (
            json!([{"name": "backlog"}, {"name": "in_progress"}, {"name": "review", "role": "blocked"}, {"name": "blocked"}, {"name": "done"}]),
            "two blocked",
        ),
        (
            json!([{"name": "backlog"}, {"name": "in_progress", "role": "doing"}, {"name": "done"}]),
            "bad role",
        ),
        // `working` and `blocked` are aliases: only the role column has the name.
        (
            json!([{"name": "backlog"}, {"name": "in_progress"}, {"name": "working"}, {"name": "done"}]),
            "reserved name",
        ),
        (
            json!([{"name": "backlog"}, {"name": "in_progress"}, {"name": "Bad-Name"}, {"name": "done"}]),
            "bad name",
        ),
        (
            json!([{"name": "backlog"}, {"name": "in_progress", "title": "x".repeat(41)}, {"name": "done"}]),
            "long title",
        ),
    ] {
        let r = set_cols(&td, cols).await;
        assert_eq!(r.status.as_u16(), 422, "{why}: {:?}", r.body);
    }
    // No blocked column is allowed; the role moves with a swap.
    let b = set_cols(
        &td,
        json!([{"name": "backlog"}, {"name": "ready", "role": "working"},
               {"name": "in_progress", "role": null}, {"name": "done"}]),
    )
    .await;
    let b = b.ok();
    assert_eq!(col(b, "ready")["role"], "working");
    assert!(col(b, "in_progress").get("role").is_none());
}

#[tokio::test]
async fn rename_reorder_remove_and_claim_follows_the_role() {
    let td = setup().await;
    let a = card(&td, "proj/w1", json!({"title": "A", "column": "ready"})).await;
    // Rename in_progress to doing (with a title), move review first, and
    // remove nothing that has cards.
    let b = set_cols(
        &td,
        json!([
            {"name": "backlog"},
            {"name": "review"},
            {"name": "ready"},
            {"name": "doing", "rename_from": "in_progress", "title": "Doing now"},
            {"name": "blocked"},
            {"name": "done"}
        ]),
    )
    .await;
    let b = b.ok();
    assert_eq!(
        names(b),
        ["backlog", "review", "ready", "doing", "blocked", "done"]
    );
    assert_eq!(col(b, "doing")["role"], "working");
    assert_eq!(col(b, "doing")["title"], "Doing now");
    let claimed = td
        .post_empty_as("proj/w1", &format!("/api/cards/{a}/claim"))
        .await;
    assert_eq!(claimed.ok()["column"], "doing");
    // The agent's current card follows the role.
    let w1 = td.get("/api/agents/proj%2Fw1").await;
    assert_eq!(w1.ok()["current_card"], a);
    // `working` is an alias of the role column in a filter and a move.
    let mine = td
        .get_as("proj/w1", "/api/projects/proj/cards?column=working")
        .await;
    assert_eq!(mine.ok().as_array().unwrap().len(), 1);
    // A column with cards cannot go; the error lists them.
    let r = set_cols(
        &td,
        json!([{"name": "backlog"}, {"name": "ready", "role": "working"}, {"name": "done"}]),
    )
    .await;
    r.err(409, "conflict");
    assert_eq!(r.body["error"]["details"]["cards"], json!([a]));
    assert!(r.body["error"]["message"].as_str().unwrap().contains(&a));
    // Renamed again with the default title: the stored title goes.
    let b = set_cols(
        &td,
        json!([{"name": "backlog"}, {"name": "ready"},
               {"name": "work_now", "rename_from": "doing", "title": null},
               {"name": "blocked"}, {"name": "done"}]),
    )
    .await;
    let b = b.ok();
    assert_eq!(col(b, "work_now")["title"], "work now");
    assert_eq!(col(b, "work_now")["count"], 1);
    let c = td.get(&format!("/api/cards/{a}")).await;
    assert_eq!(c.ok()["column"], "work_now");
}

#[tokio::test]
async fn the_blocked_rules_follow_the_role() {
    let td = setup().await;
    set_cols(
        &td,
        json!([{"name": "backlog"}, {"name": "in_progress"},
               {"name": "stuck", "rename_from": "blocked", "role": "blocked"}, {"name": "done"}]),
    )
    .await
    .ok();
    let id = card(&td, "proj/w1", json!({"title": "Hard"})).await;
    td.post_empty_as("proj/w1", &format!("/api/cards/{id}/claim"))
        .await
        .ok();
    td.post_as(
        "proj/w1",
        &format!("/api/cards/{id}/move"),
        json!({"column": "stuck"}),
    )
    .await
    .err(422, "comment_required");
    // The alias `blocked` names the role column.
    let c = td
        .post_as(
            "proj/w1",
            &format!("/api/cards/{id}/move"),
            json!({"column": "blocked", "comment": "need a key"}),
        )
        .await;
    assert_eq!(c.ok()["column"], "stuck");
    let ops = td.get("/api/mail?to=operator").await;
    let ops = ops.ok().as_array().unwrap().clone();
    assert_eq!(ops.len(), 1);
    assert!(ops[0]["body"].as_str().unwrap().contains("moved to stuck"));
    let ev = td.get(&format!("/api/cards/{id}/events")).await;
    let moved = ev
        .ok()
        .as_array()
        .unwrap()
        .iter()
        .find(|e| e["data"]["change"] == "moved" && e["data"]["to"] == "stuck")
        .cloned()
        .unwrap();
    assert_eq!(moved["data"]["to_role"], "blocked");
    assert_eq!(moved["data"]["from_role"], "working");
    let listed = td.get("/api/projects/proj/cards?column=blocked").await;
    assert_eq!(listed.ok().as_array().unwrap().len(), 1);

    // With no blocked column, a move to `blocked` is refused by name.
    td.post_as(
        "proj/w1",
        &format!("/api/cards/{id}/move"),
        json!({"column": "in_progress"}),
    )
    .await
    .ok();
    set_cols(
        &td,
        json!([{"name": "backlog"}, {"name": "in_progress"},
               {"name": "on_hold", "rename_from": "stuck", "role": null}, {"name": "done"}]),
    )
    .await
    .ok();
    let r = td
        .post_as(
            "proj/w1",
            &format!("/api/cards/{id}/move"),
            json!({"column": "blocked", "comment": "x"}),
        )
        .await;
    r.err(422, "invalid");
    let m = r.body["error"]["message"].as_str().unwrap();
    assert!(
        m.contains("no `blocked` column") && m.contains("on_hold"),
        "{m}"
    );
    // `on_hold` is a plain column now: no comment needed.
    td.post_as(
        "proj/w1",
        &format!("/api/cards/{id}/move"),
        json!({"column": "on_hold"}),
    )
    .await
    .ok();
}

#[tokio::test]
async fn wip_limits_on_create_and_below_the_count() {
    let td = setup().await;
    let a = card(&td, "proj/w1", json!({"title": "A", "column": "ready"})).await;
    let b = card(&td, "proj/w1", json!({"title": "B", "column": "ready"})).await;
    // A limit under the count is allowed: the column is over its limit.
    let r = set_cols(
        &td,
        json!([{"name": "backlog"}, {"name": "ready", "wip_limit": 1}, {"name": "in_progress"},
               {"name": "review"}, {"name": "blocked"}, {"name": "done"}]),
    )
    .await;
    let bd = r.ok();
    assert_eq!(col(bd, "ready")["count"], 2);
    assert_eq!(col(bd, "ready")["wip_limit"], 1);
    // A new card in the full column is refused, and leaves nothing behind.
    let r = td
        .post_as(
            "proj/w1",
            "/api/projects/proj/cards",
            json!({"title": "C", "column": "ready"}),
        )
        .await;
    r.err(409, "wip_limit");
    let all = td.get("/api/projects/proj/cards").await;
    assert_eq!(all.ok().as_array().unwrap().len(), 2);
    let c = card(&td, "proj/w1", json!({"title": "C"})).await;
    td.post_as(
        "proj/w1",
        &format!("/api/cards/{c}/move"),
        json!({"column": "ready"}),
    )
    .await
    .err(409, "wip_limit");
    // Two cards leave: one more fits.
    for id in [&a, &b] {
        td.post_as(
            "proj/w1",
            &format!("/api/cards/{id}/move"),
            json!({"column": "review"}),
        )
        .await
        .ok();
    }
    td.post_as(
        "proj/w1",
        &format!("/api/cards/{c}/move"),
        json!({"column": "ready"}),
    )
    .await
    .ok();
}

#[tokio::test]
async fn columns_rights_audit_and_event() {
    let td = setup().await;
    let cols = json!({"columns": [
        {"name": "backlog"}, {"name": "in_progress", "wip_limit": 2},
        {"name": "qa", "title": "QA"}, {"name": "blocked"}, {"name": "done"}
    ]});
    let r = td
        .patch_as("proj/w1", "/api/projects/proj/board", cols.clone())
        .await;
    r.err(403, "forbidden");
    let m = r.body["error"]["message"].as_str().unwrap();
    assert!(
        m.contains("editing the board columns needs project_admin"),
        "{m}"
    );
    // An agent of another project may not.
    td.patch_as("other/x", "/api/projects/proj/board", cols.clone())
        .await
        .err(403, "forbidden");
    let b = td
        .patch_as("proj/lead", "/api/projects/proj/board", cols)
        .await;
    assert_eq!(col(b.ok(), "qa")["title"], "QA");
    let ev = td.get("/api/events?kind=board&project=proj").await;
    let ev = ev.ok().as_array().unwrap().last().unwrap().clone();
    let qa = ev["data"]["columns"]
        .as_array()
        .unwrap()
        .iter()
        .find(|c| c["name"] == "qa")
        .unwrap()
        .clone();
    assert_eq!(qa["title"], "QA");
    assert_eq!(
        ev["data"]["columns"][1],
        json!({"name": "in_progress", "title": null, "wip_limit": 2, "role": "working",
               "rename_from": null})
    );
    let audit = td.get("/api/audit?action=board.columns").await;
    assert_eq!(audit.ok().as_array().unwrap().len(), 1);
}

/// TER-43: `GET /api/cards` lists the cards in the working column and in
/// the `research` column of every board that the caller may read, with the
/// column kind and the assignee's status; backlog and archived cards stay
/// out.
#[tokio::test]
async fn active_cards_across_projects_follow_the_rights() {
    let td = setup().await;
    td.project("ops").await;
    td.agent("ops", "root", json!({"roles": ["sysadmin"]}))
        .await;
    td.patch(
        "/api/projects/proj/board",
        json!({"columns": [{"name": "backlog"}, {"name": "ready"}, {"name": "research"},
                           {"name": "in_progress"}, {"name": "review"}, {"name": "blocked"},
                           {"name": "done"}]}),
    )
    .await
    .ok();
    let k1 = card(&td, "proj/w1", json!({"title": "Parser"})).await;
    td.post_empty_as("proj/w1", &format!("/api/cards/{k1}/claim"))
        .await
        .ok();
    let k2 = card(&td, "proj/w2", json!({"title": "Spike"})).await;
    td.post(
        &format!("/api/cards/{k2}/move"),
        json!({"column": "research"}),
    )
    .await
    .ok();
    td.post(&format!("/api/cards/{k2}/assign"), json!({"agent": "proj/w2"}))
        .await
        .ok();
    let k3 = card(&td, "proj/w1", json!({"title": "Later"})).await;
    let k4 = card(&td, "proj/w2", json!({"title": "Gone"})).await;
    td.post_empty_as("proj/w2", &format!("/api/cards/{k4}/claim"))
        .await
        .ok();
    td.delete(&format!("/api/cards/{k4}")).await;
    let x = td
        .post_as("other/x", "/api/projects/other/cards", json!({"title": "Other"}))
        .await
        .ok()["id"]
        .as_str()
        .unwrap()
        .to_owned();
    td.post_empty_as("other/x", &format!("/api/cards/{x}/claim"))
        .await
        .ok();

    let ids = |v: &Value| -> Vec<String> {
        v.as_array()
            .unwrap()
            .iter()
            .map(|c| c["id"].as_str().unwrap().to_owned())
            .collect()
    };
    // The operator: every project, by project; the backlog card and the
    // archived one stay out.
    let all = td.get("/api/cards").await.ok().clone();
    assert_eq!(ids(&all), [x.clone(), k2.clone(), k1.clone()], "{all:#?}");
    assert!(!ids(&all).contains(&k3) && !ids(&all).contains(&k4));
    let one = |id: &str| all.as_array().unwrap().iter().find(|c| c["id"] == id).unwrap().clone();
    assert_eq!(one(&k1)["column_kind"], "working");
    assert_eq!(one(&k1)["column"], "in_progress");
    assert_eq!(one(&k1)["project"], "proj");
    assert_eq!(one(&k1)["assignee_name"], "proj/w1");
    let w1 = td.get("/api/agents/proj/w1").await.ok().clone();
    assert_eq!(one(&k1)["assignee_status"], w1["status"]);
    assert_eq!(one(&k2)["column_kind"], "research");
    assert_eq!(one(&k2)["assignee_name"], "proj/w2");
    // A sysadmin agent reads all; another agent only its own project.
    let sys = td.get_as("ops/root", "/api/cards").await.ok().clone();
    assert_eq!(ids(&sys), ids(&all));
    let mine = td.get_as("proj/w1", "/api/cards").await.ok().clone();
    assert_eq!(ids(&mine), [k2.clone(), k1.clone()]);
    let theirs = td.get_as("other/x", "/api/cards").await.ok().clone();
    assert_eq!(ids(&theirs), [x]);
    // Other columns on request.
    let r = td.get("/api/cards?columns=working").await.ok().clone();
    assert_eq!(ids(&r).len(), 2, "{r:#?}");
    let r = td.get("/api/cards?columns=backlog").await.ok().clone();
    assert_eq!(ids(&r), [k3]);
}
