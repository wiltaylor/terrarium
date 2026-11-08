//! The GitHub author gate (TER-85, docs/api.md §8.3): a GitHub card that
//! an agent makes for an author off the project's `github_authors` list
//! never lands in the backlog or `ready`. It is held in the blocked
//! column with the labels `github` and `hold`, and the daemon asks the
//! operator. Only the operator releases it. The unit tests of
//! `terrariumd::store::github_gate` cover the title and list rules.
#![allow(clippy::unwrap_used, clippy::expect_used)]

use crate::common;

use common::{TestDaemon, write};
use serde_json::{Value, json};

const CARDS: &str = "/api/projects/proj/cards";

/// Project `proj` with a lead, a worker, and a sysadmin (in `ops`).
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
    td.agent("ops", "sys", json!({"roles": ["sysadmin"]})).await;
    td
}

/// The operator sets the allow list of `proj`.
async fn allow(td: &TestDaemon, list: &[&str]) {
    td.patch("/api/projects/proj", json!({"github_authors": list}))
        .await
        .ok();
}

async fn make(td: &TestDaemon, agent: &str, body: Value) -> Value {
    td.post_as(agent, CARDS, body).await.ok().clone()
}

async fn card(td: &TestDaemon, id: &str) -> Value {
    td.get(&format!("/api/cards/{id}")).await.ok().clone()
}

fn labels(c: &Value) -> Vec<String> {
    c["labels"]
        .as_array()
        .unwrap()
        .iter()
        .map(|l| l.as_str().unwrap().to_owned())
        .collect()
}

/// The hold question of card `id` (any status).
async fn hold_question(td: &TestDaemon, id: &str) -> Value {
    let all = td.get("/api/questions?status=all").await;
    let found: Vec<Value> = all
        .ok()
        .as_array()
        .unwrap()
        .iter()
        .filter(|q| q["hold_card"] == id)
        .cloned()
        .collect();
    assert_eq!(found.len(), 1, "{found:#?}");
    found[0].clone()
}

/// No list: no gate. Other projects keep working.
#[tokio::test]
async fn without_a_list_a_github_card_is_plain_work() {
    let td = setup().await;
    let c = make(
        &td,
        "proj/w1",
        json!({"title": "GitHub #1: x", "labels": ["github"], "column": "ready",
               "github_author": "mallory"}),
    )
    .await;
    assert_eq!(c["column"], "ready");
    assert_eq!(labels(&c), ["github"]);
    // An agent may also label a card github later.
    let p = make(&td, "proj/w1", json!({"title": "Plain"})).await;
    td.patch_as(
        "proj/w1",
        &format!("/api/cards/{}", p["id"].as_str().unwrap()),
        json!({"add_labels": ["github"]}),
    )
    .await
    .ok();
}

/// Only the operator sets the list: not a lead, not a sysadmin. A bad
/// login is refused; the list shows on the project.
#[tokio::test]
async fn only_the_operator_sets_the_allow_list() {
    let td = setup().await;
    for who in ["proj/lead", "ops/sys"] {
        td.patch_as(
            who,
            "/api/projects/proj",
            json!({"github_authors": ["mallory"]}),
        )
        .await
        .err(403, "forbidden");
    }
    td.patch(
        "/api/projects/proj",
        json!({"github_authors": ["not a login"]}),
    )
    .await
    .err(422, "invalid");
    allow(&td, &["@wiltaylor", "wiltaylor-daf"]).await;
    let p = td.get("/api/projects/proj").await;
    assert_eq!(
        p.ok()["github_authors"],
        json!(["wiltaylor", "wiltaylor-daf"])
    );
    allow(&td, &[]).await;
    let p = td.get("/api/projects/proj").await;
    assert!(p.ok().get("github_authors").is_none(), "{}", p.body);
}

/// An author on the list makes work as asked, in any case.
#[tokio::test]
async fn an_allowed_author_makes_work() {
    let td = setup().await;
    allow(&td, &["wiltaylor", "wiltaylor-daf"]).await;
    let c = make(
        &td,
        "proj/w1",
        json!({"title": "GitHub #2: y", "labels": ["github"], "column": "ready",
               "github_author": "WilTaylor-DAF"}),
    )
    .await;
    assert_eq!(c["column"], "ready");
    assert_eq!(labels(&c), ["github"]);
    let c = make(
        &td,
        "proj/w1",
        json!({"title": "GitHub #3: z", "github_author": "wiltaylor"}),
    )
    .await;
    assert_eq!(c["column"], "backlog");
}

