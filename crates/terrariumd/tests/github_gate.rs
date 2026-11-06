//! The GitHub author gate (TER-85, docs/api.md §8.3): a GitHub card that
//! an agent makes for an author off the project's `github_authors` list
//! never lands in the backlog or `ready`. It is held in the blocked
//! column with the labels `github` and `hold`, and the daemon asks the
//! operator. Only the operator releases it. The unit tests of
//! `terrariumd::store::github_gate` cover the title and list rules.
#![allow(clippy::unwrap_used, clippy::expect_used)]

mod common;

use common::TestDaemon;
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
    assert_eq!(p.ok()["github_authors"], json!(["wiltaylor", "wiltaylor-daf"]));
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
async fn the_operator_answer_releases_or_keeps_the_card() {
    let td = setup().await;
    allow(&td, &["wiltaylor"]).await;
    let mk = |t: &'static str| {
        json!({"title": t, "labels": ["github", "bug"], "github_author": "eve"})
    };
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
    td.patch(&path, json!({"add_labels": ["github"]})).await.ok();
    // The operator's own GitHub cards are not gated.
    let c = td
        .post(CARDS, json!({"title": "GitHub #10: mine", "column": "ready"}))
        .await;
    assert_eq!(c.ok()["column"], "ready");
}
