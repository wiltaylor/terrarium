//! The options of a board column (TER-1112, kanban rework C1a): the move
//! checks as column options with today's defaults, the stored queue
//! settings, the versioned GET and PUT, and who may write them.
#![allow(clippy::unwrap_used, clippy::expect_used)]

use crate::common;

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
    td.project("ops").await;
    td.agent("ops", "root", json!({"roles": ["sysadmin"]}))
        .await;
    td.agent("ops", "arch", json!({"roles": ["architect"]}))
        .await;
    td.project("other").await;
    td.agent("other", "x", json!({})).await;
    td.agent("other", "boss", json!({"roles": ["project_admin"]}))
        .await;
    td
}

fn path(column: &str) -> String {
    format!("/api/projects/proj/board/columns/{column}/options")
}

async fn options(td: &TestDaemon, column: &str) -> Value {
    td.get(&path(column)).await.ok().clone()
}

/// The `PUT` body of `o` after `f` changed it.
fn changed(o: &Value, f: impl FnOnce(&mut Value)) -> Value {
    let mut b = json!({"base_version": o["version"], "checks": o["checks"], "queue": o["queue"]});
    f(&mut b);
    b
}

async fn set(td: &TestDaemon, column: &str, f: impl FnOnce(&mut Value)) -> Value {
    let o = options(td, column).await;
    td.put(&path(column), changed(&o, f)).await.ok().clone()
}

async fn card(td: &TestDaemon, agent: &str, body: Value) -> String {
    td.post_as(agent, "/api/projects/proj/cards", body)
        .await
        .ok()["id"]
        .as_str()
        .unwrap()
        .to_owned()
}

async fn count(td: &TestDaemon, path: &str) -> usize {
    td.get(path).await.ok().as_array().unwrap().len()
}

#[tokio::test]
async fn new_boards_get_todays_checks_as_defaults() {
    let td = setup().await;
    for (column, comment, deps, terminal) in [
        ("backlog", false, false, false),
        ("ready", false, false, false),
        ("in_progress", false, true, false),
        ("validating", false, false, false),
        ("review", false, false, false),
        ("merge_queue", false, false, false),
        ("accepted", false, false, false),
        ("blocked", true, false, false),
        ("done", false, false, true),
    ] {
        let o = options(&td, column).await;
        assert_eq!(o["column"], column);
        assert_eq!(o["version"], 1, "{o}");
        assert_eq!(
            o["checks"],
            json!({"require_comment_on_enter": comment, "require_dependencies_met": deps,
                   "terminal_for_members": terminal, "clear_eta_on_enter": true}),
            "{column}"
        );
        assert_eq!(
            o["queue"],
            json!({"release": "wip", "batch_size": null, "max_wait_seconds": null,
                   "max_batch_cards": null}),
            "{column}"
        );
    }
    // A role alias names the column with that role.
    assert_eq!(options(&td, "working").await["column"], "in_progress");
    td.get(&path("nope")).await.err(422, "invalid");
}

#[tokio::test]
async fn operator_and_own_project_admin_write_everyone_else_reads() {
    let td = setup().await;
    let o = options(&td, "review").await;
    let body = |v: i64, deps: bool| {
        let mut b = changed(&o, |b| {
            b["checks"]["require_dependencies_met"] = deps.into()
        });
        b["base_version"] = v.into();
        b
    };
    // Readers of the project read; sysadmin and architect read every project.
    for who in ["proj/lead", "proj/w1", "ops/root", "ops/arch"] {
        assert_eq!(
            td.get_as(who, &path("review")).await.ok()["version"],
            1,
            "{who}"
        );
    }
    td.get_as("other/x", &path("review"))
        .await
        .err(403, "forbidden");
    // Only the operator and the project_admin of proj write: not a member,
    // not a sysadmin, not an architect, not the admin of another project.
    for who in ["proj/w1", "ops/root", "ops/arch", "other/x", "other/boss"] {
        td.put_as(who, &path("review"), body(1, true))
            .await
            .err(403, "forbidden");
    }
    assert_eq!(options(&td, "review").await["version"], 1);
    let r = td.put_as("proj/lead", &path("review"), body(1, true)).await;
    assert_eq!(r.ok()["version"], 2);
    assert_eq!(r.ok()["checks"]["require_dependencies_met"], true);
    let r = td.put(&path("review"), body(2, false)).await;
    assert_eq!(r.ok()["version"], 3);
    // The legacy board PATCH keeps its rights: a sysadmin still edits columns.
    let b = td.get("/api/projects/proj/board").await.ok().clone();
    let cols: Vec<Value> = b["columns"]
        .as_array()
        .unwrap()
        .iter()
        .map(|c| json!({"name": c["name"], "role": c.get("role").cloned().unwrap_or(Value::Null)}))
        .collect();
    td.patch_as(
        "ops/root",
        "/api/projects/proj/board",
        json!({"columns": cols}),
    )
    .await
    .ok();
}

