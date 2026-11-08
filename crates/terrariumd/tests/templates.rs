//! Agent templates (docs/api.md §21): CRUD and scopes, the rights, a
//! create from a template, from-agent, project files, and fleets. These
//! daemons have no config repo: `template_files.rs` tests the files.
#![allow(clippy::unwrap_used, clippy::expect_used)]

use crate::common;

use common::{TestDaemon, write};
use serde_json::{Value, json};

fn id(v: &Value) -> String {
    v["id"].as_str().unwrap().to_owned()
}

/// Projects `p` and `q`; a member and a project_admin of `p`, a
/// project_admin of `q`, and a sysadmin in `ops`.
async fn setup() -> (TestDaemon, String, String, String, String) {
    let td = TestDaemon::start().await;
    for p in ["p", "q", "ops"] {
        td.project(p).await;
    }
    td.post(
        "/api/targets",
        json!({"name": "global-box", "kind": "ssh", "address": "u@h"}),
    )
    .await
    .ok();
    td.post(
        "/api/targets",
        json!({"name": "p-box", "kind": "ssh", "project": "p", "address": "u@h"}),
    )
    .await
    .ok();
    let member = id(&td.agent("p", "member", json!({})).await);
    let admin = id(&td
        .agent("p", "admin", json!({"roles": ["project_admin"]}))
        .await);
    let qadmin = id(&td
        .agent("q", "admin", json!({"roles": ["project_admin"]}))
        .await);
    let sys = id(&td
        .agent("ops", "root", json!({"roles": ["sysadmin"]}))
        .await);
    (td, member, admin, qadmin, sys)
}

fn tpl(name: &str, project: Value, spec: Value) -> Value {
    json!({"name": name, "project": project, "spec": spec})
}