/// The acceptance of TER-85: another author, no author, or the title
/// alone: the card is held in blocked, never in the backlog or `ready`,
/// and the daemon asks the operator.
#[tokio::test]
async fn an_issue_by_another_author_is_held_for_the_operator() {
    let td = setup().await;
    allow(&td, &["wiltaylor", "wiltaylor-daf"]).await;
    let cases = [
        json!({"title": "Crash", "labels": ["github"], "column": "ready",
               "github_author": "mallory", "comment": "from the sync"}),
        json!({"title": "Crash 2", "labels": ["GitHub"]}),
        json!({"title": "GitHub #77: no label", "column": "backlog"}),
    ];
    for body in cases {
        let c = make(&td, "proj/w1", body.clone()).await;
        let id = c["id"].as_str().unwrap().to_owned();
        assert_eq!(c["column"], "blocked", "{body}");
        let l = labels(&c);
        assert!(l.contains(&"hold".to_owned()), "{l:?}");
        assert!(l.iter().any(|x| x.eq_ignore_ascii_case("github")), "{l:?}");
        let q = hold_question(&td, &id).await;
        assert_eq!(q["status"], "open");
        assert_eq!(q["card"], id.as_str());
        assert_eq!(q["agent_name"], "proj/w1");
        assert_eq!(q["options"], json!(["Release to backlog", "Keep held"]));
        assert!(q["text"].as_str().unwrap().contains(&id), "{q}");
    }
    let first = card(&td, "TER-1").await;
    let comments: Vec<&str> = first["comments"]
        .as_array()
        .map(|a| a.iter().filter_map(|c| c["body"].as_str()).collect())
        .unwrap_or_default();
    assert!(
        comments.iter().any(|c| c.contains("from the sync")),
        "{first}"
    );
}

/// A held card stays held for every agent: no move out of blocked, no
/// claim, no edit that takes off `hold` or `github`, no answer or change
/// of its question. The operator decides.
#[tokio::test]
async fn only_the_operator_releases_a_held_card() {
    let td = setup().await;
    allow(&td, &["wiltaylor"]).await;
    let c = make(
        &td,
        "proj/w1",
        json!({"title": "GitHub #5: held", "labels": ["github"], "github_author": "eve"}),
    )
    .await;
    let id = c["id"].as_str().unwrap().to_owned();
    let path = format!("/api/cards/{id}");
    for who in ["proj/lead", "proj/w1"] {
        td.patch_as(who, &path, json!({"column": "backlog"}))
            .await
            .err(403, "forbidden");
        td.patch_as(who, &path, json!({"remove_labels": ["hold"]}))
            .await
            .err(403, "forbidden");
        td.patch_as(
            who,
            &path,
            json!({"title": "Not GitHub", "labels": ["hold"]}),
        )
        .await
        .err(403, "forbidden");
    }
    td.post_as("proj/w1", &format!("{path}/claim"), json!({}))
        .await
        .err(403, "forbidden");
    // Other edits are fine, and it stays held.
    td.patch_as("proj/lead", &path, json!({"add_labels": ["bug"]}))
        .await
        .ok();
    let q = hold_question(&td, &id).await;
    let qid = q["id"].as_str().unwrap();
    td.post_as(
        "ops/sys",
        &format!("/api/questions/{qid}/answer"),
        json!({"option": 0}),
    )
    .await
    .err(403, "forbidden");
    td.patch_as(
        "proj/w1",
        &format!("/api/questions/{qid}"),
        json!({"options": ["Keep held", "Release to backlog"]}),
    )
    .await
    .err(403, "forbidden");
    td.post_as(
        "proj/w1",
        &format!("/api/questions/{qid}/retract"),
        json!({}),
    )
    .await
    .err(403, "forbidden");
    assert_eq!(card(&td, &id).await["column"], "blocked");
    // The operator may move it by hand.
    let moved = td.patch(&path, json!({"column": "backlog"})).await;
    assert_eq!(moved.ok()["column"], "backlog");
}