#[tokio::test]
async fn a_stale_version_writes_nothing_and_each_write_is_audited_once() {
    let td = setup().await;
    let events = "/api/events?kind=board&project=proj";
    let audits = "/api/audit?action=board.column_options";
    let (e0, a0) = (count(&td, events).await, count(&td, audits).await);
    let o = options(&td, "review").await;
    let first = changed(&o, |b| {
        b["checks"]["require_comment_on_enter"] = true.into();
        b["queue"] = json!({"release": "batch", "batch_size": 4, "max_wait_seconds": 1800,
                            "max_batch_cards": 8});
    });
    let after = td.put(&path("review"), first.clone()).await.ok().clone();
    assert_eq!(after["version"], 2);
    assert_eq!(after["queue"]["release"], "batch");
    assert_eq!(after["queue"]["max_batch_cards"], 8);
    assert_eq!(count(&td, events).await, e0 + 1);
    assert_eq!(count(&td, audits).await, a0 + 1);
    let audit = td.get(audits).await.ok().clone();
    let detail = audit.as_array().unwrap().last().unwrap().to_string();
    assert!(
        detail.contains("\"before\"") && detail.contains("\"after\""),
        "{detail}"
    );
    // The same base again is stale: 409, and nothing is written.
    let stale = changed(&o, |b| b["checks"]["clear_eta_on_enter"] = false.into());
    let r = td.put(&path("review"), stale).await;
    r.err(409, "stale_base");
    assert_eq!(r.body["error"]["details"]["version"], 2);
    let now = options(&td, "review").await;
    assert_eq!(now["version"], 2);
    assert_eq!(now["checks"]["clear_eta_on_enter"], true);
    assert_eq!(now["checks"]["require_comment_on_enter"], true);
    assert_eq!(count(&td, events).await, e0 + 1);
    assert_eq!(count(&td, audits).await, a0 + 1);
    // A write that changes nothing keeps the version and writes nothing.
    let same = changed(&now, |_| {});
    assert_eq!(td.put(&path("review"), same).await.ok()["version"], 2);
    assert_eq!(count(&td, audits).await, a0 + 1);
    assert_eq!(count(&td, events).await, e0 + 1);
}

#[tokio::test]
async fn bad_options_are_refused_with_the_path() {
    let td = setup().await;
    let o = options(&td, "merge_queue").await;
    for (queue, at) in [
        (
            json!({"release": "wip", "batch_size": 4}),
            "queue.batch_size",
        ),
        (
            json!({"release": "batch", "max_wait_seconds": 60}),
            "queue.batch_size",
        ),
        (
            json!({"release": "batch", "batch_size": 4}),
            "queue.max_wait_seconds",
        ),
        (
            json!({"release": "batch", "batch_size": 4, "max_wait_seconds": 60, "max_batch_cards": 2}),
            "queue.max_batch_cards",
        ),
        (
            json!({"release": "job", "max_batch_cards": 2}),
            "queue.max_batch_cards",
        ),
    ] {
        let r = td
            .put(
                &path("merge_queue"),
                changed(&o, |b| b["queue"] = queue.clone()),
            )
            .await;
        r.err(422, "invalid");
        assert_eq!(r.body["error"]["details"]["path"], at, "{queue}");
    }
    // An unknown release, field, or a missing check is not a valid body.
    for bad in [
        changed(&o, |b| b["queue"]["release"] = "lifo".into()),
        changed(&o, |b| b["checks"]["surprise"] = true.into()),
        changed(&o, |b| {
            b["checks"]
                .as_object_mut()
                .unwrap()
                .remove("terminal_for_members");
        }),
    ] {
        assert!(
            td.put(&path("merge_queue"), bad)
                .await
                .status
                .is_client_error()
        );
    }
    assert_eq!(options(&td, "merge_queue").await["version"], 1);
}