#[tokio::test]
async fn crud_scopes_and_events() {
    let (td, ..) = setup().await;
    let g = td
        .post(
            "/api/templates",
            json!({"name": "worker", "description": "A worker",
                   "spec": {"harness": "codex", "prompt": "worker"}}),
        )
        .await;
    // The operator's default scope is global.
    assert_eq!(g.ok()["project"], Value::Null);
    assert_eq!(g.body["revision"], 1);
    assert_eq!(g.body["source"], "config");
    assert_eq!(g.body["created_by"], "operator");
    assert_eq!(
        g.body["spec"],
        json!({"harness": "codex", "prompt": "worker",
               "policy": {"targets": [], "mail_to": ["*"]}, "restart": "on-failure"})
    );
    let p = td
        .post(
            "/api/templates",
            tpl("worker", json!("p"), json!({"harness": "claude"})),
        )
        .await;
    assert_eq!(p.ok()["project"], "p");
    // Unique in its scope.
    td.post(
        "/api/templates",
        tpl("worker", json!("p"), json!({"harness": "pi"})),
    )
    .await
    .err(409, "conflict");
    let bad = td
        .post(
            "/api/templates",
            tpl("x", Value::Null, json!({"harness": "pi", "name": "x"})),
        )
        .await;
    bad.err(422, "invalid");
    assert_eq!(bad.body["error"]["details"]["field"], "spec");
    td.post(
        "/api/templates",
        tpl("x", Value::Null, json!({"model": "m"})),
    )
    .await
    .err(422, "invalid");
    td.post(
        "/api/templates",
        tpl("x", json!("nope"), json!({"harness": "pi"})),
    )
    .await
    .err(404, "not_found");

    // Lists: global first; `?project=` adds the global ones.
    let all = td.get("/api/templates").await;
    let keys: Vec<String> = all
        .ok()
        .as_array()
        .unwrap()
        .iter()
        .map(|t| {
            format!(
                "{}/{}",
                t["project"].as_str().unwrap_or("_"),
                t["name"].as_str().unwrap()
            )
        })
        .collect();
    assert_eq!(keys, ["_/worker", "p/worker"]);
    assert_eq!(
        td.get("/api/templates?project=q")
            .await
            .ok()
            .as_array()
            .unwrap()
            .len(),
        1
    );
    assert_eq!(
        td.get("/api/templates?scope=global")
            .await
            .ok()
            .as_array()
            .unwrap()
            .len(),
        1
    );

    // Keys: a name (the global one for the operator), `p/name`, `_/name`,
    // `global:name`, and the ULID.
    for (key, want) in [
        ("worker", id(g.ok())),
        ("p%2Fworker", id(p.ok())),
        ("_%2Fworker", id(g.ok())),
        ("global:worker", id(g.ok())),
    ] {
        assert_eq!(
            id(td.get(&format!("/api/templates/{key}")).await.ok()),
            want
        );
    }

    // A change adds 1 to the revision.
    let u = td
        .patch(
            &format!("/api/templates/{}", id(p.ok())),
            json!({"spec": {"harness": "claude", "model": "m"}, "description": "d"}),
        )
        .await;
    assert_eq!(u.ok()["revision"], 2);
    assert_eq!(u.body["spec"]["model"], "m");
    assert_eq!(u.body["description"], "d");

    let events = td.get("/api/events?kind=template&since=0").await;
    let changes: Vec<&str> = events
        .ok()
        .as_array()
        .unwrap()
        .iter()
        .map(|e| e["data"]["change"].as_str().unwrap())
        .collect();
    assert_eq!(changes, ["created", "created", "updated"]);
    let audit = td.get("/api/audit?action=template.update").await;
    assert!(!audit.ok().as_array().unwrap().is_empty(), "{}", audit.body);

    td.delete(&format!("/api/templates/{}", id(g.ok()))).await;
    td.get("/api/templates/global:worker")
        .await
        .err(404, "not_found");
    // Deleting a project deletes its templates.
    td.post(
        "/api/templates",
        tpl("t", json!("ops"), json!({"harness": "pi"})),
    )
    .await
    .ok();
    td.post_empty("/api/agents/ops%2Froot/stop").await;
    td.delete("/api/agents/ops%2Froot?confirm=true").await;
    td.delete("/api/projects/ops?confirm=true").await;
    td.get("/api/templates/ops%2Ft").await.err(404, "not_found");
}

#[tokio::test]
async fn rights_by_role() {
    let (td, member, admin, qadmin, sys) = setup().await;
    let spec = json!({"harness": "claude"});
    // A member creates nothing, and reads its project's and the global ones.
    td.post_as(
        &member,
        "/api/templates",
        tpl("a", json!("p"), spec.clone()),
    )
    .await
    .err(403, "forbidden");
    // A project_admin: its project's only; the default scope is its project.
    let a = td
        .post_as(&admin, "/api/templates", json!({"name": "a", "spec": spec}))
        .await;
    assert_eq!(a.ok()["project"], "p");
    td.post_as(
        &admin,
        "/api/templates",
        tpl("b", Value::Null, spec.clone()),
    )
    .await
    .err(403, "forbidden");
    td.post_as(&admin, "/api/templates", tpl("b", json!("q"), spec.clone()))
        .await
        .err(403, "forbidden");
    td.patch_as(&qadmin, "/api/templates/p%2Fa", json!({"description": "x"}))
        .await
        .err(403, "forbidden");
    // A sysadmin: global ones too.
    td.post_as(&sys, "/api/templates", tpl("g", Value::Null, spec.clone()))
        .await
        .ok();
    let seen = td.get_as(&member, "/api/templates").await;
    let names: Vec<&str> = seen
        .ok()
        .as_array()
        .unwrap()
        .iter()
        .map(|t| t["name"].as_str().unwrap())
        .collect();
    assert_eq!(names, ["g", "a"]);
    td.get_as(&qadmin, "/api/templates/p%2Fa")
        .await
        .err(403, "forbidden");
    td.get_as(&qadmin, "/api/templates?project=p")
        .await
        .err(403, "forbidden");
    td.patch_as(&member, "/api/templates/a", json!({"description": "x"}))
        .await
        .err(403, "forbidden");
    td.patch_as(&admin, "/api/templates/a", json!({"description": "x"}))
        .await
        .ok();
    td.patch_as(
        &admin,
        "/api/templates/global:g",
        json!({"description": "x"}),
    )
    .await
    .err(403, "forbidden");
}

