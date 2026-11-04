//! Agent templates as WCL files in the config repo (PLAN §5b, docs/api.md
//! §21.5): the writes of the API, the load at the start and after an edit
//! by hand, the move of the templates from before the files, the secret
//! scan, the packaged defaults, and a sync of two instances.
#![allow(clippy::unwrap_used, clippy::expect_used)]

mod common;

use std::path::{Path, PathBuf};
use std::process::Command;

use common::{TestDaemon, write};
use serde_json::{Value, json};
use terrariumd::config_repo::ConfigRepo;

/// A daemon whose config repo is configured as `main` does it: the
/// template files load.
async fn instance() -> (TestDaemon, PathBuf) {
    let td = TestDaemon::start().await;
    let repo = td.dir.path().join("config-terrarium");
    configure(&td, &repo).await;
    (td, repo)
}

async fn configure(td: &TestDaemon, repo: &Path) {
    let state = td.daemon.as_ref().unwrap().state.clone();
    // `main` makes the repo a git repo before it configures it.
    let r = ConfigRepo::new(repo.to_path_buf(), state.state_dir.clone());
    r.ensure().unwrap();
    terrariumd::template_files::configure(&state, r).await;
}

fn read(repo: &Path, rel: &str) -> String {
    std::fs::read_to_string(repo.join(rel)).unwrap_or_else(|e| panic!("{rel}: {e}"))
}

const PROMPT: &str = "You write the docs.\n\n  Keep the lines short.\n";

#[tokio::test]
async fn api_writes_edit_and_delete_the_files() {
    let (td, repo) = instance().await;
    td.project("p").await;
    let t = td
        .post(
            "/api/templates",
            json!({"name": "docs", "description": "Docs writer", "project": null,
                   "spec": {"harness": "pi", "model": "glm-5", "system_prompt": PROMPT,
                            "policy": {"mail_to": ["lead"], "cross_project": ["ops/*"]}}}),
        )
        .await;
    let t = t.ok().clone();
    assert_eq!(t["source"], "config");
    assert_eq!(t["file"], "templates/docs.wcl");
    let text = read(&repo, "templates/docs.wcl");
    assert!(
        text.starts_with("// Terrarium agent template: docs (global).\n"),
        "{text}"
    );
    assert!(text.contains("description = \"Docs writer\"\n"), "{text}");
    assert!(
        text.contains("system_prompt = <<'PROMPT'\n  You write the docs.\n\n    Keep the lines short.\n  PROMPT\n"),
        "{text}"
    );
    // The file reads back as the template.
    let back = terrarium_core::template_file::parse(&text, "docs.wcl").unwrap();
    assert_eq!(back.spec["system_prompt"], PROMPT);
    assert_eq!(back.spec["policy"]["cross_project"], json!(["ops/*"]));
    // `GET .../wcl` is the file.
    let w = td.get("/api/templates/global:docs/wcl").await;
    assert_eq!(w.ok()["path"], "templates/docs.wcl");
    assert_eq!(w.body["text"], text.as_str());
    assert_eq!(w.body["on_disk"], true);

    // An edit writes the file again; the revision goes up.
    let u = td
        .patch(
            "/api/templates/global:docs",
            json!({"spec": {"harness": "pi", "model": "glm-6"}}),
        )
        .await;
    assert_eq!(u.ok()["revision"], 2);
    let text2 = read(&repo, "templates/docs.wcl");
    assert!(text2.contains("model       = \"glm-6\"\n"), "{text2}");
    assert!(!text2.contains("system_prompt"), "{text2}");
    // The same values: no change, no new revision.
    let same = td
        .patch(
            "/api/templates/global:docs",
            json!({"spec": {"harness": "pi", "model": "glm-6"}}),
        )
        .await;
    assert_eq!(same.ok()["revision"], 2);
    assert_eq!(read(&repo, "templates/docs.wcl"), text2);

    // A project template: templates/<project>/<name>.wcl. A rename moves
    // the file.
    td.post(
        "/api/templates",
        json!({"name": "w", "project": "p", "spec": {"harness": "claude"}}),
    )
    .await
    .ok();
    assert!(repo.join("templates/p/w.wcl").is_file());
    td.patch("/api/templates/p%2Fw", json!({"name": "worker"}))
        .await
        .ok();
    assert!(!repo.join("templates/p/w.wcl").exists());
    assert!(repo.join("templates/p/worker.wcl").is_file());

    // The files stay uncommitted until a commit, which scans them.
    let s = td.get("/api/config/status").await;
    let changes: Vec<String> = s.ok()["changes"]
        .as_array()
        .unwrap()
        .iter()
        .map(|c| c["path"].as_str().unwrap().to_owned())
        .collect();
    assert!(
        changes.contains(&"templates/docs.wcl".to_owned()),
        "{changes:?}"
    );
    let c = td
        .post("/api/config/commit", json!({"message": "templates"}))
        .await;
    let files: Vec<String> = serde_json::from_value(c.ok()["files"].clone()).unwrap();
    assert!(
        files.contains(&"templates/docs.wcl".to_owned()),
        "{files:?}"
    );
    assert!(
        files.contains(&"templates/p/worker.wcl".to_owned()),
        "{files:?}"
    );
    // The export holds no template now: the files are the templates.
    for f in files.iter().filter(|f| f.starts_with("export/")) {
        let text = read(&repo, f);
        assert!(!text.contains("templates"), "{f}: {text}");
    }

    // A delete removes the file; a project delete removes its templates.
    td.delete("/api/templates/global:docs").await.ok();
    assert!(!repo.join("templates/docs.wcl").exists());
    td.delete("/api/projects/p?confirm=true").await.ok();
    assert!(!repo.join("templates/p/worker.wcl").exists());
    assert_eq!(td.get("/api/templates").await.ok(), &json!([]));
}

