//! Fleet files: validate, plan, apply, drift, prune, policy, the config
//! dir, and export (`docs/fleet.md`).
#![allow(clippy::unwrap_used, clippy::expect_used)]

use crate::common;

use std::collections::BTreeMap;

use common::{TestDaemon, write};
use serde_json::{Value, json};

const EXAMPLE: &str = include_str!("../../../examples/fleet/terrarium.fleet.toml");

fn body(content: &str) -> Value {
    json!({"content": content})
}

fn entry<'a>(plan: &'a Value, kind: &str, key: &str) -> &'a Value {
    plan["entries"]
        .as_array()
        .unwrap()
        .iter()
        .find(|e| e["kind"] == kind && e["key"] == key)
        .unwrap_or_else(|| panic!("no {kind} {key} in {plan:#}"))
}

async fn agent_by_key(td: &TestDaemon, key: &str) -> Value {
    td.get(&format!("/api/agents/{}", key.replace('/', "%2F")))
        .await
        .ok()
        .clone()
}

/// The `source` of each row of a list route, by `name` (`project/name`
/// for an agent).
async fn sources(td: &TestDaemon, path: &str) -> std::collections::BTreeMap<String, String> {
    td.get(path)
        .await
        .ok()
        .as_array()
        .unwrap()
        .iter()
        .map(|r| {
            let name = r["name"].as_str().unwrap();
            let key = match r["project"].as_str() {
                Some(p) if path == "/api/agents" => format!("{p}/{name}"),
                _ => name.to_owned(),
            };
            (key, r["source"].as_str().unwrap().to_owned())
        })
        .collect()
}

#[tokio::test]
async fn apply_creates_and_a_second_apply_changes_nothing() {
    let td = TestDaemon::start().await;

    let v = td.post("/api/fleet/validate", body(EXAMPLE)).await;
    assert_eq!(v.ok()["valid"], true, "{}", v.body);
    assert_eq!(v.body["fleet"], "terrarium");
    assert_eq!(v.body["objects"].as_array().unwrap().len(), 7);

    let plan = td.post("/api/fleet/plan", body(EXAMPLE)).await;
    let s = &plan.ok()["summary"];
    assert_eq!(s["create"], 7, "{}", plan.body);
    assert_eq!(s["unchanged"], 0);

    let r = td.post("/api/fleet/apply", body(EXAMPLE)).await;
    assert_eq!(r.ok()["summary"]["create"], 7, "{}", r.body);
    assert_eq!(r.body["prune"], false);

    let p = td.get("/api/projects/terrarium").await;
    assert_eq!(p.ok()["card_prefix"], "TER");
    assert_eq!(p.body["source"], "fleet:terrarium");
    let repos = td.get("/api/projects/terrarium/repos").await;
    assert_eq!(repos.ok()[0]["name"], "terrarium");
    let lead = agent_by_key(&td, "terrarium/lead").await;
    assert_eq!(lead["prompt"], "project-lead");
    assert_eq!(lead["roles"], json!(["project_admin"]));
    assert_eq!(lead["status"], "stopped");
    let w2 = agent_by_key(&td, "terrarium/worker-2").await;
    assert_eq!(w2["harness"], "pi");
    assert_eq!(w2["policy"]["targets"], json!(["buildbox"]));
    assert_eq!(
        td.get("/api/targets/buildbox").await.ok()["project"],
        "terrarium"
    );
    assert_eq!(
        td.get("/api/subscriptions/anthropic-max").await.ok()["plan"],
        "Max 20x"
    );

    // Idempotent: the second apply finds everything unchanged.
    let plan = td.post("/api/fleet/plan", body(EXAMPLE)).await;
    let s = &plan.ok()["summary"];
    assert_eq!(s["unchanged"], 7, "{}", plan.body);
    assert_eq!(s["create"], 0);
    assert_eq!(s["update"], 0);
    assert_eq!(s["drift"], 0);
    let r = td.post("/api/fleet/apply", body(EXAMPLE)).await;
    assert_eq!(r.ok()["summary"]["unchanged"], 7);
    assert_eq!(agent_by_key(&td, "terrarium/lead").await["id"], lead["id"]);

    let list = td.get("/api/fleet").await;
    let row = &list.ok()[0];
    assert_eq!(row["name"], "terrarium");
    assert_eq!(row["origin"], "api");
    assert_eq!(row["objects"], 7);
    assert_eq!(row["projects"], json!(["terrarium"]));
    let shown = td.get("/api/fleet/terrarium").await;
    assert_eq!(shown.ok()["content"], EXAMPLE);
    assert_eq!(shown.body["plan"]["summary"]["unchanged"], 7);
    td.get("/api/fleet/nope").await.err(404, "not_found");

    let audit = td.get("/api/audit?action=fleet.apply").await;
    assert_eq!(audit.ok().as_array().unwrap().len(), 2, "{}", audit.body);
}

#[tokio::test]
async fn a_changed_file_updates_the_changed_fields() {
    let td = TestDaemon::start().await;
    td.post("/api/fleet/apply", body(EXAMPLE)).await.ok();

    let changed = EXAMPLE
        .replace(
            "name = \"worker-1\"\nsubscription = \"anthropic-max\"",
            "name = \"worker-1\"\nsubscription = \"anthropic-max\"\nmodel = \"opus\"\nrestart = \"always\"",
        )
        .replace("address = \"build@buildbox.lan\"", "address = \"ci@buildbox.lan\"")
        .replace("description = \"The Terrarium tool itself\"", "description = \"Changed\"");
    let plan = td.post("/api/fleet/plan", body(&changed)).await;
    let s = &plan.ok()["summary"];
    assert_eq!(s["update"], 3, "{}", plan.body);
    assert_eq!(s["unchanged"], 4);
    let w1 = entry(&plan.body, "agent", "terrarium/worker-1");
    assert_eq!(w1["action"], "update");
    let fields: Vec<&str> = w1["changes"]
        .as_array()
        .unwrap()
        .iter()
        .map(|c| c["field"].as_str().unwrap())
        .collect();
    assert_eq!(fields, ["model", "restart"]);
    assert_eq!(w1["changes"][0]["from"], Value::Null);
    assert_eq!(w1["changes"][0]["to"], "opus");
    assert!(w1.get("drift").is_none());
    let t = entry(&plan.body, "target", "buildbox");
    assert_eq!(t["changes"][0]["field"], "address");

    td.post("/api/fleet/apply", body(&changed)).await.ok();
    let w1 = agent_by_key(&td, "terrarium/worker-1").await;
    assert_eq!(w1["model"], "opus");
    assert_eq!(w1["restart"], "always");
    assert_eq!(
        td.get("/api/targets/buildbox").await.ok()["address"],
        "ci@buildbox.lan"
    );
    assert_eq!(
        td.get("/api/projects/terrarium").await.ok()["description"],
        "Changed"
    );
    let again = td.post("/api/fleet/plan", body(&changed)).await;
    assert_eq!(again.ok()["summary"]["unchanged"], 7);
}

#[tokio::test]
async fn an_api_edit_shows_as_drift_and_the_apply_restores_the_file() {
    let td = TestDaemon::start().await;
    td.post("/api/fleet/apply", body(EXAMPLE)).await.ok();
    let lead = agent_by_key(&td, "terrarium/lead").await;
    let id = lead["id"].as_str().unwrap();
    // A fleet object is not file-managed: the API may change it.
    td.patch(&format!("/api/agents/{id}"), json!({"prompt": "default"}))
        .await
        .ok();
    td.patch("/api/targets/buildbox", json!({"port": 2222}))
        .await
        .ok();
    // The fleet stays the owner.
    assert_eq!(
        agent_by_key(&td, "terrarium/lead").await["source"],
        "fleet:terrarium"
    );
    assert_eq!(
        td.get("/api/targets/buildbox").await.ok()["source"],
        "fleet:terrarium"
    );

    let plan = td.post("/api/fleet/plan", body(EXAMPLE)).await;
    assert_eq!(plan.ok()["summary"]["drift"], 2, "{}", plan.body);
    let e = entry(&plan.body, "agent", "terrarium/lead");
    assert_eq!(e["action"], "update");
    assert_eq!(e["drift"], json!(["prompt"]));
    assert_eq!(e["changes"][0]["from"], "default");
    assert_eq!(e["changes"][0]["to"], "project-lead");
    assert_eq!(
        entry(&plan.body, "target", "buildbox")["drift"],
        json!(["port"])
    );

    td.post("/api/fleet/apply", body(EXAMPLE)).await.ok();
    assert_eq!(
        agent_by_key(&td, "terrarium/lead").await["prompt"],
        "project-lead"
    );
    let t = td.get("/api/targets/buildbox").await;
    assert!(t.ok().get("port").is_none(), "{}", t.body);
    assert_eq!(
        td.post("/api/fleet/plan", body(EXAMPLE)).await.ok()["summary"]["drift"],
        0
    );

    // An object deleted through the API is drift too: the apply makes it again.
    td.delete(&format!("/api/agents/{id}")).await;
    let plan = td.post("/api/fleet/plan", body(EXAMPLE)).await;
    let e = entry(plan.ok(), "agent", "terrarium/lead");
    assert_eq!(e["action"], "create");
    assert_eq!(e["drift"], json!(["(deleted)"]));
}