#[tokio::test]
async fn create_from_a_template() {
    let (td, member, admin, _, sys) = setup().await;
    td.post(
        "/api/templates",
        tpl(
            "worker",
            Value::Null,
            json!({"harness": "codex", "prompt": "global-worker", "model": "gm"}),
        ),
    )
    .await
    .ok();
    let p = td
        .post(
            "/api/templates",
            tpl(
                "worker",
                json!("p"),
                json!({"harness": "claude", "prompt": "worker", "model": "m1",
                       "policy": {"targets": ["p-box"], "mail_to": ["admin"]}}),
            ),
        )
        .await
        .ok()
        .clone();

    // The project's template wins over the global one; values are copied.
    let a = td
        .post_as(
            &admin,
            "/api/agents",
            json!({"name": "w1", "project": "p", "template": "worker"}),
        )
        .await;
    let a = a.ok();
    assert_eq!(a["harness"], "claude");
    assert_eq!(a["prompt"], "worker");
    assert_eq!(a["policy"]["targets"], json!(["p-box"]));
    assert_eq!(
        a["template"],
        json!({"id": p["id"], "name": "worker", "project": "p", "revision": 1})
    );
    // `global:` picks the global one.
    let g = td
        .post_as(
            &admin,
            "/api/agents",
            json!({"name": "w2", "project": "p", "template": "global:worker"}),
        )
        .await;
    assert_eq!(g.ok()["harness"], "codex");
    assert_eq!(g.body["template"]["project"], Value::Null);
    // Overrides win field by field; `policy` merges key by key; `null`
    // clears.
    let o = td
        .post_as(
            &admin,
            "/api/agents",
            json!({"name": "w3", "project": "p", "template": "worker",
                   "model": null, "restart": "never", "policy": {"mail_to": ["*"]}}),
        )
        .await;
    let o = o.ok();
    assert!(o.get("model").is_none(), "{o}");
    assert_eq!(o["restart"], "never");
    assert_eq!(o["policy"], json!({"targets": ["p-box"], "mail_to": ["*"]}));
    // A later change of the template does not change the agents.
    td.patch(
        &format!("/api/templates/{}", p["id"].as_str().unwrap()),
        json!({"spec": {"harness": "pi"}}),
    )
    .await
    .ok();
    let w1 = td.get("/api/agents/p%2Fw1").await;
    assert_eq!(w1.ok()["harness"], "claude");
    assert_eq!(w1.body["template"]["revision"], 1);

    // The resolve route gives what a create copies.
    let r = td.get("/api/templates/worker/resolve?project=q").await;
    assert_eq!(r.ok()["agent"]["harness"], "codex");
    assert_eq!(r.body["agent"]["project"], "q");
    assert_eq!(r.body["template"]["project"], Value::Null);
    let r = td.get("/api/templates/worker/resolve?project=p").await;
    assert_eq!(r.ok()["agent"]["harness"], "pi");

    // No such template; a template of another project.
    td.post(
        "/api/agents",
        json!({"name": "w4", "project": "p", "template": "nope"}),
    )
    .await
    .err(404, "not_found");
    td.post(
        "/api/templates",
        tpl("qt", json!("q"), json!({"harness": "pi"})),
    )
    .await
    .ok();
    td.post(
        "/api/agents",
        json!({"name": "w4", "project": "p", "template": "q/qt"}),
    )
    .await
    .err(422, "invalid");
    // The member still creates no agent.
    td.post_as(
        &member,
        "/api/agents",
        json!({"name": "w5", "project": "p", "template": "worker"}),
    )
    .await
    .err(403, "forbidden");

    // A template grants nothing: the create checks the copied roles and
    // policy against the caller.
    td.post_as(
        &sys,
        "/api/templates",
        tpl(
            "root",
            Value::Null,
            json!({"harness": "claude", "roles": ["sysadmin"]}),
        ),
    )
    .await
    .ok();
    td.post_as(
        &sys,
        "/api/templates",
        tpl(
            "wide",
            Value::Null,
            json!({"harness": "claude", "policy": {"targets": ["global-box"],
                                                   "cross_project": ["q/*"]}}),
        ),
    )
    .await
    .ok();
    for t in ["root", "wide"] {
        td.post_as(
            &admin,
            "/api/agents",
            json!({"name": "esc", "project": "p", "template": t}),
        )
        .await
        .err(403, "forbidden");
    }
    // The override can drop what the caller may not grant.
    td.post_as(
        &admin,
        "/api/agents",
        json!({"name": "esc", "project": "p", "template": "wide",
               "policy": {"targets": [], "cross_project": []}}),
    )
    .await
    .ok();
    // A sysadmin may grant the global target from the template.
    td.post_as(
        &sys,
        "/api/agents",
        json!({"name": "wide", "project": "p", "template": "wide"}),
    )
    .await
    .ok();
}

