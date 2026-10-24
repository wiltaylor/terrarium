//! Fleet files: validate, plan, apply, drift, prune, policy, the config
//! dir, and export (`docs/fleet.md`).
#![allow(clippy::unwrap_used, clippy::expect_used)]

mod common;

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

#[tokio::test]
async fn apply_creates_and_a_second_apply_changes_nothing() {
    let td = TestDaemon::start().await;

    let v = td.post("/api/fleet/validate", body(EXAMPLE)).await;
    assert_eq!(v.ok()["valid"], true, "{}", v.body);
    assert_eq!(v.body["fleet"], "terrarium");
    assert_eq!(v.body["objects"].as_array().unwrap().len(), 6);

    let plan = td.post("/api/fleet/plan", body(EXAMPLE)).await;
    let s = &plan.ok()["summary"];
    assert_eq!(s["create"], 6, "{}", plan.body);
    assert_eq!(s["unchanged"], 0);

    let r = td.post("/api/fleet/apply", body(EXAMPLE)).await;
    assert_eq!(r.ok()["summary"]["create"], 6, "{}", r.body);
    assert_eq!(r.body["prune"], false);

    let p = td.get("/api/projects/terrarium").await;
    assert_eq!(p.ok()["card_prefix"], "TER");
    assert_eq!(p.body["source"], "api");
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
    assert_eq!(s["unchanged"], 6, "{}", plan.body);
    assert_eq!(s["create"], 0);
    assert_eq!(s["update"], 0);
    assert_eq!(s["drift"], 0);
    let r = td.post("/api/fleet/apply", body(EXAMPLE)).await;
    assert_eq!(r.ok()["summary"]["unchanged"], 6);
    assert_eq!(agent_by_key(&td, "terrarium/lead").await["id"], lead["id"]);

    let list = td.get("/api/fleet").await;
    let row = &list.ok()[0];
    assert_eq!(row["name"], "terrarium");
    assert_eq!(row["origin"], "api");
    assert_eq!(row["objects"], 6);
    assert_eq!(row["projects"], json!(["terrarium"]));
    let shown = td.get("/api/fleet/terrarium").await;
    assert_eq!(shown.ok()["content"], EXAMPLE);
    assert_eq!(shown.body["plan"]["summary"]["unchanged"], 6);
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
    assert_eq!(s["unchanged"], 3);
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
    assert_eq!(again.ok()["summary"]["unchanged"], 6);
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
    assert_eq!(plan.body["summary"]["unchanged"], 5);
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
    assert_eq!(
        td.get("/api/projects/terrarium").await.ok()["card_prefix"],
        "TER"
    );
    let plan = td.post("/api/fleet/plan", body(EXAMPLE)).await;
    assert_eq!(plan.ok()["summary"]["unchanged"], 6);
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
    // Same fleet name, fewer objects: a project admin may apply it, as
    // long as it does not prune what it cannot delete.
    let r = td.post_as(lead, "/api/fleet/apply", body(&own)).await;
    assert_eq!(
        entry(r.ok(), "agent", "terrarium/helper")["action"],
        "create"
    );
    let mut b = body(&own);
    b["prune"] = json!(true);
    // The prune would release the global subscription: sysadmin only.
    td.post_as(lead, "/api/fleet/apply", b)
        .await
        .err(403, "forbidden");

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
        json!({"harness": "pi", "model": "m", "policy": {"targets": ["box"]}}),
    )
    .await;
    td.post(
        "/api/targets",
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
