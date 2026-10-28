//! Repos: the `/api/credentials` alias of the secret store, checkouts into
//! workspaces, the credential
//! helper endpoint, and the auto-checkout on agent create. No network: the
//! remotes are local bare repos.
#![allow(clippy::unwrap_used, clippy::expect_used)]

mod common;

use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::process::Command;

use common::{TestDaemon, write};
use serde_json::json;

fn sh(dir: &Path, script: &str) {
    let st = Command::new("sh")
        .arg("-c")
        .arg(script)
        .current_dir(dir)
        .env("GIT_AUTHOR_NAME", "t")
        .env("GIT_AUTHOR_EMAIL", "t@example.com")
        .env("GIT_COMMITTER_NAME", "t")
        .env("GIT_COMMITTER_EMAIL", "t@example.com")
        .status()
        .unwrap();
    assert!(st.success(), "{script}");
}

/// A bare repo with one commit on `main`, beside the state dir.
fn bare_repo(dir: &Path) -> PathBuf {
    std::fs::create_dir_all(dir).unwrap();
    sh(
        dir,
        "git init -q --bare -b main origin.git && git init -q -b main src && cd src && \
         echo one > a.txt && git add a.txt && git commit -q -m one && \
         git push -q ../origin.git main",
    );
    dir.join("origin.git")
}

fn head_of(dir: &Path) -> String {
    let o = Command::new("git")
        .arg("-C")
        .arg(dir)
        .args(["rev-parse", "HEAD"])
        .output()
        .unwrap();
    String::from_utf8(o.stdout).unwrap().trim().to_owned()
}

/// A daemon with a file project `terra` whose repo `proj` is a local bare
/// repo and is in `checkout.on_agent_create`.
async fn daemon_with_repo() -> (TestDaemon, PathBuf, tempfile::TempDir) {
    let remotes = tempfile::tempdir().unwrap();
    let origin = bare_repo(remotes.path());
    let url = origin.to_string_lossy().into_owned();
    let td = TestDaemon::with_config(|c| {
        write(
            c,
            "projects/terra.toml",
            &format!(
                r#"
description = "test"

[[repos]]
name = "proj"
url = "{url}"
default_branch = "main"

[[repos]]
name = "hub"
url = "https://git.example.com/team/hub.git"
credential = "hub-token"

[checkout]
on_agent_create = ["proj", "missing"]
"#
            ),
        );
    })
    .await;
    (td, origin, remotes)
}

#[tokio::test]
async fn credentials_are_write_only() {
    let td = TestDaemon::start().await;
    let r = td
        .post(
            "/api/credentials",
            json!({"name": "github-deploy", "kind": "token", "value": "ghp_s3cretT0kenValue"}),
        )
        .await;
    let info = r.ok().clone();
    assert_eq!(r.status.as_u16(), 201);
    assert_eq!(info["name"], "github-deploy");
    assert_eq!(info["kind"], "token");
    assert!(!info.to_string().contains("ghp_s3cret"));

    let list = td.get("/api/credentials").await;
    assert_eq!(list.ok().as_array().unwrap().len(), 1);
    assert!(!list.body.to_string().contains("ghp_s3cret"));
    assert_eq!(list.body[0]["kind"], "token");

    // The alias stores a global secret (PLAN §16b): sealed in the
    // database, no file; the key file is 0600.
    let mode = |p: &Path| std::fs::metadata(p).unwrap().permissions().mode() & 0o777;
    assert!(!td.state.join("credentials/github-deploy").exists());
    assert_eq!(mode(&td.state.join("secrets.key")), 0o600);
    let s = td.get("/api/secrets/global/github-deploy").await;
    assert_eq!(s.ok()["kind"], "token");
    assert_eq!(s.body["fields"]["username"], "x-access-token");
    assert!(!s.body.to_string().contains("ghp_s3cret"));

    // A member agent cannot list or write credentials.
    td.project("p").await;
    td.agent("p", "w", json!({})).await;
    td.get_as("p/w", "/api/credentials")
        .await
        .err(403, "forbidden");
    td.post_as(
        "p/w",
        "/api/credentials",
        json!({"name": "x", "kind": "token", "value": "v"}),
    )
    .await
    .err(403, "forbidden");
    // No route reads a secret back.
    td.get("/api/credentials/github-deploy")
        .await
        .err(405, "method_not_allowed");
    // Bad values.
    td.post(
        "/api/credentials",
        json!({"name": "k", "kind": "ssh_key", "value": "nope"}),
    )
    .await
    .err(422, "invalid");

    // The audit row has the kind, not the secret.
    let audit = td.get("/api/audit?action=secret.put").await;
    assert_eq!(audit.ok().as_array().unwrap().len(), 1);
    assert!(!audit.body.to_string().contains("ghp_s3cret"));

    let r = td
        .req(
            hyper::Method::DELETE,
            "/api/credentials/github-deploy",
            None,
            None,
        )
        .await;
    assert_eq!(r.status.as_u16(), 204);
    td.delete("/api/credentials/github-deploy")
        .await
        .err(404, "not_found");
    assert_eq!(td.get("/api/credentials").await.body, json!([]));
}