#[tokio::test]
async fn a_template_from_an_agent() {
    let (td, member, admin, ..) = setup().await;
    td.agent(
        "p",
        "src",
        json!({"harness": "pi", "model": "m", "prompt": "worker",
               "policy": {"targets": ["p-box"]}}),
    )
    .await;
    let t = td
        .post_as(
            &admin,
            "/api/templates/from-agent/p%2Fsrc",
            json!({"name": "like-src"}),
        )
        .await;
    let t = t.ok();
    assert_eq!(t["project"], "p");
    assert_eq!(t["description"], "from agent p/src");
    assert_eq!(t["spec"]["harness"], "pi");
    assert_eq!(t["spec"]["model"], "m");
    assert_eq!(t["spec"]["policy"]["targets"], json!(["p-box"]));
    // A project_admin cannot make a global one; a member none.
    td.post_as(
        &admin,
        "/api/templates/from-agent/p%2Fsrc",
        json!({"name": "g", "project": null}),
    )
    .await
    .err(403, "forbidden");
    td.post_as(
        &member,
        "/api/templates/from-agent/p%2Fsrc",
        json!({"name": "m"}),
    )
    .await
    .err(403, "forbidden");
    // The operator makes a global one.
    let g = td
        .post(
            "/api/templates/from-agent/p%2Fsrc",
            json!({"name": "g", "project": null, "description": "copy"}),
        )
        .await;
    assert_eq!(g.ok()["project"], Value::Null);
    assert_eq!(g.body["description"], "copy");
}

#[tokio::test]
async fn project_file_templates() {
    let td = TestDaemon::with_config(|c| {
        write(
            c,
            "projects/filed.toml",
            "description = \"x\"\n\n[[templates]]\nname = \"worker\"\nharness = \"pi\"\nprompt = \"worker\"\n[templates.policy]\nmail_to = [\"lead\"]\n",
        );
    })
    .await;
    // `[[templates]]` of a project file is an import: the template is a
    // config template (a file of the config repo when the daemon has one),
    // and the API may change it.
    let t = td.get("/api/templates/filed%2Fworker").await;
    assert_eq!(t.ok()["source"], "config");
    assert_eq!(t.body["spec"]["policy"]["mail_to"], json!(["lead"]));
    td.patch("/api/templates/filed%2Fworker", json!({"description": "y"}))
        .await
        .ok();
    // A reload does not read the project file's template again; it tells
    // the operator to remove it.
    let r = td.post_empty("/api/reload").await;
    assert!(
        r.ok()["errors"].as_array().unwrap().iter().any(|e| e
            .as_str()
            .unwrap()
            .contains("template worker is in templates/filed/worker.wcl now")),
        "{}",
        r.body
    );
    assert_eq!(
        td.get("/api/templates/filed%2Fworker").await.ok()["description"],
        "y"
    );
    let a = td
        .post(
            "/api/agents",
            json!({"name": "w1", "project": "filed", "template": "worker"}),
        )
        .await;
    assert_eq!(a.ok()["harness"], "pi");
    assert_eq!(a.body["template"]["name"], "worker");
}