#[tokio::test]
async fn each_check_changes_only_its_own_path() {
    let td = setup().await;
    // Blocked without its comment check: a move there needs no comment.
    let a = card(&td, "proj/w1", json!({"title": "A"})).await;
    td.post_empty_as("proj/w1", &format!("/api/cards/{a}/claim"))
        .await
        .ok();
    td.post_as(
        "proj/w1",
        &format!("/api/cards/{a}/move"),
        json!({"column": "blocked"}),
    )
    .await
    .err(422, "comment_required");
    set(&td, "blocked", |b| {
        b["checks"]["require_comment_on_enter"] = false.into();
    })
    .await;
    td.post_as(
        "proj/w1",
        &format!("/api/cards/{a}/move"),
        json!({"column": "blocked"}),
    )
    .await
    .ok();
    // Review with the comment check: a move there needs one.
    set(&td, "review", |b| {
        b["checks"]["require_comment_on_enter"] = true.into();
    })
    .await;
    let r = td
        .post_as(
            "proj/w1",
            &format!("/api/cards/{a}/move"),
            json!({"column": "review"}),
        )
        .await;
    r.err(422, "comment_required");
    assert!(
        !r.body["error"]["message"]
            .as_str()
            .unwrap()
            .contains("the blocked column")
    );
    td.post_as(
        "proj/w1",
        &format!("/api/cards/{a}/move"),
        json!({"column": "review", "comment": "ready to look at"}),
    )
    .await
    .ok();

    // Dependencies: review with the check refuses, in_progress without it admits.
    let dep = card(&td, "proj/w1", json!({"title": "Dep"})).await;
    let b = card(&td, "proj/w1", json!({"title": "B", "depends_on": [dep]})).await;
    set(&td, "review", |b| {
        b["checks"]["require_comment_on_enter"] = false.into();
        b["checks"]["require_dependencies_met"] = true.into();
    })
    .await;
    td.post_as(
        "proj/w1",
        &format!("/api/cards/{b}/move"),
        json!({"column": "review"}),
    )
    .await
    .err(409, "dependencies_unmet");
    td.post_empty_as("proj/w1", &format!("/api/cards/{b}/claim"))
        .await
        .err(409, "dependencies_unmet");
    set(&td, "in_progress", |b| {
        b["checks"]["require_dependencies_met"] = false.into();
    })
    .await;
    td.post_empty_as("proj/w1", &format!("/api/cards/{b}/claim"))
        .await
        .ok();

    // ETA: a column without the clear keeps the ETA of an entering card.
    let c = card(&td, "proj/w1", json!({"title": "C", "eta_at": "1h"})).await;
    let r = td
        .post_as(
            "proj/w1",
            &format!("/api/cards/{c}/move"),
            json!({"column": "ready"}),
        )
        .await;
    assert!(r.ok()["eta_at"].is_null());
    td.patch_as(
        "proj/w1",
        &format!("/api/cards/{c}"),
        json!({"eta_at": "1h"}),
    )
    .await
    .ok();
    set(&td, "backlog", |b| {
        b["checks"]["clear_eta_on_enter"] = false.into();
    })
    .await;
    let r = td
        .post_as(
            "proj/w1",
            &format!("/api/cards/{c}/move"),
            json!({"column": "backlog"}),
        )
        .await;
    assert!(r.ok()["eta_at"].is_string(), "{}", r.body);

    // Final for members: ready with the check holds a member's card; done
    // without it lets the member reopen.
    set(&td, "ready", |b| {
        b["checks"]["terminal_for_members"] = true.into();
    })
    .await;
    let d = card(&td, "proj/w1", json!({"title": "D"})).await;
    td.post_as(
        "proj/w1",
        &format!("/api/cards/{d}/move"),
        json!({"column": "ready"}),
    )
    .await
    .ok();
    let r = td
        .post_as(
            "proj/w1",
            &format!("/api/cards/{d}/move"),
            json!({"column": "backlog"}),
        )
        .await;
    r.err(403, "forbidden");
    assert!(
        r.body["error"]["message"]
            .as_str()
            .unwrap()
            .contains("a final column")
    );
    td.patch_as(
        "proj/w1",
        &format!("/api/cards/{d}"),
        json!({"title": "D2"}),
    )
    .await
    .err(403, "forbidden");
    td.post_empty_as("proj/w1", &format!("/api/cards/{d}/claim"))
        .await
        .err(409, "conflict");
    td.post_as(
        "proj/w1",
        "/api/projects/proj/cards",
        json!({"title": "E", "column": "ready"}),
    )
    .await
    .err(403, "forbidden");
    td.post_as(
        "proj/lead",
        &format!("/api/cards/{d}/move"),
        json!({"column": "backlog"}),
    )
    .await
    .ok();
    set(&td, "done", |b| {
        b["checks"]["terminal_for_members"] = false.into();
    })
    .await;
    let f = card(&td, "proj/w1", json!({"title": "F"})).await;
    td.post_as(
        "proj/lead",
        &format!("/api/cards/{f}/move"),
        json!({"column": "done"}),
    )
    .await
    .ok();
    td.post_as(
        "proj/w1",
        &format!("/api/cards/{f}/move"),
        json!({"column": "ready"}),
    )
    .await
    .ok();
}

