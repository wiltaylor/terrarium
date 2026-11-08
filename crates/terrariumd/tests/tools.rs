//! Tool proxies, P1a (GitHub #21, docs/api.md §17.11): the definition
//! checks, the rights, the grants, and the `tools/<name>.toml` files.
#![allow(clippy::unwrap_used, clippy::expect_used)]

use crate::common;

use common::TestDaemon;
use serde_json::{Value, json};

/// Project `p` with a member, a sysadmin, and a target; project `q`; a
/// global target `box`; the global secret `tt` and the secret `pt` of `p`.
async fn setup() -> TestDaemon {
    let td = TestDaemon::start().await;
    td.project("p").await;
    td.project("q").await;
    td.project("ops").await;
    td.agent("p", "w1", json!({})).await;
    td.agent("p", "w2", json!({})).await;
    td.agent("q", "x1", json!({})).await;
    td.agent("ops", "root", json!({"roles": ["sysadmin"]}))
        .await;
    for t in [
        json!({"name": "box", "kind": "ssh", "address": "u@h"}),
        json!({"name": "box", "kind": "ssh", "project": "p", "address": "u@h"}),
    ] {
        td.post("/api/targets", t).await.ok();
    }
    let token = |v: &str| json!({"kind": "token", "fields": {"value": v}});
    td.put("/api/secrets/global/tt", token("gV7q2LmZ9xTr4KwB8nPd"))
        .await
        .ok();
    td.put("/api/secrets/p/pt", token("pX3k9QaW7vRt2YmL5cJh"))
        .await
        .ok();
    td
}

/// The ULID of agent `p/w1`, from the grant of `tracker-read` to it.
async fn r_w1(td: &TestDaemon) -> String {
    let g = td.get("/api/tools/tracker-read/grants").await.ok().clone();
    let key = g
        .as_array()
        .unwrap()
        .iter()
        .find(|x| x["grantee_label"] == "agent:p/w1")
        .unwrap()["grantee"]
        .as_str()
        .unwrap()
        .to_owned();
    key.trim_start_matches("agent:").to_owned()
}

fn tool(name: &str) -> Value {
    json!({"name": name, "target": "box", "command": ["/opt/scripts/tracker-read"]})
}

fn with(mut v: Value, extra: Value) -> Value {
    for (k, x) in extra.as_object().unwrap() {
        v[k] = x.clone();
    }
    v
}