#[tokio::test]
async fn prune_deletes_only_with_the_flag_and_confirms_live_agents() {
    let td = TestDaemon::start().await;
    td.post("/api/fleet/apply", body(EXAMPLE)).await.ok();
    let w2 = agent_by_key(&td, "terrarium/worker-2").await;
    let w2_id = w2["id"].as_str().unwrap().to_owned();

    let start = EXAMPLE
        .find("[[projects.agents]]\nname = \"worker-2\"")
        .unwrap();
    let end = EXAMPLE.find("# A project target").unwrap();
    let smaller = format!("{}{}", &EXAMPLE[..start], &EXAMPLE[end..]);

    let plan = td.post("/api/fleet/plan", body(&smaller)).await;
    let e = entry(plan.ok(), "agent", "terrarium/worker-2");
    assert_eq!(e["action"], "delete");
    assert_eq!(plan.body["summary"]["delete"], 1);

    // Without --prune the agent stays and the entry is skipped.
    let r = td.post("/api/fleet/apply", body(&smaller)).await;
    assert_eq!(
        entry(r.ok(), "agent", "terrarium/worker-2")["result"],
        "skipped"
    );
    agent_by_key(&td, "terrarium/worker-2").await;

    // A live agent needs confirm.
    td.post_empty(&format!("/api/agents/{w2_id}/start"))
        .await
        .ok();
    td.wait_status(&w2_id, "idle").await;
    let plan = td.post("/api/fleet/plan", body(&smaller)).await;
    assert_eq!(
        entry(plan.ok(), "agent", "terrarium/worker-2")["live"],
        true
    );
    let mut b = body(&smaller);
    b["prune"] = json!(true);
    let r = td.post("/api/fleet/apply", b.clone()).await;
    r.err(409, "confirm_required");
    assert_eq!(
        r.body["error"]["details"]["agents"],
        json!(["terrarium/worker-2"])
    );
    agent_by_key(&td, "terrarium/worker-2").await;

    b["confirm"] = json!(true);
    let r = td.post("/api/fleet/apply", b).await;
    assert_eq!(
        entry(r.ok(), "agent", "terrarium/worker-2")["result"],
        "done"
    );
    td.get(&format!("/api/agents/{w2_id}"))
        .await
        .err(404, "not_found");
    let plan = td.post("/api/fleet/plan", body(&smaller)).await;
    assert_eq!(plan.ok()["summary"]["delete"], 0);
    assert_eq!(plan.body["summary"]["unchanged"], 6);
}

#[tokio::test]
async fn prune_releases_projects_and_deletes_stopped_agents_without_confirm() {
    let td = TestDaemon::start().await;
    let two = r#"
name = "two"
[defaults]
harness = "codex"
[[projects]]
name = "keep"
[[projects.agents]]
name = "a"
[[projects]]
name = "drop"
[[projects.agents]]
name = "b"
"#;
    td.post("/api/fleet/apply", body(two)).await.ok();
    let one = &two[..two.find("[[projects]]\nname = \"drop\"").unwrap()];
    let mut b = body(one);
    b["prune"] = json!(true);
    let r = td.post("/api/fleet/apply", b).await;
    assert_eq!(entry(r.ok(), "agent", "drop/b")["action"], "delete");
    assert_eq!(entry(&r.body, "project", "drop")["action"], "release");
    td.get("/api/agents/drop%2Fb").await.err(404, "not_found");
    // The project stays as an API project, with its board.
    assert_eq!(td.get("/api/projects/drop").await.ok()["source"], "api");
    assert_eq!(
        td.get("/api/projects/keep").await.ok()["source"],
        "fleet:two"
    );
    let row = &td.get("/api/fleet").await.body[0];
    assert_eq!(row["projects"], json!(["keep"]));
}

#[tokio::test]
async fn invalid_files_and_unknown_references_are_refused() {
    let td = TestDaemon::start().await;
    let bad = r#"
name = "bad"
[[projects]]
name = "p"
[[projects.agents]]
name = "a"
harness = "claude"
subscription = "nowhere"
[projects.agents.policy]
targets = ["ghost"]
"#;
    let v = td.post("/api/fleet/validate", body(bad)).await;
    assert_eq!(v.ok()["valid"], false);
    let errors: Vec<String> = v.body["errors"]
        .as_array()
        .unwrap()
        .iter()
        .map(|e| e["message"].as_str().unwrap().to_owned())
        .collect();
    assert!(
        errors.iter().any(|e| e.contains("subscription `nowhere`")),
        "{errors:?}"
    );
    assert!(
        errors.iter().any(|e| e.contains("target `ghost`")),
        "{errors:?}"
    );

    let plan = td.post("/api/fleet/plan", body(bad)).await;
    assert_eq!(plan.ok()["valid"], false);
    assert_eq!(plan.body["entries"], json!([]));
    let r = td.post("/api/fleet/apply", body(bad)).await;
    r.err(422, "invalid");
    assert_eq!(
        r.body["error"]["details"]["errors"]
            .as_array()
            .unwrap()
            .len(),
        2
    );
    td.get("/api/projects/p").await.err(404, "not_found");

    let r = td.post("/api/fleet/apply", body("name = ")).await;
    r.err(422, "invalid");
    td.post("/api/fleet/apply", json!({"text": "x"}))
        .await
        .err(400, "bad_request");

    // A store refusal rolls the whole apply back.
    let refused = r#"
name = "refused"
[[projects]]
name = "q"
prefix = "lower"
[[projects.agents]]
name = "a"
harness = "claude"
"#;
    td.post("/api/fleet/apply", body(refused))
        .await
        .err(422, "invalid");
    td.get("/api/projects/q").await.err(404, "not_found");
}

#[tokio::test]
async fn file_definitions_and_other_fleets_are_conflicts() {
    let td = TestDaemon::with_config(|c| {
        write(
            c,
            "agents/lead.toml",
            "project = \"terrarium\"\nharness = \"claude\"\n",
        );
        write(c, "projects/terrarium.toml", "description = \"file\"\n");
    })
    .await;
    let plan = td.post("/api/fleet/plan", body(EXAMPLE)).await;
    assert_eq!(plan.ok()["summary"]["conflict"], 2, "{}", plan.body);
    let e = entry(&plan.body, "agent", "terrarium/lead");
    assert_eq!(e["action"], "conflict");
    assert!(e["reason"].as_str().unwrap().contains("agents/lead.toml"));
    let r = td.post("/api/fleet/apply", body(EXAMPLE)).await;
    r.err(409, "conflict");
    td.get("/api/agents/terrarium%2Fworker-1")
        .await
        .err(404, "not_found");

    // Another fleet that owns an object.
    let td = TestDaemon::start().await;
    td.post("/api/fleet/apply", body(EXAMPLE)).await.ok();
    let other = EXAMPLE.replace(
        "name = \"terrarium\"\ndescription = \"The Terrarium project",
        "name = \"other\"\ndescription = \"The Terrarium project",
    );
    let plan = td.post("/api/fleet/plan", body(&other)).await;
    let e = entry(plan.ok(), "project", "terrarium");
    assert_eq!(e["action"], "conflict");
    assert_eq!(e["reason"], "owned by fleet `terrarium`");
}

#[tokio::test]
async fn an_existing_api_project_is_adopted() {
    let td = TestDaemon::start().await;
    td.project("terrarium").await;
    td.agent(
        "terrarium",
        "lead",
        json!({"prompt": "project-lead", "roles": ["project_admin"]}),
    )
    .await;
    let plan = td.post("/api/fleet/plan", body(EXAMPLE)).await;
    let e = entry(plan.ok(), "agent", "terrarium/lead");
    assert_eq!(e["adopt"], true);
    assert_eq!(e["action"], "update", "{e}");
    let p = entry(&plan.body, "project", "terrarium");
    assert_eq!(p["adopt"], true);
    td.post("/api/fleet/apply", body(EXAMPLE)).await.ok();
    let p = td.get("/api/projects/terrarium").await;
    assert_eq!(p.ok()["card_prefix"], "TER");
    // The adopted objects show their new owner.
    assert_eq!(p.body["source"], "fleet:terrarium");
    assert_eq!(
        agent_by_key(&td, "terrarium/lead").await["source"],
        "fleet:terrarium"
    );
    let plan = td.post("/api/fleet/plan", body(EXAMPLE)).await;
    assert_eq!(plan.ok()["summary"]["unchanged"], 7);
}

