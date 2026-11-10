//! Repos: the `/api/credentials` alias of the secret store, checkouts into
//! workspaces, the credential
//! helper endpoint, and the auto-checkout on agent create. No network: the
//! remotes are local bare repos.
#![allow(clippy::unwrap_used, clippy::expect_used)]

use crate::common;

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

/// Run as the checkout's user, without the identity inherited by this test.
fn agent_git(dir: &Path, args: &[&str], env: &[(&str, &str)]) -> String {
    let mut cmd = Command::new("git");
    cmd.current_dir(dir).args(args);
    for k in [
        "GIT_AUTHOR_NAME",
        "GIT_AUTHOR_EMAIL",
        "GIT_COMMITTER_NAME",
        "GIT_COMMITTER_EMAIL",
        "TERRARIUM_GIT_TRAILERS",
        "TERRARIUM_GIT_TRAILERS_FILE",
    ] {
        cmd.env_remove(k);
    }
    cmd.envs(env.iter().copied());
    let o = cmd.output().unwrap();
    assert!(
        o.status.success(),
        "{args:?}: {}",
        String::from_utf8_lossy(&o.stderr)
    );
    String::from_utf8(o.stdout).unwrap()
}

#[tokio::test]
async fn agent_commits_have_bot_identity_and_current_terrarium_session() {
    let (td, _origin, _remotes) = daemon_with_repo().await;
    let a = td
        .agent("terra", "worker", json!({"model": "test-model"}))
        .await;
    let id = a["id"].as_str().unwrap().to_owned();
    let workspace = PathBuf::from(a["workspace"].as_str().unwrap());
    let dest = workspace.join("proj");
    let state = &td.daemon.as_ref().unwrap().state;
    let key = id.clone();
    let first = state
        .db
        .write("system", move |t| {
            terrariumd::store::agents::begin_session(t, &key, "test")
        })
        .await
        .unwrap();
    terrariumd::git_identity::refresh(state, &id).await.unwrap();
    let file = workspace
        .join(terrariumd::git_identity::TRAILERS_FILE)
        .display()
        .to_string();
    let env = [("TERRARIUM_GIT_TRAILERS_FILE", file.as_str())];
    agent_git(
        &dest,
        &[
            "commit",
            "--allow-empty",
            "-m",
            "feat: provenance",
            "-m",
            "pre-release: true\nbump: patch",
        ],
        &env,
    );
    let identity = agent_git(&dest, &["show", "-s", "--format=%an <%ae>%n%cn <%ce>"], &[]);
    assert_eq!(
        identity,
        "Terrarium <terrarium@localhost>\nTerrarium <terrarium@localhost>\n"
    );
    let parsed = agent_git(
        &dest,
        &["show", "-s", "--format=%(trailers:only,unfold)"],
        &[],
    );
    for expected in [
        format!("Terrarium-Session: {first}"),
        "Terrarium-Agent: terra/worker".into(),
        "Terrarium-Harness: claude unknown".into(),
        "Terrarium-Model: test-model".into(),
        "pre-release: true".into(),
        "bump: patch".into(),
    ] {
        assert!(parsed.contains(&expected), "missing {expected}: {parsed}");
    }
    assert!(!parsed.to_lowercase().contains("co-authored"));

    // A new session must replace the old trailer, including on --amend.
    let key = id.clone();
    let second = state
        .db
        .write("system", move |t| {
            terrariumd::store::agents::begin_session(t, &key, "test")
        })
        .await
        .unwrap();
    terrariumd::git_identity::refresh(state, &id).await.unwrap();
    agent_git(
        &dest,
        &["commit", "--allow-empty", "--amend", "--no-edit"],
        &env,
    );
    let parsed = agent_git(&dest, &["show", "-s", "--format=%(trailers:only)"], &[]);
    assert!(
        parsed.contains(&format!("Terrarium-Session: {second}")),
        "{parsed}"
    );
    assert!(!parsed.contains(&first), "{parsed}");
    assert_eq!(parsed.matches("Terrarium-Session:").count(), 1);
    assert!(parsed.contains("pre-release: true"));

    // Refresh replaces a planted link, never the file it names.
    let victim = workspace.join("victim");
    std::fs::write(&victim, "keep").unwrap();
    std::fs::remove_file(&file).unwrap();
    std::os::unix::fs::symlink(&victim, &file).unwrap();
    terrariumd::git_identity::refresh(state, &id).await.unwrap();
    assert_eq!(std::fs::read_to_string(&victim).unwrap(), "keep");
    assert!(
        !std::fs::symlink_metadata(&file)
            .unwrap()
            .file_type()
            .is_symlink()
    );
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

/// The repo browser (docs/api.md §5.3): the trees of a repo, files,
/// changes, commits, the headers of a raw file, and the refusals.
#[tokio::test]
async fn the_browser_reads_main_and_agent_trees() {
    let (td, _origin, _remotes) = daemon_with_repo().await;
    let created = td
        .post(
            "/api/agents",
            json!({"name": "worker", "project": "terra", "harness": "claude"}),
        )
        .await;
    let a = created.ok().clone();
    let agent_id = a["id"].as_str().unwrap().to_owned();
    let dest = PathBuf::from(a["workspace"].as_str().unwrap()).join("proj");
    // The agent works on a branch: one commit, one staged file, one
    // change in the work tree, one untracked file, a binary file.
    // This is browser fixture data, not an agent-authored commit. The
    // checkout has a production provenance hook, tested separately above.
    sh(
        &dest,
        "git checkout -q -b feat && printf 'one\\nmore\\n' > a.txt && mkdir -p d && \
         echo '# Doc' > d/x.md && git add . && \
         git -c core.hooksPath=/dev/null commit -q -m 'feat: more' && \
         echo staged > s.txt && git add s.txt && echo changed >> a.txt && echo u > u.txt && \
         printf '\\x89PNG\\0\\1' > img.png",
    );
    // Check the same full-message contract even on hosts where bwrap is
    // unavailable and the agent-tree API assertions below cannot run.
    assert_eq!(
        agent_git(&dest, &["show", "-s", "--format=%B"], &[]).trim(),
        "feat: more"
    );
    let repos = td.get("/api/projects/terra/repos").await;
    let id = repos
        .ok()
        .as_array()
        .unwrap()
        .iter()
        .find(|r| r["name"] == "proj")
        .unwrap()["id"]
        .as_str()
        .unwrap()
        .to_owned();
    let base = format!("/api/repos/{id}/trees");
    let tree = format!("agent:{agent_id}");

    // With no read sandbox (the host dev mode with no bubblewrap), an
    // agent tree is `503 unavailable`; the tests below need the sandbox.
    if !terrariumd::git::read_sandbox_ready() {
        // Every read of the agent tree, also the file list that runs no
        // git; `main` still works.
        for route in ["files", "file?path=a.txt", "diff", "commits"] {
            td.get(&format!("{base}/{tree}/{route}"))
                .await
                .err(503, "unavailable");
        }
        let t = td.get(&base).await;
        let trees = t.ok()["trees"].as_array().unwrap().clone();
        assert!(trees[0]["error"].is_null(), "{trees:#?}");
        assert!(
            trees[1]["error"].as_str().unwrap().contains("bwrap"),
            "{trees:#?}"
        );
        td.get(&format!("{base}/main/files")).await.ok();
        // Write directly so libtest does not hide this coverage gap when
        // the fallback assertions pass (normal output is captured).
        use std::io::Write as _;
        writeln!(
            std::io::stderr(),
            "SKIP the_browser_reads_main_and_agent_trees: agent-tree API assertions need a working bwrap read sandbox"
        )
        .unwrap();
        return;
    }

    let t = td.get(&base).await;
    let trees = t.ok()["trees"].as_array().unwrap().clone();
    assert_eq!(t.body["default_branch"], "main");
    assert_eq!(trees.len(), 2, "{trees:#?}");
    assert_eq!(trees[0]["id"], "main");
    assert!(trees[0]["mirror"]["fetched_at"].is_string(), "{trees:#?}");
    assert_eq!(trees[1]["id"], tree.as_str());
    assert_eq!(trees[1]["label"], "worker · feat");
    assert_eq!(trees[1]["ahead"], 1);
    assert_eq!(trees[1]["behind"], 0);
    assert_eq!(trees[1]["dirty"]["staged"], 1, "{trees:#?}");
    assert_eq!(trees[1]["dirty"]["unstaged"], 1);
    assert_eq!(trees[1]["dirty"]["untracked"], 2);
    // The mirror is the daemon's own, in the state dir.
    let mirror = td.state.join(format!("repo-mirrors/{id}.git"));
    assert!(mirror.join("HEAD").is_file());

    // Files: main from the mirror, the agent tree from its work tree.
    let l = td.get(&format!("{base}/main/files")).await;
    let names: Vec<&str> = l.ok()["entries"]
        .as_array()
        .unwrap()
        .iter()
        .map(|e| e["name"].as_str().unwrap())
        .collect();
    assert_eq!(names, ["a.txt"]);
    let l = td.get(&format!("{base}/{tree}/files")).await;
    let names: Vec<&str> = l.ok()["entries"]
        .as_array()
        .unwrap()
        .iter()
        .map(|e| e["name"].as_str().unwrap())
        .collect();
    assert_eq!(names, ["d", "a.txt", "img.png", "s.txt", "u.txt"]);
    let f = td.get(&format!("{base}/{tree}/file?path=d/x.md")).await;
    assert_eq!(f.status.as_u16(), 200);
    assert_eq!(f.headers["content-type"], "text/plain; charset=utf-8");
    assert_eq!(f.headers["x-content-type-options"], "nosniff");
    assert_eq!(f.headers["x-terrarium-size"], "6");
    assert_eq!(f.headers["x-terrarium-truncated"], "false");
    assert_eq!(f.body, "# Doc\n");
    let f = td.get(&format!("{base}/{tree}/file?path=img.png")).await;
    assert_eq!(f.headers["content-type"], "application/octet-stream");
    assert_eq!(f.headers["content-disposition"], "attachment");

    // Refusals.
    for bad in ["..", "/etc/passwd", ".git/config", "d/../../x"] {
        td.get(&format!("{base}/{tree}/file?path={bad}"))
            .await
            .err(422, "invalid");
    }
    td.get(&format!("{base}/agent:nope/files"))
        .await
        .err(404, "not_found");
    td.get(&format!("{base}/other/files"))
        .await
        .err(404, "not_found");
    td.get(&format!("{base}/main/commits/HEAD"))
        .await
        .err(422, "invalid");

    // Changes: committed and uncommitted.
    let d = td.get(&format!("{base}/{tree}/diff")).await;
    let c = &d.ok()["committed"]["files"];
    let paths: Vec<&str> = c
        .as_array()
        .unwrap()
        .iter()
        .map(|f| f["path"].as_str().unwrap())
        .collect();
    assert_eq!(paths, ["a.txt", "d/x.md"], "{}", d.body);
    assert_eq!(d.body["base_ref"], "refs/remotes/origin/main");
    let u = &d.body["uncommitted"];
    assert_eq!(u["staged"]["files"][0]["path"], "s.txt");
    assert_eq!(u["unstaged"]["files"][0]["path"], "a.txt");
    assert!(
        u["unstaged"]["files"][0]["patch"]
            .as_str()
            .unwrap()
            .contains("+changed")
    );
    assert_eq!(u["untracked"], json!(["img.png", "u.txt"]));
    let d = td.get(&format!("{base}/main/diff")).await;
    assert_eq!(d.ok()["note"], "main is the base");

    // Commits: the branch's own, then one commit with its diff.
    let c = td.get(&format!("{base}/{tree}/commits")).await;
    let list = c.ok().as_array().unwrap().clone();
    assert_eq!(list.len(), 1);
    assert_eq!(list[0]["subject"], "feat: more");
    assert!(c.next_cursor().is_none());
    let sha = list[0]["sha"].as_str().unwrap();
    let one = td.get(&format!("{base}/{tree}/commits/{sha}")).await;
    assert_eq!(one.ok()["message"], "feat: more");
    assert_eq!(one.body["diff"]["files"].as_array().unwrap().len(), 2);
    let m = td.get(&format!("{base}/main/commits?limit=1")).await;
    assert_eq!(m.ok().as_array().unwrap().len(), 1);
    assert!(m.next_cursor().is_none());

    // An agent of another project cannot read it; one of the project can.
    td.agent("terra", "peer", json!({})).await;
    td.get_as("terra/peer", &format!("{base}/{tree}/files"))
        .await
        .ok();
    td.project("other").await;
    td.agent("other", "x", json!({})).await;
    td.get_as("other/x", &base).await.err(403, "forbidden");

    // A planted repo config: the agent tree is refused, and the list says so.
    // No error answer names the state dir or the workspace, or has stderr.
    let workspace = dest.parent().unwrap().to_str().unwrap().to_owned();
    let state = td.state.to_str().unwrap().to_owned();
    let clean = |body: &serde_json::Value| {
        let text = body.to_string();
        assert!(!text.contains(&state), "{text}");
        assert!(!text.contains(&workspace), "{text}");
        assert!(!text.contains("stderr"), "{text}");
    };
    sh(&dest, "git config core.fsmonitor 'touch /tmp/never'");
    let r = td.get(&format!("{base}/{tree}/files")).await;
    clean(&r.body);
    r.err(409, "conflict");
    let t = td.get(&base).await;
    assert!(
        t.ok()["trees"][1]["error"]
            .as_str()
            .unwrap()
            .contains("core.fsmonitor"),
        "{}",
        t.body
    );
    clean(&t.body);
    sh(&dest, "git config --unset core.fsmonitor");

    // A symlinked HEAD: `409` with the path in `.git` only.
    sh(
        &dest,
        "mv .git/HEAD .git/HEAD.real && ln -s HEAD.real .git/HEAD",
    );
    let r = td.get(&format!("{base}/{tree}/commits")).await;
    clean(&r.body);
    assert!(r.body.to_string().contains(".git/HEAD"), "{}", r.body);
    r.err(409, "conflict");
    sh(&dest, "rm .git/HEAD && mv .git/HEAD.real .git/HEAD");

    // An unknown commit: `404`, with no stderr.
    let r = td.get(&format!("{base}/{tree}/commits/abcd1234")).await;
    clean(&r.body);
    r.err(404, "not_found");
}

/// Issue #1: a repo's `hooks_dir` and `push_deny` (from a project file and
/// the API) put hooks into the agent's checkout on agent create; bad
/// values are `422`.
#[tokio::test]
async fn a_checkout_gets_the_hooks_of_the_repo() {
    let root = tempfile::tempdir().unwrap();
    let origin = bare_repo(root.path());
    sh(
        &root.path().join("src"),
        "mkdir .githooks && printf '#!/bin/sh\\nexit 0\\n' > .githooks/commit-msg && \
         git add .githooks && git commit -q -m hooks && git push -q ../origin.git main",
    );
    let url = origin.to_string_lossy().into_owned();
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
hooks_dir = ".githooks"
push_deny = ["op/*"]

[checkout]
on_agent_create = ["app"]
"#
            ),
        );
    })
    .await;
    let repos = td.get("/api/projects/demo/repos").await;
    let repo = &repos.ok()[0];
    assert_eq!(repo["hooks_dir"], ".githooks", "{repo}");
    assert_eq!(repo["push_deny"], json!(["op/*"]));
    let created = td
        .post(
            "/api/agents",
            json!({"name": "w", "project": "demo", "harness": "claude"}),
        )
        .await;
    let a = created.ok().clone();
    let checkout = &a["checkout"][0];
    assert_eq!(checkout["action"], "cloned", "{a}");
    assert_eq!(
        checkout["hooks"],
        json!(["commit-msg", "pre-push", "prepare-commit-msg"]),
        "{a}"
    );
    let hooks = PathBuf::from(a["workspace"].as_str().unwrap()).join("app/.git/hooks");
    assert!(hooks.join("commit-msg").is_file());
    let guard = std::fs::read_to_string(hooks.join("pre-push")).unwrap();
    assert!(guard.contains("op/*"), "{guard}");

    // The API: the fields in the answer, and the checks.
    td.project("p").await;
    let r = td
        .post(
            "/api/projects/p/repos",
            json!({"url": "https://git.example.com/team/app.git",
                   "hooks_dir": "tools/hooks", "push_deny": ["op/*", "wip"]}),
        )
        .await;
    assert_eq!(r.ok()["hooks_dir"], "tools/hooks");
    assert_eq!(r.body["push_deny"], json!(["op/*", "wip"]));
    let plain = td
        .post(
            "/api/projects/p/repos",
            json!({"url": "https://git.example.com/team/plain.git"}),
        )
        .await;
    assert!(plain.ok().get("hooks_dir").is_none());
    assert!(plain.body.get("push_deny").is_none());
    for bad in [
        json!({"url": "https://git.example.com/team/x.git", "hooks_dir": "../x"}),
        json!({"url": "https://git.example.com/team/y.git", "hooks_dir": ".git/hooks"}),
        json!({"url": "https://git.example.com/team/z.git", "push_deny": ["$(id)"]}),
    ] {
        td.post("/api/projects/p/repos", bad)
            .await
            .err(422, "invalid");
    }
}