/// Each definition rule of design D1 is checked at `POST /api/tools`.
#[tokio::test]
async fn a_definition_follows_the_rules() {
    let td = setup().await;
    let r = td
        .post(
            "/api/tools",
            with(
                tool("tracker-read"),
                json!({"description": "Read tickets", "timeout": "120s", "env_secrets": {"TRACKER_TOKEN": "tt"},
                       "args": {"allow": ["show"], "deny_flags": ["--config"]}, "max_output": "1MiB", "rate": "30/m"}),
            ),
        )
        .await;
    assert_eq!(r.status, 201, "{}", r.body);
    assert_eq!(r.body["source"], "api");
    assert_eq!(r.body["scope"], "global");
    assert_eq!(r.body["grants"], json!([]));
    assert_eq!(r.body["env_secrets"]["TRACKER_TOKEN"], "tt");
    // No secret value anywhere in the answer.
    assert!(!r.body.to_string().contains("gV7q2LmZ9xTr4KwB8nPd"));
    td.post("/api/tools", tool("tracker-read"))
        .await
        .err(409, "conflict");

    // A project tool on its own project's target, with its own secret.
    let r = td
        .post(
            "/api/tools",
            json!({"name": "deploy", "project": "p", "target": "p-box", "command": ["/opt/deploy"],
                   "env_secrets": {"TOKEN": "pt"}}),
        )
        .await;
    assert_eq!(r.status, 201, "{}", r.body);
    assert_eq!(r.body["scope"], "project");

    for (body, says) in [
        (tool("Bad_Name"), "bad tool name"),
        (tool("terrarium"), "terrarium"),
        (tool("git"), "sandbox PATH"),
        (tool("ls"), "sandbox PATH"),
        (with(tool("t1"), json!({"command": []})), "non-empty argv"),
        (with(tool("t2"), json!({"command": ["bash"]})), "no script"),
        (
            with(tool("t3"), json!({"command": ["/bin/sh", "-c", "x"]})),
            "-c",
        ),
        (
            with(tool("t4"), json!({"command": ["uv", "run", "/s/t.py"]})),
            "args.allow",
        ),
        (
            with(tool("t5"), json!({"target": "nowhere"})),
            "no target `nowhere`",
        ),
        (
            with(tool("t6"), json!({"project": "p"})),
            "not a target of project `p`",
        ),
        (
            with(tool("t7"), json!({"project": "q", "target": "p-box"})),
            "not a target of project `q`",
        ),
        (
            with(tool("t8"), json!({"env_secrets": {"T": "pt"}})),
            "not a global secret",
        ),
        (
            json!({"name": "t9", "project": "p", "target": "p-box", "command": ["/x"], "env_secrets": {"T": "tt"}}),
            "not a project `p` secret",
        ),
        (
            with(tool("t10"), json!({"env_secrets": {"LD_PRELOAD": "tt"}})),
            "may not set",
        ),
        (with(tool("t11"), json!({"timeout": "2d"})), "over 24h"),
        (with(tool("t12"), json!({"rate": "lots"})), "bad rate"),
    ] {
        let r = td.post("/api/tools", body.clone()).await;
        assert_eq!(r.status, 422, "{body}: {}", r.body);
        assert!(r.body.to_string().contains(says), "{body}: {}", r.body);
    }
    // There is no `shell` field: an unknown field is refused.
    let r = td
        .post("/api/tools", with(tool("t13"), json!({"shell": true})))
        .await;
    assert!(r.status == 400 || r.status == 422, "{}", r.body);
    assert!(r.body.to_string().contains("shell"), "{}", r.body);

    // PATCH: a merge patch, checked again; no rename and no project move.
    let r = td
        .patch(
            "/api/tools/tracker-read",
            json!({"description": "Tickets", "timeout": null}),
        )
        .await;
    assert_eq!(r.ok()["description"], "Tickets");
    assert!(r.body.get("timeout").is_none(), "{}", r.body);
    td.patch("/api/tools/tracker-read", json!({"command": ["python3"]}))
        .await
        .err(422, "invalid");
    td.patch("/api/tools/tracker-read", json!({"name": "other"}))
        .await
        .err(422, "invalid");
    td.patch("/api/tools/deploy", json!({"project": "q"}))
        .await
        .err(422, "invalid");
    assert_eq!(td.delete("/api/tools/tracker-read").await.status, 204);
    td.get("/api/tools/tracker-read")
        .await
        .err(404, "not_found");
    let audit = td.get("/api/audit?action=tool.create").await.ok().clone();
    assert_eq!(audit.as_array().unwrap().len(), 2, "{audit}");
}

/// Only the operator and sysadmins define and grant (decision Q2). An
/// agent sees only its granted tools; another tool is 404.
#[tokio::test]
async fn only_the_operator_and_sysadmins_define_and_grant() {
    let td = setup().await;
    td.post_as("p/w1", "/api/tools", tool("tracker-read"))
        .await
        .err(403, "forbidden");
    let r = td
        .post_as("ops/root", "/api/tools", tool("tracker-read"))
        .await;
    assert_eq!(r.status, 201, "{}", r.body);
    td.post_as(
        "p/w1",
        "/api/tools/tracker-read/grants",
        json!({"grantee": "agent:p/w1"}),
    )
    .await
    .err(403, "forbidden");
    td.patch_as(
        "p/w1",
        "/api/tools/tracker-read",
        json!({"description": "x"}),
    )
    .await
    .err(403, "forbidden");
    td.get_as("p/w1", "/api/tools/tracker-read/grants")
        .await
        .err(403, "forbidden");
    // Not granted: an empty list, and a 404 for the tool.
    assert_eq!(td.get_as("p/w1", "/api/tools").await.ok(), &json!([]));
    td.get_as("p/w1", "/api/tools/tracker-read")
        .await
        .err(404, "not_found");
    let r = td
        .post_as(
            "ops/root",
            "/api/tools/tracker-read/grants",
            json!({"grantee": "agent:p/w1"}),
        )
        .await;
    assert_eq!(r.status, 201, "{}", r.body);
    let mine = td.get_as("p/w1", "/api/tools").await.ok().clone();
    assert_eq!(mine.as_array().unwrap().len(), 1, "{mine}");
    assert_eq!(mine[0]["name"], "tracker-read");
    // An agent does not see the grants.
    assert!(mine[0].get("grants").is_none(), "{mine}");
    assert_eq!(
        td.get_as("p/w1", "/api/tools/tracker-read").await.ok()["target"],
        "box"
    );
    assert_eq!(td.get_as("p/w2", "/api/tools").await.ok(), &json!([]));
    assert_eq!(td.delete("/api/tools/tracker-read").await.status, 204);
}