#[tokio::test]
async fn policy_refuses_members_and_limits_project_admins() {
    let td = TestDaemon::start().await;
    td.post("/api/fleet/apply", body(EXAMPLE)).await.ok();
    td.project("ops").await;
    let member = td.agent("terrarium", "member", json!({})).await;
    let member = member["id"].as_str().unwrap();
    let lead = agent_by_key(&td, "terrarium/lead").await;
    let lead = lead["id"].as_str().unwrap();

    let own = r#"
name = "team"
[[projects]]
name = "terrarium"
description = "The Terrarium tool itself"
prefix = "TER"
[[projects.repos]]
name = "terrarium"
url = "git@github.com:wiltaylor/terrarium.git"
default_branch = "main"
[[projects.agents]]
name = "helper"
harness = "claude"
"#;
    td.post_as(member, "/api/fleet/apply", body(own))
        .await
        .err(403, "forbidden");
    td.post_as(member, "/api/fleet/plan", body(own))
        .await
        .err(403, "forbidden");
    // Validate is open to any caller.
    td.post_as(member, "/api/fleet/validate", body(own))
        .await
        .ok();

    // The project is owned by fleet `terrarium`: `team` conflicts on it.
    td.post_as(lead, "/api/fleet/apply", body(own))
        .await
        .err(409, "conflict");
    let own = own.replace("name = \"team\"", "name = \"terrarium\"");
    // Same fleet name, fewer objects: the stored fleet owns the global
    // subscription, so a project admin may not replace its record, with
    // or without a prune (F10).
    let r = td.post_as(lead, "/api/fleet/apply", body(&own)).await;
    r.err(403, "forbidden");
    assert!(
        r.body["error"]["message"]
            .as_str()
            .unwrap()
            .contains("owns subscription `anthropic-max`"),
        "{}",
        r.body
    );
    let mut b = body(&own);
    b["prune"] = json!(true);
    td.post_as(lead, "/api/fleet/apply", b)
        .await
        .err(403, "forbidden");
    td.get("/api/agents/terrarium%2Fhelper")
        .await
        .err(404, "not_found");

    let other = r#"
name = "elsewhere"
[[projects]]
name = "ops"
[[projects.agents]]
name = "x"
harness = "claude"
"#;
    td.post_as(lead, "/api/fleet/apply", body(other))
        .await
        .err(403, "forbidden");
    let new_project = other.replace("name = \"ops\"", "name = \"brand-new\"");
    td.post_as(lead, "/api/fleet/apply", body(&new_project))
        .await
        .err(403, "forbidden");
    let global = r#"
name = "globals"
[[targets]]
name = "shared"
kind = "ssh"
address = "u@h"
"#;
    td.post_as(lead, "/api/fleet/apply", body(global))
        .await
        .err(403, "forbidden");
    let sysadmin_grant = r#"
name = "grant"
[[projects]]
name = "terrarium"
description = "The Terrarium tool itself"
prefix = "TER"
[[projects.agents]]
name = "boss"
harness = "claude"
roles = ["sysadmin"]
"#;
    td.post_as(lead, "/api/fleet/apply", body(sysadmin_grant))
        .await
        .err(403, "forbidden");
    td.get("/api/agents/terrarium%2Fboss")
        .await
        .err(404, "not_found");

    // A project admin sees the fleets of its project only.
    td.post("/api/fleet/apply", body(other)).await.ok();
    let names: Vec<String> = td
        .get_as(lead, "/api/fleet")
        .await
        .ok()
        .as_array()
        .unwrap()
        .iter()
        .map(|r| r["name"].as_str().unwrap().to_owned())
        .collect();
    assert_eq!(names, ["terrarium"]);
    td.get_as(lead, "/api/fleet/elsewhere")
        .await
        .err(404, "not_found");
}

#[tokio::test]
async fn fleet_files_in_the_config_dir_apply_at_start_and_reload() {
    let small = r#"
name = "dir"
[[projects]]
name = "dirproj"
[[projects.agents]]
name = "a"
harness = "claude"
start = true
"#;
    let mut td = TestDaemon::with_config(|c| {
        write(c, "fleets/dir.toml", small);
        write(c, "broken.fleet.toml", "name = \"broken\"\n[[projects]]\n");
    })
    .await;
    let a = agent_by_key(&td, "dirproj/a").await;
    // No auto-start.
    assert_eq!(a["status"], "stopped");
    let rows = td.get("/api/fleet").await;
    assert_eq!(rows.ok()[0]["origin"], "file:fleets/dir.toml");

    let r = td.post_empty("/api/reload").await;
    let errors = r.ok()["errors"].as_array().unwrap().clone();
    assert!(
        errors
            .iter()
            .any(|e| e.as_str().unwrap().starts_with("broken.fleet.toml:")),
        "{errors:?}"
    );

    // A changed file updates on reload; a removed agent stays (no prune).
    write(
        &td.config,
        "fleets/dir.toml",
        &small.replace(
            "name = \"a\"\nharness = \"claude\"",
            "name = \"b\"\nharness = \"pi\"",
        ),
    );
    td.post_empty("/api/reload").await.ok();
    agent_by_key(&td, "dirproj/b").await;
    agent_by_key(&td, "dirproj/a").await;

    // The fleet survives a daemon restart, and the plan stays clean.
    td.boot(None, None).await;
    let plan = td
        .post(
            "/api/fleet/plan",
            body(&std::fs::read_to_string(td.config.join("fleets/dir.toml")).unwrap()),
        )
        .await;
    assert_eq!(plan.ok()["summary"]["unchanged"], 2, "{}", plan.body);
    assert_eq!(entry(&plan.body, "agent", "dirproj/a")["action"], "delete");
}

#[tokio::test]
async fn start_starts_the_agents_that_ask_for_it() {
    let td = TestDaemon::start().await;
    let text = r#"
name = "s"
[[projects]]
name = "sp"
[[projects.agents]]
name = "on"
harness = "claude"
start = true
[[projects.agents]]
name = "off"
harness = "claude"
"#;
    let mut b = body(text);
    b["start"] = json!(true);
    let r = td.post("/api/fleet/apply", b.clone()).await;
    assert_eq!(r.ok()["started"], json!(["sp/on"]), "{}", r.body);
    let on = agent_by_key(&td, "sp/on").await;
    td.wait_status(on["id"].as_str().unwrap(), "idle").await;
    assert_eq!(agent_by_key(&td, "sp/off").await["status"], "stopped");
    // It runs already: nothing more starts.
    let r = td.post("/api/fleet/apply", b).await;
    assert!(r.ok().get("started").is_none(), "{}", r.body);
}

#[tokio::test]
async fn export_captures_a_hand_built_project() {
    let td = TestDaemon::start().await;
    td.project("hand").await;
    td.post(
        "/api/projects/hand/repos",
        json!({"url": "https://example.com/x/tool.git", "path": "src"}),
    )
    .await
    .ok();
    td.agent(
        "hand",
        "lead",
        json!({"roles": ["project_admin"], "prompt": "project-lead"}),
    )
    .await;
    td.agent(
        "hand",
        "worker",
        json!({"harness": "pi", "model": "m", "policy": {"targets": ["hand-box"]}}),
    )
    .await;
    td.post(
        "/api/targets",
        // An API project target gets the project prefix: `hand-box`.
        json!({"name": "box", "kind": "ssh", "project": "hand", "address": "u@h"}),
    )
    .await
    .ok();

    let e = td.get("/api/fleet/export?project=hand").await;
    let content = e.ok()["content"].as_str().unwrap().to_owned();
    assert_eq!(e.body["fleet"], "hand");
    assert!(
        content.contains("[[projects.agents]]\nname = \"lead\""),
        "{content}"
    );
    assert!(content.contains("path = \"src\""), "{content}");

    // The export applies as a fleet that adopts the project unchanged.
    let plan = td.post("/api/fleet/plan", body(&content)).await;
    let s = &plan.ok()["summary"];
    assert_eq!(s["unchanged"], 4, "{}", plan.body);
    assert_eq!(s["create"], 0);
    td.post("/api/fleet/apply", body(&content)).await.ok();
    let plan = td.post("/api/fleet/plan", body(&content)).await;
    assert_eq!(plan.ok()["summary"]["unchanged"], 4);
    assert!(
        plan.body["entries"]
            .as_array()
            .unwrap()
            .iter()
            .all(|e| e.get("adopt").is_none())
    );

    td.get("/api/fleet/export?project=nope")
        .await
        .err(404, "not_found");
    td.get("/api/fleet/export?project=hand&name=apply")
        .await
        .err(422, "invalid");
}