#[tokio::test]
async fn agent_create_clones_and_checkout_updates() {
    let (td, origin, remotes) = daemon_with_repo().await;
    let created = td
        .post(
            "/api/agents",
            json!({"name": "worker", "project": "terra", "harness": "claude"}),
        )
        .await;
    let a = created.ok().clone();
    assert_eq!(created.status.as_u16(), 201);
    let checkout = a["checkout"].as_array().unwrap();
    assert_eq!(checkout.len(), 2, "{a}");
    assert_eq!(checkout[0]["repo"], "proj");
    assert_eq!(checkout[0]["action"], "cloned", "{a}");
    assert_eq!(checkout[0]["branch"], "main");
    assert_eq!(checkout[0]["path"], "proj");
    // A name that is not a repo fails without blocking the create.
    assert_eq!(checkout[1]["repo"], "missing");
    assert!(checkout[1]["error"].is_string());

    let ws = PathBuf::from(a["workspace"].as_str().unwrap());
    let dest = ws.join("proj");
    assert!(dest.join("a.txt").is_file());
    assert_eq!(checkout[0]["head"], head_of(&origin));
    let config = std::fs::read_to_string(dest.join(".git/config")).unwrap();
    assert!(
        config.contains("helper = !terrarium repo credential"),
        "{config}"
    );
    assert!(config.contains("useHttpPath = true"), "{config}");

    // Events: a done event and an error event.
    let events = td.get("/api/events?kind=repo.checkout").await;
    let all = events.ok().as_array().unwrap().clone();
    assert!(all.iter().all(|e| e["kind"] == "repo.checkout"), "{all:#?}");
    assert!(all.iter().any(|e| e["data"]["phase"] == "done"), "{all:#?}");
    assert!(all.iter().any(|e| e["data"]["phase"] == "error"));
    // An audit row.
    let audit = td.get("/api/audit?action=repo.checkout").await;
    assert_eq!(audit.ok().as_array().unwrap().len(), 1);

    // A new upstream commit: the agent updates its own workspace.
    sh(
        &remotes.path().join("src"),
        "echo two > b.txt && git add b.txt && git commit -q -m two && git push -q ../origin.git main",
    );
    let repos = td.get("/api/projects/terra/repos").await;
    let id = repos.ok().as_array().unwrap()[1]["id"]
        .as_str()
        .unwrap()
        .to_owned();
    assert_eq!(repos.body[1]["name"], "proj");
    td.get(&format!("/api/repos/{id}")).await.ok();
    let r = td
        .post_as(
            "terra/worker",
            &format!("/api/repos/{id}/checkout"),
            json!({}),
        )
        .await;
    assert_eq!(r.ok()["action"], "updated", "{}", r.body);
    assert_eq!(r.body["head"], head_of(&origin));
    assert!(dest.join("b.txt").is_file());
    // A name works for an agent of the project.
    let r = td
        .post_as("terra/worker", "/api/repos/proj/checkout", json!({}))
        .await;
    assert_eq!(r.ok()["action"], "up_to_date");

    // A checkout on a target runs on the target (tests/hosts.rs); an
    // unknown target is a 404.
    td.post_as(
        "terra/worker",
        &format!("/api/repos/{id}/checkout"),
        json!({"target": "buildbox"}),
    )
    .await
    .err(404, "not_found");
    // The operator names the agent.
    td.post(&format!("/api/repos/{id}/checkout"), json!({}))
        .await
        .err(422, "invalid");
    let r = td
        .post(
            &format!("/api/repos/{id}/checkout"),
            json!({"agent": "terra/worker", "path": "second"}),
        )
        .await;
    assert_eq!(r.ok()["action"], "cloned");
    assert!(ws.join("second/a.txt").is_file());
    td.post(
        &format!("/api/repos/{id}/checkout"),
        json!({"agent": "terra/worker", "path": "../x"}),
    )
    .await
    .err(422, "invalid");

    // A member cannot check out into a peer's workspace; another project's
    // agent cannot see the repo.
    td.agent("terra", "peer", json!({})).await;
    td.post_as(
        "terra/peer",
        &format!("/api/repos/{id}/checkout"),
        json!({"agent": "worker"}),
    )
    .await
    .err(403, "forbidden");
    td.project("other").await;
    td.agent("other", "x", json!({})).await;
    td.post_as("other/x", &format!("/api/repos/{id}/checkout"), json!({}))
        .await
        .err(403, "forbidden");
}