/// `DELETE /api/repos/{id}` (TER-658): project_admin only; refused while a
/// job, `next_repos`, a github pipeline, or a bound agent's checkout uses
/// the repo, and for a repo of a project file; audited; the clones stay.
#[tokio::test]
async fn repo_rm_refuses_while_in_use_then_removes() {
    let (td, _origin, _remotes) = daemon_with_repo().await;
    // A repo of the project file belongs to the file.
    let proj = td.get("/api/projects/terra/repos").await.ok().as_array().unwrap()
        .iter()
        .find(|r| r["name"] == "proj")
        .unwrap()["id"]
        .as_str()
        .unwrap()
        .to_owned();
    td.delete(&format!("/api/repos/{proj}"))
        .await
        .err(409, "file_managed");

    td.agent("terra", "lead", json!({"roles": ["project_admin"]}))
        .await;
    let w = td.agent("terra", "w", json!({})).await;
    let r = td
        .post_as(
            "terra/lead",
            "/api/projects/terra/repos",
            json!({"url": "https://github.com/Acme/App.git", "name": "app"}),
        )
        .await;
    assert_eq!(r.status, 201, "{}", r.body);
    let id = r.body["id"].as_str().unwrap().to_owned();
    let path = format!("/api/repos/{id}");

    // A member may not remove it; another project cannot see it.
    td.delete_as("terra/w", &path).await.err(403, "forbidden");
    td.project("other").await;
    td.agent("other", "x", json!({"roles": ["project_admin"]}))
        .await;
    td.delete_as("other/x", &path).await.err(403, "forbidden");

    let conn = terrariumd::db::open(&td.state.join("terrarium.db")).unwrap();
    let in_use = |r: common::Resp, what: &str| {
        r.err(409, "conflict");
        let msg = r.body["error"]["message"].as_str().unwrap();
        assert!(msg.contains(what), "{what}: {msg}");
    };

    // A queued job.
    conn.execute(
        "INSERT INTO jobs (id, project, card, kind, repo, branch, state, attempt, created_by,
                           created_at)
         VALUES ('j1', 'terra', 'TER-1', 'merge', 'app', 'b', 'queued', 1, 'operator',
                 '2026-11-10T00:00:00Z')",
        [],
    )
    .unwrap();
    in_use(td.delete_as("terra/lead", &path).await, "queued merge job of TER-1");
    // A finished job does not count.
    conn.execute("UPDATE jobs SET state = 'passed' WHERE id = 'j1'", [])
        .unwrap();

    // The delivery config.
    conn.execute(
        "INSERT INTO project_delivery (project, def, updated_by, updated_at)
         VALUES ('terra', '{\"next_repos\": [\"app\"]}', 'operator', '2026-11-10T00:00:00Z')",
        [],
    )
    .unwrap();
    in_use(td.delete_as("terra/lead", &path).await, "next_repos");
    conn.execute("DELETE FROM project_delivery WHERE project = 'terra'", [])
        .unwrap();

    // A github pipeline of the same owner/name, in another case.
    conn.execute(
        "INSERT INTO pipelines (id, project, name, adapter, enabled, def, created_by, run_as,
                                created_at, updated_at, anchor_at)
         VALUES ('P1', 'terra', 'ci', 'github', 0,
                 '{\"name\": \"ci\", \"source\": {\"kind\": \"github\", \"repo\": \"acme/app\"}}',
                 'operator', 'operator', '2026-11-10T00:00:00Z', '2026-11-10T00:00:00Z',
                 '2026-11-10T00:00:00Z')",
        [],
    )
    .unwrap();
    in_use(td.delete_as("terra/lead", &path).await, "pipeline `ci`");
    conn.execute("DELETE FROM pipelines WHERE id = 'P1'", [])
        .unwrap();

    // An open bound agent with a checkout at the repo's path.
    let ws = PathBuf::from(w["workspace"].as_str().unwrap());
    std::fs::create_dir_all(ws.join("app/.git")).unwrap();
    std::fs::write(ws.join("app/keep.txt"), "x").unwrap();
    conn.execute(
        "INSERT INTO spawns (id, owner, owner_name, project, name, template, harness, task,
         spawned_by, spawned_at, deadline_at, bound_kind, bound_id)
         VALUES (?1, 'operator', 'operator', 'terra', 'w', 'worker', 'claude', 'test',
         'operator', '2026-01-01', '2099-01-01', 'card', 'TER-1')",
        [w["id"].as_str().unwrap()],
    )
    .unwrap();
    in_use(td.delete_as("terra/lead", &path).await, "bound agent `w`");
    conn.execute(
        "UPDATE spawns SET finished_at = '2026-11-10T00:00:00Z' WHERE id = ?1",
        [w["id"].as_str().unwrap()],
    )
    .unwrap();

    // Free: removed, audited, and the clone stays.
    let r = td.delete_as("terra/lead", &path).await;
    assert_eq!(r.ok()["name"], "app");
    td.get(&path).await.err(404, "not_found");
    let names: Vec<String> = td.get("/api/projects/terra/repos").await.ok()
        .as_array()
        .unwrap()
        .iter()
        .map(|r| r["name"].as_str().unwrap().to_owned())
        .collect();
    assert!(!names.contains(&"app".to_owned()), "{names:?}");
    let audit = td.get("/api/audit?action=repo.delete").await;
    let rows = audit.ok().as_array().unwrap().clone();
    assert_eq!(rows.len(), 1, "{rows:?}");
    assert_eq!(rows[0]["target"], "terra/app");
    assert!(ws.join("app/keep.txt").is_file());
    td.delete_as("terra/lead", &path).await.err(404, "not_found");
}