/// TER-87: an export carries the job queue config of a project as
/// `[projects.delivery]`. Export then plan shows no change, and an apply
/// on a new daemon gives the same config. A fleet keeps the target rule
/// of `PUT /api/projects/{name}/delivery`.
#[tokio::test]
async fn export_and_apply_carry_the_job_queue_config() {
    let td = TestDaemon::start().await;
    td.project("dq").await;
    td.post(
        "/api/projects/dq/repos",
        json!({"url": "https://example.com/x/tool.git"}),
    )
    .await
    .ok();
    td.post(
        "/api/targets",
        json!({"name": "box", "kind": "ssh", "project": "dq", "address": "u@h"}),
    )
    .await
    .ok();
    td.agent("dq", "lead", json!({"roles": ["project_admin"]}))
        .await;
    let def = json!({
        "target": "dq-box",
        "recipe": "just ci::validate",
        "slots": 3,
        "next_repos": ["tool"],
        "work_dir": "/srv/jobs",
        "timeout_secs": 5400,
    });
    td.put("/api/projects/dq/delivery", def.clone()).await.ok();

    let e = td.get("/api/fleet/export?project=dq").await;
    let content = e.ok()["content"].as_str().unwrap().to_owned();
    assert!(content.contains("[projects.delivery]"), "{content}");
    let plan = td.post("/api/fleet/plan", body(&content)).await;
    let s = &plan.ok()["summary"];
    assert_eq!(s["update"], 0, "{}", plan.body);
    assert_eq!(s["create"], 0, "{}", plan.body);
    let p = entry(&plan.body, "project", "dq");
    assert_eq!(p["action"], "unchanged", "{p}");

    // A new daemon: the apply gives the same config, and a second plan
    // shows no change.
    let nd = TestDaemon::start().await;
    let r = nd.post("/api/fleet/apply", body(&content)).await;
    assert_eq!(r.ok()["summary"]["conflict"], 0, "{}", r.body);
    let got = nd.get("/api/projects/dq/delivery").await;
    assert_eq!(got.ok()["delivery"], def, "{}", got.body);
    assert_eq!(got.body["enabled"], true);
    let plan = nd.post("/api/fleet/plan", body(&content)).await;
    let s = &plan.ok()["summary"];
    assert_eq!(
        (s["update"].as_u64(), s["create"].as_u64()),
        (Some(0), Some(0)),
        "{}",
        plan.body
    );
    let again = nd.get("/api/fleet/export?project=dq").await;
    assert_eq!(again.ok()["content"].as_str().unwrap(), content);

    // The plan names the field; the project admin may change the recipe.
    let lead = agent_by_key(&nd, "dq/lead").await["id"]
        .as_str()
        .unwrap()
        .to_owned();
    let recipe = content.replace("recipe = \"just ci::validate\"", "recipe = \"true\"");
    assert_ne!(recipe, content);
    let plan = nd.post("/api/fleet/plan", body(&recipe)).await;
    let p = entry(plan.ok(), "project", "dq");
    assert_eq!(p["action"], "update", "{p}");
    assert_eq!(p["changes"][0]["field"], "delivery", "{p}");
    nd.post_as(&lead, "/api/fleet/apply", body(&recipe))
        .await
        .ok();
    let got = nd.get("/api/projects/dq/delivery").await;
    assert_eq!(got.ok()["delivery"]["recipe"], "true");

    // A global target: the operator yes, the project admin no.
    nd.post(
        "/api/targets",
        json!({"name": "gbox", "kind": "ssh", "address": "u@g"}),
    )
    .await
    .ok();
    let global = recipe.replace("target = \"dq-box\"", "target = \"gbox\"");
    assert_ne!(global, recipe);
    let r = nd.post_as(&lead, "/api/fleet/apply", body(&global)).await;
    r.err(403, "forbidden");
    assert!(
        r.body["error"]["message"]
            .as_str()
            .unwrap()
            .contains("projects.dq.delivery"),
        "{}",
        r.body
    );
    assert_eq!(
        nd.get("/api/projects/dq/delivery").await.ok()["delivery"]["target"],
        "dq-box"
    );
    nd.post("/api/fleet/apply", body(&global)).await.ok();
    assert_eq!(
        nd.get("/api/projects/dq/delivery").await.ok()["delivery"]["target"],
        "gbox"
    );
    // An unchanged config needs no right: the lead applies the same file.
    nd.post_as(&lead, "/api/fleet/apply", body(&global))
        .await
        .ok();

    // The target of another project: no, also for the operator.
    nd.project("other").await;
    nd.post(
        "/api/targets",
        json!({"name": "box", "kind": "ssh", "project": "other", "address": "u@o"}),
    )
    .await
    .ok();
    let foreign = global.replace("target = \"gbox\"", "target = \"other-box\"");
    nd.post("/api/fleet/apply", body(&foreign))
        .await
        .err(403, "forbidden");
    // A repo that the project does not have: no.
    let bad_repo = global.replace("next_repos = [\"tool\"]", "next_repos = [\"nope\"]");
    assert_ne!(bad_repo, global);
    nd.post("/api/fleet/apply", body(&bad_repo))
        .await
        .err(422, "invalid");

    // The file drops the table the fleet applied: the apply clears the
    // config, and the fleet no longer manages it.
    let head = "[projects.delivery]\n";
    let at = global.find(head).unwrap();
    let end = global[at..]
        .find("\n\n")
        .map_or(global.len(), |n| at + n + 1);
    let dropped = format!("{}{}", &global[..at], &global[end..]);
    let plan = nd.post("/api/fleet/plan", body(&dropped)).await;
    let p = entry(plan.ok(), "project", "dq");
    assert_eq!(p["changes"][0]["field"], "delivery", "{p}");
    assert_eq!(p["changes"][0]["to"], json!({}), "{p}");
    nd.post("/api/fleet/apply", body(&dropped)).await.ok();
    let got = nd.get("/api/projects/dq/delivery").await;
    assert_eq!(got.ok()["delivery"], json!({}), "{}", got.body);
    assert_eq!(got.body["enabled"], false);
    let plan = nd.post("/api/fleet/plan", body(&dropped)).await;
    assert_eq!(plan.ok()["summary"]["update"], 0, "{}", plan.body);
    // A config set by hand now stays: the fleet does not manage it.
    nd.put("/api/projects/dq/delivery", def.clone()).await.ok();
    let plan = nd.post("/api/fleet/plan", body(&dropped)).await;
    assert_eq!(plan.ok()["summary"]["update"], 0, "{}", plan.body);
}

/// Switch `project` to the column rules, as the C5 upgrade does.
async fn columns_flow(td: &TestDaemon, project: &str) {
    let p = project.to_owned();
    td.daemon
        .as_ref()
        .unwrap()
        .state
        .db
        .write("operator", move |t| {
            terrariumd::store::projects::set_columns_flow(t, &p)?;
            Ok(())
        })
        .await
        .unwrap();
}

fn options_path(project: &str, column: &str) -> String {
    format!("/api/projects/{project}/board/columns/{column}/options")
}

async fn options(td: &TestDaemon, project: &str, column: &str) -> Value {
    td.get(&options_path(project, column)).await.ok().clone()
}

/// The rules of a column without their IDs.
fn rules_of(o: &Value) -> Vec<Value> {
    o["rules"]
        .as_array()
        .unwrap()
        .iter()
        .map(|r| {
            let mut r = r.clone();
            r.as_object_mut().unwrap().remove("id");
            r
        })
        .collect()
}