const FLEET: &str = r#"
name = "tf"

[[templates]]
name = "base"
harness = "codex"

[[projects]]
name = "tf"

[[projects.templates]]
name = "worker"
description = "A worker"
harness = "claude"
prompt = "default"
[projects.templates.policy]
mail_to = ["lead"]

[[projects.agents]]
name = "lead"
harness = "claude"
roles = ["project_admin"]

[[projects.agents]]
name = "w1"
template = "worker"
model = "m1"

[[projects.agents]]
name = "w2"
template = "base"
"#;

#[tokio::test]
async fn fleets_declare_and_use_templates() {
    let td = TestDaemon::start().await;
    let plan = td.post("/api/fleet/plan", json!({"content": FLEET})).await;
    let entries = plan.ok()["entries"].as_array().unwrap().clone();
    let tpl_keys: Vec<&str> = entries
        .iter()
        .filter(|e| e["kind"] == "template")
        .map(|e| e["key"].as_str().unwrap())
        .collect();
    assert_eq!(tpl_keys, ["_/base", "tf/worker"]);
    td.post("/api/fleet/apply", json!({"content": FLEET}))
        .await
        .ok();
    let t = td.get("/api/templates/tf%2Fworker").await;
    assert_eq!(t.ok()["source"], "fleet:tf");
    assert_eq!(
        td.get("/api/templates/global:base").await.ok()["source"],
        "fleet:tf"
    );
    let w1 = td.get("/api/agents/tf%2Fw1").await;
    assert_eq!(w1.ok()["harness"], "claude");
    assert_eq!(w1.body["model"], "m1");
    assert_eq!(w1.body["policy"]["mail_to"], json!(["lead"]));
    assert_eq!(w1.body["template"]["name"], "worker");
    assert_eq!(td.get("/api/agents/tf%2Fw2").await.ok()["harness"], "codex");
    // A second apply changes nothing.
    let plan = td.post("/api/fleet/plan", json!({"content": FLEET})).await;
    assert_eq!(plan.ok()["summary"]["unchanged"], 6, "{}", plan.body);

    // An entry may name a stored template that the file does not declare.
    td.post(
        "/api/templates",
        tpl("stored", json!("tf"), json!({"harness": "pi"})),
    )
    .await
    .ok();
    let more = format!("{FLEET}\n[[projects.agents]]\nname = \"w3\"\ntemplate = \"stored\"\n");
    let v = td
        .post("/api/fleet/validate", json!({"content": more}))
        .await;
    assert_eq!(v.ok()["valid"], true, "{}", v.body);
    let missing = more.replace("template = \"stored\"", "template = \"gone\"");
    let v = td
        .post("/api/fleet/validate", json!({"content": missing}))
        .await;
    assert_eq!(v.ok()["valid"], false, "{}", v.body);

    // The export writes the project's templates and the agents' template
    // names, and applies again with no change.
    let e = td.get("/api/fleet/export?project=tf").await;
    let content = e.ok()["content"].as_str().unwrap().to_owned();
    assert!(content.contains("[[projects.templates]]"), "{content}");
    assert!(content.contains("template = \"worker\""), "{content}");
    assert!(content.contains("template = \"global:base\""), "{content}");
    let plan = td
        .post("/api/fleet/plan", json!({"content": content}))
        .await;
    let s = &plan.ok()["summary"];
    assert_eq!(s["create"], 0, "{}", plan.body);
    assert_eq!(s["update"], 0, "{}", plan.body);

    // A prune of a template the file drops deletes it.
    let fewer = FLEET
        .replace("[[templates]]\nname = \"base\"\nharness = \"codex\"\n", "")
        .replace("template = \"base\"", "harness = \"codex\"");
    td.post("/api/fleet/apply", json!({"content": fewer, "prune": true}))
        .await
        .ok();
    td.get("/api/templates/global:base")
        .await
        .err(404, "not_found");
}

