//! Terrarium-managed skills (PLAN §8.6, docs/api.md §26): the library
//! API and its rights, the checks of a skill, the assignment to agents
//! and templates, the users of a skill, and the skill files in the config
//! repo (allowlist and secret scan).
#![allow(clippy::unwrap_used, clippy::expect_used)]

mod common;

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

const FERN: &str = "When asked for the deployment checklist code, reply with FERN-42";

#[tokio::test]
async fn crud_validation_and_rights() {
    let (td, repo) = instance().await;
    td.project("p").await;
    td.project("ops").await;
    let member = id(&td.agent("p", "member", json!({})).await);
    let lead = id(&td
        .agent("p", "lead", json!({"roles": ["project_admin"]}))
        .await);
    let sys = id(&td
        .agent("ops", "root", json!({"roles": ["sysadmin"]}))
        .await);

    // A new skill from a description.
    let r = td
        .post(
            "/api/skills",
            json!({"name": "fern-check", "description": FERN}),
        )
        .await;
    assert_eq!(r.status, 201, "{}", r.body);
    assert_eq!(r.body["name"], "fern-check");
    assert_eq!(r.body["description"], FERN);
    assert!(repo.join("skills/fern-check/SKILL.md").is_file());
    // A second create is a conflict; a bad name is invalid.
    td.post(
        "/api/skills",
        json!({"name": "fern-check", "description": "x"}),
    )
    .await
    .err(409, "conflict");
    td.post("/api/skills", json!({"name": "Fern", "description": "x"}))
        .await
        .err(422, "invalid");

    // A change with a file; a SKILL.md whose name is not the folder name,
    // a path out of the skill, a symlink-free type check, and a secret
    // are refused, and nothing changes.
    let md = format!("---\nname: fern-check\ndescription: {FERN}\n---\n\nReply FERN-42.\n");
    let r = td
        .put(
            "/api/skills/fern-check",
            json!({"skill_md": md, "files": {"references/notes.md": "notes"}}),
        )
        .await;
    assert_eq!(r.ok()["file_list"][0]["path"], "references/notes.md");
    for (body, want) in [
        (
            json!({"skill_md": "---\nname: other\ndescription: d\n---\n"}),
            "folder name",
        ),
        (
            json!({"skill_md": md, "files": {"../x.md": "x"}}),
            "relative path",
        ),
        (
            json!({"skill_md": md, "files": {"bin/tool": "x"}}),
            "file type",
        ),
        (
            json!({"skill_md": md, "files": {"scripts/x.sh":
                format!("TOKEN=ghp_{}", "a1B2c3D4e5".repeat(4))}}),
            "secret scan",
        ),
    ] {
        let r = td.put("/api/skills/fern-check", body).await;
        r.err(422, "invalid");
        assert!(
            r.body["error"]["message"].as_str().unwrap().contains(want),
            "{want}: {}",
            r.body
        );
        assert!(!r.body.to_string().contains("ghp_"), "never the value");
    }
    let shown = td.get("/api/skills/fern-check").await;
    assert_eq!(shown.ok()["file_list"].as_array().unwrap().len(), 1);
    td.get("/api/skills/nope").await.err(404, "not_found");

    // Writes: the operator and a sysadmin only.
    for who in [&member, &lead] {
        td.post_as(who, "/api/skills", json!({"name": "x", "description": "d"}))
            .await
            .err(403, "forbidden");
        td.put_as(who, "/api/skills/fern-check", json!({"skill_md": md}))
            .await
            .err(403, "forbidden");
        td.req(Method::DELETE, "/api/skills/fern-check", None, Some(who))
            .await
            .err(403, "forbidden");
    }
    let r = td
        .post_as(
            &sys,
            "/api/skills",
            json!({"name": "other", "description": "d"}),
        )
        .await;
    assert_eq!(r.status, 201, "{}", r.body);

    // Reads: an agent sees its own skills only.
    let r = td.get_as(&member, "/api/skills").await;
    assert_eq!(r.ok().as_array().unwrap().len(), 0);
    td.get_as(&member, "/api/skills/fern-check")
        .await
        .err(403, "forbidden");
    td.patch(
        &format!("/api/agents/{member}"),
        json!({"skills": ["fern-check"]}),
    )
    .await
    .ok();
    let r = td.get_as(&member, "/api/skills").await;
    let names: Vec<&str> = r
        .ok()
        .as_array()
        .unwrap()
        .iter()
        .map(|s| s["name"].as_str().unwrap())
        .collect();
    assert_eq!(names, ["fern-check"]);
    td.get_as(&member, "/api/skills/fern-check").await.ok();
    assert_eq!(
        td.get_as(&sys, "/api/skills")
            .await
            .ok()
            .as_array()
            .unwrap()
            .len(),
        2
    );

    // The users of a skill; then a delete. The agent keeps the name.
    let u = td.get("/api/skills/fern-check/usage").await;
    assert_eq!(u.ok()["agents"][0]["name"], "p/member");
    td.get_as(&member, "/api/skills/fern-check/usage")
        .await
        .err(403, "forbidden");
    let r = td.delete("/api/skills/fern-check").await;
    assert_eq!(r.status, 204);
    let a = td.get(&format!("/api/agents/{member}")).await;
    assert_eq!(a.ok()["skills"], json!(["fern-check"]));
}