/// TER-1305: an export of a board on column rules carries the options of
/// each column (checks, queue, rules), and an apply on a new daemon makes
/// the same board. A second apply writes nothing; a rule that the file
/// changes keeps its ID. A new column whose role another column holds is
/// refused with nothing written.
#[tokio::test]
async fn export_and_apply_carry_a_converted_board() {
    let td = TestDaemon::start().await;
    td.project("cb").await;
    columns_flow(&td, "cb").await;
    let review = options(&td, "cb", "review").await;
    let mut checks = review["checks"].clone();
    checks["require_comment_on_enter"] = json!(true);
    let rules = json!([
        {"name": "two", "trigger": {"kind": "count_reached", "count": 2},
         "actions": [{"verb": "mail", "args": {"to": ["operator"],
                      "subject": "two in \"review\"\nsee the board"}}]},
        {"name": "back", "trigger": {"kind": "lands"},
         "if_card": [{"not_column": ["ready"], "column": ["@review"]}],
         "actions": [{"verb": "card.move", "args": {"to": "ready"}}]},
    ]);
    td.put(
        &options_path("cb", "review"),
        json!({"base_version": review["version"], "checks": checks,
               "queue": review["queue"], "rules": rules}),
    )
    .await
    .ok();
    let mq = options(&td, "cb", "merge_queue").await;
    let queue = json!({"release": "batch", "batch_size": 3, "max_wait_seconds": 900,
                       "max_batch_cards": 6});
    td.put(
        &options_path("cb", "merge_queue"),
        json!({"base_version": mq["version"], "checks": mq["checks"], "queue": queue}),
    )
    .await
    .ok();
    let source = td.get("/api/projects/cb/board").await.ok().clone();
    let columns: Vec<String> = source["columns"]
        .as_array()
        .unwrap()
        .iter()
        .map(|c| c["name"].as_str().unwrap().to_owned())
        .collect();
    let mut want = BTreeMap::new();
    for c in &columns {
        want.insert(c.clone(), options(&td, "cb", c).await);
    }

    let e = td.get("/api/fleet/export?project=cb").await;
    let content = e.ok()["content"].as_str().unwrap().to_owned();
    assert!(content.contains("board_flow = \"columns\""), "{content}");
    assert_eq!(
        content.matches("[[projects.columns]]").count(),
        columns.len(),
        "{content}"
    );
    assert!(!content.contains(" id = \""), "{content}");
    assert!(content.contains("role = \"merge_queue\""), "{content}");
    // The export plans with no change on its own daemon.
    let plan = td.post("/api/fleet/plan", body(&content)).await;
    let p = entry(plan.ok(), "project", "cb");
    assert_eq!(p["action"], "unchanged", "{p}");

    // A new daemon: the apply makes the same board.
    let nd = TestDaemon::start().await;
    let r = nd.post("/api/fleet/apply", body(&content)).await;
    assert_eq!(r.ok()["summary"]["conflict"], 0, "{}", r.body);
    assert_eq!(
        nd.get("/api/projects/cb").await.ok()["board_flow"],
        "columns"
    );
    let mut versions = BTreeMap::new();
    for c in &columns {
        let got = options(&nd, "cb", c).await;
        let w = &want[c];
        assert_eq!(got["checks"], w["checks"], "{c}");
        assert_eq!(got["queue"], w["queue"], "{c}");
        assert_eq!(rules_of(&got), rules_of(w), "{c}");
        versions.insert(c.clone(), got["version"].clone());
    }
    let ids: Vec<Value> = options(&nd, "cb", "review").await["rules"]
        .as_array()
        .unwrap()
        .iter()
        .map(|r| r["id"].clone())
        .collect();
    assert_eq!(ids.len(), 2);

    // Idempotent: no change in the plan, and a second apply writes no
    // option (the versions stay). The export is the same file.
    let plan = nd.post("/api/fleet/plan", body(&content)).await;
    let s = &plan.ok()["summary"];
    assert_eq!(
        (s["update"].as_u64(), s["create"].as_u64()),
        (Some(0), Some(0)),
        "{}",
        plan.body
    );
    nd.post("/api/fleet/apply", body(&content)).await.ok();
    for c in &columns {
        assert_eq!(options(&nd, "cb", c).await["version"], versions[c], "{c}");
    }
    let again = nd.get("/api/fleet/export?project=cb").await;
    assert_eq!(again.ok()["content"].as_str().unwrap(), content);

    // The project admin changes one rule: the plan names `columns`, and
    // the rule keeps its ID.
    let lead = nd
        .agent("cb", "lead", json!({"roles": ["project_admin"]}))
        .await["id"]
        .as_str()
        .unwrap()
        .to_owned();
    let edited = content.replacen("count = 2", "count = 3", 1);
    assert_ne!(edited, content);
    let plan = nd.post("/api/fleet/plan", body(&edited)).await;
    let p = entry(plan.ok(), "project", "cb");
    assert_eq!(p["action"], "update", "{p}");
    let fields: Vec<&str> = p["changes"]
        .as_array()
        .unwrap()
        .iter()
        .map(|c| c["field"].as_str().unwrap())
        .collect();
    assert_eq!(fields, ["columns"], "{p}");
    nd.post_as(&lead, "/api/fleet/apply", body(&edited))
        .await
        .ok();
    let got = options(&nd, "cb", "review").await;
    assert_eq!(got["rules"][0]["trigger"]["count"], 3, "{got}");
    let now: Vec<Value> = got["rules"]
        .as_array()
        .unwrap()
        .iter()
        .map(|r| r["id"].clone())
        .collect();
    assert_eq!(now, ids);

    // A new column with the role of `merge_queue`: 422, nothing written.
    let bad = edited.replacen("name = \"merge_queue\"", "name = \"no_such_column\"", 1);
    assert_ne!(bad, edited);
    let bad = bad.replacen("count = 3", "count = 4", 1);
    let r = nd.post("/api/fleet/apply", body(&bad)).await;
    r.err(422, "invalid");
    assert!(
        r.body["error"]["message"]
            .as_str()
            .unwrap()
            .contains("projects.cb.columns.no_such_column"),
        "{}",
        r.body
    );
    assert_eq!(
        options(&nd, "cb", "review").await["rules"][0]["trigger"]["count"],
        3
    );
    let board = nd.get("/api/projects/cb/board").await.ok().clone();
    assert!(
        board["columns"]
            .as_array()
            .unwrap()
            .iter()
            .all(|c| c["name"] != "no_such_column"),
        "{board}"
    );
}

/// The column names of a board, in order.
async fn column_names(td: &TestDaemon, project: &str) -> Vec<String> {
    td.get(&format!("/api/projects/{project}/board"))
        .await
        .ok()["columns"]
        .as_array()
        .unwrap()
        .iter()
        .map(|c| c["name"].as_str().unwrap().to_owned())
        .collect()
}

/// Set the board of `project` to `columns`: each `(name, role, wip_limit)`,
/// a column of the board keeping its title.
async fn set_board(td: &TestDaemon, project: &str, columns: &[(&str, Option<&str>, Option<i64>)]) {
    let cols: Vec<Value> = columns
        .iter()
        .map(|(n, r, w)| json!({"name": n, "role": r, "wip_limit": w}))
        .collect();
    td.patch(
        &format!("/api/projects/{project}/board"),
        json!({ "columns": cols }),
    )
    .await
    .ok();
}

/// TER-1305, the legacy case: a converted board with a `validating`
/// column (a WIP limit and a rule on it), exported and applied to a
/// project whose board lacks the column. The apply adds it at its place,
/// with its role and WIP limit, then its options; a second plan shows no
/// change.
#[tokio::test]
async fn apply_adds_the_validating_column_of_a_legacy_board() {
    let legacy: [(&str, Option<&str>, Option<i64>); 9] = [
        ("backlog", None, None),
        ("ready", None, None),
        ("in_progress", Some("working"), None),
        ("validating", Some("validating"), Some(2)),
        ("review", Some("review"), None),
        ("merge_queue", Some("merge_queue"), None),
        ("accepted", Some("accepted"), None),
        ("blocked", Some("blocked"), None),
        ("done", None, None),
    ];
    let td = TestDaemon::start().await;
    td.project("lg").await;
    set_board(&td, "lg", &legacy).await;
    columns_flow(&td, "lg").await;
    let v = options(&td, "lg", "validating").await;
    let rules = json!([
        {"name": "validated", "trigger": {"kind": "lands"},
         "actions": [{"verb": "mail", "args": {"to": ["operator"],
                      "subject": "{{card.id}} validating"}}]},
    ]);
    td.put(
        &options_path("lg", "validating"),
        json!({"base_version": v["version"], "checks": v["checks"],
               "queue": v["queue"], "rules": rules}),
    )
    .await
    .ok();
    let want = options(&td, "lg", "validating").await;
    let content = td.get("/api/fleet/export?project=lg").await.ok()["content"]
        .as_str()
        .unwrap()
        .to_owned();
    assert!(
        content.contains("name = \"validating\"\nrole = \"validating\"\nwip_limit = 2\n"),
        "{content}"
    );

    // The new project has the standard board: no validating column.
    let nd = TestDaemon::start().await;
    nd.project("lg").await;
    let standard: Vec<(&str, Option<&str>, Option<i64>)> = legacy
        .iter()
        .copied()
        .filter(|(n, _, _)| *n != "validating")
        .collect();
    set_board(&nd, "lg", &standard).await;
    let r = nd.post("/api/fleet/apply", body(&content)).await;
    assert_eq!(r.ok()["summary"]["conflict"], 0, "{}", r.body);
    let names: Vec<&str> = legacy.iter().map(|(n, _, _)| *n).collect();
    assert_eq!(column_names(&nd, "lg").await, names);
    let board = nd.get("/api/projects/lg/board").await.ok().clone();
    let col = board["columns"]
        .as_array()
        .unwrap()
        .iter()
        .find(|c| c["name"] == "validating")
        .unwrap()
        .clone();
    assert_eq!(col["role"], "validating", "{col}");
    assert_eq!(col["wip_limit"], 2, "{col}");
    let got = options(&nd, "lg", "validating").await;
    assert_eq!(got["checks"], want["checks"]);
    assert_eq!(got["queue"], want["queue"]);
    assert_eq!(rules_of(&got), rules_of(&want));
    let plan = nd.post("/api/fleet/plan", body(&content)).await;
    let p = entry(plan.ok(), "project", "lg");
    assert_eq!(p["action"], "unchanged", "{p}");
}

