//! The system prompt library (PLAN §8.1, docs/api.md §28): the API and
//! its rights, the secret scan, the refusal of a delete in use, the read
//! of a file edited by hand, and the older `prompts/` personas.
#![allow(clippy::unwrap_used, clippy::expect_used)]

use crate::common;

use std::path::PathBuf;

use axum::http::Method;
use common::{TestDaemon, write};
use serde_json::{Value, json};
use terrariumd::config_repo::ConfigRepo;

fn id(v: &Value) -> String {
    v["id"].as_str().unwrap().to_owned()
}

/// A daemon with a config repo in its temp dir.
async fn instance() -> (TestDaemon, PathBuf) {
    let td = TestDaemon::start().await;
    let repo = td.dir.path().join("config-terrarium");
    let state = td.daemon.as_ref().unwrap().state.clone();
    state
        .config_repo
        .configure(ConfigRepo::new(repo.clone(), state.state_dir.clone()));
    (td, repo)
}

fn names(v: &Value) -> Vec<String> {
    v.as_array()
        .unwrap()
        .iter()
        .map(|s| s["name"].as_str().unwrap().to_owned())
        .collect()
}

#[tokio::test]
async fn crud_secret_scan_and_hand_edits() {
    let (td, repo) = instance().await;
    // An older config repo: the personas in `prompts/`, with a protocol
    // template next to them.
    write(&repo, "prompts/leader.md", "Lead.\n");
    write(&repo, "prompts/worker.md", "Work.\n");
    write(&repo, "prompts/default.md", "Protocol.\n");
    let r = td.get("/api/system-prompts").await;
    assert_eq!(names(r.ok()), ["leader", "worker"]);
    assert_eq!(r.body[0]["path"], "prompts/leader.md");
    assert_eq!(r.body[0]["builtin"], true);
    td.get("/api/system-prompts/default")
        .await
        .err(404, "not_found");

    // A new entry, with a description line.
    let r = td
        .post(
            "/api/system-prompts",
            json!({"name": "terse", "text": "Be terse.\n", "description": "Short answers"}),
        )
        .await;
    assert_eq!(r.status, 201, "{}", r.body);
    assert_eq!(r.body["description"], "Short answers");
    assert_eq!(r.body["text"], "Be terse.\n");
    assert_eq!(r.body["source"], "config");
    let file = repo.join("system-prompts/terse.md");
    assert_eq!(
        std::fs::read_to_string(&file).unwrap(),
        "<!-- description: Short answers -->\n\nBe terse.\n"
    );
    // A second create is a conflict, unless `replace`.
    td.post("/api/system-prompts", json!({"name": "terse", "text": "x"}))
        .await
        .err(409, "conflict");
    let r = td
        .post(
            "/api/system-prompts",
            json!({"name": "terse", "text": "Be very terse.", "replace": true}),
        )
        .await;
    assert_eq!(r.status, 200, "{}", r.body);
    assert_eq!(r.body["text"], "Be very terse.\n");
    // Bad names, an empty text, and a secret are refused.
    for (body, want) in [
        (json!({"name": "Bad", "text": "x"}), "not valid"),
        (json!({"name": "none", "text": "x"}), "`none`"),
        (json!({"name": "e", "text": " \n"}), "needs `text`"),
        (
            json!({"name": "leak", "text": format!("token ghp_{}", "a1B2c3D4e5".repeat(4))}),
            "secret scan",
        ),
    ] {
        let r = td.post("/api/system-prompts", body).await;
        r.err(422, "invalid");
        let m = r.body["error"]["message"].as_str().unwrap();
        assert!(m.contains(want), "{want}: {m}");
        assert!(!r.body.to_string().contains("ghp_a1"), "never the value");
    }
    assert!(!repo.join("system-prompts/leak.md").exists());

    // An edit: only the fields given change.
    let r = td
        .put("/api/system-prompts/terse", json!({"description": "Brief"}))
        .await;
    assert_eq!(r.ok()["description"], "Brief");
    assert_eq!(r.body["text"], "Be very terse.\n");
    td.put("/api/system-prompts/nope", json!({"text": "x"}))
        .await
        .err(404, "not_found");

    // An edit by hand shows at the next read.
    std::fs::write(&file, "<!-- description: By hand -->\nEdited by hand.\n").unwrap();
    write(
        &repo,
        "system-prompts/manual.md",
        "A file of the operator.\n",
    );
    let r = td.get("/api/system-prompts/terse").await;
    assert_eq!(r.ok()["text"], "Edited by hand.\n");
    assert_eq!(r.body["description"], "By hand");
    let r = td.get("/api/system-prompts").await;
    assert_eq!(names(r.ok()), ["leader", "manual", "terse", "worker"]);

    // The library copy of `worker` wins over `prompts/worker.md`.
    let r = td
        .put("/api/system-prompts/worker", json!({"text": "New work."}))
        .await;
    assert_eq!(r.ok()["path"], "system-prompts/worker.md");
    assert_eq!(
        std::fs::read_to_string(repo.join("prompts/worker.md")).unwrap(),
        "Work.\n"
    );

    // The audit rows and the events.
    for action in ["system_prompt.create", "system_prompt.update"] {
        let audit = td.get(&format!("/api/audit?action={action}")).await;
        let rows = audit.ok().as_array().unwrap().clone();
        assert!(!rows.is_empty(), "{action}");
        assert!(!rows[0]["detail"].to_string().contains("Be "), "no text");
    }
    let events = td.get("/api/events?kind=system_prompt").await;
    assert!(
        events
            .ok()
            .to_string()
            .contains("system prompt terse created"),
        "{}",
        events.body
    );

    // A delete of an entry: `leader` and `worker` cannot go.
    td.req(Method::DELETE, "/api/system-prompts/worker", None, None)
        .await
        .err(409, "conflict");
    let r = td
        .req(Method::DELETE, "/api/system-prompts/manual", None, None)
        .await;
    assert_eq!(r.status, 204, "{}", r.body);
    assert!(!repo.join("system-prompts/manual.md").exists());
}