/// The answer of the hold question: "Keep held" keeps it; "Release to
/// backlog" takes off `hold` and moves the card to the backlog.
#[tokio::test]
async fn sysadmin_cannot_answer_a_hold_question() {
    let td = setup().await;
    allow(&td, &["wiltaylor"]).await;
    let c = make(
        &td,
        "proj/w1",
        json!({"title": "GitHub #8: release", "github_author": "eve"}),
    )
    .await;
    let id = c["id"].as_str().unwrap();
    let q = hold_question(&td, id).await;
    td.post_as(
        "ops/sys",
        &format!("/api/questions/{}/answer", q["id"].as_str().unwrap()),
        json!({"option": 0}),
    )
    .await
    .err(403, "forbidden");
    let c = card(&td, id).await;
    assert_eq!(c["column"], "blocked");
    assert!(labels(&c).contains(&"hold".to_owned()));
}

#[tokio::test]
async fn the_operator_answer_releases_or_keeps_the_card() {
    let td = setup().await;
    allow(&td, &["wiltaylor"]).await;
    let mk =
        |t: &'static str| json!({"title": t, "labels": ["github", "bug"], "github_author": "eve"});
    let kept = make(&td, "proj/w1", mk("GitHub #6: keep")).await["id"]
        .as_str()
        .unwrap()
        .to_owned();
    let freed = make(&td, "proj/w1", mk("GitHub #7: free")).await["id"]
        .as_str()
        .unwrap()
        .to_owned();
    let q = hold_question(&td, &kept).await;
    td.post(
        &format!("/api/questions/{}/answer", q["id"].as_str().unwrap()),
        json!({"option": "Keep held"}),
    )
    .await
    .ok();
    let k = card(&td, &kept).await;
    assert_eq!(k["column"], "blocked");
    assert!(labels(&k).contains(&"hold".to_owned()));
    let q = hold_question(&td, &freed).await;
    td.post(
        &format!("/api/questions/{}/answer", q["id"].as_str().unwrap()),
        json!({"option": 0}),
    )
    .await
    .ok();
    let f = card(&td, &freed).await;
    assert_eq!(f["column"], "backlog");
    assert_eq!(labels(&f), ["github", "bug"]);
    // Released: plain work now, the worker may claim it.
    td.post_as("proj/w1", &format!("/api/cards/{freed}/claim"), json!({}))
        .await
        .ok();
}

/// With a list, an agent cannot make a card a GitHub card after `card
/// new`, so the gate cannot be skipped by labelling later. The operator
/// can.
#[tokio::test]
async fn an_agent_cannot_label_a_card_github_later() {
    let td = setup().await;
    allow(&td, &["wiltaylor"]).await;
    let p = make(&td, "proj/w1", json!({"title": "Plain"})).await;
    let path = format!("/api/cards/{}", p["id"].as_str().unwrap());
    td.patch_as("proj/w1", &path, json!({"add_labels": ["github"]}))
        .await
        .err(403, "forbidden");
    td.patch_as("proj/lead", &path, json!({"title": "GitHub #9: later"}))
        .await
        .err(403, "forbidden");
    td.patch(&path, json!({"add_labels": ["github"]}))
        .await
        .ok();
    // The operator's own GitHub cards are not gated.
    let c = td
        .post(
            CARDS,
            json!({"title": "GitHub #10: mine", "column": "ready"}),
        )
        .await;
    assert_eq!(c.ok()["column"], "ready");
}

/// Make a held card in `proj` for an author off the list.
async fn held(td: &TestDaemon, n: u32) -> String {
    allow(td, &["wiltaylor"]).await;
    let c = make(
        td,
        "proj/w1",
        json!({"title": format!("GitHub #{n}: held"), "labels": ["github"],
               "github_author": "eve"}),
    )
    .await;
    assert_eq!(c["column"], "blocked");
    c["id"].as_str().unwrap().to_owned()
}