/// TER-1305: a board that is not on column rules exports as before: no
/// `[[projects.columns]]`, and `board_flow` only for `rules`.
#[tokio::test]
async fn export_of_a_board_off_column_rules_has_no_columns() {
    let td = TestDaemon::start().await;
    td.project("pl").await;
    let flow = td.get("/api/projects/pl").await.ok()["board_flow"].clone();
    let content = td.get("/api/fleet/export?project=pl").await.ok()["content"]
        .as_str()
        .unwrap()
        .to_owned();
    if flow == "columns" {
        // The standard board of a new project (TER-1117) is on column
        // rules: nothing more to check here.
        return;
    }
    assert!(!content.contains("[[projects.columns]]"), "{content}");
    assert!(!content.contains("board_flow"), "{content}");
    td.patch("/api/projects/pl", json!({"board_flow": "rules"}))
        .await
        .ok();
    let content = td.get("/api/fleet/export?project=pl").await.ok()["content"]
        .as_str()
        .unwrap()
        .to_owned();
    assert!(content.contains("board_flow = \"rules\""), "{content}");
    assert!(!content.contains("[[projects.columns]]"), "{content}");
    // It plans with no change: `board_flow` is a key of a fleet project.
    let plan = td.post("/api/fleet/plan", body(&content)).await;
    assert_eq!(entry(plan.ok(), "project", "pl")["action"], "unchanged");
}

/// TER-86: an export carries the spawn policy (with the bound fields of
/// TER-74), `thread_open` (TER-93), and `calendar_write`, so export then
/// plan shows no change. A plan names the policy key that an update
/// changes, and a fleet keeps the rules of the spawn rights.
#[tokio::test]
async fn export_keeps_the_spawn_policy_and_rights() {
    let td = TestDaemon::start().await;
    td.project("sp").await;
    let spawn = json!({
        "templates": ["worker", "global:*"],
        "max_concurrent": 3,
        "max_per_day": 10,
        "max_lifetime": "4h",
        "may_spawn_children": true,
        "bound": true,
        "max_bound_lifetime": "3d",
    });
    let lead = td
        .agent(
            "sp",
            "lead",
            json!({"roles": ["project_admin"], "policy": {
                "spawn": spawn, "thread_open": true, "calendar_write": true}}),
        )
        .await;
    let lead = lead["id"].as_str().unwrap().to_owned();
    td.agent(
        "sp",
        "worker",
        json!({"policy": {"spawn": {"templates": ["worker"]}}}),
    )
    .await;

    let e = td.get("/api/fleet/export?project=sp").await;
    let content = e.ok()["content"].as_str().unwrap().to_owned();
    assert!(
        content.contains("[projects.agents.policy.spawn]"),
        "{content}"
    );
    let plan = td.post("/api/fleet/plan", body(&content)).await;
    let s = &plan.ok()["summary"];
    assert_eq!(s["unchanged"], 3, "{}", plan.body);
    assert_eq!(s["update"], 0, "{}", plan.body);
    for key in ["sp/lead", "sp/worker"] {
        let a = entry(&plan.body, "agent", key);
        assert_eq!(a["action"], "unchanged", "{a}");
        assert!(a.get("changes").is_none(), "{a}");
    }
    td.post("/api/fleet/apply", body(&content)).await.ok();
    let l = agent_by_key(&td, "sp/lead").await;
    assert_eq!(l["policy"]["spawn"], spawn, "{l}");
    assert_eq!(l["policy"]["thread_open"], true);
    assert_eq!(l["policy"]["calendar_write"], true);
    let plan = td.post("/api/fleet/plan", body(&content)).await;
    assert_eq!(plan.ok()["summary"]["update"], 0, "{}", plan.body);

    // A file without the lead's spawn block: the plan names `policy.spawn`.
    let head = "[projects.agents.policy.spawn]\n";
    let at = content.find(head).unwrap();
    let end = content[at..]
        .find("\n\n")
        .map_or(content.len(), |n| at + n + 1);
    let dropped = format!("{}{}", &content[..at], &content[end..]);
    let plan = td.post("/api/fleet/plan", body(&dropped)).await;
    let a = entry(plan.ok(), "agent", "sp/lead");
    assert_eq!(a["action"], "update", "{a}");
    assert_eq!(a["changes"][0]["field"], "policy.spawn", "{a}");
    assert_eq!(a["changes"][0]["to"], Value::Null);

    // The lead may not set `spawn.bound` on a member through a fleet.
    let bound = content.replace(
        "[projects.agents.policy.spawn]\ntemplates = [\"worker\"]",
        "[projects.agents.policy.spawn]\ntemplates = [\"worker\"]\nbound = true",
    );
    assert_ne!(bound, content);
    let r = td.post_as(&lead, "/api/fleet/apply", body(&bound)).await;
    r.err(403, "forbidden");
    assert!(
        r.body["error"]["message"]
            .as_str()
            .unwrap()
            .contains("spawn.bound"),
        "{}",
        r.body
    );
    // Nor drop its own spawn rights.
    td.post_as(&lead, "/api/fleet/apply", body(&dropped))
        .await
        .err(403, "forbidden");
}

/// TER-193: a lead made from a global template keeps its spawn policy,
/// `thread_open`, and `calendar_write` through an export: the entry names
/// the template and sets the policy keys over it, so export then plan
/// shows no change, and an apply keeps the rights.
#[tokio::test]
async fn export_of_a_template_agent_keeps_the_spawn_policy_and_rights() {
    let td = TestDaemon::start().await;
    td.project("tp").await;
    td.post(
        "/api/templates",
        json!({"name": "lead-tpl", "spec": {"harness": "claude", "roles": ["project_admin"],
                                            "policy": {"mail_to": ["*"]}}}),
    )
    .await
    .ok();
    let spawn = json!({
        "templates": ["card-impl"],
        "max_concurrent": 4,
        "bound": true,
        "max_bound_lifetime": "3d",
    });
    let lead = td
        .post(
            "/api/agents",
            json!({"name": "lead", "project": "tp", "template": "global:lead-tpl",
                   "policy": {"targets": [], "spawn": spawn, "thread_open": true,
                              "calendar_write": true}}),
        )
        .await;
    assert_eq!(lead.ok()["policy"]["spawn"], spawn, "{}", lead.body);

    let e = td.get("/api/fleet/export?project=tp").await;
    let content = e.ok()["content"].as_str().unwrap().to_owned();
    assert!(
        content.contains("template = \"global:lead-tpl\""),
        "{content}"
    );
    assert!(
        content.contains("[projects.agents.policy.spawn]"),
        "{content}"
    );
    assert!(content.contains("thread_open = true"), "{content}");
    assert!(content.contains("calendar_write = true"), "{content}");
    let plan = td.post("/api/fleet/plan", body(&content)).await;
    assert_eq!(plan.ok()["summary"]["update"], 0, "{}", plan.body);
    let a = entry(&plan.body, "agent", "tp/lead");
    assert_eq!(a["action"], "unchanged", "{a}");

    td.post("/api/fleet/apply", body(&content)).await.ok();
    let l = agent_by_key(&td, "tp/lead").await;
    assert_eq!(l["policy"]["spawn"], spawn, "{l}");
    assert_eq!(l["policy"]["thread_open"], true);
    assert_eq!(l["policy"]["calendar_write"], true);
}

#[tokio::test]
async fn the_lists_show_the_fleet_source_and_a_file_object_stays_file() {
    let mut td = TestDaemon::with_config(|c| {
        write(c, "projects/hand.toml", "description = \"file\"\n");
        write(
            c,
            "agents/helper.toml",
            "project = \"hand\"\nharness = \"claude\"\n",
        );
    })
    .await;
    td.post("/api/fleet/apply", body(EXAMPLE)).await.ok();

    let fleet = "fleet:terrarium".to_owned();
    let projects = sources(&td, "/api/projects").await;
    assert_eq!(projects["terrarium"], fleet, "{projects:?}");
    assert_eq!(projects["hand"], "file");
    let agents = sources(&td, "/api/agents").await;
    for a in ["terrarium/lead", "terrarium/worker-1", "terrarium/worker-2"] {
        assert_eq!(agents[a], fleet, "{agents:?}");
    }
    assert_eq!(agents["hand/helper"], "file");
    assert_eq!(sources(&td, "/api/targets").await["buildbox"], fleet);
    assert_eq!(
        sources(&td, "/api/subscriptions").await["anthropic-max"],
        fleet
    );

    // A reload syncs the definition files. It does not take the fleet
    // objects for file objects: it deletes and releases none of them.
    td.post_empty("/api/reload").await.ok();
    let agents = sources(&td, "/api/agents").await;
    assert_eq!(agents.len(), 4, "{agents:?}");
    assert_eq!(agents["terrarium/lead"], fleet);
    assert_eq!(agents["hand/helper"], "file");
    assert_eq!(sources(&td, "/api/projects").await["terrarium"], fleet);
    assert_eq!(sources(&td, "/api/targets").await["buildbox"], fleet);
    assert_eq!(
        sources(&td, "/api/subscriptions").await["anthropic-max"],
        fleet
    );

    // A file agent whose file is gone is deleted; the fleet agents stay.
    std::fs::remove_file(td.config.join("agents/helper.toml")).unwrap();
    td.post_empty("/api/reload").await.ok();
    let agents = sources(&td, "/api/agents").await;
    assert_eq!(agents.len(), 3, "{agents:?}");
    assert!(agents.values().all(|s| *s == fleet), "{agents:?}");

    // The labels survive a restart, and the plan stays clean.
    td.boot(None, None).await;
    assert_eq!(sources(&td, "/api/projects").await["terrarium"], fleet);
    let plan = td.post("/api/fleet/plan", body(EXAMPLE)).await;
    assert_eq!(plan.ok()["summary"]["unchanged"], 7, "{}", plan.body);
}