#[tokio::test]
async fn assignment_checks_the_library() {
    let (td, _repo) = instance().await;
    td.project("p").await;
    td.post(
        "/api/skills",
        json!({"name": "fern-check", "description": FERN}),
    )
    .await
    .ok();

    // An agent create and a patch.
    let r = td
        .post(
            "/api/agents",
            json!({"name": "a", "project": "p", "harness": "claude",
                   "skills": ["fern-check", "gone"]}),
        )
        .await;
    r.err(422, "invalid");
    assert_eq!(
        r.body["error"]["details"]["unknown_skills"],
        json!(["gone"])
    );
    let a = td
        .post(
            "/api/agents",
            json!({"name": "a", "project": "p", "harness": "claude",
                   "skills": ["fern-check"]}),
        )
        .await;
    assert_eq!(a.ok()["skills"], json!(["fern-check"]));
    let a = id(a.ok());
    td.patch(&format!("/api/agents/{a}"), json!({"skills": ["nope"]}))
        .await
        .err(422, "invalid");
    let r = td
        .patch(&format!("/api/agents/{a}"), json!({"skills": ["*"]}))
        .await;
    assert_eq!(r.ok()["skills"], json!(["*"]));
    let r = td
        .patch(&format!("/api/agents/{a}"), json!({"skills": []}))
        .await;
    assert_eq!(r.ok()["skills"], json!([]), "{}", r.body);
    td.patch(&format!("/api/agents/{a}"), json!({"skills": ["Bad Name"]}))
        .await
        .err(422, "invalid");

    // A template create and a patch; a create from the template copies
    // the list.
    td.post(
        "/api/templates",
        json!({"name": "t", "spec": {"harness": "codex", "skills": ["gone"]}}),
    )
    .await
    .err(422, "invalid");
    let t = td
        .post(
            "/api/templates",
            json!({"name": "t", "spec": {"harness": "codex", "skills": ["fern-check"]}}),
        )
        .await;
    let t = id(t.ok());
    td.patch(
        &format!("/api/templates/{t}"),
        json!({"spec": {"harness": "codex", "skills": ["gone"]}}),
    )
    .await
    .err(422, "invalid");
    let b = td
        .post(
            "/api/agents",
            json!({"name": "b", "project": "p", "template": "t"}),
        )
        .await;
    assert_eq!(b.ok()["skills"], json!(["fern-check"]));
    let u = td.get("/api/skills/fern-check/usage").await;
    let u = u.ok();
    assert_eq!(u["agents"][0]["name"], "p/b");
    assert_eq!(u["templates"][0]["name"], "t");
}