#[tokio::test]
async fn the_board_patch_keeps_options_and_carries_role_checks() {
    let td = setup().await;
    set(&td, "review", |b| {
        b["checks"]["terminal_for_members"] = true.into();
        b["queue"] = json!({"release": "job"});
    })
    .await;
    // A rename and a reorder keep the options and their version.
    td.patch(
        "/api/projects/proj/board",
        json!({"columns": [
            {"name": "backlog"}, {"name": "ready"},
            {"name": "in_progress", "role": "working"},
            {"name": "looking", "rename_from": "review", "role": "review"},
            {"name": "validating", "role": "validating"},
            {"name": "merge_queue", "role": "merge_queue"},
            {"name": "accepted", "role": "accepted"},
            {"name": "blocked", "role": "blocked"}, {"name": "done"}
        ]}),
    )
    .await
    .ok();
    let o = options(&td, "looking").await;
    assert_eq!(o["version"], 2);
    assert_eq!(o["checks"]["terminal_for_members"], true);
    assert_eq!(o["queue"]["release"], "job");
    // The role checks go with the role, as before: `ready` takes `working`,
    // a new column takes `blocked`.
    td.patch(
        "/api/projects/proj/board",
        json!({"columns": [
            {"name": "backlog"}, {"name": "ready", "role": "working"},
            {"name": "in_progress", "role": null},
            {"name": "looking", "role": "review"},
            {"name": "validating", "role": "validating"},
            {"name": "merge_queue", "role": "merge_queue"},
            {"name": "accepted", "role": "accepted"},
            {"name": "held", "rename_from": "blocked", "role": null},
            {"name": "stuck", "role": "blocked"}, {"name": "done"}
        ]}),
    )
    .await
    .ok();
    let ready = options(&td, "ready").await;
    assert_eq!(ready["checks"]["require_dependencies_met"], true);
    assert_eq!(ready["version"], 2);
    assert_eq!(
        options(&td, "in_progress").await["checks"]["require_dependencies_met"],
        false
    );
    let held = options(&td, "held").await;
    assert_eq!(held["checks"]["require_comment_on_enter"], false);
    assert_eq!(held["version"], 2);
    let stuck = options(&td, "stuck").await;
    assert_eq!(stuck["checks"]["require_comment_on_enter"], true);
    assert_eq!(stuck["version"], 1);
    assert_eq!(options(&td, "looking").await["version"], 2);
}