/// F10 (`docs/security-review.md`): a fleet apply with the name of a
/// stored fleet needs the rights over every object that the stored fleet
/// owns. The scratch scenario of the review: a member applies an empty
/// fleet with the name `terrarium`.
#[tokio::test]
async fn a_stored_fleet_is_replaced_only_by_its_owners() {
    let td = TestDaemon::start().await;
    td.post("/api/fleet/apply", body(EXAMPLE)).await.ok();
    td.project("ops").await;
    let member = td.agent("terrarium", "member", json!({})).await;
    let member = member["id"].as_str().unwrap();
    let lead = agent_by_key(&td, "terrarium/lead").await;
    let lead = lead["id"].as_str().unwrap();
    let ops_admin = td
        .agent("ops", "boss", json!({"roles": ["project_admin"]}))
        .await;
    let ops_admin = ops_admin["id"].as_str().unwrap();

    let empty = "version = 1\nname = \"terrarium\"\n";
    // A member: no fleet at all.
    for path in ["/api/fleet/apply", "/api/fleet/plan"] {
        td.post_as(member, path, body(empty))
            .await
            .err(403, "forbidden");
    }
    // A project admin of another project, empty or with its own project.
    td.post_as(ops_admin, "/api/fleet/apply", body(empty))
        .await
        .err(403, "forbidden");
    let ops_only = r#"
version = 1
name = "terrarium"
[[projects]]
name = "ops"
[[projects.agents]]
name = "x"
harness = "claude"
"#;
    td.post_as(ops_admin, "/api/fleet/apply", body(ops_only))
        .await
        .err(403, "forbidden");
    // The project admin of `terrarium`: the fleet owns a subscription.
    td.post_as(lead, "/api/fleet/apply", body(empty))
        .await
        .err(403, "forbidden");

    // The record did not change: the operator's content, origin, and
    // applier; the plan of the stored content is clean.
    let shown = td.get("/api/fleet/terrarium").await;
    let shown = shown.ok();
    assert_eq!(shown["origin"], "api");
    assert_eq!(shown["applied_by"], "operator", "{shown}");
    assert_eq!(shown["content"], EXAMPLE);
    assert_eq!(shown["plan"]["summary"]["delete"], 0, "{shown}");
    assert_eq!(shown["plan"]["summary"]["release"], 0, "{shown}");

    // The owner succeeds.
    td.post("/api/fleet/apply", body(EXAMPLE)).await.ok();

    // A fleet that only a project admin's project touches: that admin may
    // apply it again; the other project's admin, a member, and an empty
    // fleet may not.
    td.project("side").await;
    let lead = td
        .agent("side", "lead", json!({"roles": ["project_admin"]}))
        .await;
    let lead = lead["id"].as_str().unwrap();
    let mine = r#"
name = "leads"
[[projects]]
name = "side"
[[projects.agents]]
name = "helper"
harness = "claude"
"#;
    td.post_as(lead, "/api/fleet/apply", body(mine)).await.ok();
    let mine2 = mine.replace("harness = \"claude\"", "harness = \"pi\"");
    td.post_as(lead, "/api/fleet/apply", body(&mine2))
        .await
        .ok();
    let shown = td.get("/api/fleet/leads").await;
    assert_eq!(
        shown.ok()["applied_by"],
        format!("agent:{lead}"),
        "{}",
        shown.body
    );
    let theirs = ops_only.replace("name = \"terrarium\"", "name = \"leads\"");
    td.post_as(ops_admin, "/api/fleet/apply", body(&theirs))
        .await
        .err(403, "forbidden");
    td.post_as(member, "/api/fleet/apply", body(&mine2))
        .await
        .err(403, "forbidden");
    let r = td
        .post_as(lead, "/api/fleet/apply", body("name = \"leads\"\n"))
        .await;
    r.err(403, "forbidden");
    assert!(
        r.body["error"]["message"]
            .as_str()
            .unwrap()
            .contains("declares nothing"),
        "{}",
        r.body
    );
    assert_eq!(agent_by_key(&td, "side/helper").await["harness"], "pi");
}

/// F10: an API apply over a fleet from a file needs `confirm`.
#[tokio::test]
async fn an_api_apply_over_a_file_fleet_needs_confirm() {
    let small = r#"
name = "dir"
[[projects]]
name = "dirproj"
[[projects.agents]]
name = "a"
harness = "claude"
"#;
    let td = TestDaemon::with_config(|c| write(c, "fleets/dir.toml", small)).await;
    let changed = small.replace("harness = \"claude\"", "harness = \"pi\"");
    td.post("/api/fleet/apply", body(&changed))
        .await
        .err(409, "confirm_required");
    // A plan is fine.
    td.post("/api/fleet/plan", body(&changed)).await.ok();
    assert_eq!(
        td.get("/api/fleet/dir").await.ok()["origin"],
        "file:fleets/dir.toml"
    );
    let mut b = body(&changed);
    b["confirm"] = json!(true);
    td.post("/api/fleet/apply", b).await.ok();
    assert_eq!(td.get("/api/fleet/dir").await.ok()["origin"], "api");
}

/// F13: a fleet that adds a global target or a `cross_project` entry to an
/// agent's policy needs a sysadmin; project targets are fine for a
/// project admin.
#[tokio::test]
async fn fleet_policy_grants_need_sysadmin() {
    let td = TestDaemon::start().await;
    td.post("/api/fleet/apply", body(EXAMPLE)).await.ok();
    td.post(
        "/api/targets",
        json!({"name": "shared", "kind": "ssh", "address": "u@h"}),
    )
    .await
    .ok();
    td.project("side").await;
    td.post(
        "/api/targets",
        json!({"name": "side-box", "kind": "ssh", "project": "side", "address": "u@h"}),
    )
    .await
    .ok();
    let lead = td
        .agent("side", "lead", json!({"roles": ["project_admin"]}))
        .await;
    let lead = lead["id"].as_str().unwrap();
    let sys = td
        .agent("terrarium", "root", json!({"roles": ["sysadmin"]}))
        .await;
    let sys = sys["id"].as_str().unwrap();
    let fleet = |policy: &str| {
        format!(
            r#"
name = "grants"
[[projects]]
name = "side"
[[projects.agents]]
name = "w"
harness = "claude"
[projects.agents.policy]
{policy}
"#
        )
    };
    let local = fleet("targets = [\"side-box\"]\nmail_to = [\"lead\"]");
    let global = fleet("targets = [\"shared\"]");
    let cross = fleet("cross_project = [\"ops/*\"]");
    for f in [&global, &cross] {
        let r = td.post_as(lead, "/api/fleet/apply", body(f)).await;
        r.err(403, "forbidden");
        assert!(
            r.body["error"]["message"]
                .as_str()
                .unwrap()
                .contains("policy"),
            "{}",
            r.body
        );
        td.post_as(lead, "/api/fleet/plan", body(f))
            .await
            .err(403, "forbidden");
    }
    td.post_as(lead, "/api/fleet/apply", body(&local))
        .await
        .ok();
    td.post_as(sys, "/api/fleet/apply", body(&global))
        .await
        .ok();
    let w = agent_by_key(&td, "side/w").await;
    assert_eq!(w["policy"]["targets"], json!(["shared"]));
}