#[tokio::test]
async fn a_file_edit_by_hand_loads() {
    let (td, repo) = instance().await;
    // A new file: a new template, `source = config`.
    write(
        &repo,
        "templates/hand.wcl",
        "// By hand.\ndescription = \"Hand made\"\nharness = \"codex\"\n",
    );
    let t = td.get("/api/templates/global:hand").await;
    assert_eq!(t.ok()["source"], "config");
    assert_eq!(t.body["revision"], 1);
    assert_eq!(t.body["description"], "Hand made");
    assert_eq!(t.body["spec"]["harness"], "codex");
    let id = t.body["id"].as_str().unwrap().to_owned();
    // `GET .../wcl` shows the file as it is, with its comment.
    let w = td.get("/api/templates/global:hand/wcl").await;
    assert!(
        w.ok()["text"]
            .as_str()
            .unwrap()
            .starts_with("// By hand.\n")
    );

    // A comment only: the file changed, so the revision changes.
    write(
        &repo,
        "templates/hand.wcl",
        "// By hand, v2.\ndescription = \"Hand made\"\nharness = \"codex\"\n",
    );
    let t = td.get("/api/templates/global:hand").await;
    assert_eq!(
        (t.ok()["id"].as_str(), &t.body["revision"]),
        (Some(id.as_str()), &json!(2))
    );
    // A value: the store has it.
    write(
        &repo,
        "templates/hand.wcl",
        "description = \"Hand made\"\nharness = \"codex\"\nmodel = \"gpt-5.5\"\n",
    );
    let t = td.get("/api/templates/global:hand").await;
    assert_eq!(t.ok()["revision"], 3);
    assert_eq!(t.body["spec"]["model"], "gpt-5.5");

    // A file that does not read keeps the last good template; the reload
    // names the file and the line.
    write(
        &repo,
        "templates/hand.wcl",
        "harness = \"codex\"\nmodle = \"x\"\n",
    );
    let r = td.post_empty("/api/reload").await;
    let errors: Vec<String> = serde_json::from_value(r.ok()["errors"].clone()).unwrap();
    assert!(
        errors
            .iter()
            .any(|e| e.starts_with("templates/hand.wcl:2:1: top-level field 'modle'")),
        "{errors:?}"
    );
    let t = td.get("/api/templates/global:hand").await;
    assert_eq!(t.ok()["spec"]["model"], "gpt-5.5");
    assert_eq!(t.body["revision"], 3);
    // A file of a project that does not exist is an error too.
    write(&repo, "templates/nope/w.wcl", "harness = \"pi\"\n");
    let r = td.post_empty("/api/reload").await;
    assert!(
        r.ok()["errors"]
            .as_array()
            .unwrap()
            .contains(&json!("templates/nope/w.wcl: no project `nope`")),
        "{}",
        r.body
    );

    // An agent create reads the file as it is now.
    td.project("p").await;
    write(
        &repo,
        "templates/p/builder.wcl",
        "harness = \"claude\"\nprompt = \"worker\"\n",
    );
    let a = td
        .post(
            "/api/agents",
            json!({"name": "b1", "project": "p", "template": "builder"}),
        )
        .await;
    assert_eq!(a.ok()["harness"], "claude");
    assert_eq!(a.body["template"]["name"], "builder");
    assert_eq!(a.body["template"]["revision"], 1);

    // A removed file removes the template; the agent keeps its copy.
    std::fs::remove_file(repo.join("templates/p/builder.wcl")).unwrap();
    td.get("/api/templates/p%2Fbuilder")
        .await
        .err(404, "not_found");
    assert_eq!(td.get("/api/agents/p%2Fb1").await.ok()["harness"], "claude");
}