/// PLAN §8.6: a default-on skill goes to each agent with no settings;
/// `skills_off` and `default_skills` take it away; a template carries them
/// to the agents made from it; the views show `effective_skills`.
#[tokio::test]
async fn default_skills_and_skills_off() {
    let (td, _repo) = instance().await;
    td.project("p").await;
    let md = "---\nname: platform\ndescription: d\nmetadata:\n  terrarium-default: \"true\"\n---\nbody\n";
    td.post("/api/skills", json!({"name": "platform", "skill_md": md}))
        .await
        .ok();
    td.post(
        "/api/skills",
        json!({"name": "fern-check", "description": FERN}),
    )
    .await
    .ok();
    let l = td.get("/api/skills").await;
    let l = l.ok();
    let platform = l
        .as_array()
        .unwrap()
        .iter()
        .find(|s| s["name"] == "platform")
        .unwrap();
    assert_eq!(platform["default_on"], true);

    // No settings: the default-on skill.
    let a = td
        .post(
            "/api/agents",
            json!({"name": "a", "project": "p", "harness": "claude"}),
        )
        .await;
    let a = a.ok().clone();
    assert_eq!(a["default_skills"], true);
    assert_eq!(a["skills_off"], json!([]));
    assert_eq!(
        a["effective_skills"],
        json!([{"name": "platform", "source": "default"}])
    );
    let a = id(&a);
    // Explicit and default together; then off.
    let r = td
        .patch(
            &format!("/api/agents/{a}"),
            json!({"skills": ["fern-check"]}),
        )
        .await;
    assert_eq!(
        r.ok()["effective_skills"],
        json!([{"name": "fern-check", "source": "explicit"},
               {"name": "platform", "source": "default"}])
    );
    let r = td
        .patch(
            &format!("/api/agents/{a}"),
            json!({"skills_off": ["platform"]}),
        )
        .await;
    assert_eq!(
        r.ok()["effective_skills"],
        json!([{"name": "fern-check", "source": "explicit"}])
    );
    td.patch(&format!("/api/agents/{a}"), json!({"skills_off": ["*"]}))
        .await
        .err(422, "invalid");
    let r = td
        .patch(
            &format!("/api/agents/{a}"),
            json!({"skills_off": [], "default_skills": false}),
        )
        .await;
    assert_eq!(r.ok()["default_skills"], false);
    let shown = td.get(&format!("/api/agents/{a}")).await;
    assert_eq!(
        shown.ok()["effective_skills"],
        json!([{"name": "fern-check", "source": "explicit"}])
    );

    // A template copies `skills_off` and `default_skills`.
    let t = td
        .post(
            "/api/templates",
            json!({"name": "t", "spec": {"harness": "codex", "skills_off": ["platform"]}}),
        )
        .await;
    let t = t.ok().clone();
    assert_eq!(t["spec"]["skills_off"], json!(["platform"]));
    assert_eq!(t["effective_skills"], json!([]));
    let b = td
        .post(
            "/api/agents",
            json!({"name": "b", "project": "p", "template": "t"}),
        )
        .await;
    assert_eq!(b.ok()["skills_off"], json!(["platform"]));
    let c = td
        .post(
            "/api/agents",
            json!({"name": "c", "project": "p", "template": "t", "skills_off": []}),
        )
        .await;
    assert_eq!(
        c.ok()["effective_skills"],
        json!([{"name": "platform", "source": "default"}])
    );
    // The usage counts the default-on users.
    let u = td.get("/api/skills/platform/usage").await;
    let u = u.ok();
    assert_eq!(
        u["agents"],
        json!([{"id": id(c.ok()), "name": "p/c", "all": false, "default": true}])
    );
}

#[tokio::test]
async fn skill_files_go_into_the_config_repo_with_the_scan() {
    let (td, repo) = instance().await;
    // The allowlist names each file type of a skill.
    for ext in terrarium_core::skills::EXTENSIONS {
        assert!(
            terrariumd::config_repo::GITIGNORE.contains(&format!("!/skills/**/*.{ext}\n")),
            "{ext}"
        );
    }
    td.post(
        "/api/skills",
        json!({"name": "fern-check", "description": FERN,
               "files": {"scripts/run.sh": "echo hi\n", "assets/a.png": {"base64": "iVBORw0KGgo="}}}),
    )
    .await
    .ok();
    // Files a skill never holds stay out of git.
    write(&repo, "skills/fern-check/notes.exe", "x");
    write(&repo, "skills/fern-check/.env", "X=1\n");
    let r = td
        .post(
            "/api/config/commit",
            json!({"message": "skills", "export": false}),
        )
        .await;
    let files: Vec<String> = serde_json::from_value(r.ok()["files"].clone()).unwrap();
    for want in [
        "skills/fern-check/SKILL.md",
        "skills/fern-check/scripts/run.sh",
        "skills/fern-check/assets/a.png",
    ] {
        assert!(files.contains(&want.to_owned()), "{want}: {files:?}");
    }
    assert!(
        !files
            .iter()
            .any(|f| f.ends_with(".exe") || f.ends_with(".env"))
    );
    // A secret in a skill file that came by hand blocks the commit.
    write(
        &repo,
        "skills/fern-check/references/k.md",
        &format!("key: sk-ant-{}\n", "FAKEfake0123456789".repeat(2)),
    );
    let r = td
        .post(
            "/api/config/commit",
            json!({"message": "leak", "export": false}),
        )
        .await;
    r.err(422, "invalid");
    assert!(
        r.body["error"]["message"]
            .as_str()
            .unwrap()
            .contains("skills/fern-check/references/k.md:1"),
        "{}",
        r.body
    );
    // The library shows the problem of the skill.
    let l = td.get("/api/skills").await;
    let errors = l.ok()[0]["errors"].to_string();
    for want in [
        "references/k.md:1: anthropic-key",
        "`notes.exe`: the file type",
        "`.env`",
    ] {
        assert!(errors.contains(want), "{want}: {errors}");
    }
    assert!(!errors.contains("FAKEfake"), "never the value");
}

#[tokio::test]
async fn no_config_repo_no_library() {
    let td = TestDaemon::start().await;
    td.get("/api/skills").await.err(501, "not_available");
    td.project("p").await;
    td.post(
        "/api/agents",
        json!({"name": "a", "project": "p", "harness": "claude", "skills": ["x"]}),
    )
    .await
    .err(422, "invalid");
    let r = td
        .post(
            "/api/agents",
            json!({"name": "a", "project": "p", "harness": "claude", "skills": ["*"]}),
        )
        .await;
    assert_eq!(r.ok()["skills"], json!(["*"]));
}