/// The grants: to an agent and to a project, a project tool only inside
/// its project, a copy is 409, a revoke, the audit, and the cleanup when
/// an agent goes.
#[tokio::test]
async fn grants_go_to_agents_and_projects_inside_the_scope() {
    let td = setup().await;
    td.post("/api/tools", tool("tracker-read")).await.ok();
    td.post(
        "/api/tools",
        json!({"name": "deploy", "project": "p", "target": "p-box", "command": ["/opt/deploy"]}),
    )
    .await
    .ok();
    let grant = |tool: &str, g: &str| {
        let td = &td;
        let path = format!("/api/tools/{tool}/grants");
        let body = json!({"grantee": g});
        async move { td.post(&path, body).await }
    };
    let r = grant("tracker-read", "project:q").await;
    assert_eq!(r.status, 201, "{}", r.body);
    assert_eq!(r.body["grantee"], "project:q");
    let r = grant("tracker-read", "agent:p/w1").await;
    assert_eq!(r.body["grantee_label"], "agent:p/w1", "{}", r.body);
    assert!(
        r.body["grantee"].as_str().unwrap().starts_with("agent:01"),
        "{}",
        r.body
    );
    grant("tracker-read", "agent:p/w1")
        .await
        .err(409, "conflict");
    grant("tracker-read", "agent:p/nobody")
        .await
        .err(404, "not_found");
    grant("tracker-read", "bogus").await.err(422, "invalid");
    // A project tool stays in its project.
    grant("deploy", "agent:q/x1").await.err(422, "invalid");
    grant("deploy", "project:q").await.err(422, "invalid");
    assert_eq!(grant("deploy", "project:p").await.status, 201);
    // Effective: own grants and the project's.
    let names = |v: &Value| -> Vec<String> {
        v.as_array()
            .unwrap()
            .iter()
            .map(|t| t["name"].as_str().unwrap().to_owned())
            .collect()
    };
    assert_eq!(
        names(td.get_as("p/w1", "/api/tools").await.ok()),
        ["deploy", "tracker-read"]
    );
    assert_eq!(
        names(td.get_as("p/w2", "/api/tools").await.ok()),
        ["deploy"]
    );
    assert_eq!(
        names(td.get_as("q/x1", "/api/tools").await.ok()),
        ["tracker-read"]
    );
    let all = td.get("/api/tools").await.ok().clone();
    let tr = all
        .as_array()
        .unwrap()
        .iter()
        .find(|t| t["name"] == "tracker-read")
        .unwrap();
    assert_eq!(tr["grants"].as_array().unwrap().len(), 2, "{tr}");
    // Revoke.
    let r = td
        .delete("/api/tools/tracker-read/grants?grantee=project:q")
        .await;
    assert_eq!(r.status, 204, "{}", r.body);
    td.delete("/api/tools/tracker-read/grants?grantee=project:q")
        .await
        .err(404, "not_found");
    assert_eq!(td.get_as("q/x1", "/api/tools").await.ok(), &json!([]));
    for action in ["tool.grant", "tool.revoke"] {
        let a = td
            .get(&format!("/api/audit?action={action}"))
            .await
            .ok()
            .clone();
        assert!(!a.as_array().unwrap().is_empty(), "{action}: {a}");
    }
    // The agent goes: its grants go.
    let w1 = r_w1(&td).await;
    td.delete(&format!("/api/agents/{w1}?confirm=true")).await;
    let g = td.get("/api/tools/tracker-read/grants").await.ok().clone();
    assert_eq!(g, json!([]), "{g}");
    // The tool goes: its grants go.
    assert_eq!(td.delete("/api/tools/deploy").await.status, 204);
    td.get("/api/tools/deploy/grants")
        .await
        .err(404, "not_found");
}