/// A prune does what `DELETE /api/agents/{id}` does: it stops the live
/// agent and moves the agent dir to `<state>/trash/<ulid>-<ts>/`.
#[tokio::test]
async fn prune_moves_the_agent_dir_to_the_trash() {
    let td = TestDaemon::start().await;
    let two = r#"
name = "two"
[defaults]
harness = "codex"
[[projects]]
name = "keep"
[[projects.agents]]
name = "a"
[[projects]]
name = "drop"
[[projects.agents]]
name = "b"
"#;
    td.post("/api/fleet/apply", body(two)).await.ok();
    let id = agent_by_key(&td, "drop/b").await["id"]
        .as_str()
        .unwrap()
        .to_owned();
    td.post_empty(&format!("/api/agents/{id}/start")).await.ok();
    td.wait_status(&id, "idle").await;
    let agent_dir = td.state.join("agents").join(&id);
    write(&agent_dir, "private/events.jsonl", "{}\n");

    let one = &two[..two.find("[[projects]]\nname = \"drop\"").unwrap()];
    let mut b = body(one);
    b["prune"] = json!(true);
    b["confirm"] = json!(true);
    let r = td.post("/api/fleet/apply", b).await;
    assert_eq!(entry(r.ok(), "agent", "drop/b")["result"], "done");
    td.get(&format!("/api/agents/{id}"))
        .await
        .err(404, "not_found");
    assert!(
        !terrariumd::supervisor::Supervisor::status(&*td.sup, id.parse().unwrap())
            .await
            .is_some_and(|s| s.is_live()),
        "the pruned agent still runs"
    );
    assert!(!agent_dir.exists());
    let trash: Vec<_> = std::fs::read_dir(td.state.join("trash"))
        .unwrap()
        .map(|e| e.unwrap().path())
        .collect();
    assert_eq!(trash.len(), 1, "{trash:?}");
    let name = trash[0].file_name().unwrap().to_string_lossy().into_owned();
    assert!(name.starts_with(&format!("{id}-")), "{name}");
    assert_eq!(
        std::fs::read(trash[0].join("private/events.jsonl")).unwrap(),
        b"{}\n"
    );
}

/// The grants of the secret `secret` to the agent with the ID `agent`.
async fn secret_grants_to(td: &TestDaemon, secret: &str, agent: &str) -> Vec<Value> {
    let key = format!("agent:{agent}");
    td.get(&format!("/api/secrets/{secret}/grants"))
        .await
        .ok()
        .as_array()
        .unwrap()
        .iter()
        .filter(|g| g["grantee"] == key)
        .cloned()
        .collect()
}

/// TER-88: a fleet apply that creates an agent from a template with
/// secret grants needs the applier's right to grant each secret: a
/// project_admin gets 403 for a global secret, and the apply makes no
/// agent. A fleet template with grants needs that right too. A secret of
/// the admin's own project is fine. Linking an agent that exists to a
/// template with grants gives it no grants.
#[tokio::test]
async fn fleet_template_grants_need_the_right_to_grant_them() {
    let td = TestDaemon::start().await;
    td.project("side").await;
    let lead = td
        .agent("side", "lead", json!({"roles": ["project_admin"]}))
        .await;
    let lead = lead["id"].as_str().unwrap();
    for s in ["global/gk", "side/sk"] {
        let r = td
            .put(
                &format!("/api/secrets/{s}"),
                json!({"kind": "api_key", "fields": {"value": "fleet-grant-value-9Xq2"}}),
            )
            .await;
        assert!(r.status.is_success(), "{}", r.body);
    }
    let r = td
        .post(
            "/api/templates",
            json!({"name": "tg", "project": "side",
                   "spec": {"harness": "claude", "grants": ["global/gk:read"]}}),
        )
        .await;
    assert_eq!(r.status, 201, "{}", r.body);

    // The create path: an agent from the operator's template.
    let create = r#"
name = "g1"
[[projects]]
name = "side"
[[projects.agents]]
name = "c1"
template = "tg"
"#;
    let r = td.post_as(lead, "/api/fleet/apply", body(create)).await;
    r.err(403, "forbidden");
    let m = r.body["error"]["message"].as_str().unwrap();
    assert!(m.contains("secret `global/gk`"), "{}", r.body);
    assert_eq!(td.get("/api/agents/side%2Fc1").await.status, 404);
    assert!(
        td.get("/api/secrets/global/gk/grants")
            .await
            .ok()
            .as_array()
            .unwrap()
            .is_empty()
    );

    // The template path: a fleet template with a global grant.
    let own = r#"
name = "g2"
[[projects]]
name = "side"
[[projects.templates]]
name = "own"
harness = "claude"
grants = ["global/gk"]
"#;
    let r = td.post_as(lead, "/api/fleet/apply", body(own)).await;
    r.err(403, "forbidden");
    let m = r.body["error"]["message"].as_str().unwrap();
    assert!(m.contains("secret `global/gk`"), "{}", r.body);
    let list = td.get("/api/templates?project=side").await.body.to_string();
    assert!(!list.contains("\"own\""), "{list}");

    // A secret of the admin's own project: the template and the agent.
    let mine = r#"
name = "g3"
[[projects]]
name = "side"
[[projects.templates]]
name = "mine"
harness = "claude"
grants = ["sk:read"]
[[projects.agents]]
name = "c2"
template = "mine"
"#;
    td.post_as(lead, "/api/fleet/apply", body(mine)).await.ok();
    let c2 = agent_by_key(&td, "side/c2").await;
    let g = secret_grants_to(&td, "side/sk", c2["id"].as_str().unwrap()).await;
    assert_eq!(g.len(), 1, "{g:?}");
    assert_eq!(g[0]["level"], "read");

    // An agent that exists, linked to a template with grants: no grant.
    let old = td.agent("side", "old", json!({})).await;
    let old = old["id"].as_str().unwrap().to_owned();
    // The project belongs to fleet `g3` now: the operator applies it
    // again with the agent `old` added.
    let relink = format!(
        "{mine}\n[[projects.agents]]\nname = \"old\"\nharness = \"claude\"\ntemplate = \"tg\"\n"
    );
    td.post("/api/fleet/apply", body(&relink)).await.ok();
    assert_eq!(
        agent_by_key(&td, "side/old").await["template"]["name"],
        "tg"
    );
    assert!(secret_grants_to(&td, "global/gk", &old).await.is_empty());
}

/// The template `fx/tg`.
async fn fx_template(td: &TestDaemon) -> Value {
    td.get("/api/templates?project=fx")
        .await
        .ok()
        .as_array()
        .unwrap()
        .iter()
        .find(|t| t["name"] == "tg")
        .cloned()
        .unwrap()
}

/// TER-88 round 2 finding 1: a fleet upsert of a template with grants
/// needs the right to grant each secret, also when it keeps the grants
/// list. A project_admin may not change the model of the operator's
/// template with a global grant; an apply that changes nothing, and one
/// that drops all the grants, pass. A sysadmin's change passes and makes
/// it `grants_by`.
#[tokio::test]
async fn a_fleet_upsert_that_keeps_template_grants_needs_the_right_to_grant_them() {
    let td = TestDaemon::start().await;
    td.project("fx").await;
    let lead = td
        .agent("fx", "lead", json!({"roles": ["project_admin"]}))
        .await;
    let lead = lead["id"].as_str().unwrap().to_owned();
    let sys = td.agent("fx", "sys", json!({"roles": ["sysadmin"]})).await;
    let sys = sys["id"].as_str().unwrap().to_owned();
    let r = td
        .put(
            "/api/secrets/global/gk",
            json!({"kind": "api_key", "fields": {"value": "fleet-grant-value-7Hd4"}}),
        )
        .await;
    assert!(r.status.is_success(), "{}", r.body);
    let fleet = |extra: &str| {
        format!(
            "name = \"fx\"\n[[projects]]\nname = \"fx\"\n[[projects.templates]]\n\
             name = \"tg\"\nharness = \"claude\"\n{extra}"
        )
    };
    let kept = |model: &str| {
        fleet(&format!(
            "model = \"{model}\"\ngrants = [\"global/gk:read\"]\n"
        ))
    };
    td.post("/api/fleet/apply", body(&kept("m1"))).await.ok();
    let t = fx_template(&td).await;
    assert_eq!(t["grants_by"], "operator", "{t}");

    // The project_admin changes the model and keeps the grant: 403.
    let r = td
        .post_as(&lead, "/api/fleet/apply", body(&kept("m2")))
        .await;
    r.err(403, "forbidden");
    let m = r.body["error"]["message"].as_str().unwrap();
    assert!(m.contains("the grant `global/gk:read`"), "{}", r.body);
    assert!(m.contains("secret `global/gk`"), "{}", r.body);
    let t = fx_template(&td).await;
    assert_eq!(t["spec"]["model"], "m1", "{t}");
    assert_eq!(t["grants_by"], "operator", "{t}");

    // The same values: no write, nothing to check.
    td.post_as(&lead, "/api/fleet/apply", body(&kept("m1")))
        .await
        .ok();
    assert_eq!(fx_template(&td).await["grants_by"], "operator");

    // A sysadmin changes the model: it becomes grants_by.
    td.post_as(&sys, "/api/fleet/apply", body(&kept("m3")))
        .await
        .ok();
    let t = fx_template(&td).await;
    assert_eq!(t["spec"]["model"], "m3", "{t}");
    assert_eq!(t["grants_by"], format!("agent:{sys}"), "{t}");

    // The project_admin changes the model and drops the grants: fine.
    td.post_as(&lead, "/api/fleet/apply", body(&fleet("model = \"m4\"\n")))
        .await
        .ok();
    let t = fx_template(&td).await;
    assert_eq!(t["spec"]["model"], "m4", "{t}");
    assert!(t["spec"].get("grants").is_none(), "{t}");
    assert!(t.get("grants_by").is_none(), "{t}");
}