/// An agent's base system prompt (PLAN §8): create, PATCH, clear, the
/// limit, a template that sets it, a create that overrides or clears it,
/// and from-agent.
#[tokio::test]
async fn system_prompt_through_the_api_and_templates() {
    let td = TestDaemon::start().await;
    td.project("p").await;
    let a = td
        .agent(
            "p",
            "a",
            json!({"system_prompt": "You are {{agent.name}}."}),
        )
        .await;
    assert_eq!(a["system_prompt"], "You are {{agent.name}}.");
    let v = td
        .patch("/api/agents/p%2Fa", json!({"system_prompt": "Two."}))
        .await;
    assert_eq!(v.ok()["system_prompt"], "Two.");
    // Another field leaves it as it is.
    let v = td.patch("/api/agents/p%2Fa", json!({"model": "m"})).await;
    assert_eq!(v.ok()["system_prompt"], "Two.");
    // An empty text and `null` clear it.
    let v = td
        .patch("/api/agents/p%2Fa", json!({"system_prompt": " "}))
        .await;
    assert!(v.ok().get("system_prompt").is_none(), "{}", v.body);
    td.patch("/api/agents/p%2Fa", json!({"system_prompt": "Three."}))
        .await
        .ok();
    let v = td
        .patch("/api/agents/p%2Fa", json!({"system_prompt": null}))
        .await;
    assert!(v.ok().get("system_prompt").is_none(), "{}", v.body);
    // The limit is 32 KiB.
    let long = "x".repeat(32 * 1024 + 1);
    td.patch("/api/agents/p%2Fa", json!({"system_prompt": long}))
        .await
        .err(422, "invalid");
    td.post(
        "/api/agents",
        json!({"name": "b", "project": "p", "harness": "claude", "system_prompt": long}),
    )
    .await
    .err(422, "invalid");
    let at_limit = "x".repeat(32 * 1024);
    td.patch("/api/agents/p%2Fa", json!({"system_prompt": at_limit}))
        .await
        .ok();

    // A template copies it; the body wins; `null` clears it.
    let t = td
        .post(
            "/api/templates",
            tpl(
                "rev",
                json!("p"),
                json!({"harness": "claude", "system_prompt": "Review only."}),
            ),
        )
        .await;
    assert_eq!(t.ok()["spec"]["system_prompt"], "Review only.");
    let w = td
        .post(
            "/api/agents",
            json!({"name": "w1", "project": "p", "template": "rev"}),
        )
        .await;
    assert_eq!(w.ok()["system_prompt"], "Review only.");
    let w = td
        .post(
            "/api/agents",
            json!({"name": "w2", "project": "p", "template": "rev", "system_prompt": "Own."}),
        )
        .await;
    assert_eq!(w.ok()["system_prompt"], "Own.");
    let w = td
        .post(
            "/api/agents",
            json!({"name": "w3", "project": "p", "template": "rev", "system_prompt": null}),
        )
        .await;
    assert!(w.ok().get("system_prompt").is_none(), "{}", w.body);
    // An empty text in a spec is no system prompt.
    let t = td
        .post(
            "/api/templates",
            tpl(
                "empty",
                json!("p"),
                json!({"harness": "claude", "system_prompt": ""}),
            ),
        )
        .await;
    assert!(t.ok()["spec"].get("system_prompt").is_none(), "{}", t.body);
    // from-agent copies it.
    let t = td
        .post(
            "/api/templates/from-agent/p%2Fw2",
            json!({"name": "like-w2"}),
        )
        .await;
    assert_eq!(t.ok()["spec"]["system_prompt"], "Own.");
}