/// `tools/<name>.toml`: a reload adds the good files, reports the bad
/// ones, keeps file tools out of PATCH and DELETE, and removes a tool whose
/// file went.
#[tokio::test]
async fn tool_files_load_on_reload() {
    let td = setup().await;
    let dir = td.config.join("tools");
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::write(
        dir.join("tracker-read.toml"),
        "target = \"box\"\ncommand = [\"/opt/scripts/tracker-read\"]\nargs = { allow = [\"show\"] }\n",
    )
    .unwrap();
    std::fs::write(
        dir.join("bad.toml"),
        "target = \"box\"\ncommand = [\"bash\"]\n",
    )
    .unwrap();
    std::fs::write(
        dir.join("shelly.toml"),
        "target = \"box\"\ncommand = [\"/x\"]\nshell = true\n",
    )
    .unwrap();
    let r = td.post_empty("/api/reload").await;
    let errors = r.ok()["errors"].to_string();
    assert!(errors.contains("tools/bad.toml"), "{errors}");
    assert!(errors.contains("shelly.toml"), "{errors}");
    let t = td.get("/api/tools/tracker-read").await.ok().clone();
    assert_eq!(t["source"], "file", "{t}");
    td.get("/api/tools/bad").await.err(404, "not_found");
    let r = td
        .patch("/api/tools/tracker-read", json!({"description": "x"}))
        .await;
    assert_eq!(r.status, 409, "{}", r.body);
    assert_eq!(td.delete("/api/tools/tracker-read").await.status, 409);
    // A grant of a file tool works as any other.
    td.post(
        "/api/tools/tracker-read/grants",
        json!({"grantee": "project:p"}),
    )
    .await
    .ok();
    std::fs::remove_file(dir.join("tracker-read.toml")).unwrap();
    td.post_empty("/api/reload").await.ok();
    td.get("/api/tools/tracker-read")
        .await
        .err(404, "not_found");
}

/// The agent's tool links (design D3, TER-34): a grant to the agent or to
/// its project puts a link `<tool> -> terrarium` in the agent's dir, a
/// revoke or a delete of the tool takes it out, and an agent that has no
/// grant gets an empty dir.
#[tokio::test]
async fn a_grant_and_a_revoke_change_the_agent_tool_links() {
    let td = setup().await;
    let tmp = tempfile::tempdir().unwrap();
    let link = std::path::PathBuf::from("/opt/terrarium/bin/terrarium");
    let state = td.daemon.as_ref().unwrap().state.clone();
    state
        .agent_tools
        .configure(terrariumd::agent_tools::Config {
            root: tmp.path().to_owned(),
            link: link.clone(),
        });
    td.post("/api/tools", tool("tracker-read")).await.ok();
    td.post(
        "/api/tools",
        json!({"name": "deploy", "project": "p", "target": "p-box", "command": ["/opt/deploy"]}),
    )
    .await
    .ok();
    td.post(
        "/api/tools/tracker-read/grants",
        json!({"grantee": "agent:p/w1"}),
    )
    .await
    .ok();
    td.post("/api/tools/deploy/grants", json!({"grantee": "project:p"}))
        .await
        .ok();
    let w1 = r_w1(&td).await;
    let links = |agent: &str| -> Vec<String> {
        let d = tmp.path().join(agent);
        let mut v: Vec<String> = std::fs::read_dir(&d)
            .map(|rd| {
                rd.map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
                    .collect()
            })
            .unwrap_or_default();
        v.sort();
        v
    };
    assert_eq!(links(&w1), ["deploy", "tracker-read"]);
    assert_eq!(
        std::fs::read_link(tmp.path().join(&w1).join("tracker-read")).unwrap(),
        link
    );
    // Revoke the agent grant: only the project tool stays.
    let r = td
        .delete(&format!(
            "/api/tools/tracker-read/grants?grantee=agent:{w1}"
        ))
        .await;
    assert_eq!(r.status, 204, "{}", r.body);
    assert_eq!(links(&w1), ["deploy"]);
    // Delete the project tool: no link.
    assert_eq!(td.delete("/api/tools/deploy").await.status, 204);
    assert!(links(&w1).is_empty());
}