#[tokio::test]
async fn credential_helper_answers_only_its_project() {
    let (td, _origin, _remotes) = daemon_with_repo().await;
    td.post(
        "/api/credentials",
        json!({"name": "hub-token", "kind": "token", "value": "tok-123", "username": "bot"}),
    )
    .await
    .ok();
    td.agent("terra", "w", json!({})).await;
    td.project("other").await;
    td.agent("other", "x", json!({})).await;
    // The other project has a repo on the same host and path, with no
    // credential.
    td.post(
        "/api/projects/other/repos",
        json!({"url": "https://git.example.com/team/hub.git"}),
    )
    .await
    .ok();

    let ask = json!({"protocol": "https", "host": "git.example.com", "path": "team/hub.git"});
    let r = td
        .post_as("terra/w", "/api/repos/credential", ask.clone())
        .await;
    assert_eq!(r.ok()["username"], "bot");
    assert_eq!(r.body["password"], "tok-123");

    // Another project's agent: 404, so git falls back.
    td.post_as("other/x", "/api/repos/credential", ask.clone())
        .await
        .err(404, "not_found");
    // The operator is not an agent.
    td.post("/api/repos/credential", ask.clone())
        .await
        .err(403, "forbidden");
    // Another host or path, or no path: 404.
    for q in [
        json!({"protocol": "https", "host": "evil.example.com", "path": "team/hub.git"}),
        json!({"protocol": "https", "host": "git.example.com", "path": "team/other.git"}),
        json!({"protocol": "http", "host": "git.example.com", "path": "team/hub.git"}),
        json!({"protocol": "https", "host": "git.example.com"}),
    ] {
        td.post_as("terra/w", "/api/repos/credential", q)
            .await
            .err(404, "not_found");
    }
    // Each answer is audited.
    let audit = td.get("/api/audit?action=repo.credential").await;
    let rows = audit.ok().as_array().unwrap().clone();
    assert_eq!(rows.len(), 1);
    assert!(!audit.body.to_string().contains("tok-123"));

    // Rate limit per agent.
    td.agent("terra", "busy", json!({})).await;
    let mut limited = false;
    for _ in 0..40 {
        let r = td
            .post_as("terra/busy", "/api/repos/credential", ask.clone())
            .await;
        if r.status.as_u16() == 429 {
            assert_eq!(r.code(), "rate_limited");
            limited = true;
            break;
        }
        r.ok();
    }
    assert!(limited);
}

#[tokio::test]
async fn checkout_refuses_paths_in_the_state_dir() {
    let td = TestDaemon::start().await;
    td.project("p").await;
    let a = td.agent("p", "w", json!({})).await;
    td.agent("p", "victim", json!({})).await;
    // A project admin attaches a repo that points into the state dir.
    let victim = td.state.join("agents");
    let r = td
        .post(
            "/api/projects/p/repos",
            json!({"name": "steal", "url": victim.to_string_lossy()}),
        )
        .await;
    let id = r.ok()["id"].as_str().unwrap().to_owned();
    td.post_as("p/w", &format!("/api/repos/{id}/checkout"), json!({}))
        .await
        .err(409, "conflict");
    let ws = PathBuf::from(a["workspace"].as_str().unwrap());
    assert!(!ws.join("steal").exists());
}