/// The lock does not depend on the column role (review round 1, F1). The
/// lead moves the `blocked` role to another column, or drops it: the held
/// card still moves only by the operator. A new held card goes to the
/// column with the role.
#[tokio::test]
async fn the_lock_holds_when_the_lead_changes_the_column_roles() {
    let td = setup().await;
    let id = held(&td, 20).await;
    let path = format!("/api/cards/{id}");
    let board = "/api/projects/proj/board";
    // The default board. The lead renames `blocked` to `parked` with no
    // role (the name `blocked` is kept for the role) and gives the role
    // `blocked` to a new column `stuck`. The held card is in `parked`, a
    // column with no role. An absent role keeps the role, so `null`
    // clears it.
    // The first change renames; the second keeps `parked` as it is.
    let columns = |blocked_role: bool| {
        let (role, parked) = if blocked_role {
            (
                json!("blocked"),
                json!({"name": "parked", "rename_from": "blocked", "role": null}),
            )
        } else {
            (Value::Null, json!({"name": "parked"}))
        };
        json!({"columns": [
            {"name": "backlog"}, {"name": "ready"},
            {"name": "in_progress", "role": "working"}, {"name": "review"},
            parked, {"name": "stuck", "role": role}, {"name": "done"}]})
    };
    td.patch_as("proj/lead", board, columns(true)).await.ok();
    assert_eq!(card(&td, &id).await["column"], "parked");
    for to in ["ready", "backlog", "stuck"] {
        td.patch_as("proj/lead", &path, json!({"column": to}))
            .await
            .err(403, "forbidden");
    }
    td.post_as("proj/w1", &format!("{path}/claim"), json!({}))
        .await
        .err(403, "forbidden");
    // A new held card goes to `stuck`, the column with the role.
    let c = make(
        &td,
        "proj/w1",
        json!({"title": "GitHub #21: held", "labels": ["github"], "github_author": "eve"}),
    )
    .await;
    assert_eq!(c["column"], "stuck");
    let id2 = c["id"].as_str().unwrap().to_owned();
    // A column with the name `blocked` and no role, while `stuck` has the
    // role (review round 2, minor 2). The API refuses such a board, so
    // the test writes it in the database, as an old board could have it.
    // A new held card still goes to `stuck`, the column with the role.
    rename_column(&td, "parked", "blocked").await;
    let c = make(
        &td,
        "proj/w1",
        json!({"title": "GitHub #24: held", "labels": ["github"], "github_author": "eve"}),
    )
    .await;
    assert_eq!(c["column"], "stuck", "{c}");
    assert_eq!(card(&td, &id).await["column"], "blocked");
    rename_column(&td, "blocked", "parked").await;
    // No column has the role `blocked` now: still no move, and a new
    // GitHub card off the list is refused (fails closed), not made.
    td.patch_as("proj/lead", board, columns(false)).await.ok();
    td.post_as(
        "proj/w1",
        CARDS,
        json!({"title": "GitHub #23: no role", "labels": ["github"], "github_author": "eve"}),
    )
    .await
    .err(422, "invalid");
    for card_id in [&id, &id2] {
        td.patch_as(
            "proj/lead",
            &format!("/api/cards/{card_id}"),
            json!({"column": "ready"}),
        )
        .await
        .err(403, "forbidden");
    }
    // The operator still moves it.
    let m = td.patch(&path, json!({"column": "backlog"})).await;
    assert_eq!(m.ok()["column"], "backlog");
}

/// Rename a column of `proj` in the database, past the board rules of
/// the API.
async fn rename_column(td: &TestDaemon, from: &str, to: &str) {
    let (from, to) = (from.to_owned(), to.to_owned());
    let n = td
        .daemon
        .as_ref()
        .unwrap()
        .state
        .db
        .write("test", move |t| {
            Ok(t.execute(
                "UPDATE board_columns SET name = ?2 WHERE project = 'proj' AND name = ?1",
                [&from, &to],
            )?)
        })
        .await
        .unwrap();
    assert_eq!(n, 1);
}