#[tokio::test]
async fn rights_and_in_use_refusal() {
    let (td, repo) = instance().await;
    write(&repo, "system-prompts/worker.md", "Work.\n");
    td.project("p").await;
    td.project("q").await;
    let member = id(&td.agent("p", "member", json!({})).await);
    let lead = id(&td
        .agent("p", "lead", json!({"roles": ["project_admin"]}))
        .await);
    let other_lead = id(&td
        .agent("q", "lead", json!({"roles": ["project_admin"]}))
        .await);
    let sys = id(&td.agent("q", "root", json!({"roles": ["sysadmin"]})).await);

    // A member cannot read or write the library.
    td.get_as(&member, "/api/system-prompts")
        .await
        .err(403, "forbidden");
    td.post_as(
        &member,
        "/api/system-prompts",
        json!({"name": "m", "text": "x"}),
    )
    .await
    .err(403, "forbidden");

    // A project_admin adds an entry and uses it in its own project.
    let r = td
        .post_as(
            &lead,
            "/api/system-prompts",
            json!({"name": "reviewer", "text": "Review."}),
        )
        .await;
    assert_eq!(r.status, 201, "{}", r.body);
    td.patch_as(
        &lead,
        &format!("/api/agents/{member}"),
        json!({"persona": "reviewer"}),
    )
    .await
    .ok();
    let r = td.get_as(&lead, "/api/system-prompts/reviewer").await;
    assert_eq!(r.ok()["users"][0]["name"], "p/member");
    // Its own project only: it may change the entry.
    td.put_as(
        &lead,
        "/api/system-prompts/reviewer",
        json!({"text": "Review well."}),
    )
    .await
    .ok();
    // `worker` is the persona by role of every project.
    td.put_as(&lead, "/api/system-prompts/worker", json!({"text": "x"}))
        .await
        .err(403, "forbidden");

    // A template of another project uses it: now only a sysadmin or the
    // operator may change or delete it.
    let r = td
        .post(
            "/api/templates",
            json!({"name": "rev", "project": "q", "spec": {"harness": "claude", "persona": "reviewer"}}),
        )
        .await;
    assert_eq!(r.status, 201, "{}", r.body);
    td.put_as(&lead, "/api/system-prompts/reviewer", json!({"text": "x"}))
        .await
        .err(403, "forbidden");
    td.req(
        Method::DELETE,
        "/api/system-prompts/reviewer",
        None,
        Some(&other_lead),
    )
    .await
    .err(403, "forbidden");
    td.put_as(
        &sys,
        "/api/system-prompts/reviewer",
        json!({"text": "Sys."}),
    )
    .await
    .ok();

    // In use: the delete is refused and names the users; `force` deletes.
    let r = td
        .req(Method::DELETE, "/api/system-prompts/reviewer", None, None)
        .await;
    r.err(409, "conflict");
    let users: Vec<&str> = r.body["error"]["details"]["users"]
        .as_array()
        .unwrap()
        .iter()
        .map(|u| u["name"].as_str().unwrap())
        .collect();
    assert_eq!(users, ["p/member", "q/rev"]);
    assert!(repo.join("system-prompts/reviewer.md").is_file());
    let r = td
        .req(
            Method::DELETE,
            "/api/system-prompts/reviewer?force=true",
            None,
            None,
        )
        .await;
    assert_eq!(r.status, 204, "{}", r.body);
    assert!(!repo.join("system-prompts/reviewer.md").exists());
}