/// A local repo under a repos dir (`file:///repos/<name>.git` in Docker):
/// the daemon clones it on agent create, and a push from the workspace
/// reaches it with no credential.
#[tokio::test]
async fn agent_create_clones_a_file_url_under_the_repos_dir() {
    let root = tempfile::tempdir().unwrap();
    let repos = root.path().join("repos");
    bare_repo(&repos);
    let origin = repos.join("origin.git");
    let url = format!("file://{}", origin.display());
    let td = TestDaemon::with_config(|c| {
        write(
            c,
            "projects/demo.toml",
            &format!(
                r#"
[[repos]]
name = "app"
url = "{url}"
default_branch = "main"

[checkout]
on_agent_create = ["app"]
"#
            ),
        );
    })
    .await;
    let created = td
        .post(
            "/api/agents",
            json!({"name": "w", "project": "demo", "harness": "claude"}),
        )
        .await;
    let a = created.ok().clone();
    let checkout = a["checkout"].as_array().unwrap();
    assert_eq!(checkout[0]["action"], "cloned", "{a}");
    assert_eq!(checkout[0]["head"], head_of(&origin));
    let dest = PathBuf::from(a["workspace"].as_str().unwrap()).join("app");
    assert!(dest.join("a.txt").is_file());
    // A branch pushed from the workspace lands in the bare repo.
    sh(
        &dest,
        "git checkout -q -b dem-1 && echo two > b.txt && git add b.txt && \
         git commit -q -m two && git push -q origin dem-1",
    );
    let o = Command::new("git")
        .arg("-C")
        .arg(&origin)
        .args(["rev-parse", "--verify", "dem-1"])
        .output()
        .unwrap();
    assert!(o.status.success());
}

/// The sandbox binds a local repo read-write into each agent of each
/// project that has it. So a `project_admin` cannot attach a local-path
/// URL (it could name the repo of another project); a `sysadmin` and the
/// operator can.
#[tokio::test]
async fn only_a_sysadmin_attaches_a_local_repo() {
    let td = TestDaemon::start().await;
    td.project("p").await;
    td.project("ops").await;
    td.agent("p", "lead", json!({"roles": ["project_admin"]}))
        .await;
    td.agent("ops", "root", json!({"roles": ["sysadmin"]}))
        .await;
    for url in ["file:///repos/other.git", "/repos/other.git"] {
        let r = td
            .post_as("p/lead", "/api/projects/p/repos", json!({"url": url}))
            .await;
        r.err(403, "forbidden");
        assert!(r.body.to_string().contains("sysadmin"), "{}", r.body);
    }
    // A remote URL stays a project_admin action.
    td.post_as(
        "p/lead",
        "/api/projects/p/repos",
        json!({"url": "https://git.example.com/team/hub.git"}),
    )
    .await
    .ok();
    td.post_as(
        "ops/root",
        "/api/projects/p/repos",
        json!({"url": "file:///repos/app.git", "default_branch": "main"}),
    )
    .await
    .ok();
    td.post(
        "/api/projects/p/repos",
        json!({"url": "file:///repos/lib.git"}),
    )
    .await
    .ok();
}

/// `branch` is an alias of `default_branch` in the repo body; both at once
/// is a `400` that names both, and so is an unknown field.
#[tokio::test]
async fn repo_body_takes_branch_as_an_alias_of_default_branch() {
    let td = TestDaemon::start().await;
    td.project("p").await;
    let r = td
        .post(
            "/api/projects/p/repos",
            json!({"url": "https://git.example.com/team/app.git", "branch": "dev"}),
        )
        .await;
    assert_eq!(r.ok()["default_branch"], "dev");
    let r = td
        .post(
            "/api/projects/p/repos",
            json!({"url": "https://git.example.com/team/lib.git", "name": "lib",
                   "branch": "dev", "default_branch": "main"}),
        )
        .await;
    r.err(400, "bad_request");
    let msg = r.body["error"]["message"].as_str().unwrap();
    assert!(
        msg.contains("`default_branch`") && msg.contains("`branch`"),
        "{msg}"
    );
    let r = td
        .post(
            "/api/projects/p/repos",
            json!({"url": "https://git.example.com/team/lib.git", "brnch": "dev"}),
        )
        .await;
    r.err(400, "bad_request");
    let msg = r.body["error"]["message"].as_str().unwrap();
    assert!(msg.contains("brnch"), "{msg}");
    assert!(
        msg.contains("`default_branch` (or its alias `branch`)"),
        "{msg}"
    );
}