/// A schedule `card` action does not move a held card (review round 2,
/// blocker 1). A project-owned schedule runs as the operator. The lead
/// makes one that closes the card with the label `trigger:k` to `ready`,
/// and puts that label on the held card. Run now finds no open card, and
/// the held card stays in blocked. A plain card with the label is closed,
/// so the gate is the reason.
#[tokio::test]
async fn a_schedule_card_action_does_not_move_a_held_card() {
    let td = setup().await;
    let id = held(&td, 25).await;
    let path = format!("/api/cards/{id}");
    let s = td
        .post_as(
            "proj/lead",
            "/api/schedules",
            json!({"name": "close-k", "project": "proj", "owner": "project",
                   "spec": {"kind": "interval", "every": "1h"},
                   "action": {"type": "card", "op": "close", "key": "k", "column": "ready"}}),
        )
        .await;
    let sid = s.ok()["id"].as_str().unwrap().to_owned();
    assert_eq!(s.body["owner"], "project", "{}", s.body);
    td.patch_as("proj/lead", &path, json!({"add_labels": ["trigger:k"]}))
        .await
        .ok();
    let c = card(&td, &id).await;
    assert!(labels(&c).contains(&"trigger:k".to_owned()), "{c}");
    let run = td
        .post_empty_as("proj/lead", &format!("/api/schedules/{sid}/run"))
        .await;
    let run = run.ok().clone();
    assert_eq!(run["status"], "ok", "{run}");
    assert!(run["output"]["card"].is_null(), "{run}");
    let c = card(&td, &id).await;
    assert_eq!(c["column"], "blocked", "{c}");
    assert!(labels(&c).contains(&"hold".to_owned()), "{c}");
    // A plain card with the label: the schedule closes it, and the held
    // card stays.
    let p = make(
        &td,
        "proj/w1",
        json!({"title": "Plain", "labels": ["trigger:k"]}),
    )
    .await;
    let run = td
        .post_empty_as("proj/lead", &format!("/api/schedules/{sid}/run"))
        .await;
    let run = run.ok().clone();
    assert_eq!(run["status"], "ok", "{run}");
    assert_eq!(run["output"]["card"], p["id"], "{run}");
    assert_eq!(
        card(&td, p["id"].as_str().unwrap()).await["column"],
        "ready"
    );
    assert_eq!(card(&td, &id).await["column"], "blocked");
}

/// No job takes a held card out of blocked (review round 1, F2): a
/// worker cannot assign itself, and no one queues a validate or merge job
/// for it, the operator included. Job moves run as the operator, so the
/// refusal is at the queue. A plain card gets past the gate (and stops at
/// the missing job queue).
#[tokio::test]
async fn no_assign_validate_or_merge_for_a_held_card() {
    let td = setup().await;
    let id = held(&td, 22).await;
    let path = format!("/api/cards/{id}");
    for who in ["w1", "me"] {
        td.patch_as("proj/w1", &path, json!({"assignee": who}))
            .await
            .err(403, "forbidden");
    }
    td.patch_as("proj/lead", &path, json!({"assignee": "w1"}))
        .await
        .err(403, "forbidden");
    td.post_as(
        "proj/w1",
        &format!("{path}/assign"),
        json!({"assignee": "me"}),
    )
    .await
    .err(403, "forbidden");
    let c = card(&td, &id).await;
    assert!(c["assignee"].is_null(), "{c}");
    for who in ["proj/lead", "proj/w1"] {
        td.post_as(who, &format!("{path}/validate"), json!({}))
            .await
            .err(403, "forbidden");
    }
    td.post(&format!("{path}/validate"), json!({}))
        .await
        .err(403, "forbidden");
    td.post_as("proj/lead", &format!("{path}/merge"), json!({}))
        .await
        .err(403, "forbidden");
    td.post(&format!("{path}/merge"), json!({}))
        .await
        .err(403, "forbidden");
    td.post_as("proj/lead", "/api/merges", json!({"cards": [id]}))
        .await
        .err(403, "forbidden");
    assert_eq!(card(&td, &id).await["column"], "blocked");
    // The gate is the reason: a plain card passes it and meets the
    // missing job queue.
    let p = make(&td, "proj/w1", json!({"title": "Plain", "assignee": "me"})).await;
    let r = td
        .post_as(
            "proj/w1",
            &format!("/api/cards/{}/validate", p["id"].as_str().unwrap()),
            json!({}),
        )
        .await;
    assert_eq!(r.status.as_u16(), 409, "{}", r.body);
}

/// A file project takes its list from `projects/<name>.toml` (review
/// round 1, F3). The API refuses a change (`file_managed`), so a sync
/// never undoes an operator PATCH with no notice.
#[tokio::test]
async fn a_file_project_takes_the_list_only_from_its_file() {
    let td = TestDaemon::with_config(|c| {
        write(
            c,
            "projects/gh.toml",
            "description = \"x\"\nprefix = \"GH\"\ngithub_authors = [\"wiltaylor\"]\n",
        );
    })
    .await;
    let p = td.get("/api/projects/gh").await;
    assert_eq!(p.ok()["source"], "file");
    assert_eq!(p.body["github_authors"], json!(["wiltaylor"]));
    for list in [json!([]), json!(["wiltaylor", "mallory"])] {
        td.patch("/api/projects/gh", json!({"github_authors": list}))
            .await
            .err(409, "file_managed");
    }
    let p = td.get("/api/projects/gh").await;
    assert_eq!(p.ok()["github_authors"], json!(["wiltaylor"]));
}