/// `system_prompt_file` in an agent file and in a project file template:
/// the path is relative to the file.
#[tokio::test]
async fn system_prompt_file_in_definition_files() {
    let td = TestDaemon::with_config(|c| {
        write(c, "prompts-own/lead.md", "Lead {{agent.name}}.\n");
        write(c, "projects/filed.toml", "[[templates]]\nname = \"worker\"\nharness = \"pi\"\nsystem_prompt_file = \"../prompts-own/lead.md\"\n");
        write(c, "agents/lead.toml", "project = \"filed\"\nharness = \"claude\"\nsystem_prompt_file = \"../prompts-own/lead.md\"\n");
    })
    .await;
    let a = td.get("/api/agents/filed%2Flead").await;
    assert_eq!(a.ok()["system_prompt"], "Lead {{agent.name}}.\n");
    let t = td.get("/api/templates/filed%2Fworker").await;
    assert_eq!(t.ok()["spec"]["system_prompt"], "Lead {{agent.name}}.\n");
}

const FLEET_SP: &str = r#"
name = "sp"

[[projects]]
name = "sp"

[[projects.templates]]
name = "worker"
harness = "claude"
system_prompt = """
You are a careful worker.
Write tests first.
"""

[[projects.agents]]
name = "lead"
harness = "codex"
system_prompt = "You lead {{project.name}}."

[[projects.agents]]
name = "w1"
template = "worker"
"#;

/// A fleet sets `system_prompt` on an agent and a template, and the
/// export applies again with no change.
#[tokio::test]
async fn fleets_carry_the_system_prompt() {
    let td = TestDaemon::start().await;
    td.post("/api/fleet/apply", json!({"content": FLEET_SP}))
        .await
        .ok();
    let lead = td.get("/api/agents/sp%2Flead").await;
    assert_eq!(lead.ok()["system_prompt"], "You lead {{project.name}}.");
    let w1 = td.get("/api/agents/sp%2Fw1").await;
    assert_eq!(
        w1.ok()["system_prompt"],
        "You are a careful worker.\nWrite tests first.\n"
    );
    let e = td.get("/api/fleet/export?project=sp").await;
    let content = e.ok()["content"].as_str().unwrap().to_owned();
    assert!(content.contains("system_prompt"), "{content}");
    let plan = td
        .post("/api/fleet/plan", json!({"content": content}))
        .await;
    let s = &plan.ok()["summary"];
    assert_eq!(s["create"], 0, "{}", plan.body);
    assert_eq!(s["update"], 0, "{}", plan.body);
    // A changed text is an update.
    let changed = FLEET_SP.replace("You lead", "You run");
    let plan = td
        .post("/api/fleet/plan", json!({"content": changed}))
        .await;
    assert_eq!(plan.ok()["summary"]["update"], 1, "{}", plan.body);
    // `system_prompt_file` needs the file path: the API refuses it.
    let v = td
        .post(
            "/api/fleet/validate",
            json!({"content": FLEET_SP.replace("system_prompt = \"You lead {{project.name}}.\"", "system_prompt_file = \"lead.md\"")}),
        )
        .await;
    assert_eq!(v.ok()["valid"], false, "{}", v.body);
    assert!(
        v.body.to_string().contains("system_prompt_file"),
        "{}",
        v.body
    );
}