#[tokio::test]
async fn templates_from_before_the_files_move_to_files_once() {
    // No config repo yet: a template is a row with no file.
    let td = TestDaemon::start().await;
    td.project("p").await;
    let t = td
        .post(
            "/api/templates",
            json!({"name": "old", "project": null, "description": "From the database",
                   "spec": {"harness": "codex", "persona": "worker"}}),
        )
        .await;
    assert_eq!(t.ok()["file"], Value::Null);
    td.post(
        "/api/templates",
        json!({"name": "pw", "project": "p", "spec": {"harness": "claude"}}),
    )
    .await
    .ok();
    // A row of the old kind: `source = api`.
    let state = td.daemon.as_ref().unwrap().state.clone();
    state
        .db
        .write("system", |t| {
            t.execute("UPDATE agent_templates SET source = 'api'", [])?;
            Ok(())
        })
        .await
        .unwrap();
    // A packaged default at the same path, written before the load (the
    // seed of the same start): the row wins.
    let repo = td.dir.path().join("config-terrarium");
    write(&repo, "templates/old.wcl", "harness = \"pi\"\n");
    configure(&td, &repo).await;
    let text = read(&repo, "templates/old.wcl");
    assert!(text.contains("harness     = \"codex\""), "{text}");
    assert!(
        text.contains("description = \"From the database\""),
        "{text}"
    );
    assert!(repo.join("templates/p/pw.wcl").is_file());
    let t = td.get("/api/templates/global:old").await;
    assert_eq!(t.ok()["source"], "config");
    assert_eq!(t.body["file"], "templates/old.wcl");
    assert_eq!(t.body["revision"], 1);
    // Once: the next start reads the files, and writes nothing.
    std::fs::write(repo.join("templates/old.wcl"), "harness = \"pi\"\n").unwrap();
    let mut td = td;
    td.boot(None, None).await;
    configure(&td, &repo).await;
    let t = td.get("/api/templates/global:old").await;
    assert_eq!(t.ok()["spec"]["harness"], "pi");
    assert_eq!(read(&repo, "templates/old.wcl"), "harness = \"pi\"\n");
}

#[tokio::test]
async fn the_secret_scan_refuses_a_secret_in_a_template() {
    let (td, repo) = instance().await;
    let secret = format!("sk-ant-api03-{}", "FAKEfake0123456789".repeat(2));
    let r = td
        .post(
            "/api/templates",
            json!({"name": "leaky", "project": null,
                   "spec": {"harness": "claude", "system_prompt": format!("Use {secret}")}}),
        )
        .await;
    r.err(422, "invalid");
    let e = &r.body["error"];
    let message = e["message"].as_str().unwrap();
    assert!(message.contains("templates/leaky.wcl:"), "{message}");
    assert!(!message.contains(&secret), "{message}");
    assert_eq!(e["details"]["findings"][0]["rule"], "anthropic-key");
    assert!(!repo.join("templates/leaky.wcl").exists());
    td.get("/api/templates/global:leaky")
        .await
        .err(404, "not_found");
    // A file by hand with a secret loads, but the commit refuses it.
    write(
        &repo,
        "templates/hand.wcl",
        &format!("harness = \"claude\"\nsystem_prompt = \"{secret}\"\n"),
    );
    let c = td.post("/api/config/commit", json!({"message": "x"})).await;
    c.err(422, "invalid");
    let e = &c.body["error"];
    assert!(
        e["message"]
            .as_str()
            .unwrap()
            .contains("templates/hand.wcl:2"),
        "{e}"
    );
}

