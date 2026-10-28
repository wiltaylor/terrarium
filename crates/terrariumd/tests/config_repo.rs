//! The config repo (PLAN §8.3, docs/api.md §25): the `.gitignore`
//! allowlist, the secret scan of daemon commits and of the `pre-commit`
//! hook, the export, and a sync of two instances through a local bare
//! remote. No real remote: every remote here is a temp bare repo.
#![allow(clippy::unwrap_used, clippy::expect_used)]

mod common;

use std::path::{Path, PathBuf};
use std::process::Command;

use common::{TestDaemon, write};
use serde_json::{Value, json};
use terrariumd::config_repo::ConfigRepo;

fn git(dir: &Path, args: &[&str]) -> String {
    let out = Command::new("git")
        .arg("-C")
        .arg(dir)
        .args(args)
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .env("GIT_CONFIG_NOSYSTEM", "1")
        .output()
        .unwrap();
    assert!(
        out.status.success(),
        "git {args:?}: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    String::from_utf8_lossy(&out.stdout).into_owned()
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

/// Every blob in every commit of `repo`, as text.
fn all_history(repo: &Path) -> String {
    let mut out = String::new();
    for rev in git(repo, &["rev-list", "--all"]).lines() {
        out.push_str(&git(repo, &["show", "--format=%B", "--stat", rev]));
        for f in git(repo, &["ls-tree", "-r", "--name-only", rev]).lines() {
            out.push_str(&git(repo, &["show", &format!("{rev}:{f}")]));
        }
    }
    out
}

#[tokio::test]
async fn init_writes_the_allowlist_and_only_known_paths_are_committed() {
    let (td, repo) = instance().await;
    for (rel, text) in [
        ("prompts/leader.md", "Lead.\n"),
        ("prompts/sub/extra.md", "Extra.\n"),
        ("harness/claude/settings.json", "{}\n"),
        ("harness/codex/config.toml", "model = \"m\"\n"),
        ("README.md", "# r\n"),
        // Not allowed: credential files, other types, other dirs.
        ("harness/claude/.credentials.json", "{}\n"),
        ("harness/pi/auth.json", "{}\n"),
        ("harness/claude/history.jsonl", "{}\n"),
        ("prompts/leader.md.new", "New.\n"),
        ("prompts/notes.txt", "x\n"),
        (".env", "X=1\n"),
        ("state/terrarium.db", "x"),
        ("sessions/a.jsonl", "x"),
        (".terrarium-defaults.json", "{}\n"),
        ("export/projects/p.fleet.toml", "version = 1\n"),
        ("export/key.pem", "x"),
    ] {
        write(&repo, rel, text);
    }
    let r = td
        .post(
            "/api/config/commit",
            json!({"message": "first", "export": false}),
        )
        .await;
    let files: Vec<String> = serde_json::from_value(r.ok()["files"].clone()).unwrap();
    let mut want = vec![
        ".gitignore",
        "README.md",
        "export/projects/p.fleet.toml",
        "harness/claude/settings.json",
        "harness/codex/config.toml",
        "prompts/leader.md",
        "prompts/sub/extra.md",
    ];
    want.sort_unstable();
    assert_eq!(files, want);
    assert_eq!(
        std::fs::read_to_string(repo.join(".gitignore")).unwrap(),
        terrariumd::config_repo::GITIGNORE
    );
    let hook = repo.join(".git/hooks/pre-commit");
    assert_eq!(
        std::fs::read_to_string(&hook).unwrap(),
        terrariumd::config_repo::HOOK
    );
    let s = td.get("/api/config/status").await;
    let s = s.ok();
    assert_eq!(s["initialized"], true);
    assert_eq!(s["branch"], "main");
    assert!(s["changes"].as_array().unwrap().is_empty(), "{s}");
    let log = td.get("/api/config/log").await;
    assert_eq!(log.ok()[0]["subject"], "first");
}

/// Each secret pattern blocks a daemon commit; nothing is committed, the
/// answer names the file, the line, and the rule, and never the value.
#[tokio::test]
async fn the_scan_blocks_each_secret_pattern() {
    let (td, repo) = instance().await;
    write(&repo, "prompts/ok.md", "fine\n");
    td.post(
        "/api/config/commit",
        json!({"message": "base", "export": false}),
    )
    .await
    .ok();
    let fake = |p: &str| format!("{p}{}", "FAKEfake0123456789".repeat(2));
    let cases = [
        ("anthropic-key", fake("sk-ant-api03-")),
        ("openrouter-key", fake("sk-or-v1-")),
        ("github-token", fake("ghp_")),
        ("github-token", fake("github_pat_")),
        ("aws-access-key", "AKIAFAKEFAKEFAKEFAKE".to_owned()),
        ("slack-token", "xoxb-0000-FAKE-fake".to_owned()),
        (
            "private-key",
            "-----BEGIN OPENSSH PRIVATE KEY-----".to_owned(),
        ),
        (
            "jwt",
            "eyJhbGciOiJIUzI1NiJ9.eyJzdWIiOiJmYWtlIn0.c2ln".to_owned(),
        ),
        ("terrarium-host-token", fake("thk_")),
        ("terrarium-host-token", fake("the_")),
        (
            "credential-key",
            "\"apiKey\": \"FAKEfakeFAKEfake1234\"".to_owned(),
        ),
        (
            "high-entropy",
            "Zx8Qp2Lm7Vt4Rn9Ks1Wd6Hy3Fj5Bc0GaUe".to_owned(),
        ),
    ];
    let head = git(&repo, &["rev-parse", "HEAD"]);
    for (rule, value) in cases {
        write(
            &repo,
            "prompts/leak.md",
            &format!("line one\nx {value} y\n"),
        );
        let r = td
            .post(
                "/api/config/commit",
                json!({"message": "leak", "export": false}),
            )
            .await;
        r.err(422, "invalid");
        let shown = r.body.to_string();
        assert!(
            !shown.contains(&value),
            "{rule}: the value is shown: {shown}"
        );
        let findings = r.body["error"]["details"]["findings"].as_array().unwrap();
        assert!(
            findings
                .iter()
                .any(|f| f["rule"] == rule && f["path"] == "prompts/leak.md" && f["line"] == 2),
            "{rule}: {shown}"
        );
        assert_eq!(git(&repo, &["rev-parse", "HEAD"]), head, "{rule}");
        // Unstaged again: a manual commit takes nothing by accident.
        assert_eq!(git(&repo, &["diff", "--cached", "--name-only"]), "");
    }
    std::fs::remove_file(repo.join("prompts/leak.md")).unwrap();
}

/// The `pre-commit` hook scans a commit made by hand with git.
#[tokio::test]
async fn the_hook_blocks_a_manual_commit_with_a_secret() {
    let (td, repo) = instance().await;
    write(&repo, "prompts/ok.md", "fine\n");
    td.post(
        "/api/config/commit",
        json!({"message": "base", "export": false}),
    )
    .await
    .ok();
    // PATH with only terrariumd from this build, and the system git.
    let bin = td.dir.path().join("bin");
    std::fs::create_dir_all(&bin).unwrap();
    std::os::unix::fs::symlink(env!("CARGO_BIN_EXE_terrariumd"), bin.join("terrariumd")).unwrap();
    let path = format!("{}:/usr/local/bin:/usr/bin:/bin", bin.display());
    let commit = |msg: &str| {
        Command::new("git")
            .arg("-C")
            .arg(&repo)
            .args([
                "-c",
                "user.name=t",
                "-c",
                "user.email=t@t",
                "commit",
                "-q",
                "-m",
                msg,
            ])
            .env("PATH", &path)
            .env("GIT_CONFIG_GLOBAL", "/dev/null")
            .output()
            .unwrap()
    };
    let secret = format!("ghp_{}", "FAKEfake0123456789".repeat(2));
    write(&repo, "prompts/leak.md", &format!("token {secret}\n"));
    git(&repo, &["add", "-A"]);
    let out = commit("leak");
    assert!(!out.status.success());
    let err = String::from_utf8_lossy(&out.stderr);
    assert!(err.contains("prompts/leak.md:1: github-token"), "{err}");
    assert!(!err.contains(&secret), "{err}");
    // A clean file commits.
    write(&repo, "prompts/leak.md", "nothing here\n");
    git(&repo, &["add", "-A"]);
    let out = commit("clean");
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    // With no Terrarium binary on PATH, the hook fails closed.
    write(&repo, "prompts/leak.md", "more\n");
    git(&repo, &["add", "-A"]);
    let out = Command::new("git")
        .arg("-C")
        .arg(&repo)
        .args([
            "-c",
            "user.name=t",
            "-c",
            "user.email=t@t",
            "commit",
            "-q",
            "-m",
            "x",
        ])
        .env("PATH", "/usr/local/bin:/usr/bin:/bin")
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .output()
        .unwrap();
    if !Path::new("/usr/local/bin/terrarium").exists() && !Path::new("/usr/bin/terrarium").exists()
    {
        assert!(!out.status.success());
    }
}

/// The export holds the projects, agents, targets, repos, templates, and
/// schedules, with no secret and no ID of this instance.
#[tokio::test]
async fn the_export_has_no_secrets_and_no_instance_ids() {
    let (td, repo) = instance().await;
    td.project("demo").await;
    td.post(
        "/api/credentials",
        json!({"name": "deploy", "kind": "token", "value": format!("ghp_{}", "FAKEfake0123456789".repeat(2))}),
    )
    .await
    .ok();
    td.post(
        "/api/projects/demo/repos",
        json!({"name": "app", "url": "https://user@example.com/o/app.git", "credential": "deploy"}),
    )
    .await
    .ok();
    td.agent(
        "demo",
        "lead",
        json!({"roles": ["project_admin"], "persona": "leader", "model": "haiku"}),
    )
    .await;
    td.post(
        "/api/templates",
        json!({"name": "worker", "project": "demo", "spec": {"harness": "codex", "persona": "worker"}}),
    )
    .await
    .ok();
    let r = td
        .post("/api/config/commit", json!({"message": "export"}))
        .await;
    let body = r.ok().clone();
    let export: Vec<String> = serde_json::from_value(body["export"].clone()).unwrap();
    assert_eq!(
        export,
        vec![
            "export/projects/demo.fleet.toml",
            "export/secrets/global.toml"
        ]
    );
    let secrets = std::fs::read_to_string(repo.join("export/secrets/global.toml")).unwrap();
    assert!(secrets.contains("name = \"deploy\""), "{secrets}");
    assert!(!secrets.contains("ghp_"), "{secrets}");
    assert!(
        terrarium_core::secret_scan::scan("export/secrets/global.toml", secrets.as_bytes())
            .is_empty()
    );
    let text = std::fs::read_to_string(repo.join("export/projects/demo.fleet.toml")).unwrap();
    assert!(text.contains("[[projects.agents]]"), "{text}");
    assert!(text.contains("persona = \"leader\""), "{text}");
    assert!(text.contains("credential = \"deploy\""), "{text}");
    assert!(
        text.contains("url = \"https://example.com/o/app.git\""),
        "{text}"
    );
    assert!(!text.contains("ghp_"), "{text}");
    assert!(!text.contains("user@"), "{text}");
    // No ULID of this instance.
    let ulid = regex::Regex::new(r"\b[0-9A-HJKMNP-TV-Z]{26}\b").unwrap();
    assert!(!ulid.is_match(&text), "{text}");
    assert!(
        terrarium_core::secret_scan::scan("export/projects/demo.fleet.toml", text.as_bytes())
            .is_empty()
    );
    // The export applies as a fleet: its plan is valid.
    let plan = td
        .post(
            "/api/fleet/plan",
            json!({"content": text, "path": "demo.fleet.toml"}),
        )
        .await;
    assert_eq!(plan.ok()["valid"], true, "{}", plan.body);
}

/// Two instances share a prompt edit both ways through a bare remote. The
/// remote URL and the credential stay out of `.git/config` and the repo.
#[tokio::test]
async fn two_instances_sync_a_prompt_edit_both_ways() {
    let tmp = tempfile::tempdir().unwrap();
    let remote = bare(tmp.path());
    let url = remote.display().to_string();
    let (a, repo_a) = instance().await;
    let (b, repo_b) = instance().await;
    let token = format!("ghp_{}", "FAKEtokenFAKEtoken12".repeat(2));
    for td in [&a, &b] {
        td.post(
            "/api/credentials",
            json!({"name": "config-remote", "kind": "token", "value": token}),
        )
        .await
        .ok();
        let s = td
            .put(
                "/api/config/remote",
                json!({"url": url, "credential": "config-remote"}),
            )
            .await;
        assert_eq!(s.ok()["remote"]["url"], url.as_str());
        assert_eq!(s.ok()["remote"]["credential"], "config-remote");
    }
    // A: edit, sync (commit + push).
    write(&repo_a, "prompts/leader.md", "Lead v1\n");
    let r = a
        .post("/api/config/sync", json!({"message": "a: leader v1"}))
        .await;
    assert_eq!(r.ok()["pushed"], true, "{}", r.body);
    // B: sync pulls it.
    let r = b.post("/api/config/sync", json!({})).await;
    let changed = r.ok()["changed"].as_array().unwrap().clone();
    assert!(changed.contains(&json!("prompts/leader.md")), "{}", r.body);
    assert_eq!(
        std::fs::read_to_string(repo_b.join("prompts/leader.md")).unwrap(),
        "Lead v1\n"
    );
    // B edits; A pulls it.
    write(&repo_b, "prompts/leader.md", "Lead v2\n");
    b.post("/api/config/sync", json!({"message": "b: leader v2"}))
        .await
        .ok();
    let r = a.post("/api/config/pull", json!({})).await;
    assert_eq!(r.ok()["updated"], true, "{}", r.body);
    assert_eq!(
        std::fs::read_to_string(repo_a.join("prompts/leader.md")).unwrap(),
        "Lead v2\n"
    );
    let s = a.get("/api/config/status").await;
    assert_eq!(s.ok()["ahead"], 0);
    assert_eq!(s.ok()["behind"], 0);

    // The remote and its credential are not in the repo or its config.
    for repo in [&repo_a, &repo_b] {
        let cfg = std::fs::read_to_string(repo.join(".git/config")).unwrap();
        assert!(!cfg.contains("[remote"), "{cfg}");
        assert!(!cfg.contains(&url), "{cfg}");
        assert!(!cfg.contains(&token), "{cfg}");
        assert!(!all_history(repo).contains(&token));
    }
    assert!(!all_history(&remote).contains(&token));
    let st = std::fs::read_to_string(a.state.join("config-repo.json")).unwrap();
    assert!(st.contains("config-remote") && !st.contains(&token), "{st}");
}

/// A conflict stops the sync with the files listed. The local commit
/// stays, the repo waits in the merge, and abort goes back to it.
#[tokio::test]
async fn a_conflict_stops_with_no_data_loss() {
    let tmp = tempfile::tempdir().unwrap();
    let remote = bare(tmp.path());
    let url = remote.display().to_string();
    let (a, repo_a) = instance().await;
    let (b, repo_b) = instance().await;
    for td in [&a, &b] {
        td.put("/api/config/remote", json!({"url": url})).await.ok();
    }
    write(&repo_a, "prompts/worker.md", "base\n");
    a.post("/api/config/sync", json!({"message": "base"}))
        .await
        .ok();
    b.post("/api/config/sync", json!({})).await.ok();
    write(&repo_a, "prompts/worker.md", "from a\n");
    a.post("/api/config/sync", json!({"message": "a"}))
        .await
        .ok();
    write(&repo_b, "prompts/worker.md", "from b\n");
    let r = b.post("/api/config/sync", json!({"message": "b"})).await;
    r.err(409, "conflict");
    assert_eq!(
        r.body["error"]["details"]["conflicts"],
        json!(["prompts/worker.md"])
    );
    // B's own commit is there.
    let log = git(&repo_b, &["log", "--format=%s"]);
    assert!(log.lines().any(|l| l == "b"), "{log}");
    let s = b.get("/api/config/status").await;
    assert_eq!(s.ok()["merging"], true);
    assert_eq!(s.ok()["conflicts"], json!(["prompts/worker.md"]));
    // A commit with the markers still in the file is refused.
    b.post(
        "/api/config/commit",
        json!({"message": "m", "export": false}),
    )
    .await
    .err(409, "conflict");
    // A sync while the merge waits is refused too.
    b.post("/api/config/sync", json!({}))
        .await
        .err(409, "conflict");
    // Abort: back to B's commit, with B's text.
    b.post("/api/config/abort", json!({})).await.ok();
    assert_eq!(
        std::fs::read_to_string(repo_b.join("prompts/worker.md")).unwrap(),
        "from b\n"
    );
    assert_eq!(git(&repo_b, &["log", "-1", "--format=%s"]).trim(), "b");
    // Resolve: sync again, take a hand-merged text, commit, sync.
    let r = b.post("/api/config/sync", json!({})).await;
    r.err(409, "conflict");
    write(&repo_b, "prompts/worker.md", "from a and b\n");
    b.post(
        "/api/config/commit",
        json!({"message": "merge", "export": false}),
    )
    .await
    .ok();
    let r = b.post("/api/config/sync", json!({})).await;
    assert_eq!(r.ok()["pushed"], true, "{}", r.body);
    a.post("/api/config/pull", json!({})).await.ok();
    assert_eq!(
        std::fs::read_to_string(repo_a.join("prompts/worker.md")).unwrap(),
        "from a and b\n"
    );
    // The history has both edits.
    let log = git(&repo_a, &["log", "--format=%s"]);
    for s in ["a", "b", "merge"] {
        assert!(log.lines().any(|l| l == s), "{s}: {log}");
    }
}

/// A pull that changes `export/` shows a fleet plan, and applies it only
/// with `apply`.
#[tokio::test]
async fn a_pulled_export_is_a_plan_until_apply() {
    let tmp = tempfile::tempdir().unwrap();
    let remote = bare(tmp.path());
    let url = remote.display().to_string();
    let (a, _) = instance().await;
    let (b, _) = instance().await;
    for td in [&a, &b] {
        td.put("/api/config/remote", json!({"url": url})).await.ok();
    }
    a.project("shared").await;
    a.agent("shared", "w1", json!({"persona": "none"})).await;
    a.post("/api/config/sync", json!({"message": "export"}))
        .await
        .ok();
    let r = b.post("/api/config/pull", json!({})).await;
    let body = r.ok().clone();
    let export = body["export"].as_array().unwrap();
    assert_eq!(export.len(), 1, "{body}");
    assert_eq!(export[0]["path"], "export/projects/shared.fleet.toml");
    assert_eq!(export[0]["plan"]["valid"], true, "{body}");
    assert!(
        b.get("/api/agents/shared%2Fw1")
            .await
            .status
            .is_client_error()
    );
    let s = b.get("/api/config/status").await;
    assert_eq!(s.ok()["export_pending"], true);
    // A commit on B now does not write B's store over the pulled export.
    let c = b
        .post("/api/config/commit", json!({"message": "nothing"}))
        .await;
    assert_eq!(c.ok()["export"], json!([]));
    let r = b.post("/api/config/pull", json!({"apply": true})).await;
    let body = r.ok().clone();
    assert!(body["export"][0]["applied"].is_object(), "{body}");
    let w1 = b.get("/api/agents/shared%2Fw1").await;
    assert_eq!(w1.ok()["persona"], "none");
    let s = b.get("/api/config/status").await;
    assert_eq!(s.ok()["export_pending"], false);
}

/// A pulled export brings the secret names as placeholders with no value,
/// and their grants. A second apply changes nothing; a secret with a value
/// stays.
#[tokio::test]
async fn a_pulled_export_makes_secret_placeholders_and_grants() {
    let tmp = tempfile::tempdir().unwrap();
    let remote = bare(tmp.path());
    let url = remote.display().to_string();
    let (a, repo_a) = instance().await;
    let (b, _) = instance().await;
    for td in [&a, &b] {
        td.put("/api/config/remote", json!({"url": url})).await.ok();
    }
    let token = format!("ghp_{}", "FAKEtokenFAKEtoken12".repeat(2));
    a.project("shared").await;
    a.agent("shared", "w1", json!({"persona": "none"})).await;
    a.post(
        "/api/secrets",
        json!({"name": "gh", "kind": "github_token", "fields": {"value": token}}),
    )
    .await
    .ok();
    a.post(
        "/api/secrets",
        json!({"scope": "shared", "name": "db", "kind": "login",
               "fields": {"username": "app", "password": "Pa55word-FAKE-fake-1234"}}),
    )
    .await
    .ok();
    a.post(
        "/api/secrets/global/gh/grants",
        json!({"grantee": "agent:shared/w1", "level": "read", "env": "GH_TOKEN"}),
    )
    .await
    .ok();
    a.post("/api/config/sync", json!({"message": "export"}))
        .await
        .ok();
    assert!(!all_history(&repo_a).contains(&token));
    assert!(!all_history(&remote).contains("Pa55word"));
    // B: a secret with a value that the export does not have.
    b.post(
        "/api/secrets",
        json!({"name": "mine", "kind": "api_key", "fields": {"value": "sk-FAKE-local-1234"}}),
    )
    .await
    .ok();
    let r = b.post("/api/config/pull", json!({})).await;
    let body = r.ok().clone();
    let changes = body["secrets"]["changes"].as_array().unwrap();
    assert!(
        changes.contains(&json!({"action": "create", "secret": "global/gh"})),
        "{body}"
    );
    assert!(
        b.get("/api/secrets/global/gh")
            .await
            .status
            .is_client_error()
    );
    let r = b.post("/api/config/pull", json!({"apply": true})).await;
    let body = r.ok().clone();
    assert!(body["secrets"]["error"].is_null(), "{body}");
    let gh = b.get("/api/secrets/global/gh").await;
    let gh = gh.ok().clone();
    assert_eq!(gh["has_value"], false, "{gh}");
    assert_eq!(gh["fields"]["host"], "github.com");
    let db = b.get("/api/secrets/shared/db").await;
    assert_eq!(db.ok()["has_value"], false);
    assert_eq!(db.ok()["fields"]["username"], "app");
    let g = b.get("/api/secrets/global/gh/grants").await;
    let g = g.ok().clone();
    assert_eq!(g[0]["grantee_label"], "agent:shared/w1", "{g}");
    assert_eq!(g[0]["env"], "GH_TOKEN");
    // The local secret with a value stays.
    let mine = b.get("/api/secrets/global/mine").await;
    assert_eq!(mine.ok()["has_value"], true);
    // The list shows the local secret and the placeholders.
    let list = b.get("/api/secrets").await;
    let names: Vec<String> = list
        .ok()
        .as_array()
        .unwrap()
        .iter()
        .map(|s| {
            format!(
                "{}/{}",
                s["scope"].as_str().unwrap(),
                s["name"].as_str().unwrap()
            )
        })
        .collect();
    assert!(names.contains(&"global/mine".to_owned()), "{names:?}");
    // The apply is done: nothing waits.
    let s = b.get("/api/config/status").await;
    assert_eq!(s.ok()["export_pending"], false);
}

#[tokio::test]
async fn remote_urls_with_secrets_and_agents_are_refused() {
    let (td, _) = instance().await;
    td.put(
        "/api/config/remote",
        json!({"url": "https://x:FAKE@example.com/r.git"}),
    )
    .await
    .err(422, "invalid");
    td.put(
        "/api/config/remote",
        json!({"url": "https://example.com/r.git", "credential": "missing"}),
    )
    .await
    .err(422, "invalid");
    // A member agent may not read or change the config repo.
    td.project("p").await;
    td.agent("p", "m", json!({})).await;
    let r = td.get_as("p/m", "/api/config/status").await;
    r.err(403, "forbidden");
    let r = td
        .post_as("p/m", "/api/config/commit", json!({"message": "x"}))
        .await;
    r.err(403, "forbidden");
    // A sysadmin agent may.
    td.agent("p", "root", json!({"roles": ["sysadmin"]})).await;
    td.get_as("p/root", "/api/config/status").await.ok();
}

#[tokio::test]
async fn pending_defaults_show_with_a_diff_and_accept_or_keep() {
    let (td, repo) = instance().await;
    let tmp = tempfile::tempdir().unwrap();
    let pack = |dir: &Path, text: &str| {
        write(dir, "prompts/leader.md", text);
        let m = terrariumd::defaults::make_manifest(dir).unwrap();
        std::fs::write(
            dir.join("manifest.json"),
            terrariumd::defaults::manifest_text(&m),
        )
        .unwrap();
        terrariumd::defaults::Defaults::load(dir).unwrap()
    };
    let v1 = pack(&tmp.path().join("v1"), "L1\n");
    terrariumd::defaults::seed(&v1, &repo).unwrap();
    write(&repo, "prompts/leader.md", "mine\n");
    let v2 = pack(&tmp.path().join("v2"), "L2\n");
    terrariumd::defaults::seed(&v2, &repo).unwrap();
    let p = td.get("/api/config/defaults").await;
    let p = p.ok().clone();
    assert_eq!(p[0]["path"], "prompts/leader.md");
    assert!(p[0]["diff"].as_str().unwrap().contains("+L2"), "{p}");
    let s = td.get("/api/config/status").await;
    assert_eq!(s.ok()["pending_defaults"], json!(["prompts/leader.md"]));
    let left = td
        .post(
            "/api/config/defaults/keep",
            json!({"path": "prompts/leader.md"}),
        )
        .await;
    assert_eq!(left.ok(), &Value::Array(vec![]));
    assert_eq!(
        std::fs::read_to_string(repo.join("prompts/leader.md")).unwrap(),
        "mine\n"
    );
    let v3 = pack(&tmp.path().join("v3"), "L3\n");
    terrariumd::defaults::seed(&v3, &repo).unwrap();
    td.post(
        "/api/config/defaults/accept",
        json!({"path": "prompts/leader.md"}),
    )
    .await
    .ok();
    assert_eq!(
        std::fs::read_to_string(repo.join("prompts/leader.md")).unwrap(),
        "L3\n"
    );
    td.post(
        "/api/config/defaults/accept",
        json!({"path": "prompts/leader.md"}),
    )
    .await
    .err(422, "invalid");
}