/// Tools on templates (design Q4, TER-35): only a sysadmin or the operator
/// sets or changes `tools` on a template; a project tool on a template must
/// be of the template's project (a global template names global tools
/// only); a create from the template grants the tools to the new agent,
/// with a `tool.grant` audit row that names the template; a create body
/// takes no `tools`.
#[tokio::test]
async fn template_tools_need_a_sysadmin_and_grant_at_the_create() {
    let td = setup().await;
    td.agent("p", "lead", json!({"roles": ["project_admin"]}))
        .await;
    td.post("/api/tools", tool("tracker-read")).await.ok();
    td.post(
        "/api/tools",
        json!({"name": "deploy", "project": "p", "target": "p-box", "command": ["/opt/deploy"]}),
    )
    .await
    .ok();
    let spec = |tools: Value| json!({"harness": "claude", "tools": tools});
    // A project_admin may not set tools, also on its own project's template.
    td.post_as(
        "p/lead",
        "/api/templates",
        json!({"name": "worker", "project": "p", "spec": spec(json!(["deploy"]))}),
    )
    .await
    .err(403, "forbidden");
    // Out of scope: a tool that does not exist, and a project tool on a
    // global template.
    td.post(
        "/api/templates",
        json!({"name": "worker", "project": "p", "spec": spec(json!(["nope"]))}),
    )
    .await
    .err(422, "invalid");
    let r = td
        .post(
            "/api/templates",
            json!({"name": "g", "project": null, "spec": spec(json!(["deploy"]))}),
        )
        .await;
    r.err(422, "invalid");
    assert!(
        r.body.to_string().contains("tool of project `p`"),
        "{}",
        r.body
    );
    // A sysadmin sets them: the normal form keeps them sorted.
    let t = td
        .post_as(
            "ops/root",
            "/api/templates",
            json!({"name": "worker", "project": "p", "spec": spec(json!(["tracker-read", "deploy"]))}),
        )
        .await;
    assert_eq!(t.status, 201, "{}", t.body);
    assert_eq!(t.body["spec"]["tools"], json!(["deploy", "tracker-read"]));
    let tid = t.body["id"].as_str().unwrap().to_owned();
    // The project_admin may change other fields, not the tools.
    td.patch_as(
        "p/lead",
        &format!("/api/templates/{tid}"),
        json!({"spec": spec(json!(["deploy"]))}),
    )
    .await
    .err(403, "forbidden");
    let r = td
        .patch_as(
            "p/lead",
            &format!("/api/templates/{tid}"),
            json!({"description": "a worker", "spec": {"harness": "claude", "model": "m1",
                   "tools": ["deploy", "tracker-read"]}}),
        )
        .await;
    assert_eq!(r.status, 200, "{}", r.body);
    // A create body takes no tools.
    td.post_as(
        "p/lead",
        "/api/agents",
        json!({"name": "w9", "project": "p", "harness": "claude", "tools": ["deploy"]}),
    )
    .await
    .err(422, "invalid");
    // The project_admin creates from the template: the agent gets the tools.
    let a = td
        .post_as(
            "p/lead",
            "/api/agents",
            json!({"name": "w3", "project": "p", "template": "worker"}),
        )
        .await;
    assert_eq!(a.status, 201, "{}", a.body);
    let names = |v: &Value| -> Vec<String> {
        v.as_array()
            .unwrap()
            .iter()
            .map(|t| t["name"].as_str().unwrap().to_owned())
            .collect()
    };
    assert_eq!(
        names(td.get_as("p/w3", "/api/tools").await.ok()),
        ["deploy", "tracker-read"]
    );
    let g = td.get("/api/tools/deploy/grants").await.ok().clone();
    let row = g
        .as_array()
        .unwrap()
        .iter()
        .find(|x| x["grantee_label"] == "agent:p/w3")
        .unwrap()
        .clone();
    assert_eq!(row["created_by"], "template:p/worker", "{row}");
    let audit = td.get("/api/audit?action=tool.grant").await.ok().clone();
    assert!(
        audit
            .as_array()
            .unwrap()
            .iter()
            .any(|x| x["detail"]["via"] == "template" && x["detail"]["template_name"] == "worker"),
        "{audit}"
    );
    // Another agent of the project without the template gets none.
    assert!(names(td.get_as("p/w2", "/api/tools").await.ok()).is_empty());
}