#[tokio::test]
async fn the_packaged_default_templates_load() {
    let defaults = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../defaults");
    let d = terrariumd::defaults::Defaults::load(&defaults).unwrap();
    let names: Vec<String> = d
        .manifest
        .files
        .keys()
        .filter_map(|rel| terrarium_core::template_file::key_of_rel(rel))
        .map(|(p, n)| {
            assert_eq!(p, None, "a packaged template is global");
            n
        })
        .collect();
    assert_eq!(
        names,
        [
            "claude-haiku-helper",
            "claude-lead",
            "claude-worker",
            "codex-reviewer",
            "codex-worker",
            "copilot-worker",
            "glm-docs-writer",
            "kimi-worker"
        ]
    );
    // Each one is in the form that the daemon writes, so a template with
    // the same values writes the same file.
    for n in &names {
        let rel = format!("templates/{n}.wcl");
        let text = d.text(&rel).unwrap();
        let p = terrarium_core::template_file::parse(&text, &rel).unwrap();
        let spec = terrarium_core::agent_template::normalize(&p.spec).unwrap();
        assert_eq!(spec, p.spec, "{rel}: not in the normal form");
        let again = terrarium_core::template_file::render(n, None, p.description.as_deref(), &spec)
            .unwrap();
        assert_eq!(again, text, "{rel}");
    }

    // The seed writes them; they load as global templates that name their
    // subscription, which need not exist.
    let (td, repo) = {
        let td = TestDaemon::start().await;
        let repo = td.dir.path().join("config-terrarium");
        terrariumd::defaults::seed(&d, &repo).unwrap();
        configure(&td, &repo).await;
        (td, repo)
    };
    let list = td.get("/api/templates?scope=global").await;
    let got: Vec<&str> = list
        .ok()
        .as_array()
        .unwrap()
        .iter()
        .map(|t| t["name"].as_str().unwrap())
        .collect();
    assert_eq!(got, names);
    let lead = td.get("/api/templates/global:claude-lead").await;
    // No subscription: the default account of the harness applies (#14).
    assert!(
        lead.ok()["spec"].get("subscription").is_none(),
        "{}",
        lead.body
    );
    assert_eq!(lead.body["spec"]["roles"], json!(["project_admin"]));
    // A default that the operator deletes stays deleted at the next seed.
    td.delete("/api/templates/global:kimi-worker").await.ok();
    let report = terrariumd::defaults::seed(&d, &repo).unwrap();
    assert!(!repo.join("templates/kimi-worker.wcl").exists());
    assert!(
        report.iter().any(|r| r.path == "templates/kimi-worker.wcl"
            && r.action == terrariumd::defaults::SeedAction::Removed),
        "{report:?}"
    );
}

fn bare(dir: &Path) -> PathBuf {
    let path = dir.join("remote.git");
    let out = Command::new("git")
        .args(["init", "-q", "--bare", "-b", "main"])
        .arg(&path)
        .output()
        .unwrap();
    assert!(out.status.success());
    path
}

#[tokio::test]
async fn a_sync_brings_the_templates_of_another_instance() {
    let tmp = tempfile::tempdir().unwrap();
    let url = bare(tmp.path()).display().to_string();
    let (a, _) = instance().await;
    let (b, repo_b) = instance().await;
    for td in [&a, &b] {
        td.put("/api/config/remote", json!({"url": url})).await.ok();
    }
    a.post(
        "/api/templates",
        json!({"name": "shared", "project": null, "spec": {"harness": "codex"}}),
    )
    .await
    .ok();
    let r = a
        .post("/api/config/sync", json!({"message": "a: template"}))
        .await;
    assert_eq!(r.ok()["pushed"], true, "{}", r.body);
    // B: the pull loads the file into the store at once.
    let r = b.post("/api/config/pull", json!({})).await;
    assert!(
        r.ok()["changed"]
            .as_array()
            .unwrap()
            .contains(&json!("templates/shared.wcl")),
        "{}",
        r.body
    );
    assert!(repo_b.join("templates/shared.wcl").is_file());
    let state = b.daemon.as_ref().unwrap().state.clone();
    let t = state
        .db
        .read(|c| terrariumd::store::templates::find_in(c, None, "shared"))
        .await
        .unwrap();
    assert_eq!(t.unwrap().spec["harness"], "codex");
}