/// Issue #14: `POST /api/agents`, `PATCH /api/agents/{id}`, and the
/// template writes refuse a subscription that does not exist, with the
/// subscriptions of the harness in the message; the list flags a template
/// whose subscription went away.
#[tokio::test]
async fn a_missing_subscription_is_refused_and_flagged() {
    let td = TestDaemon::start().await;
    td.project("p").await;
    td.post(
        "/api/subscriptions",
        json!({"name": "anthropic-ultra", "provider": "anthropic", "harnesses": ["claude"]}),
    )
    .await
    .ok();
    td.post(
        "/api/subscriptions",
        json!({"name": "spare", "provider": "anthropic", "harnesses": ["claude"]}),
    )
    .await
    .ok();
    let missing = |r: &common::Resp| {
        r.err(422, "invalid");
        assert_eq!(
            r.body["error"]["details"]["field"], "subscription",
            "{}",
            r.body
        );
        let m = r.body["error"]["message"].as_str().unwrap();
        assert!(m.contains("`anthropic-max-20x`"), "{m}");
        assert!(m.contains("anthropic-ultra"), "{m}");
        assert!(m.contains("default account"), "{m}");
    };

    // agent new
    let r = td
        .post(
            "/api/agents",
            json!({"name": "w1", "project": "p", "harness": "claude",
                   "subscription": "anthropic-max-20x"}),
        )
        .await;
    missing(&r);

    // template new
    let r = td
        .post(
            "/api/templates",
            tpl(
                "cw",
                Value::Null,
                json!({"harness": "claude", "subscription": "anthropic-max-20x"}),
            ),
        )
        .await;
    missing(&r);
    let t = td
        .post(
            "/api/templates",
            tpl(
                "cw",
                Value::Null,
                json!({"harness": "claude", "subscription": "spare"}),
            ),
        )
        .await;
    t.ok();
    // template edit
    let r = td
        .patch(
            "/api/templates/global:cw",
            json!({"spec": {"harness": "claude", "subscription": "anthropic-max-20x"}}),
        )
        .await;
    missing(&r);

    // agent set
    let w = td.agent("p", "w2", json!({})).await;
    let w = id(&w);
    let r = td
        .patch(
            &format!("/api/agents/{w}"),
            json!({"subscription": "anthropic-max-20x"}),
        )
        .await;
    missing(&r);
    let r = td
        .patch(
            &format!("/api/agents/{w}"),
            json!({"subscription": "anthropic-ultra"}),
        )
        .await;
    assert_eq!(r.ok()["subscription"], "anthropic-ultra");
    td.patch(&format!("/api/agents/{w}"), json!({"subscription": null}))
        .await
        .ok();

    // The subscription of the template goes away: template ls flags it,
    // and an agent create from it is refused.
    td.delete("/api/subscriptions/spare").await.ok();
    let list = td.get("/api/templates?scope=global").await;
    let row = list
        .ok()
        .as_array()
        .unwrap()
        .iter()
        .find(|t| t["name"] == "cw")
        .unwrap()
        .clone();
    let why = row["subscription_missing"].as_str().unwrap();
    assert!(
        why.contains("`spare`") && why.contains("anthropic-ultra"),
        "{why}"
    );
    let show = td.get("/api/templates/global:cw").await;
    assert!(
        show.ok()["subscription_missing"].is_string(),
        "{}",
        show.body
    );
    let r = td
        .post(
            "/api/agents",
            json!({"name": "w3", "project": "p", "template": "global:cw"}),
        )
        .await;
    r.err(422, "invalid");
    assert_eq!(
        r.body["error"]["details"]["field"], "subscription",
        "{}",
        r.body
    );
    // A template with a subscription that exists has no flag.
    td.patch(
        "/api/templates/global:cw",
        json!({"spec": {"harness": "claude"}}),
    )
    .await
    .ok();
    let show = td.get("/api/templates/global:cw").await;
    assert!(
        show.ok().get("subscription_missing").is_none(),
        "{}",
        show.body
    );
}
